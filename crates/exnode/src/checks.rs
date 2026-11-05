//! Safety checks: run before the node serves, every [`CHECK_INTERVAL`] while
//! it does, and by `exnode doctor`.
//!
//! * the backend's version against [`crate::advisories`]: Ollama through
//!   `/api/version`, llama.cpp's server through the build in `/props`
//! * the NVIDIA Container Toolkit (`nvidia-ctk --version`), when installed
//! * exposure: a backend URL off this machine, an `OLLAMA_HOST` that opens
//!   Ollama to the network, or the backend's port listening beyond loopback
//! * whether this process can read a known model directory (it should see
//!   models only through the backend's API; the systemd unit hides them)
//!
//! A `Refuse` or `Fail` stops the node taking jobs, with an `error` log line
//! starting "refusing to serve".  At startup a `Refuse` exits with
//! [`EXIT_REFUSED`], which the systemd unit does not restart.  The checks
//! only read local state and the backend: they contact no other host.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::Result;
use tracing::{error, info, warn};

pub use crate::advisories::Level;
use crate::{
    advisories::{self, Component},
    backend::{Backend, BackendKind},
    config::{display_url, is_local, NodeConfig},
    exposure,
};

/// The exit status for a refusal: `RestartPreventExitStatus=` in the unit.
pub const EXIT_REFUSED: u8 = 78;
/// How often a serving node runs the checks again.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(600);
/// How often a node that stopped serving looks again.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Where `nvidia-ctk` is looked for.  Not `$PATH`: what runs is fixed.
const NVIDIA_CTK: &[&str] = &["/usr/bin/nvidia-ctk", "/usr/local/bin/nvidia-ctk", "/usr/sbin/nvidia-ctk"];

/// Where model weights usually live.  Keep in step with `InaccessiblePaths=`
/// in packaging/systemd/exnode.service.  `$OLLAMA_MODELS` is added.
const MODEL_DIRS: &[&str] = &[
    "/usr/share/ollama/.ollama/models", "/var/lib/ollama", "/var/lib/docker", "/var/lib/containers",
    "/opt/models", "/srv/models", "/var/lib/models",
];

/// The node refuses to serve: a vulnerable or exposed backend.
#[derive(Debug, thiserror::Error)]
#[error("refusing to serve: {0}")]
pub struct Refused(pub String);

#[derive(Debug, Clone)]
pub struct Check {
    pub name:   &'static str,
    pub level:  Level,
    pub detail: String,
}

