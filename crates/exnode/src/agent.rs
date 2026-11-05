//! Node agent: register, heartbeat, and serve jobs from the local backend.
//!
//! Output streams to the hub as it is generated, in numbered chunks.  When the
//! backend finishes, the agent signs a receipt binding the job, the SHA-256 of
//! the exact output, the model hash and the output and input token counts.
//!
//! Each job runs in the context window advertised for its model.  A job that
//! overflows it gets no receipt: the agent reports `context_exceeded`, and
//! the hub re-queues the job for a node with a larger window.
//!
//! The agent never logs prompts or outputs, never listens on a port, and talks
//! only to the hub and the local backend.
//!
//! With [`Agent::with_safety_checks`] (the `exnode` binary always sets it) the
//! agent runs [`crate::checks`] before registering and again every
//! [`checks::CHECK_INTERVAL`].  A failing check stops it asking the hub for
//! jobs (heartbeats go on) until the checks pass again; a job already taken
//! finishes.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Semaphore};
use tracing::{error, info, warn};

use ex_common::identity::{canonical, sha256_hex, sign};
use ex_common::models::{effective_context, JobReceipt, FAIL_CONTEXT_EXCEEDED};

use crate::{
    backend::{Backend, BackendKind, GenError},
    checks::{self, Level, Report},
    config::NodeConfig,
    hub_client::{HubClient, HubError},
};

/// Deltas sent per stream request at most.
const MAX_BATCH: usize = 256;
/// How often the agent reads the market feed.
const MARKET_INTERVAL: Duration = Duration::from_secs(300);

