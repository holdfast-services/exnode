//! Node configuration loaded from TOML plus `EXNODE_*` environment overrides.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use ex_common::schedule::{parse_blackout, Rule, Schedule, Zone};

use crate::{backend::BackendKind, hours::ServeHours};

/// One line of the weekly schedule (`[[schedule]]` in node.toml): on these days, in this window,
/// bring this share of the machine.  Rules are read in order and the first that covers a moment decides it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScheduleRule {
    /// `mon` … `sun`, `weekdays`, `weekends`.  Empty or absent: every day.
    #[serde(default)]
    pub days: Vec<String>,
    /// `18:00-08:00` (24-hour; a window may cross midnight, and its morning belongs to the day it
    /// started on), or `all day`.
    pub hours: String,
    /// Percent of `max_concurrency` to bring in this window, 1 to 100.
    #[serde(default = "full_share")]
    pub share: u32,
}

fn full_share() -> u32 { 100 }

/// Dates when nothing is served, whatever the schedule says (`[blackout]` in node.toml).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Blackout {
    /// `2026-12-25`, or a span `2026-12-24..2027-01-02`.
    #[serde(default)]
    pub dates: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeConfig {
    /// Hub base URL, e.g. "https://hub.example.com"
    pub hub_url: String,

    /// Path to the Ed25519 private key file (base64, must be mode 0600).
    pub key_file: String,

    /// One-time enrollment token from the console.  Needed only on the first
    /// run; prefer the EXNODE_ENROLL_TOKEN environment variable.
    #[serde(default)]
    pub enroll_token: Option<String>,

    /// "ollama" (default) or "openai" for any OpenAI-compatible server.
    #[serde(default = "default_backend")]
    pub backend: String,

    /// Backend base URL.  Defaults to `ollama_url` for older configs.
    #[serde(default)]
    pub backend_url: Option<String>,

    /// Legacy name for the Ollama URL.
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// Name of an environment variable holding the backend's API key, if any.
    #[serde(default)]
    pub backend_api_key_env: Option<String>,

    /// Models to advertise and serve.
    pub models: Vec<ModelConfig>,

    /// Heartbeat interval in seconds.  The console marks a node stale after 45 s.
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,

    /// How long to long-poll the hub for a job, in seconds (hub caps at 30).
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,

    /// Maximum concurrent jobs.
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,

    /// Daily serving window in local time, e.g. "18:00-08:00" ([`ServeHours`]).
    /// Outside it the agent takes no jobs but keeps heartbeating.  Empty or
    /// absent: always.  A shorthand for one `[[schedule]]` rule on every day;
    /// use one or the other, not both.
    #[serde(default)]
    pub serve_hours: Option<String>,

    /// The clock the schedule is read on: a zone name such as `America/Chicago`.  Absent: this
    /// machine's local time (a datacenter server is usually on UTC, so name the zone).
    #[serde(default)]
    pub timezone: Option<String>,

    /// The weekly schedule: when to bring capacity to the exchange, and how much.
    #[serde(default)]
    pub schedule: Vec<ScheduleRule>,

    /// Dates when nothing is served.
    #[serde(default)]
    pub blackout: Blackout,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    /// Model name exactly as the backend knows it, e.g. "qwen3:30b".
    pub name: String,
    /// Model digest.  Fetched from Ollama when empty; required for "openai".
    #[serde(default)]
    pub hash: String,
    /// Context window size in tokens.
    #[serde(default = "default_context")]
    pub context: u32,
    /// The engine takes OpenAI-style `tools` for this model and returns the
    /// calls the model makes: Ollama with a model whose template has tools,
    /// vLLM started with `--enable-auto-tool-choice --tool-call-parser …`,
    /// llama-server started with `--jinja`.  The hub sends tool-use jobs to a
    /// node only if it says so, and tests it on it like any other claim.
    #[serde(default)]
    pub tools: bool,
}

fn default_backend()        -> String { "ollama".into() }
fn default_ollama_url()     -> String { "http://127.0.0.1:11434".into() }
fn default_heartbeat_secs() -> u64    { 15 }
fn default_poll_secs()      -> u64    { 20 }
fn default_concurrency()    -> usize  { 2 }
fn default_context()        -> u32    { 4096 }

