//! Ed25519 identity primitives shared between hub and node.
//!
//! Keys are stored / transmitted as standard base64 (no padding stripped).
//! The canonical wire format for signatures is base64(raw 64-byte signature).
//! The canonical message format for request signing is:
//!   METHOD\nPATH\nTIMESTAMP\nSHA256(body_hex)

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{
    Signature, Signer, SigningKey, Verifier, VerifyingKey,
};
use rand::rngs::OsRng;
use serde_json::Value;
use sha2::{Digest, Sha256};

// ── key generation ────────────────────────────────────────────────────────────

/// Generate a fresh Ed25519 keypair.  Returns (private_b64, public_b64).
pub fn generate_keypair() -> (String, String) {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    (B64.encode(sk.to_bytes()), B64.encode(vk.to_bytes()))
}

/// Load a signing key from base64.
pub fn load_signing_key(b64: &str) -> Result<SigningKey> {
    let bytes: [u8; 32] = B64
        .decode(b64)
        .context("base64 decode")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Load a verifying key from base64.
pub fn load_verifying_key(b64: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = B64
        .decode(b64)
        .context("base64 decode")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("verifying key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).context("invalid verifying key")
}

// ── canonical serialisation ───────────────────────────────────────────────────

/// Deterministic JSON bytes: keys sorted, no extra whitespace.
pub fn canonical(value: &Value) -> Vec<u8> {
    canonical_value(value).into_bytes()
}

fn canonical_value(v: &Value) -> String {
    match v {
        Value::Null           => "null".into(),
        Value::Bool(b)        => b.to_string(),
        Value::Number(n)      => n.to_string(),
        Value::String(s)      => serde_json::to_string(s).unwrap(),
        Value::Array(arr)     => {
            let items: Vec<String> = arr.iter().map(canonical_value).collect();
            format!("[{}]", items.join(","))
        }
        Value::Object(map)    => {
            let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
            pairs.sort_by_key(|(k, _)| *k);
            let items: Vec<String> = pairs
                .into_iter()
                .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical_value(v)))
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

// ── signing / verification ────────────────────────────────────────────────────

/// Sign `message` with the given signing key.  Returns base64 signature.
pub fn sign(sk: &SigningKey, message: &[u8]) -> String {
    let sig: Signature = sk.sign(message);
    B64.encode(sig.to_bytes())
}

/// Verify a base64 signature over `message` against a base64 public key.
pub fn verify_signature(public_key_b64: &str, message: &[u8], sig_b64: &str) -> Result<()> {
    let vk  = load_verifying_key(public_key_b64)?;
    let raw = B64.decode(sig_b64).context("sig base64")?;
    let sig = Signature::from_slice(&raw).context("sig bytes")?;
    vk.verify(message, &sig).context("signature invalid")
}

// ── request signing ───────────────────────────────────────────────────────────

/// Header carrying the node id on every node request.
pub const HDR_NODE_ID: &str = "x-node-id";
/// Header carrying the unix timestamp (seconds) the request was signed at.
pub const HDR_NODE_TS: &str = "x-node-ts";
/// Header carrying a random per-request nonce (hex), used for replay protection.
pub const HDR_NODE_NONCE: &str = "x-node-nonce";
/// Header carrying the base64 Ed25519 signature over [`signing_message`].
pub const HDR_NODE_SIG: &str = "x-node-sig";

/// How far a request timestamp may drift from the hub clock, in seconds.
pub const SIG_WINDOW_SECS: u64 = 60;

/// Build the canonical signing message for an HTTP request.
///
/// `path` is the path *and* query string exactly as sent (e.g.
/// `/v0/nodes/ab12/jobs/next?model=qwen&wait_s=20`).
pub fn signing_message(
    method: &str,
    path: &str,
    timestamp: u64,
    nonce: &str,
    body: &[u8],
) -> Vec<u8> {
    let body_hash = hex::encode(Sha256::digest(body));
    format!("{method}\n{path}\n{timestamp}\n{nonce}\n{body_hash}").into_bytes()
}

/// Fresh random nonce for a signed request.
pub fn new_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Current unix time in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── node-id derivation ────────────────────────────────────────────────────────

/// Derive a node_id from a public key: first 16 hex chars of SHA256.
pub fn node_id_for(public_key_b64: &str) -> Result<String> {
    let vk = load_verifying_key(public_key_b64)?;
    let hash = Sha256::digest(vk.to_bytes());
    Ok(hex::encode(&hash[..8]))
}

// ── SHA-256 helper ────────────────────────────────────────────────────────────

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sign_verify() {
        let (sk_b64, vk_b64) = generate_keypair();
        let sk = load_signing_key(&sk_b64).unwrap();
        let msg = b"hello exchange";
        let sig = sign(&sk, msg);
        verify_signature(&vk_b64, msg, &sig).unwrap();
    }

    #[test]
    fn wrong_key_fails() {
        let (sk_b64, _)     = generate_keypair();
        let (_, vk2_b64)    = generate_keypair();
        let sk = load_signing_key(&sk_b64).unwrap();
        let sig = sign(&sk, b"msg");
        assert!(verify_signature(&vk2_b64, b"msg", &sig).is_err());
    }

    #[test]
    fn signing_message_binds_every_field() {
        let base = signing_message("POST", "/p", 1, "n", b"{}");
        assert_ne!(base, signing_message("GET",  "/p", 1, "n", b"{}"));
        assert_ne!(base, signing_message("POST", "/q", 1, "n", b"{}"));
        assert_ne!(base, signing_message("POST", "/p", 2, "n", b"{}"));
        assert_ne!(base, signing_message("POST", "/p", 1, "m", b"{}"));
        assert_ne!(base, signing_message("POST", "/p", 1, "n", b"[]"));
    }

    #[test]
    fn canonical_sorts_keys() {
        let v: Value = serde_json::json!({"z": 1, "a": 2, "m": 3});
        let c = String::from_utf8(canonical(&v)).unwrap();
        assert_eq!(c, r#"{"a":2,"m":3,"z":1}"#);
    }
}