/// Log lines summarising the market for an operator: the models this node
/// serves, then up to three in-demand models it doesn't.
pub fn market_report(market: &Value, served: &[&str]) -> Vec<String> {
    let describe = |m: &Value| {
        let mut s = format!(
            "{}: {}x demand, {} nodes, queue {}",
            m["model"].as_str().unwrap_or("?"),
            m["demand_multiplier"].as_str().unwrap_or("1"),
            m["active_nodes"].as_u64().unwrap_or(0),
            m["queue"].as_str().unwrap_or("?"),
        );
        let unmet = m["unmet_requests_1h"].as_u64().unwrap_or(0);
        if unmet > 0 { s += &format!(", {unmet} turned away in the last hour"); }
        if let Some(rate) = m["bounty"]["bonus_rate"].as_str() {
            s += &format!(", bounty +{}%", (rate.parse::<f64>().unwrap_or(0.0) * 100.0).round());
        }
        s
    };
    let models = market["models"].as_array().cloned().unwrap_or_default();
    let mut lines: Vec<String> = models.iter()
        .filter(|m| served.contains(&m["model"].as_str().unwrap_or("")))
        .map(|m| format!("market (serving) {}", describe(m)))
        .collect();
    let wanted: Vec<String> = models.iter()
        .filter(|m| !served.contains(&m["model"].as_str().unwrap_or("")))
        .filter(|m| m["demand_multiplier"].as_str() != Some("1")
            || m["unmet_requests_1h"].as_u64().unwrap_or(0) > 0
            || !m["bounty"].is_null())
        .take(3)
        .map(|m| format!("market (in demand elsewhere) {}", describe(m)))
        .collect();
    lines.extend(wanted);
    for b in market["bounties"].as_array().into_iter().flatten() {
        let pattern = b["model_pattern"].as_str().unwrap_or("?");
        if pattern.ends_with('*') && !served.iter().any(|s| s.starts_with(pattern.trim_end_matches('*'))) {
            let rate = b["bonus_rate"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            lines.push(format!("market bounty +{}% for any {pattern} model", (rate * 100.0).round()));
        }
    }
    lines
}

/// A receipt as the hub verifies it, and the node's signature over its
/// canonical JSON (every field, the input token count included).
pub fn signed_receipt(sk: &SigningKey, receipt: &JobReceipt) -> Result<(Value, String)> {
    let value = serde_json::to_value(receipt)?;
    let signature = sign(sk, &canonical(&value));
    Ok((value, signature))
}

pub struct Agent {
    cfg:          NodeConfig,
    hub:          Arc<HubClient>,
    backend:      Arc<Backend>,
    sk:           SigningKey,
    model_hashes: HashMap<String, String>,
    /// The context window each model runs in: what is advertised for it
    /// (4,096 when advertised as 0, as the hub takes it).
    windows:      HashMap<String, u32>,
    /// Run the safety checks before serving and while serving.
    safety_checks: bool,
}

impl Agent {
    pub async fn new(cfg: NodeConfig, node_id: String, sk: SigningKey) -> Result<Self> {
        let kind    = cfg.backend_kind()?;
        let hub     = Arc::new(HubClient::new(&cfg.hub_url, &node_id, sk.clone()));
        let backend = Arc::new(Backend::new(kind, &cfg.backend_url(), cfg.backend_api_key()));

        let mut model_hashes = HashMap::new();
        for m in &cfg.models {
            let hash = if !m.hash.is_empty() {
                m.hash.clone()
            } else if kind == BackendKind::Ollama {
                backend.model_hash(&m.name).await?
            } else {
                anyhow::bail!("model {} needs a hash", m.name);
            };
            info!(model = %m.name, %hash, context = effective_context(m.context), "serving model");
            model_hashes.insert(m.name.clone(), hash);
        }
        let windows = cfg.models.iter().map(|m| (m.name.clone(), effective_context(m.context))).collect();
        Ok(Self { cfg, hub, backend, sk, model_hashes, windows, safety_checks: false })
    }

    /// Check the backend and host before serving and every
    /// [`checks::CHECK_INTERVAL`] while serving ([`crate::checks`]).
    pub fn with_safety_checks(mut self) -> Self {
        self.safety_checks = true;
        self
    }

    /// Run the safety checks once and log them.  An error when the node must
    /// not serve: [`checks::Refused`] for a vulnerable or exposed backend.
    pub async fn preflight(&self) -> Result<Report> {
        let report = checks::run(&self.cfg, &self.backend).await;
        report.log();
        report.ensure_serving()?;
        Ok(report)
    }

    /// The models as the hub's register call takes them.
    fn advertised(&self) -> Vec<Value> {
        self.cfg.models.iter().map(|m| json!({
            "name":    m.name,
            "hash":    self.model_hashes[&m.name],
            "context": m.context,
        })).collect()
    }

    /// Register (first run) or reconnect (later runs), with the enrollment
    /// token from EXNODE_ENROLL_TOKEN or node.toml, if any.
    pub async fn register(&self, public_key_b64: &str) -> Result<()> {
        let token = std::env::var("EXNODE_ENROLL_TOKEN").ok().or_else(|| self.cfg.enroll_token.clone());
        let resp  = self.register_with(public_key_b64, token.as_deref()).await?;
        if resp["status"] == "pending" {
            info!("waiting for the hub to activate this node");
        }
        Ok(())
    }

    /// Register with exactly this `enroll_token` (`exnode init --token`);
    /// the hub's reply (`node_id`, `status`, `tier`).
    pub async fn register_with(&self, public_key_b64: &str, enroll_token: Option<&str>) -> Result<Value> {
        let resp = self.hub.register(public_key_b64, &self.advertised(), enroll_token).await?;
        info!(
            status = resp["status"].as_str().unwrap_or("?"),
            tier   = resp["tier"].as_str().unwrap_or("?"),
            "registered with hub",
        );
        Ok(resp)
    }

    /// Run until the process is stopped.
    pub async fn run(self, public_key_b64: String) -> Result<()> {
        // Checked before the hub hears of us: a refused node never registers.
        let first = if self.safety_checks { Some(self.preflight().await?) } else { None };
        self.register(&public_key_b64).await?;
        let agent = Arc::new(self);
        let sem   = Arc::new(Semaphore::new(agent.cfg.max_concurrency));

        // Open while the safety checks pass; the job loops wait on it.
        let gate = first.map(|report| {
            let (tx, rx) = watch::channel(true);
            tokio::spawn(watch_safety(agent.clone(), tx, report));
            rx
        });

        let hb = agent.clone();
        tokio::spawn(async move {
            let mut last = String::new();
            loop {
                match hb.hub.heartbeat().await {
                    Ok(status) => {
                        if status != last { info!(%status, "node status"); last = status; }
                    }
                    Err(e) => warn!("heartbeat failed: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(hb.cfg.heartbeat_secs)).await;
            }
        });

        // Market watch: tell the operator where demand and bounties are.
        let mk = agent.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            loop {
                match mk.hub.market().await {
                    Ok(m) => {
                        let served: Vec<&str> = mk.cfg.models.iter().map(|m| m.name.as_str()).collect();
                        for line in market_report(&m, &served) {
                            info!("{line}");
                        }
                    }
                    Err(e) => warn!("market feed unavailable: {e}"),
                }
                tokio::time::sleep(MARKET_INTERVAL).await;
            }
        });

        let mut loops = Vec::new();
        for model in agent.cfg.models.iter().map(|m| m.name.clone()) {
            let a   = agent.clone();
            let sem = sem.clone();
            let mut gate = gate.clone();
            loops.push(tokio::spawn(async move {
                loop {
                    if let Some(g) = gate.as_mut() {
                        if g.wait_for(|open| *open).await.is_err() {
                            error!(%model, "refusing to serve: the safety checks stopped running");
                            return;
                        }
                    }
                    let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
                    match a.hub.next_job(&model, a.cfg.poll_secs).await {
                        Ok(None) => drop(permit),
                        Ok(Some(job)) => {
                            let a2 = a.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                a2.handle_job(job).await;
                            });
                        }
                        Err(e) => {
                            drop(permit);
                            warn!(%model, "poll failed: {e}");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            }));
        }
        for l in loops {
            l.await?;
        }
        // the loops end only when the safety checks stopped: let the service manager restart us
        anyhow::bail!("stopped serving: the safety checks stopped running")
    }

    async fn handle_job(&self, job: Value) {
        let job_id     = job["job_id"].as_str().unwrap_or_default().to_string();
        let model      = job["model"].as_str().unwrap_or_default().to_string();
        let messages   = job["messages"].as_array().cloned().unwrap_or_default();
        let params     = job["params"].clone();
        let max_tokens = job["max_tokens"].as_u64().unwrap_or(512) as u32;
        let (Some(model_hash), Some(&window)) = (self.model_hashes.get(&model).cloned(), self.windows.get(&model)) else {
            let _ = self.hub.fail_job(&job_id, "model not served by this node").await;
            return;
        };
        info!(%job_id, %model, "job started");

        let (tx, rx)  = mpsc::unbounded_channel::<String>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let forwarder = tokio::spawn(forward(self.hub.clone(), job_id.clone(), rx, cancelled.clone()));

        let mut output = String::new();
        let generated = self.backend.chat_stream(&model, &messages, &params, max_tokens, window, |delta| {
            if cancelled.load(Ordering::Relaxed) {
                return false;
            }
            output.push_str(delta);
            tx.send(delta.to_string()).is_ok()
        }).await;
        drop(tx);
        let forwarded = forwarder.await.unwrap_or_else(|e| Err(HubError::Other(e.into())));

        match (generated, forwarded) {
            (_, Err(HubError::JobGone)) => {
                info!(%job_id, "job withdrawn by the hub; stopped");
            }
            (Err(GenError::ContextExceeded), _) => {
                // refused, not truncated: the hub re-queues it for a larger window
                info!(%job_id, window, "job does not fit the context window; refused");
                let _ = self.hub.fail_job_with_code(&job_id, "context window exceeded", FAIL_CONTEXT_EXCEEDED).await;
            }
            (Err(e), _) => {
                error!(%job_id, "backend failed: {e}");
                let _ = self.hub.fail_job(&job_id, "backend failed").await;
            }
            (_, Err(e)) => {
                error!(%job_id, "could not stream output: {e}");
                let _ = self.hub.fail_job(&job_id, "output stream to hub failed").await;
            }
            (Ok(gen), Ok(())) if gen.cancelled => {
                info!(%job_id, "job cancelled");
            }
            (Ok(gen), Ok(())) => {
                let receipt = JobReceipt {
                    job_id:        job_id.clone(),
                    node_id:       self.hub.node_id().to_string(),
                    output_sha256: sha256_hex(output.as_bytes()),
                    model_hash,
                    output_tokens: gen.output_tokens,
                    prompt_tokens: gen.prompt_tokens,
                };
                let (receipt, signature) = match signed_receipt(&self.sk, &receipt) {
                    Ok(r)  => r,
                    Err(e) => {
                        error!(%job_id, "could not sign the receipt: {e}");
                        let _ = self.hub.fail_job(&job_id, "receipt signing failed").await;
                        return;
                    }
                };
                match self.hub.finish_job(&job_id, gen.output_tokens, &receipt, &signature).await {
                    Ok(v)  => info!(%job_id, billed = v["billed_tokens"].as_u64().unwrap_or(0),
                                    billed_input = v["billed_input_tokens"].as_u64().unwrap_or(0),
                                    pay = v["pay"].as_str().unwrap_or("?"), "job settled"),
                    Err(e) => error!(%job_id, "settlement failed: {e}"),
                }
            }
        }
    }
}

/// Re-run the safety checks for as long as the agent runs, holding `gate`
/// open while they pass.  Logs when serving stops or resumes, and when the
/// reasons or warnings change, not on every pass.
async fn watch_safety(agent: Arc<Agent>, gate: watch::Sender<bool>, first: Report) {
    const BLOCKING: &[Level] = &[Level::Fail, Level::Refuse];
    let mut serving  = true;
    let mut reasons  = String::new();
    let mut warnings = first.summary(&[Level::Warn]);
    loop {
        tokio::time::sleep(if serving { checks::CHECK_INTERVAL } else { checks::RECHECK_INTERVAL }).await;
        let report = checks::run(&agent.cfg, &agent.backend).await;
        let ok     = report.allows_serving();
        let now    = report.summary(BLOCKING);
        if !ok && (serving || now != reasons) {
            for c in report.at_least(Level::Fail) {
                error!("refusing to serve: {}: {}", c.name, c.detail);
            }
            error!("not taking jobs until the safety checks pass (checking every {} s)",
                   checks::RECHECK_INTERVAL.as_secs());
        }
        if ok && !serving {
            info!("safety checks pass again; taking jobs");
        }
        let warned = report.summary(&[Level::Warn]);
        if warned != warnings {
            for c in report.checks.iter().filter(|c| c.level == Level::Warn) {
                warn!("check {}: {}", c.name, c.detail);
            }
        }
        (serving, reasons, warnings) = (ok, now, warned);
        gate.send_replace(ok);
    }
}

/// Send output deltas to the hub in order, batching whatever has queued up
/// while the previous request was in flight.  Retries transient failures.
async fn forward(
    hub: Arc<HubClient>,
    job_id: String,
    mut rx: mpsc::UnboundedReceiver<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), HubError> {
    let mut seq = 0u64;
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(d)  => batch.push(d),
                Err(_) => break,
            }
        }
        let mut attempt = 0;
        loop {
            match hub.stream_chunk(&job_id, seq, &batch).await {
                Ok(()) => break,
                Err(HubError::JobGone) => {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(HubError::JobGone);
                }
                Err(e) if attempt < 3 => {
                    attempt += 1;
                    warn!(%job_id, attempt, "stream retry: {e}");
                    tokio::time::sleep(Duration::from_millis(200 * attempt)).await;
                }
                Err(e) => {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        seq += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ex_common::identity::{generate_keypair, load_signing_key, verify_signature};

    #[test]
    fn the_receipt_signs_its_input_token_count() {
        let (sk_b64, pk_b64) = generate_keypair();
        let sk = load_signing_key(&sk_b64).unwrap();
        let receipt = JobReceipt {
            job_id: "j1".into(), node_id: "n1".into(), output_sha256: sha256_hex(b"hi"),
            model_hash: "sha256:abc".into(), output_tokens: 1, prompt_tokens: 17,
        };
        let (value, sig) = signed_receipt(&sk, &receipt).unwrap();
        assert_eq!(value["prompt_tokens"], 17);
        verify_signature(&pk_b64, &canonical(&value), &sig).unwrap();
        // a hub that changed the count would break the signature
        let mut forged = value.clone();
        forged["prompt_tokens"] = json!(500);
        assert!(verify_signature(&pk_b64, &canonical(&forged), &sig).is_err());
    }

    #[test]
    fn market_report_covers_own_models_and_nudges_toward_demand() {
        let m = json!({
            "models": [
                {"model": "llama4:70b", "demand_multiplier": "1", "active_nodes": 0, "queue": "empty", "unmet_requests_1h": 12, "bounty": null},
                {"model": "qwen3:30b", "demand_multiplier": "1.5", "active_nodes": 3, "queue": "busy", "unmet_requests_1h": 0,
                 "bounty": {"bonus_rate": "0.15"}},
                {"model": "gemma3:12b", "demand_multiplier": "1", "active_nodes": 4, "queue": "empty", "unmet_requests_1h": 0, "bounty": null},
            ],
            "bounties": [{"model_pattern": "qwen3-coder*", "bonus_rate": "0.1"}],
        });
        let lines = market_report(&m, &["qwen3:30b"]);
        assert_eq!(lines, vec![
            "market (serving) qwen3:30b: 1.5x demand, 3 nodes, queue busy, bounty +15%".to_string(),
            "market (in demand elsewhere) llama4:70b: 1x demand, 0 nodes, queue empty, 12 turned away in the last hour".to_string(),
            "market bounty +10% for any qwen3-coder* model".to_string(),
        ]);
    }
}