impl NodeConfig {
    /// Load from a TOML file with environment overrides, e.g.
    /// `EXNODE_HUB_URL=https://…`, `EXNODE_ENROLL_TOKEN=qd_reg_…`.
    pub fn load(path: &str) -> Result<Self> {
        let cfg: Self = config::Config::builder()
            .add_source(config::File::with_name(path).format(config::FileFormat::Toml))
            .add_source(config::Environment::with_prefix("EXNODE").separator("__"))
            .build()?
            .try_deserialize()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let kind = self.backend_kind()?;
        if self.models.is_empty() {
            bail!("node.toml lists no models");
        }
        if kind == BackendKind::OpenAi {
            if let Some(m) = self.models.iter().find(|m| m.hash.is_empty()) {
                bail!("model {} needs a `hash` with the openai backend", m.name);
            }
        }
        if !self.hub_url.starts_with("https://") && !is_local(&self.hub_url) {
            bail!("hub_url must use https:// (plain http is allowed only for a hub on localhost)");
        }
        if !is_local(&self.backend_url()) {
            bail!("backend_url must point at this machine (localhost or 127.0.0.1)");
        }
        if self.max_concurrency == 0 {
            bail!("max_concurrency must be at least 1");
        }
        self.serve_window()?;
        self.duty_schedule()?;
        Ok(())
    }

    /// When this machine serves and how much of it: `serve_hours` or the `[[schedule]]` rules, on
    /// the configured clock, less the blackout dates.  With neither, always, in full.
    pub fn duty_schedule(&self) -> Result<Schedule> {
        let zone = Zone::parse(self.timezone.as_deref()).map_err(anyhow::Error::msg)?;
        let hours = self.serve_hours.as_deref().map(str::trim).filter(|h| !h.is_empty());
        if hours.is_some() && !self.schedule.is_empty() {
            bail!("set serve_hours or [[schedule]] rules, not both: serve_hours is a shorthand for one rule on every day");
        }
        let mut rules = Vec::new();
        if let Some(h) = hours {
            // the old key's own parser says what it expects ("always" and empty mean no rule)
            if ServeHours::parse(h).map_err(anyhow::Error::msg)?.is_some() {
                rules.push(Rule::parse(&[], h, 100).map_err(anyhow::Error::msg)?);
            }
        }
        for (i, r) in self.schedule.iter().enumerate() {
            rules.push(Rule::parse(&r.days, &r.hours, r.share).map_err(|e| anyhow::anyhow!("[[schedule]] rule {}: {e}", i + 1))?);
        }
        let blackout = parse_blackout(&self.blackout.dates).map_err(anyhow::Error::msg)?;
        Schedule::new(zone, rules, blackout).map_err(anyhow::Error::msg)
    }

    /// The serving window, `None` for always.
    pub fn serve_window(&self) -> Result<Option<ServeHours>> {
        ServeHours::parse(self.serve_hours.as_deref().unwrap_or("")).map_err(anyhow::Error::msg)
    }

    pub fn backend_kind(&self) -> Result<BackendKind> { self.backend.parse() }

    pub fn backend_url(&self) -> String {
        self.backend_url.clone().unwrap_or_else(|| self.ollama_url.clone())
    }

    pub fn backend_api_key(&self) -> Option<String> {
        self.backend_api_key_env.as_ref().and_then(|v| std::env::var(v).ok()).filter(|k| !k.is_empty())
    }
}

/// True for a URL whose host is this machine.
pub fn is_local(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
}

/// A URL safe to print in logs and `exnode doctor`: no user name,
/// password, query or fragment, any of which could carry a secret.
pub fn display_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "(not a valid URL)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_urls_carry_no_credentials() {
        assert_eq!(display_url("https://user:pw@hub.example.com/x?token=abc#f"), "https://hub.example.com/x");
        assert_eq!(display_url("http://127.0.0.1:11434"), "http://127.0.0.1:11434/");
        assert_eq!(display_url("not a url"), "(not a valid URL)");
    }

    fn minimal(extra: &str) -> Result<NodeConfig> {
        let cfg: NodeConfig = toml::from_str(&format!(
            "hub_url = \"https://hub.example.com\"\nkey_file = \"k\"\n{extra}\n[[models]]\nname = \"m\"\n"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn serve_hours_are_optional_and_validated() {
        assert_eq!(minimal("").unwrap().serve_window().unwrap(), None);
        assert_eq!(minimal("serve_hours = \"\"").unwrap().serve_window().unwrap(), None);
        let w = minimal("serve_hours = \"18:00-08:00\"").unwrap().serve_window().unwrap().unwrap();
        assert_eq!(w.to_string(), "18:00-08:00");
        let e = minimal("serve_hours = \"6pm-8am\"").unwrap_err().to_string();
        assert!(e.contains("serve_hours"), "{e}");
        assert!(minimal("serve_hours = \"08:00-08:00\"").is_err());
    }

    #[test]
    fn local_urls_only() {
        assert!(is_local("http://127.0.0.1:11434"));
        assert!(is_local("http://localhost:8000/"));
        assert!(is_local("http://[::1]:8000"));
        assert!(!is_local("http://10.0.0.5:8000"));
        assert!(!is_local("https://api.example.com"));
        assert!(!is_local("http://127.0.0.1.evil.com"));
        assert!(!is_local("http://localhost@evil.com/"));
    }
}
