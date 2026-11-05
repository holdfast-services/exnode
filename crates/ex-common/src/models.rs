//! Domain types shared between hub and node.
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

// ── tiers ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeTier {
    Community = 0,
    Verified  = 1,
    Reserved  = 2,
}

impl NodeTier {
    pub fn rate_multiplier(&self) -> Decimal {
        match self {
            NodeTier::Community => Decimal::from_str("1.0").unwrap(),
            NodeTier::Verified  => Decimal::from_str("1.3").unwrap(),
            NodeTier::Reserved  => Decimal::from_str("1.8").unwrap(),
        }
    }
}

impl std::fmt::Display for NodeTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NodeTier::Community => "community",
            NodeTier::Verified  => "verified",
            NodeTier::Reserved  => "reserved",
        };
        write!(f, "{s}")
    }
}

impl FromStr for NodeTier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "community" => Ok(NodeTier::Community),
            "verified"  => Ok(NodeTier::Verified),
            "reserved"  => Ok(NodeTier::Reserved),
            _           => Err(anyhow::anyhow!("unknown tier: {s}")),
        }
    }
}

// ── node status ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Pending,
    Active,
    Paused,
    Draining,
    /// Taken out of service by a verification penalty (a wrong model hash, or
    /// a second failed test on a community node).  Gets no jobs; an operator
    /// may reverse it.
    Suspended,
    Banned,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NodeStatus::Pending   => "pending",
            NodeStatus::Active    => "active",
            NodeStatus::Paused    => "paused",
            NodeStatus::Draining  => "draining",
            NodeStatus::Suspended => "suspended",
            NodeStatus::Banned    => "banned",
        };
        write!(f, "{s}")
    }
}

// ── key scope ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyScope {
    Full,
    Spend,
    Read,
}

// ── constants ─────────────────────────────────────────────────────────────────

/// Platform fee rate: 8 % of job value charged to both consumer and provider.
pub fn platform_fee_rate() -> Decimal {
    Decimal::from_str("0.08").unwrap()
}

/// Monthly credit decay rate: 1.5 %.
pub fn monthly_decay_rate() -> Decimal {
    Decimal::from_str("0.015").unwrap()
}

/// Required bond for verified-tier nodes.
pub fn verified_bond() -> Decimal {
    Decimal::from(50_000u32)
}

/// Credits owed to provider for `output_tokens` at the node's tier.
pub fn credits_for_job(output_tokens: u64, tier: NodeTier) -> Decimal {
    Decimal::from(output_tokens) * tier.rate_multiplier()
}

/// The sampling parameters a buyer may set and a node may see (an allowlist:
/// anything else in a request never leaves the hub).  `seed` is not among
/// them: a buyer's fixed seed would tag every job it sends.
pub const INFERENCE_PARAMS: &[&str] = &["temperature", "top_p", "stop", "presence_penalty", "frequency_penalty"];

// ── wire types ────────────────────────────────────────────────────────────────

/// Model advertisement from a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAd {
    pub name:        String,
    pub hash:        String,  // sha256:<hex>
    /// Context window in tokens; 0 means unknown, taken as [`UNKNOWN_CONTEXT`].
    pub context:     u32,
    pub tok_per_sec: f32,
}

/// The window assumed for a model advertised with a context of 0 (unknown).
pub const UNKNOWN_CONTEXT: u32 = 4096;

/// A model's usable context window: the advertised one, or
/// [`UNKNOWN_CONTEXT`] when it was advertised as 0.  Hub and node both use it,
/// so they agree on the window the node runs with.
pub fn effective_context(advertised: u32) -> u32 {
    if advertised == 0 { UNKNOWN_CONTEXT } else { advertised }
}

/// The `code` a node reports, with `POST /v0/jobs/{id}/fail`, when a job's
/// prompt and output don't fit its context window.  It refuses the job rather
/// than truncate it.  The hub then re-queues the job once for a node with a
/// larger window, and pays and charges nothing for the attempt.
pub const FAIL_CONTEXT_EXCEEDED: &str = "context_exceeded";

/// Receipt bound to a completed job — signed by the node's Ed25519 key over
/// its canonical JSON (every field below, `prompt_tokens` included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobReceipt {
    pub job_id:       String,
    pub node_id:      String,
    pub output_sha256: String,
    pub model_hash:   String,
    pub output_tokens: u64,
    /// Input tokens the backend reported.  Receipts from nodes that predate
    /// input billing don't carry it, and bill no input.
    #[serde(default)]
    pub prompt_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_without_prompt_tokens_bills_no_input() {
        let old: JobReceipt = serde_json::from_value(serde_json::json!({
            "job_id": "j", "node_id": "n", "output_sha256": "00", "model_hash": "sha256:x", "output_tokens": 3,
        })).unwrap();
        assert_eq!(old.prompt_tokens, 0);
        let new = JobReceipt { prompt_tokens: 12, ..old };
        assert_eq!(serde_json::to_value(&new).unwrap()["prompt_tokens"], 12, "signed with the rest");
    }

    #[test]
    fn an_unknown_context_is_4096() {
        assert_eq!((effective_context(0), effective_context(32768)), (4096, 32768));
    }
}
