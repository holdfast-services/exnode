//! Node configuration loaded from TOML plus `EXNODE_*` environment overrides.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::backend::BackendKind;

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
        Ok(())
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
