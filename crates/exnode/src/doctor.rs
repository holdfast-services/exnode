//! `exnode doctor`: what the agent would check and use, without serving.
//!
//! Prints the config in use, the hub URL and whether it answers, the key
//! file's permissions (it must be 0600), the node id, the backend and whether
//! it answers, the safety checks ([`crate::checks`]) and the advertised
//! models with their hashes.  It prints no secret: not the key, not the
//! enrollment token, not the backend's API key (only whether each is set),
//! and URLs without credentials.  It never sees a prompt.

use std::time::Duration;

use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

use ex_common::identity::{load_signing_key, node_id_for};
use ex_common::models::effective_context;

use crate::{
    advisories::{self, Component, ADVISORIES},
    backend::{Backend, BackendKind},
    checks::{self, Level},
    config::{display_url, is_local, NodeConfig},
    keyfile,
};

fn line(level: Level, what: &str, detail: &str) {
    let tag = match level {
        Level::Ok     => "ok",
        Level::Info   => "info",
        Level::Warn   => "warn",
        Level::Fail   => "FAIL",
        Level::Refuse => "REFUSE",
    };
    println!("  [{tag:^6}] {what:<25} {detail}");
}

/// Print the report.  `Ok(true)` when nothing needs fixing.
pub async fn run(config_path: &str) -> Result<bool> {
    let mut problems = 0usize;
    let mut note = |level: Level, what: &str, detail: &str| {
        if level >= Level::Fail { problems += 1; }
        line(level, what, detail);
    };
    println!("exnode {} doctor\n", env!("CARGO_PKG_VERSION"));

    let cfg = match NodeConfig::load(config_path) {
        Ok(c)  => { note(Level::Ok, "config", config_path); c }
        Err(e) => { note(Level::Fail, "config", &format!("{config_path}: {e}")); return Ok(false); }
    };

    let hub = display_url(&cfg.hub_url);
    match hub_health(&cfg.hub_url).await {
        Ok(())  => note(Level::Ok, "hub", &format!("{hub} answers /health")),
        Err(e)  => note(Level::Fail, "hub", &format!("{hub}: {}", e.root_cause())),
    }

    #[cfg(unix)]
    match keyfile::key_file_mode(&cfg.key_file) {
        Err(e) => note(Level::Fail, "key file", &format!(
            "{}: {} (enroll with exnode init, or run doctor as the exnode user)", cfg.key_file, e.root_cause())),
        Ok((mode, uid)) => match keyfile::key_mode_problem(mode) {
            None => note(Level::Ok, "key file", &format!("{}: mode 0600, owner uid {uid}", cfg.key_file)),
            Some((msg, fatal)) => note(if fatal { Level::Fail } else { Level::Warn }, "key file",
                                       &format!("{}: {msg}", cfg.key_file)),
        },
    }
    if let Ok(sk) = keyfile::read_key_file(&cfg.key_file).and_then(|k| load_signing_key(&k)) {
        let pk = STANDARD.encode(sk.verifying_key().as_bytes());
        if let Ok(id) = node_id_for(&pk) {
            note(Level::Info, "node id", &id);
        }
    }

    match (std::env::var_os("EXNODE_ENROLL_TOKEN").is_some(), cfg.enroll_token.is_some()) {
        (true, _)     => note(Level::Info, "enroll token", "set in EXNODE_ENROLL_TOKEN (used only to enroll)"),
        (false, true) => note(Level::Warn, "enroll token",
                              "saved in node.toml; once enrolled, delete it (exnode init --token keeps it off disk)"),
        _             => note(Level::Info, "enroll token", "not set (needed only to enroll: exnode init --token …)"),
    }
    if let Some(var) = &cfg.backend_api_key_env {
        let var = advisories::clean(var);
        match cfg.backend_api_key() {
            Some(_) => note(Level::Ok, "backend api key", &format!("read from ${var} (set)")),
            None    => note(Level::Warn, "backend api key", &format!("${var} is not set")),
        }
    }

    let kind    = cfg.backend_kind()?;
    let backend = Backend::new(kind, &cfg.backend_url(), cfg.backend_api_key());
    let shown   = format!("{} at {}", cfg.backend, display_url(&cfg.backend_url()));
    match backend.ping().await {
        Ok(())  => note(Level::Ok, "backend", &format!("{shown} answers")),
        Err(e)  => note(Level::Fail, "backend", &format!("{shown}: {}", e.root_cause())),
    }

    println!("\nsafety checks (re-run every {} min while serving)", checks::CHECK_INTERVAL.as_secs() / 60);
    let report = checks::run(&cfg, &backend).await;
    for c in &report.checks {
        note(c.level, c.name, &c.detail);
    }
    let minimums: Vec<String> = [Component::Ollama, Component::LlamaCpp, Component::NvidiaContainerToolkit].iter()
        .filter_map(|&c| advisories::minimum(c).map(|m| format!("{} {m}", c.name())))
        .collect();
    note(Level::Info, "advisory table", &format!("{} rows; minimums: {}", ADVISORIES.len(), minimums.join(", ")));

    println!("\nadvertised models");
    for m in &cfg.models {
        let what = format!("{} ({} tokens)", m.name, effective_context(m.context));
        let configured = (!m.hash.is_empty()).then(|| m.hash.clone());
        let ollama = match kind {
            BackendKind::Ollama => Some(backend.model_hash(&m.name).await),
            BackendKind::OpenAi => None,
        };
        match (configured, ollama) {
            (Some(h), None) => note(Level::Ok, &what, &format!("{h} (from node.toml)")),
            (Some(h), Some(Ok(d))) if h == d => note(Level::Ok, &what, &format!("{h} (node.toml, matches Ollama)")),
            (Some(h), Some(Ok(d))) => note(Level::Warn, &what, &format!(
                "node.toml says {h} but Ollama has {d}; the hub penalises a wrong model hash")),
            (Some(h), Some(Err(_))) => note(Level::Warn, &what, &format!("{h} (node.toml); Ollama does not list this model")),
            (None, Some(Ok(d))) => note(Level::Ok, &what, &format!("{d} (from Ollama)")),
            (None, Some(Err(_))) => note(Level::Fail, &what, "no hash: Ollama does not list this model (ollama pull it)"),
            (None, None) => note(Level::Fail, &what, "no hash: set `hash` in node.toml"),
        }
    }

    println!();
    if problems == 0 {
        println!("result: ok");
    } else {
        println!("result: {problems} problem(s) to fix; the agent will not serve with a FAIL or REFUSE above");
    }
    Ok(problems == 0)
}

/// Whether the hub answers `GET /health`, on the same terms as the agent's
/// own client: https unless on localhost, no redirects.
async fn hub_health(url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(!is_local(url))
        .build()?;
    let resp = client.get(format!("{}/health", url.trim_end_matches('/'))).send().await?;
    if !resp.status().is_success() {
        bail!("/health returned {}", resp.status());
    }
    Ok(())
}
