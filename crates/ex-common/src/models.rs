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
    /// The node's engine takes OpenAI-style `tools` for this model and returns
    /// the calls the model makes.  The hub sends tool-use jobs only to nodes
    /// that say so, and tests them on it like any other claim.
    #[serde(default)]
    pub tools:       bool,
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
///
/// The signature covers the JSON object the node sent, exactly as sent, and
/// not this struct written out again.  So a field can be added without
/// breaking either side: a reader that doesn't know it ignores it and still
/// verifies the signature over the object it was given, and a reader that
/// knows it takes its default when an older node left it out.  A new field
/// must therefore carry `#[serde(default)]`, and the struct must never refuse
/// unknown fields.
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
    /// The part of `prompt_tokens` the backend read from its prefix cache
    /// instead of computing; 0 when it doesn't say.  Never more than
    /// `prompt_tokens`.  Receipts from nodes that predate it don't carry it.
    #[serde(default)]
    pub cached_input_tokens: u64,
}

// ── tool use ──────────────────────────────────────────────────────────────────

/// A function call a model made, as a node reports it and the hub hands it on.
/// The hub gives each call the id its buyer sees; a node's own ids are not used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// The arguments as the model wrote them: a JSON document in a string.  It
    /// is not checked to be valid JSON, because models sometimes write
    /// something that isn't, and the buyer's code is the judge of that.
    pub arguments: String,
}

/// Most calls one answer may carry.
pub const MAX_TOOL_CALLS_PER_ANSWER: usize = 64;
/// Longest function name a node may report, in bytes.
pub const MAX_TOOL_CALL_NAME_BYTES: usize = 256;

