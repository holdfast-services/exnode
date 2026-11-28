//! Enrollment: `exnode init --token` and `exnode setup` both come here.
//!
//! Creates the node's key (mode 0600) unless it exists, runs the safety
//! checks, and registers with the hub using the one-time `qd_reg_…` token.
//! The token is never written to disk or printed.

use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

use ex_common::identity::{generate_keypair, load_signing_key, node_id_for};

use crate::{agent::Agent, config::NodeConfig, keyfile};

/// The prefix every enrollment token starts with.
pub const TOKEN_PREFIX: &str = "qd_reg_";

/// What the hub said when the node enrolled.
#[derive(Debug, Clone)]
pub struct Enrolled {
    pub node_id: String,
    pub status:  String,
    pub tier:    String,
}

/// A token as typed or piped: trimmed, and refused unless it reads as an
/// enrollment token.  The refusal never repeats what was given.
pub fn parse_token(raw: &str) -> Result<String> {
    let t = raw.trim();
    let rest = t.strip_prefix(TOKEN_PREFIX);
    match rest {
        Some(r) if !r.is_empty() && r.len() <= 256
            && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') => Ok(t.to_string()),
        _ => bail!("that is not an enrollment token (they start with {TOKEN_PREFIX})"),
    }
}

/// Enroll this node with `token`, using (or creating) the key at `key`.
/// Refuses, before the hub hears of the node, when a safety check fails.
pub async fn enroll(mut cfg: NodeConfig, key: &str, token: &str) -> Result<Enrolled> {
    let token = parse_token(token)?;
    cfg.key_file = key.to_string();
    // the same key again if enrollment is being retried
    if !std::path::Path::new(key).exists() {
        keyfile::write_key_file(key, &generate_keypair().0)?;
    }
    let sk      = load_signing_key(&keyfile::read_key_file(key)?)?;
    let pub_b64 = STANDARD.encode(sk.verifying_key().as_bytes());
    let node_id = node_id_for(&pub_b64)?;

    let agent = Agent::new(cfg, node_id.clone(), sk).await?;
    agent.preflight().await?; // never enroll a node that would refuse to serve
    let resp = agent.register_with(&pub_b64, Some(&token)).await?;
    Ok(Enrolled {
        node_id,
        status: resp["status"].as_str().unwrap_or("?").to_string(),
        tier:   resp["tier"].as_str().unwrap_or("?").to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_checked_without_echoing_them() {
        let real = format!("qd_reg_{}", "ab12".repeat(12));
        assert_eq!(parse_token(&format!("  {real}\n")).unwrap(), real);
        assert!(parse_token("").is_err() && parse_token("qd_reg_").is_err());
        for bad in ["qd_key_abc123", "qd_reg_a b", "qd_reg_a;rm", "hunter2"] {
            let e = parse_token(bad).unwrap_err().to_string();
            assert!(!e.contains(bad), "{e}");
        }
    }
}
