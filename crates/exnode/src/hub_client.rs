//! Signed HTTP client for the hub.  Every request carries the node id, a
//! timestamp, a fresh nonce and an Ed25519 signature over all of them plus the
//! exact body bytes.

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};

use ex_common::identity::{
    new_nonce, sign, signing_message, unix_now, HDR_NODE_ID, HDR_NODE_NONCE, HDR_NODE_SIG, HDR_NODE_TS,
};

/// A hub reply the agent acts on.
#[derive(Debug)]
pub enum HubError {
    /// The job is no longer this node's to run (consumer gone, reassigned).
    JobGone,
    Other(anyhow::Error),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::JobGone  => f.write_str("job no longer assigned to this node"),
            HubError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for HubError {
    fn from(e: anyhow::Error) -> Self { HubError::Other(e) }
}

pub struct HubClient {
    client:  Client,
    base:    String,
    node_id: String,
    sk:      SigningKey,
}

impl HubClient {
    pub fn new(base_url: &str, node_id: &str, sk: SigningKey) -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(90))
                .redirect(reqwest::redirect::Policy::none())
                .https_only(!crate::config::is_local(base_url))
                .build()
                .expect("reqwest client"),
            base:    base_url.trim_end_matches('/').to_string(),
            node_id: node_id.to_string(),
            sk,
        }
    }

    pub fn node_id(&self) -> &str { &self.node_id }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> Result<(StatusCode, Value)> {
        let bytes = match body {
            Some(b) => serde_json::to_vec(b)?,
            None    => Vec::new(),
        };
        let ts    = unix_now();
        let nonce = new_nonce();
        let sig   = sign(&self.sk, &signing_message(method.as_str(), path, ts, &nonce, &bytes));

        let mut req = self.client
            .request(method, format!("{}{}", self.base, path))
            .header(HDR_NODE_ID, &self.node_id)
            .header(HDR_NODE_TS, ts.to_string())
            .header(HDR_NODE_NONCE, &nonce)
            .header(HDR_NODE_SIG, sig);
        if body.is_some() {
            req = req.header("content-type", "application/json").body(bytes);
        }
        let resp   = req.send().await.with_context(|| format!("hub {path}"))?;
        let status = resp.status();
        let text   = resp.text().await.unwrap_or_default();
        let value  = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(json!({"raw": text})) };
        Ok((status, value))
    }

    fn error_text(v: &Value) -> String {
        v["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
    }

    pub async fn register(&self, public_key_b64: &str, models: &[Value], enroll_token: Option<&str>) -> Result<Value> {
        let body = json!({
            "node_id":      self.node_id,
            "public_key":   public_key_b64,
            "models":       models,
            "enroll_token": enroll_token,
        });
        let (status, v) = self.send(Method::POST, "/v0/nodes/register", Some(&body)).await?;
        if !status.is_success() {
            bail!("register failed ({status}): {}", Self::error_text(&v));
        }
        Ok(v)
    }

    pub async fn heartbeat(&self) -> Result<String> {
        let path = format!("/v0/nodes/{}/heartbeat", self.node_id);
        let (status, v) = self.send(Method::POST, &path, Some(&json!({}))).await?;
        if !status.is_success() {
            bail!("heartbeat failed ({status}): {}", Self::error_text(&v));
        }
        Ok(v["status"].as_str().unwrap_or("unknown").to_string())
    }

    /// Long-poll for the next job.  `None` means no work within the wait.
    pub async fn next_job(&self, model: &str, wait_secs: u64) -> Result<Option<Value>> {
        let path = format!(
            "/v0/nodes/{}/jobs/next?model={}&wait_s={}",
            self.node_id, urlencode(model), wait_secs
        );
        let (status, v) = self.send(Method::GET, &path, None).await?;
        match status {
            StatusCode::NO_CONTENT => Ok(None),
            s if s.is_success()    => Ok(Some(v)),
            s => bail!("next_job failed ({s}): {}", Self::error_text(&v)),
        }
    }

    pub async fn stream_chunk(&self, job_id: &str, seq: u64, deltas: &[String]) -> Result<(), HubError> {
        let path = format!("/v0/jobs/{job_id}/stream");
        let (status, v) = self.send(Method::POST, &path, Some(&json!({"seq": seq, "deltas": deltas}))).await?;
        let gone = match status {
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => true,
            StatusCode::CONFLICT => Self::error_text(&v).contains("not running"),
            _ => false,
        };
        if status.is_success() {
            Ok(())
        } else if gone {
            Err(HubError::JobGone)
        } else {
            Err(HubError::Other(anyhow::anyhow!("stream failed ({status}): {}", Self::error_text(&v))))
        }
    }

    pub async fn finish_job(&self, job_id: &str, output_tokens: u64, receipt: &Value, signature: &str) -> Result<Value> {
        let path = format!("/v0/jobs/{job_id}/finish");
        let body = json!({"output_tokens": output_tokens, "receipt": receipt, "signature": signature});
        let (status, v) = self.send(Method::POST, &path, Some(&body)).await?;
        if !status.is_success() {
            bail!("finish failed ({status}): {}", Self::error_text(&v));
        }
        Ok(v)
    }

    /// Public market feed: demand multipliers, unmet requests and bounties.
    pub async fn market(&self) -> Result<Value> {
        let resp = self.client.get(format!("{}/v0/market", self.base)).send().await.context("hub /v0/market")?;
        if !resp.status().is_success() {
            bail!("market failed ({})", resp.status());
        }
        Ok(resp.json().await?)
    }

    pub async fn fail_job(&self, job_id: &str, error: &str) -> Result<()> {
        let path = format!("/v0/jobs/{job_id}/fail");
        let _ = self.send(Method::POST, &path, Some(&json!({"error": error}))).await?;
        Ok(())
    }

    /// Give up a job for a reason the hub acts on, such as
    /// [`ex_common::models::FAIL_CONTEXT_EXCEEDED`].
    pub async fn fail_job_with_code(&self, job_id: &str, error: &str, code: &str) -> Result<()> {
        let path = format!("/v0/jobs/{job_id}/fail");
        let (status, v) = self.send(Method::POST, &path, Some(&json!({"error": error, "code": code}))).await?;
        if !status.is_success() {
            bail!("fail failed ({status}): {}", Self::error_text(&v));
        }
        Ok(())
    }
}

/// Percent-encode a query value (model names carry ':' and '/').
fn urlencode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}