/// The text a model's whole answer comes to: its content, then each call on a
/// line of its own as `⟦name arguments⟧`, with the arguments in canonical JSON
/// (keys sorted, no spaces) when they parse and as the plain string when they
/// don't.  With no calls it is the content itself.
///
/// This is the text a receipt's `output_sha256` covers, the text a test job's
/// answer is graded on, and the text the one-token-per-three-bytes billing cap
/// counts.  Node and hub both build it from the same fields, so they agree on
/// the hash without the receipt carrying a new field.  The framing is two
/// characters on purpose: grading counts tokens, and a long frame around a
/// short call would let a wrong function name and wrong arguments through as
/// "agreeing" on the frame.
pub fn answer_text<'a>(content: &'a str, calls: &[ToolCall]) -> std::borrow::Cow<'a, str> {
    if calls.is_empty() {
        return std::borrow::Cow::Borrowed(content);
    }
    let mut out = String::from(content);
    for call in calls {
        let arguments = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
            Ok(v)  => String::from_utf8_lossy(&crate::identity::canonical(&v)).into_owned(),
            Err(_) => call.arguments.clone(),
        };
        out.push_str("\n\u{27E6}");
        out.push_str(&call.name);
        out.push(' ');
        out.push_str(&arguments);
        out.push('\u{27E7}');
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall { name: name.into(), arguments: arguments.into() }
    }

    #[test]
    fn an_answer_without_calls_is_its_content() {
        assert!(matches!(answer_text("hello", &[]), std::borrow::Cow::Borrowed("hello")));
        assert_eq!(answer_text("", &[]), "");
    }

    #[test]
    fn calls_are_rendered_in_canonical_json_so_key_order_and_spacing_do_not_matter() {
        let a = answer_text("", &[call("get_weather", r#"{"unit": "c", "city": "Paris"}"#)]);
        let b = answer_text("", &[call("get_weather", r#"{"city":"Paris","unit":"c"}"#)]);
        assert_eq!(a, b);
        assert_eq!(a, "\n\u{27E6}get_weather {\"city\":\"Paris\",\"unit\":\"c\"}\u{27E7}");
        // content comes first, calls follow in order
        let c = answer_text("Checking.", &[call("a", "{}"), call("b", "{\"x\":1}")]);
        assert_eq!(c, "Checking.\n\u{27E6}a {}\u{27E7}\n\u{27E6}b {\"x\":1}\u{27E7}");
    }

    #[test]
    fn arguments_that_are_not_json_are_kept_as_a_string() {
        let t = answer_text("", &[call("f", "{broken")]);
        assert_eq!(t, "\n\u{27E6}f {broken\u{27E7}");
        // a different mangling is a different answer
        assert_ne!(t, answer_text("", &[call("f", "{broken!")]));
    }

    #[test]
    fn a_model_ad_without_tools_does_not_serve_them() {
        let ad: ModelAd = serde_json::from_value(serde_json::json!({
            "name": "m", "hash": "sha256:x", "context": 8192, "tok_per_sec": 10.0,
        })).unwrap();
        assert!(!ad.tools);
    }

    #[test]
    fn a_receipt_without_prompt_tokens_bills_no_input() {
        let old: JobReceipt = serde_json::from_value(serde_json::json!({
            "job_id": "j", "node_id": "n", "output_sha256": "00", "model_hash": "sha256:x", "output_tokens": 3,
        })).unwrap();
        assert_eq!(old.prompt_tokens, 0);
        let new = JobReceipt { prompt_tokens: 12, ..old };
        assert_eq!(serde_json::to_value(&new).unwrap()["prompt_tokens"], 12, "signed with the rest");
    }

    /// A receipt as a node built before `cached_input_tokens` writes it.
    fn old_shape() -> serde_json::Value {
        serde_json::json!({
            "job_id": "a_1f", "node_id": "n1", "output_sha256": "00", "model_hash": "sha256:x",
            "output_tokens": 3, "prompt_tokens": 12,
        })
    }

    /// Sign `receipt` as a node does and check it as a hub does: the
    /// signature is over the canonical form of the object as it was sent.
    fn signs_and_verifies(receipt: &serde_json::Value) -> JobReceipt {
        use crate::identity::{canonical, generate_keypair, load_signing_key, sign, verify_signature};
        let (sk, pk) = generate_keypair();
        let signature = sign(&load_signing_key(&sk).unwrap(), &canonical(receipt));
        let parsed: JobReceipt = serde_json::from_value(receipt.clone()).expect("every field typed");
        verify_signature(&pk, &canonical(receipt), &signature).expect("the signature covers the object as sent");
        parsed
    }

    #[test]
    fn a_receipt_from_an_older_node_verifies_and_claims_no_cached_tokens() {
        let old = signs_and_verifies(&old_shape());
        assert_eq!((old.prompt_tokens, old.cached_input_tokens), (12, 0));
    }

    #[test]
    fn a_receipt_with_cached_tokens_verifies_and_the_count_is_signed() {
        use crate::identity::{canonical, generate_keypair, load_signing_key, sign, verify_signature};
        let receipt = JobReceipt { cached_input_tokens: 8, ..serde_json::from_value(old_shape()).unwrap() };
        let value = serde_json::to_value(&receipt).unwrap();
        assert_eq!(value["cached_input_tokens"], 8);
        assert_eq!(signs_and_verifies(&value), receipt);
        // a count changed after signing no longer verifies
        let (sk, pk) = generate_keypair();
        let signature = sign(&load_signing_key(&sk).unwrap(), &canonical(&value));
        let mut forged = value.clone();
        forged["cached_input_tokens"] = serde_json::json!(0);
        assert!(verify_signature(&pk, &canonical(&forged), &signature).is_err());
    }

    #[test]
    fn a_hub_that_predates_a_field_still_reads_and_verifies_the_receipt() {
        /// The receipt as a hub built before `cached_input_tokens` knows it.
        #[derive(Deserialize)]
        struct Before {
            #[allow(dead_code)]
            job_id: String,
            #[serde(default)]
            prompt_tokens: u64,
        }
        use crate::identity::{canonical, generate_keypair, load_signing_key, sign, verify_signature};
        let mut new = old_shape();
        new["cached_input_tokens"] = serde_json::json!(8);
        let (sk, pk) = generate_keypair();
        let signature = sign(&load_signing_key(&sk).unwrap(), &canonical(&new));
        // it ignores the field it doesn't know, and checks the signature over the object it was sent
        let read: Before = serde_json::from_value(new.clone()).unwrap();
        assert_eq!(read.prompt_tokens, 12);
        verify_signature(&pk, &canonical(&new), &signature).unwrap();
    }

    #[test]
    fn an_unknown_context_is_4096() {
        assert_eq!((effective_context(0), effective_context(32768)), (4096, 32768));
    }
}