impl Check {
    fn new(name: &'static str, level: Level, detail: impl Into<String>) -> Self {
        Self { name, level, detail: detail.into() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// No check failed or refused.
    pub fn allows_serving(&self) -> bool { self.checks.iter().all(|c| c.level < Level::Fail) }

    pub fn at_least(&self, level: Level) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(move |c| c.level >= level)
    }

    /// One line per check at exactly `level`, to notice changes.
    pub fn summary(&self, levels: &[Level]) -> String {
        self.checks.iter().filter(|c| levels.contains(&c.level))
            .map(|c| format!("{}: {}\n", c.name, c.detail)).collect()
    }

    /// Log every check: problems as `error`, warnings as `warn`.
    pub fn log(&self) {
        for c in &self.checks {
            match c.level {
                Level::Ok | Level::Info => info!("check {}: {}", c.name, c.detail),
                Level::Warn             => warn!("check {}: {}", c.name, c.detail),
                Level::Fail | Level::Refuse => error!("refusing to serve: {}: {}", c.name, c.detail),
            }
        }
    }

    /// An error unless the node may serve: [`Refused`] for a refusal
    /// (permanent until fixed), a plain error when a check could not run.
    pub fn ensure_serving(&self) -> Result<()> {
        let join = |level| self.checks.iter().filter(|c| c.level == level)
            .map(|c| format!("{}: {}", c.name, c.detail)).collect::<Vec<_>>().join("; ");
        let refused = join(Level::Refuse);
        if !refused.is_empty() {
            return Err(Refused(refused).into());
        }
        let failed = join(Level::Fail);
        if !failed.is_empty() {
            anyhow::bail!("refusing to serve until the safety checks can run: {failed}");
        }
        Ok(())
    }
}

/// Run every check.  Never errors: a check that can't run is a `Fail`.
pub async fn run(cfg: &NodeConfig, backend: &Backend) -> Report {
    let mut checks = exposure_checks(cfg);
    checks.push(backend_version(cfg, backend).await);
    checks.push(nvidia_ctk().await);
    checks.push(model_dirs());
    Report { checks }
}

/// Exposure: the configured URL, `OLLAMA_HOST`, the listening sockets.
fn exposure_checks(cfg: &NodeConfig) -> Vec<Check> {
    let url = cfg.backend_url();
    let mut out = vec![if is_local(&url) {
        Check::new("backend url", Level::Ok, format!("{} is on loopback", display_url(&url)))
    } else {
        Check::new("backend url", Level::Refuse, format!("{} is not on this machine's loopback", display_url(&url)))
    }];

    if cfg.backend_kind().ok() == Some(BackendKind::Ollama) {
        if let Some(v) = std::env::var("OLLAMA_HOST").ok().filter(|v| !v.trim().is_empty()) {
            let shown = advisories::clean(&v);
            out.push(if exposure::ollama_host_is_exposed(&v) {
                Check::new("OLLAMA_HOST", Level::Refuse, format!(
                    "OLLAMA_HOST={shown} opens Ollama, which has no authentication, to the network; \
                     set OLLAMA_HOST=127.0.0.1:11434"))
            } else {
                Check::new("OLLAMA_HOST", Level::Ok, format!("OLLAMA_HOST={shown} is loopback"))
            });
        }
    }

    let Some(port) = reqwest::Url::parse(&url).ok().and_then(|u| u.port_or_known_default()) else {
        return out;
    };
    let tables: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"].iter()
        .filter_map(|p| std::fs::read_to_string(p).ok()).collect();
    if tables.is_empty() {
        out.push(Check::new("backend exposure", Level::Info,
            "listening sockets are not visible here; make sure the backend listens on 127.0.0.1 only"));
        return out;
    }
    let listeners: Vec<_> = tables.iter().flat_map(|t| exposure::listening(t)).collect();
    let exposed = exposure::exposed_on(&listeners, port);
    out.push(if !exposed.is_empty() {
        let addrs = exposed.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
        Check::new("backend exposure", Level::Refuse, format!(
            "the backend's port {port} is listening on {addrs}, so other machines can use it{}; \
             bind it to 127.0.0.1 (Ollama: OLLAMA_HOST=127.0.0.1:11434, llama-server: --host 127.0.0.1)",
            if cfg.backend_kind().ok() == Some(BackendKind::Ollama) { " without authentication" } else { "" }))
    } else if listeners.iter().any(|a| a.port() == port) {
        Check::new("backend exposure", Level::Ok, format!("port {port} listens on loopback only"))
    } else {
        Check::new("backend exposure", Level::Info, format!("nothing is listening on port {port} in this network namespace"))
    });
    out
}

/// The backend's version against the advisory table.
async fn backend_version(cfg: &NodeConfig, backend: &Backend) -> Check {
    const NAME: &str = "backend version";
    let unreachable = |e: anyhow::Error| Check::new(NAME, Level::Fail, format!(
        "cannot reach the backend at {}: {}", display_url(&cfg.backend_url()), e.root_cause()));
    match backend.kind() {
        BackendKind::Ollama => match backend.get_json("/api/version").await {
            Err(e) => unreachable(e),
            Ok(doc) => {
                let reported = doc.as_ref().and_then(|d| d["version"].as_str());
                let (level, detail) = advisories::verdict(Component::Ollama, reported);
                Check::new(NAME, level, detail)
            }
        },
        BackendKind::OpenAi => match backend.get_json("/props").await {
            Err(e) => unreachable(e),
            Ok(Some(p)) if p.get("build_info").is_some() || p.get("default_generation_settings").is_some() => {
                let (level, detail) = advisories::verdict(Component::LlamaCpp, p["build_info"].as_str());
                Check::new(NAME, level, detail)
            }
            Ok(_) => Check::new(NAME, Level::Warn,
                "not a llama.cpp server (no /props), so its version is not checked: the advisory table \
                 covers Ollama and llama.cpp only; keep this server patched yourself"),
        },
    }
}

/// The NVIDIA Container Toolkit, when it is installed.
async fn nvidia_ctk() -> Check {
    const NAME: &str = "nvidia container toolkit";
    let Some(path) = NVIDIA_CTK.iter().find(|p| Path::new(p).exists()) else {
        return Check::new(NAME, Level::Info, "nvidia-ctk is not installed");
    };
    let run = tokio::process::Command::new(path)
        .arg("--version")
        .env_clear() // it needs nothing of ours, least of all a backend API key
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), run).await {
        Ok(Ok(out)) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let (level, detail) = advisories::verdict(Component::NvidiaContainerToolkit, advisories::version_in_banner(&text));
            Check::new(NAME, level, detail)
        }
        _ => Check::new(NAME, advisories::Component::NvidiaContainerToolkit.if_unreadable(),
            format!("{path} is installed but `{path} --version` did not run, so its version is not checked")),
    }
}

/// Known model directories this process can list.  exnode needs models only
/// through the backend's API; the systemd unit makes these inaccessible.
fn model_dirs() -> Check {
    const NAME: &str = "model directories";
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0) {
            return Check::new(NAME, Level::Info, "not checked as root; run as the exnode user to see what it can read");
        }
    }
    let mut dirs: Vec<String> = MODEL_DIRS.iter().map(|d| d.to_string()).collect();
    dirs.extend(std::env::var("OLLAMA_MODELS").ok().filter(|d| d.starts_with('/')));
    let readable: Vec<String> = dirs.into_iter().filter(|d| std::fs::read_dir(d).is_ok()).collect();
    if readable.is_empty() {
        return Check::new(NAME, Level::Ok, "no known model directory is readable");
    }
    // systemd sets INVOCATION_ID for a service: there the sandbox should have hidden them
    let in_service = std::env::var_os("INVOCATION_ID").is_some();
    Check::new(NAME, if in_service { Level::Warn } else { Level::Info }, format!(
        "this process can read {}; exnode reads models only through the backend's API{}",
        readable.join(", "),
        if in_service { ", so hide them with InaccessiblePaths= in a drop-in (systemctl edit exnode)" }
        else { " (outside the systemd service, which hides these)" }))
}
