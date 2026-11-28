//! `exnode setup`: the plain-terminal configurator, step 3 of a provider
//! install.  `install.sh` runs it; it can be run again at any time.
//!
//! It asks, in order: the hub URL, the engine on loopback, the models to
//! serve and each one's context window, the level of service, how many jobs
//! at once, and the serving hours.  It runs the same safety checks the agent
//! runs ([`crate::checks`]) and stops if they would refuse.  Then it writes
//! `/etc/exnode/node.toml` atomically (root:exnode, mode 0640), showing what
//! changes and asking first when a file is already there; asks for the
//! one-time enrollment token with the input hidden and enrolls exactly as
//! `exnode init --token` does, as the `exnode` user; and offers to start the
//! service.
//!
//! With `--yes` it asks nothing: every answer comes from a flag, the existing
//! config or the default, an existing config is replaced, `--token-stdin`
//! reads the token from a pipe, and the service is started.
//!
//! It prints no secret: not the token, not the key, not a backend API key,
//! and URLs only without credentials.  It talks only to the hub (`/health`,
//! `/v0/public/models`) and the engine on loopback, reads the engine's models
//! only through its API, and listens on no port.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::{
    advisories::clean,
    backend::{Backend, BackendKind, EngineModel},
    checks::{self, Refused, EXIT_REFUSED},
    config::{display_url, is_local, ModelConfig, NodeConfig},
    doctor,
    enroll::{self, TOKEN_PREFIX},
    hours::{self, ServeHours},
};

pub const DEFAULT_HUB: &str = "https://exchange.holdfast.services";
pub const DEFAULT_CONFIG: &str = "/etc/exnode/node.toml";
pub const DEFAULT_KEY: &str = "/var/lib/exnode/node.key";
/// The service's user and group (packaging/sysusers.d/exnode.conf).
pub const SERVICE_USER: &str = "exnode";
/// The window offered when the engine says nothing better.
pub const FALLBACK_CONTEXT: u32 = 8192;
pub const MIN_CONTEXT: u32 = 512;
pub const MAX_CONTEXT: u32 = 1 << 21;
pub const MAX_JOBS: usize = 64;
/// More total VRAM than this (in MiB) defaults to 2 jobs at once.
pub const BIG_VRAM_MIB: u64 = 40 * 1024;

/// Where engines usually listen: Ollama, then OpenAI-compatible servers
/// (vLLM's default port, then llama.cpp's).
const ENGINE_URLS: &[&str] = &["http://127.0.0.1:11434", "http://127.0.0.1:8000", "http://127.0.0.1:8080"];

/// Programs setup runs, by fixed path, not `$PATH`: what runs is fixed.
const NVIDIA_SMI: &[&str] = &["/usr/bin/nvidia-smi", "/usr/local/bin/nvidia-smi", "/usr/sbin/nvidia-smi",
                              "/usr/lib/wsl/lib/nvidia-smi"];
const SYSTEMCTL: &[&str] = &["/usr/bin/systemctl", "/bin/systemctl"];
const RUNUSER: &[&str] = &["/usr/sbin/runuser", "/sbin/runuser", "/usr/bin/runuser", "/bin/runuser"];
const SETPRIV: &[&str] = &["/usr/bin/setpriv", "/bin/setpriv"];
const STTY: &[&str] = &["/usr/bin/stty", "/bin/stty"];

/// The largest registry reply read.
const MAX_REGISTRY_BODY: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, clap::Args)]
pub struct SetupArgs {
    /// The config file to write
    #[arg(long, short, default_value = DEFAULT_CONFIG)]
    pub config: String,
    /// Hub URL: https, or http only for a hub on this machine [default: https://exchange.holdfast.services]
    #[arg(long, value_name = "URL")]
    pub hub: Option<String>,
    /// The engine's URL on this machine, instead of looking on the usual ports
    #[arg(long, value_name = "URL")]
    pub backend_url: Option<String>,
    /// A model to serve, as the engine names it, optionally with its context window and, for an
    /// OpenAI-compatible server, its weights hash: qwen3:30b, qwen3:30b:32768,
    /// Qwen3.8-Flash-Next:32768@sha256:<hex>.  Repeat for more.
    #[arg(long = "model", value_name = "NAME[:CONTEXT][@HASH]")]
    pub models: Vec<String>,
    /// Most jobs at once [default: 1, or 2 with more than 40 GB of NVIDIA VRAM]
    #[arg(long, value_name = "N")]
    pub max_concurrency: Option<usize>,
    /// Daily serving window in local time, such as 18:00-08:00, or "always"
    #[arg(long, value_name = "HH:MM-HH:MM")]
    pub serve_hours: Option<String>,
    /// Read the enrollment token from stdin (with --yes)
    #[arg(long)]
    pub token_stdin: bool,
    /// Ask nothing: use the flags, the existing config and the defaults, replace an existing
    /// config, and start the service
    #[arg(long, short = 'y')]
    pub yes: bool,
}

// ── pure parts ────────────────────────────────────────────────────────────────

/// A hub URL as setup takes it: https (http only on loopback), no
/// credentials, path, query or fragment.  Returns its origin, `https://host[:port]`.
pub fn check_hub_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let u = reqwest::Url::parse(s).map_err(|_| "that is not a URL; it looks like https://exchange.holdfast.services".to_string())?;
    match u.scheme() {
        "https" => {}
        "http" if is_local(s) => {}
        "http" => return Err("the hub must use https:// (plain http only for a hub on this machine)".into()),
        _ => return Err("the hub URL must start with https://".into()),
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err("the hub URL has no host".into());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("leave the user name and password out of the hub URL".into());
    }
    if u.path() != "/" || u.query().is_some() || u.fragment().is_some() {
        return Err("give the hub's address only, without a path, such as https://exchange.holdfast.services".into());
    }
    Ok(u.origin().ascii_serialization())
}

/// An engine URL as setup takes it: http or https on this machine's loopback.
pub fn check_engine_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    let u = reqwest::Url::parse(s).map_err(|_| "that is not a URL; it looks like http://127.0.0.1:11434".to_string())?;
    if !matches!(u.scheme(), "http" | "https") || !is_local(s) {
        return Err("the engine must be on this machine: http://127.0.0.1:<port> or http://localhost:<port>".into());
    }
    if !u.username().is_empty() || u.password().is_some() || u.query().is_some() || u.fragment().is_some() {
        return Err("give the engine's address only, such as http://127.0.0.1:8000".into());
    }
    Ok(s.to_string())
}

/// `sha256:<64 lowercase hex>` from that form or bare hex, as the hub normalises hashes.
pub fn normalize_hash(h: &str) -> Option<String> {
    let h = h.trim();
    let hex = h.strip_prefix("sha256:").or_else(|| h.strip_prefix("SHA256:")).unwrap_or(h);
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("sha256:{}", hex.to_ascii_lowercase()))
}

/// A model name as typed: no spaces or control characters, at most 200 bytes.
fn check_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 200 || name.starts_with([':', '@']) || name.ends_with(':')
        || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{:?} is not a model name", clean(name)));
    }
    Ok(())
}

/// A context window as typed: a whole number of tokens in range, and no
/// more than the engine allows (`max`), when it says.
pub fn check_context(raw: &str, max: Option<u32>) -> Result<u32, String> {
    let n: u32 = raw.trim().replace('_', "").parse()
        .map_err(|_| format!("the context window is a number of tokens, such as {FALLBACK_CONTEXT}"))?;
    if !(MIN_CONTEXT..=MAX_CONTEXT).contains(&n) {
        return Err(format!("the context window must be {MIN_CONTEXT} to {MAX_CONTEXT} tokens"));
    }
    if let Some(m) = max.filter(|m| n > *m) {
        return Err(format!("the engine allows at most {m} tokens for this model"));
    }
    Ok(n)
}

/// `--model`: `name`, `name:context` or either with `@sha256:<hex>`.  A
/// trailing `:digits` is the context window; a name that itself ends in
/// `:digits` needs the context after it (`model:7:8192`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub name:    String,
    pub context: Option<u32>,
    pub hash:    Option<String>,
}

pub fn parse_model_spec(raw: &str) -> Result<ModelSpec, String> {
    let s = raw.trim();
    let (rest, hash) = match s.split_once('@') {
        Some((r, h)) => (r, Some(normalize_hash(h).ok_or_else(|| format!(
            "--model {}: the hash must be sha256:<64 hex digits>", clean(r)))?)),
        None => (s, None),
    };
    let (name, context) = match rest.rsplit_once(':') {
        Some((n, c)) if !n.is_empty() && !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) => {
            let ctx = check_context(c, None).map_err(|e| format!(
                "--model {}: {e} (if :{c} is part of the name, add the window after it: {}:{FALLBACK_CONTEXT})",
                clean(rest), clean(rest)))?;
            (n, Some(ctx))
        }
        _ => (rest, None),
    };
    check_model_name(name)?;
    Ok(ModelSpec { name: name.to_string(), context, hash })
}

/// A list of choices by number, `1,3` or `1 3` or `all`, out of `n`; in order, no repeats.
pub fn parse_selection(raw: &str, n: usize) -> Result<Vec<usize>, String> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("all") {
        return Ok((0..n).collect());
    }
    let mut out = Vec::new();
    for part in s.split([',', ' ']).filter(|p| !p.is_empty()) {
        let i: usize = part.parse().map_err(|_| format!("choose by number, such as 1 or 1,3 (got {:?})", clean(part)))?;
        if i == 0 || i > n {
            return Err(format!("there is no model {i}; choose 1 to {n}"));
        }
        if !out.contains(&(i - 1)) {
            out.push(i - 1);
        }
    }
    if out.is_empty() {
        return Err("choose at least one model".into());
    }
    Ok(out)
}

/// One active entry of the hub's model registry (`GET /v0/public/models`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub pattern:      String,
    pub quantization: String,
    pub license:      String,
    pub hashes:       Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    /// The hub serves only models the registry lists (`require_registry`).
    pub required: bool,
    pub entries:  Vec<RegistryEntry>,
}

/// The registry from the hub's reply: active entries only, hashes normalised.
pub fn parse_registry(v: &Value) -> Registry {
    let text = |e: &Value, k: &str| clean(e[k].as_str().unwrap_or(""));
    Registry {
        // when the hub does not say, assume it requires the registry: the stricter reading
        required: v["require_registry"].as_bool().unwrap_or(true),
        entries:  v["entries"].as_array().into_iter().flatten()
            .filter(|e| e["status"].as_str().is_none_or(|s| s == "active"))
            .filter_map(|e| {
                let pattern = e["model_pattern"].as_str()?.to_string();
                let hashes = e["hashes"].as_array().into_iter().flatten()
                    .filter_map(|h| h.as_str().and_then(normalize_hash)).collect();
                Some(RegistryEntry { pattern, quantization: text(e, "quantization"), license: text(e, "license"), hashes })
            })
            .collect(),
    }
}

/// A registry pattern: an exact name, or a prefix ending in `*`.
pub fn pattern_matches(pattern: &str, model: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None         => pattern == model,
    }
}

/// What the registry says about a model and the hash the agent would advertise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// An active entry allows this name with this hash.
    Listed { quantization: String, license: String },
    /// An entry names it, but not this hash (`hash_known`), or the hash is not known yet.
    NameOnly { hash_known: bool },
    Unlisted,
    /// The registry could not be read.
    Unchecked,
}

impl Listing {
    pub fn mark(&self) -> char {
        match self {
            Listing::Listed { .. }   => '*',
            Listing::NameOnly { .. } => '~',
            _                        => ' ',
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Listing::Listed { quantization, license } => format!("listed ({quantization}, {license})"),
            Listing::NameOnly { hash_known: true } => "name listed, but not this hash".into(),
            Listing::NameOnly { hash_known: false } => "name listed; the hash is checked once you give it".into(),
            Listing::Unlisted  => "not listed".into(),
            Listing::Unchecked => "registry not checked".into(),
        }
    }
}

/// Match a model against the registry the way the hub does: the entry
/// pattern must match the name and list the hash.  An exact name beats a
/// prefix, and a longer prefix a shorter one.
pub fn listing(reg: Option<&Registry>, name: &str, hash: Option<&str>) -> Listing {
    let Some(reg) = reg else { return Listing::Unchecked };
    let named: Vec<&RegistryEntry> = reg.entries.iter().filter(|e| pattern_matches(&e.pattern, name)).collect();
    if named.is_empty() {
        return Listing::Unlisted;
    }
    let Some(h) = hash.and_then(normalize_hash) else { return Listing::NameOnly { hash_known: false } };
    named.iter()
        .filter(|e| e.hashes.contains(&h))
        .max_by_key(|e| match e.pattern.strip_suffix('*') {
            None         => (1, e.pattern.len()),
            Some(prefix) => (0, prefix.len()),
        })
        .map(|e| Listing::Listed { quantization: e.quantization.clone(), license: e.license.clone() })
        .unwrap_or(Listing::NameOnly { hash_known: true })
}

/// The GPUs' count and total memory from
/// `nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits` (MiB per line).
pub fn parse_vram_mib(out: &str) -> Option<(usize, u64)> {
    let mibs: Vec<u64> = out.lines().map(str::trim).filter(|l| !l.is_empty())
        .map(|l| l.parse::<u64>().ok()).collect::<Option<_>>()?;
    (!mibs.is_empty()).then(|| (mibs.len(), mibs.iter().sum()))
}

/// 2 jobs at once with more than 40 GB of VRAM, else 1.
pub fn default_concurrency(vram: Option<(usize, u64)>) -> usize {
    match vram {
        Some((_, mib)) if mib > BIG_VRAM_MIB => 2,
        _ => 1,
    }
}

/// A TOML string literal.
fn q(s: &str) -> String { toml::Value::String(s.to_string()).to_string() }

/// node.toml as setup writes it.  Ollama models get no `hash`: the agent
/// reads the digest from Ollama at each start, so a re-pulled model is never
/// advertised under its old hash.
pub fn render_config(cfg: &NodeConfig, system: bool) -> String {
    let mut t = String::new();
    t += "# node.toml, written by `exnode setup`.  Run `sudo exnode setup` again to\n";
    t += "# change it; node.toml.example explains every key.  It holds no secret.\n";
    if system {
        t += "# root:exnode, mode 0640: the agent can read it and cannot change it.\n";
    }
    t += "#\n# Level of service: Open (runs on your own Linux, checked by hidden tests).\n\n";
    let _ = writeln!(t, "hub_url  = {}", q(&cfg.hub_url));
    let _ = writeln!(t, "key_file = {}\n", q(&cfg.key_file));
    let _ = writeln!(t, "backend     = {}", q(&cfg.backend));
    let _ = writeln!(t, "backend_url = {}", q(&cfg.backend_url()));
    if let Some(v) = &cfg.backend_api_key_env {
        let _ = writeln!(t, "backend_api_key_env = {}", q(v));
    }
    let _ = writeln!(t, "\nheartbeat_secs  = {}", cfg.heartbeat_secs);
    let _ = writeln!(t, "poll_secs       = {}", cfg.poll_secs);
    let _ = writeln!(t, "max_concurrency = {}", cfg.max_concurrency);
    t += "# Local time.  Outside it the agent takes no jobs but keeps heartbeating.\n";
    match cfg.serve_hours.as_deref().filter(|h| !h.trim().is_empty()) {
        Some(h) => { let _ = writeln!(t, "serve_hours     = {}", q(h)); }
        None    => t += "# serve_hours   = \"18:00-08:00\"   (absent: always)\n",
    }
    if let Some(z) = cfg.timezone.as_deref().map(str::trim).filter(|z| !z.is_empty()) {
        let _ = writeln!(t, "timezone        = {}", q(z));
    }
    let list = |items: &[String]| items.iter().map(|i| q(i)).collect::<Vec<_>>().join(", ");
    for r in &cfg.schedule {
        t += "\n[[schedule]]\n";
        if !r.days.is_empty() {
            let _ = writeln!(t, "days  = [{}]", list(&r.days));
        }
        let _ = writeln!(t, "hours = {}\nshare = {}", q(&r.hours), r.share);
    }
    if !cfg.blackout.dates.is_empty() {
        let _ = writeln!(t, "\n[blackout]\ndates = [{}]", list(&cfg.blackout.dates));
    }
    for m in &cfg.models {
        let _ = writeln!(t, "\n[[models]]\nname    = {}\ncontext = {}", q(&m.name), m.context);
        if !m.hash.is_empty() {
            let _ = writeln!(t, "hash    = {}", q(&m.hash));
        }
        if m.tools {
            t += "tools   = true\n";
        }
    }
    t
}

/// A config's settings as `key -> value` lines, models as `model <name>`,
/// for showing what a rewrite changes.  The enrollment token shows as `(set)`.
fn flatten(t: &toml::Table) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in t {
        match (k.as_str(), v) {
            ("models", toml::Value::Array(ms)) => {
                for m in ms {
                    let name = m.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let mut d = m.get("context").map(|c| format!("context {c}")).unwrap_or_else(|| "context default".into());
                    if let Some(h) = m.get("hash").and_then(|h| h.as_str()).filter(|h| !h.is_empty()) {
                        d += &format!(", hash {}", clean_line(h));
                    }
                    if m.get("tools").and_then(|t| t.as_bool()) == Some(true) {
                        d += ", tools";
                    }
                    out.insert(format!("model {}", clean(name)), d);
                }
            }
            ("enroll_token", _) => { out.insert(k.clone(), "(set)".into()); }
            _ => { out.insert(k.clone(), display_value(v)); }
        }
    }
    out
}

fn display_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) if reqwest::Url::parse(s).is_ok() && s.contains("://") => q(display_url(s).trim_end_matches('/')),
        toml::Value::String(s) => q(&clean_line(s)),
        other => clean_line(&other.to_string()),
    }
}

/// At most 120 printable characters.
fn clean_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(120).collect()
}

/// What rewriting `old` as `new` changes, one line per setting: `+` added,
/// `-` removed, `~` changed.  Empty when nothing changes.
pub fn summarize_changes(old: &toml::Table, new: &toml::Table) -> Vec<String> {
    let (a, b) = (flatten(old), flatten(new));
    let mut out = Vec::new();
    for (k, v) in &a {
        match b.get(k) {
            None                => out.push(format!("- {k} = {v}")),
            Some(w) if w != v   => out.push(format!("~ {k}: {v} -> {w}")),
            _ => {}
        }
    }
    for (k, w) in &b {
        if !a.contains_key(k) {
            out.push(format!("+ {k} = {w}"));
        }
    }
    out
}

/// A numeric id for `name` in an `/etc/passwd` or `/etc/group` style file.
pub fn id_in(db: &str, name: &str) -> Option<u32> {
    db.lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 3 && f[0] == name)
        .and_then(|f| f[2].parse().ok())
}

/// Write `text` to `path` atomically: a new file beside it, mode 0640,
/// owned by `owner` (uid, gid) when given, synced, then renamed over it.
pub fn write_atomic(path: &Path, text: &str, owner: Option<(u32, u32)>) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = path.file_name().context("the config path has no file name")?.to_string_lossy().to_string();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o640).open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::fchown(&f, Some(uid), Some(gid)).context("set the config's owner")?;
        }
        f.set_permissions(std::fs::Permissions::from_mode(0o640))?; // whatever the umask
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        std::fs::File::open(&dir).and_then(|d| d.sync_all()).ok();
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ── the terminal ──────────────────────────────────────────────────────────────

/// The terminal settings to put back if setup is stopped while input is hidden.
static SAVED_TTY: Mutex<Option<String>> = Mutex::new(None);

fn first_existing(paths: &[&'static str]) -> Option<&'static str> {
    paths.iter().copied().find(|p| Path::new(p).exists())
}

fn stty(args: &[&str]) -> Result<String> {
    let path = first_existing(STTY).context("stty is not installed")?;
    let out = Command::new(path).args(args).stdin(Stdio::inherit()).stderr(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("stty {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn restore_tty() {
    if let Some(saved) = SAVED_TTY.lock().ok().and_then(|mut s| s.take()) {
        let _ = stty(&[&saved]);
    }
}

/// Echo off on the terminal while this lives.
struct HiddenInput;

impl HiddenInput {
    fn new() -> Result<Self> {
        let saved = stty(&["-g"])?;
        if saved.is_empty() || !saved.bytes().all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'-') {
            bail!("cannot read the terminal's settings");
        }
        *SAVED_TTY.lock().map_err(|_| anyhow!("terminal state"))? = Some(saved);
        if let Err(e) = stty(&["-echo"]) {
            restore_tty();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for HiddenInput {
    fn drop(&mut self) { restore_tty(); }
}

struct Ui {
    interactive: bool,
}

impl Ui {
    fn read_line(&self) -> Result<String> {
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            bail!("input ended; setup stopped");
        }
        Ok(line.trim().to_string())
    }

    /// Ask, with `default` taken on Enter.
    fn ask(&self, question: &str, default: &str) -> Result<String> {
        if default.is_empty() { print!("{question}: ") } else { print!("{question} [{default}]: ") }
        std::io::stdout().flush()?;
        let a = self.read_line()?;
        Ok(if a.is_empty() { default.to_string() } else { a })
    }

    /// Ask until `check` takes the answer.  Without a terminal, `check` the default.
    fn ask_valid<T>(&self, question: &str, default: &str, check: impl Fn(&str) -> Result<T, String>) -> Result<T> {
        if !self.interactive {
            let v = check(default).map_err(|e| anyhow!("{question}: {e}"))?;
            println!("{question}: {}", clean_line(default));
            return Ok(v);
        }
        loop {
            match check(&self.ask(question, default)?) {
                Ok(v)  => return Ok(v),
                Err(e) => println!("  {e}"),
            }
        }
    }

    /// A yes or no.  Without a terminal (`--yes`), yes.
    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        if !self.interactive {
            return Ok(true);
        }
        loop {
            print!("{question} [{}]: ", if default { "Y/n" } else { "y/N" });
            std::io::stdout().flush()?;
            match self.read_line()?.to_ascii_lowercase().as_str() {
                ""          => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no"  => return Ok(false),
                _           => println!("  answer y or n"),
            }
        }
    }

    /// Read a secret with the terminal's echo off.
    fn ask_secret(&self, question: &str) -> Result<String> {
        print!("{question}: ");
        std::io::stdout().flush()?;
        let hidden = HiddenInput::new().context(
            "cannot hide the input on this terminal; pipe the token instead: exnode setup --yes --token-stdin ...")?;
        let line = self.read_line();
        drop(hidden);
        println!();
        line
    }
}

fn heading(s: &str) {
    println!("\n{s}");
}

// ── the steps ─────────────────────────────────────────────────────────────────

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

/// The file setup is about to replace, if there is one.
struct Existing {
    table: Option<toml::Table>,
    /// It loads and validates as a node config: its values are the defaults.
    valid: Option<NodeConfig>,
    there: bool,
}

impl Existing {
    fn read(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self { table: None, valid: None, there: false }),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let table = toml::from_str::<toml::Table>(&text).ok();
        let valid = toml::from_str::<NodeConfig>(&text).ok().filter(|c| c.validate().is_ok());
        Ok(Self { table, valid, there: true })
    }
}

/// An engine that answered on loopback.
#[derive(Debug, Clone)]
struct Engine {
    kind:  BackendKind,
    url:   String,
    label: String,
}

impl Engine {
    fn backend(&self, api_key: &Option<String>) -> Backend { Backend::new(self.kind, &self.url, api_key.clone()) }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            BackendKind::Ollama => "ollama",
            BackendKind::OpenAi => "openai",
        }
    }
}

/// What answers at `url`: Ollama (`/api/version`), else an OpenAI-compatible
/// server (`/v1/models`).  Ollama also serves `/v1/models`, so it is asked first.
async fn probe(url: &str, api_key: &Option<String>) -> Option<Engine> {
    let ollama = Backend::new(BackendKind::Ollama, url, api_key.clone());
    if let Ok(Some(doc)) = ollama.get_json("/api/version").await {
        if doc.is_object() {
            let v = doc["version"].as_str().map(clean).filter(|v| !v.is_empty()).unwrap_or_else(|| "(version unknown)".into());
            return Some(Engine { kind: BackendKind::Ollama, url: url.into(), label: format!("Ollama {v}") });
        }
    }
    let openai = Backend::new(BackendKind::OpenAi, url, api_key.clone());
    if let Ok(Some(doc)) = openai.get_json("/v1/models").await {
        if doc["data"].is_array() {
            let label = match openai.get_json("/props").await {
                Ok(Some(p)) if p["build_info"].is_string() => format!("llama.cpp server {}", clean(p["build_info"].as_str().unwrap_or(""))),
                Ok(Some(p)) if p.get("default_generation_settings").is_some() => "llama.cpp server".into(),
                _ => "OpenAI-compatible server".into(),
            };
            return Some(Engine { kind: BackendKind::OpenAi, url: url.into(), label });
        }
    }
    None
}

fn http_client(url: &str, timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .https_only(!is_local(url))
        .build()?)
}

async fn fetch_registry(hub: &str) -> Result<Registry> {
    let resp = http_client(hub, Duration::from_secs(20))?.get(format!("{hub}/v0/public/models")).send().await?;
    if !resp.status().is_success() {
        bail!("/v0/public/models returned {}", resp.status());
    }
    let body = resp.bytes().await?;
    if body.len() > MAX_REGISTRY_BODY {
        bail!("the registry reply is too large");
    }
    Ok(parse_registry(&serde_json::from_slice(&body).context("the registry reply is not JSON")?))
}

async fn step_hub(ui: &Ui, args: &SetupArgs, prior: Option<&NodeConfig>) -> Result<String> {
    heading("Hub");
    let default = args.hub.clone().or_else(|| prior.map(|c| c.hub_url.clone())).unwrap_or_else(|| DEFAULT_HUB.into());
    loop {
        let hub = ui.ask_valid("Hub URL", &default, check_hub_url)?;
        match doctor::hub_health(&hub).await {
            Ok(()) => {
                println!("  {hub} answers");
                return Ok(hub);
            }
            Err(e) if ui.interactive => println!("  cannot reach {hub}: {}", e.root_cause()),
            Err(e) => bail!("cannot reach the hub at {hub}: {}", e.root_cause()),
        }
    }
}

async fn step_engine(ui: &Ui, args: &SetupArgs, prior: Option<&NodeConfig>, api_key: &Option<String>) -> Result<Engine> {
    heading("Engine");
    let wanted = match &args.backend_url {
        Some(u) => Some(check_engine_url(u).map_err(|e| anyhow!("--backend-url: {e}"))?),
        None    => None,
    };
    let prior_url = prior.map(|c| c.backend_url().trim_end_matches('/').to_string());
    if !ui.interactive {
        if let Some(url) = wanted {
            let e = probe(&url, api_key).await
                .with_context(|| format!("nothing at {url} answers as Ollama or an OpenAI-compatible server"))?;
            println!("  {} at {}", e.label, e.url);
            return Ok(e);
        }
        if let Some(url) = &prior_url {
            if let Some(e) = probe(url, api_key).await {
                println!("  {} at {} (from the current config)", e.label, e.url);
                return Ok(e);
            }
        }
    }
    loop {
        let mut urls: Vec<String> = wanted.iter().chain(prior_url.iter()).cloned()
            .chain(ENGINE_URLS.iter().map(|s| s.to_string())).collect();
        let mut seen = Vec::new();
        urls.retain(|u| if seen.contains(u) { false } else { seen.push(u.clone()); true });
        let found: Vec<Engine> = futures::future::join_all(urls.iter().map(|u| probe(u, api_key))).await
            .into_iter().flatten().collect();

        if !ui.interactive {
            return match found.as_slice() {
                [one] => { println!("  found {} at {}", one.label, one.url); Ok(one.clone()) }
                []    => bail!("no engine answers on 127.0.0.1:11434, :8000 or :8080; start it, or pass --backend-url"),
                many  => bail!("several engines answer ({}); choose one with --backend-url",
                               many.iter().map(|e| e.url.as_str()).collect::<Vec<_>>().join(", ")),
            };
        }

        let other = found.len() + 1;
        if found.is_empty() {
            println!("No engine answers on 127.0.0.1:11434, :8000 or :8080.");
        } else {
            println!("Engines on this machine:");
            for (i, e) in found.iter().enumerate() {
                println!("  {}) {} at {}", i + 1, e.label, e.url);
            }
            println!("  {other}) another URL on this machine");
        }
        let pick = if found.is_empty() {
            other
        } else {
            let default = wanted.iter().chain(prior_url.iter())
                .find_map(|u| found.iter().position(|e| &e.url == u)).unwrap_or(0) + 1;
            ui.ask_valid("Engine", &default.to_string(), |s| match s.trim().parse::<usize>() {
                Ok(n) if (1..=other).contains(&n) => Ok(n),
                _ => Err(format!("choose 1 to {other}")),
            })?
        };
        if pick < other {
            return Ok(found[pick - 1].clone());
        }
        let raw = ui.ask("Engine URL, such as http://127.0.0.1:8000 (blank to look again)", "")?;
        if raw.is_empty() {
            continue;
        }
        match check_engine_url(&raw) {
            Err(e) => println!("  {e}"),
            Ok(url) => match probe(&url, api_key).await {
                Some(e) => { println!("  found {} at {}", e.label, e.url); return Ok(e) }
                None    => println!("  nothing at {url} answers as Ollama or an OpenAI-compatible server"),
            },
        }
    }
}

/// The agent's own safety checks against this engine; an error when it would refuse to serve.
async fn step_checks(hub: &str, engine: &Engine, prior: Option<&NodeConfig>, api_key: &Option<String>) -> Result<()> {
    heading("Safety checks");
    let cfg = NodeConfig {
        hub_url: hub.into(), key_file: String::new(), enroll_token: None,
        backend: engine.kind_name().into(), backend_url: Some(engine.url.clone()),
        ollama_url: engine.url.clone(),
        backend_api_key_env: prior.and_then(|c| c.backend_api_key_env.clone()),
        models: Vec::new(), heartbeat_secs: 15, poll_secs: 20, max_concurrency: 1, serve_hours: None, timezone: None, schedule: Vec::new(), blackout: Default::default(),
    };
    let report = checks::run(&cfg, &engine.backend(api_key)).await;
    for c in &report.checks {
        doctor::line(c.level, c.name, &c.detail);
    }
    if let Err(e) = report.ensure_serving() {
        println!("\nSetup stopped: the agent would refuse to serve with this engine. Fix what is marked");
        println!("REFUSE or FAIL above, then run sudo exnode setup again.");
        return Err(e);
    }
    Ok(())
}

/// The window to offer: the engine's running window, else 8192 within the
/// engine's maximum.  (Ollama's maximum is the model's trained context,
/// often far more than fits in memory, so it is a limit, not a default.)
fn engine_default_context(m: &EngineModel) -> u32 {
    m.running_context.unwrap_or_else(|| m.max_context.map_or(FALLBACK_CONTEXT, |max| max.clamp(MIN_CONTEXT, FALLBACK_CONTEXT)))
}

async fn step_models(ui: &Ui, args: &SetupArgs, prior: Option<&NodeConfig>, engine: &Engine,
                     api_key: &Option<String>, reg: Option<&Registry>) -> Result<Vec<ModelConfig>> {
    heading("Models");
    let found = engine.backend(api_key).engine_models().await?;
    if found.is_empty() {
        bail!("{} lists no models. Load one first (Ollama: ollama pull <model>), then run setup again", engine.label);
    }
    let specs = args.models.iter().map(|m| parse_model_spec(m).map_err(anyhow::Error::msg)).collect::<Result<Vec<_>>>()?;
    let prior_model = |name: &str| prior.and_then(|c| c.models.iter().find(|m| m.name == name));
    let openai = engine.kind == BackendKind::OpenAi;
    let on_engine = |name: &str| found.iter().position(|m| m.name == name || (!openai && m.name == format!("{name}:latest")));

    let width = found.iter().map(|m| m.name.len()).max().unwrap_or(0).min(48);
    println!("Models on {} (* listed in the hub's registry):", engine.label);
    for (i, m) in found.iter().enumerate() {
        let l = listing(reg, &m.name, m.hash.as_deref());
        let hash = m.hash.as_deref().map(|h| format!("{}...  ", clean(h).chars().take(19).collect::<String>())).unwrap_or_default();
        println!("  {:>2}) {} {:<width$}  {hash}{}", i + 1, l.mark(), clean_line(&m.name), l.describe());
    }
    if reg.is_some_and(|r| r.required) {
        println!("  The hub sends jobs only for registry models with a listed hash.");
    }

    loop {
        // which models, each with the name the config will carry
        let chosen: Vec<(usize, String)> = if ui.interactive {
            let default: Vec<String> = if !specs.is_empty() {
                specs.iter().filter_map(|s| on_engine(&s.name)).map(|i| (i + 1).to_string()).collect()
            } else if let Some(p) = prior {
                p.models.iter().filter_map(|m| on_engine(&m.name)).map(|i| (i + 1).to_string()).collect()
            } else {
                found.iter().enumerate()
                    .filter(|(_, m)| matches!(listing(reg, &m.name, m.hash.as_deref()), Listing::Listed { .. }))
                    .map(|(i, _)| (i + 1).to_string()).collect()
            };
            let picks = ui.ask_valid("Models to serve, by number (such as 1 or 1,3)", &default.join(","),
                                     |s| parse_selection(s, found.len()))?;
            picks.into_iter().map(|i| (i, found[i].name.clone())).collect()
        } else {
            let names: Vec<String> = if !specs.is_empty() {
                specs.iter().map(|s| s.name.clone()).collect()
            } else if let Some(p) = prior {
                p.models.iter().map(|m| m.name.clone()).collect()
            } else {
                bail!("choose the models with --model NAME (this engine has: {})",
                      found.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "));
            };
            names.into_iter().map(|n| match on_engine(&n) {
                Some(i) => Ok((i, n)),
                None => Err(anyhow!("{} does not have the model {}", engine.label, clean(&n))),
            }).collect::<Result<_>>()?
        };

        let mut out: Vec<(ModelConfig, Listing)> = Vec::new();
        for (i, name) in &chosen {
            let m = &found[*i];
            let spec = specs.iter().find(|s| &s.name == name);
            let before = prior_model(name);

            // the hash: Ollama's digest is read at each start; an OpenAI-compatible server's is given
            let hash = if openai {
                let default = spec.and_then(|s| s.hash.clone()).or_else(|| before.map(|b| b.hash.clone()))
                    .filter(|h| normalize_hash(h).is_some()).unwrap_or_default();
                if ui.interactive {
                    println!("  {name}: this server has no digest API. Give the sha256 of the weights file it");
                    println!("  loaded (sha256sum <file>); the hub checks it against the registry.");
                }
                let h = ui.ask_valid(&format!("Weights hash for {name}"), &default, |s| {
                    normalize_hash(s).ok_or_else(|| "the hash is sha256:<64 hex digits>".to_string())
                }).map_err(|e| if ui.interactive { e } else {
                    anyhow!("{name} needs its weights hash with an OpenAI-compatible server: --model {name}:CONTEXT@sha256:<hex>")
                })?;
                println!("  {name}: {}", listing(reg, name, Some(&h)).describe());
                h
            } else {
                String::new()
            };

            let default = spec.and_then(|s| s.context).or_else(|| before.map(|b| b.context))
                .unwrap_or_else(|| engine_default_context(m));
            if ui.interactive {
                if let Some(max) = m.max_context {
                    println!("  {name}: the engine allows up to {max} tokens. More context needs more memory.");
                }
            }
            let context = ui.ask_valid(&format!("Context window for {name}, in tokens"), &default.to_string(),
                                       |s| check_context(s, m.max_context))?;
            let l = listing(reg, name, if openai { Some(hash.as_str()) } else { m.hash.as_deref() });
            out.push((ModelConfig { name: name.clone(), hash, context, tools: before.is_some_and(|b| b.tools) }, l));
        }

        if reg.is_some_and(|r| r.required) {
            let unlisted: Vec<String> = out.iter()
                .filter(|(_, l)| !matches!(l, Listing::Listed { .. }))
                .map(|(m, _)| clean_line(&m.name)).collect();
            if !unlisted.is_empty() {
                println!("  Not in the registry with this hash, so the hub will not send jobs for: {}", unlisted.join(", "));
                if unlisted.len() == out.len() && !ui.confirm("None of these can earn yet. Keep them anyway?", false)? {
                    continue;
                }
            }
        }
        return Ok(out.into_iter().map(|(m, _)| m).collect());
    }
}

fn step_level(ui: &Ui) -> Result<()> {
    heading("Level of service");
    println!("  1) Open: runs on your own Linux, checked by hidden tests");
    println!("  2) Attested: Holdfast Node OS with TPM-measured boot, coming soon");
    println!("  3) Sealed: confidential-computing GPUs, coming soon");
    ui.ask_valid("Level", if ui.interactive { "1" } else { "open" }, |s| match s.trim().to_ascii_lowercase().as_str() {
        "1" | "open" => Ok(()),
        "2" | "attested" => Err("Attested is coming soon and cannot be chosen yet; choose 1".into()),
        "3" | "sealed" => Err("Sealed is coming soon and cannot be chosen yet; choose 1".into()),
        _ => Err("choose 1".into()),
    })?;
    Ok(())
}

/// The GPUs nvidia-smi reports, if it is installed and answers.
async fn detect_vram() -> Option<(usize, u64)> {
    let path = first_existing(NVIDIA_SMI)?;
    let run = tokio::process::Command::new(path)
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), run).await {
        Ok(Ok(out)) if out.status.success() => parse_vram_mib(&String::from_utf8_lossy(&out.stdout)),
        _ => None,
    }
}

/// Most jobs at once: the flag, else the current setting, else 1 or 2 by VRAM.
async fn step_capacity(ui: &Ui, args: &SetupArgs, prior: Option<&NodeConfig>) -> Result<usize> {
    heading("Capacity");
    let vram = detect_vram().await;
    if let Some((n, mib)) = vram {
        println!("  nvidia-smi: {n} GPU{}, {} GB of memory in all", if n == 1 { "" } else { "s" }, mib / 1024);
    }
    let default = args.max_concurrency.or(prior.map(|c| c.max_concurrency)).unwrap_or_else(|| default_concurrency(vram));
    ui.ask_valid("Most jobs at once", &default.to_string(), |s| match s.trim().parse::<usize>() {
        Ok(n) if (1..=MAX_JOBS).contains(&n) => Ok(n),
        _ => Err(format!("a whole number, 1 to {MAX_JOBS}")),
    })
}

fn step_hours(ui: &Ui, args: &SetupArgs, prior: Option<&NodeConfig>) -> Result<Option<ServeHours>> {
    // a weekly [[schedule]] is richer than one daily window: setup keeps it unless --serve-hours replaces it
    if args.serve_hours.is_none() && prior.is_some_and(|c| !c.schedule.is_empty()) {
        println!("  This node has a weekly [[schedule]] in node.toml; setup keeps it. `exnode schedule` shows it.");
        return Ok(None);
    }
    let default = args.serve_hours.clone()
        .or_else(|| prior.and_then(|c| c.serve_hours.clone()))
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "always".into());
    ui.ask_valid(&format!("Serving hours, local time {} (such as 18:00-08:00, or always; a weekly schedule goes in node.toml)", hours::local_offset()),
                 &default, ServeHours::parse)
}

/// Write the config, asking before replacing one.  `Ok(None)` when the
/// person keeps the existing file; `Ok(Some(changed))` otherwise.
fn step_write(ui: &Ui, path: &Path, existing: &Existing, text: &str, owner: Option<(u32, u32)>) -> Result<Option<bool>> {
    heading("Config");
    let new: toml::Table = toml::from_str(text).context("the config setup wrote does not parse")?;
    let shown = path.display();
    if existing.there {
        let changes = match &existing.table {
            Some(old) => summarize_changes(old, &new),
            None => vec!["(the current file is not valid TOML; all of it is replaced)".into()],
        };
        if changes.is_empty() {
            println!("  {shown} already says this; not rewritten.");
            return Ok(Some(false));
        }
        println!("  {shown} exists. Writing these answers changes:");
        for c in &changes {
            println!("    {c}");
        }
        if !ui.confirm(&format!("Replace {shown}?"), existing.valid.is_none())? {
            println!("  Kept {shown}. Nothing was changed.");
            return Ok(None);
        }
    } else {
        for (k, v) in flatten(&new) {
            println!("    {k} = {v}");
        }
    }
    write_atomic(path, text, owner)?;
    println!("  wrote {shown} (mode 0640{})", if owner.is_some() { ", root:exnode" } else { "" });
    Ok(Some(true))
}

/// Run `exnode init --config <config> --token -` as the service user, the
/// token on its stdin: the same path as by hand, and the key is the user's.
fn enroll_as_service_user(config: &Path, token: &str) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the exnode binary")?;
    let (prog, pre): (&str, Vec<&str>) = if let Some(r) = first_existing(RUNUSER) {
        (r, vec!["-u", SERVICE_USER, "--"])
    } else if let Some(s) = first_existing(SETPRIV) {
        (s, vec!["--reuid", SERVICE_USER, "--regid", SERVICE_USER, "--init-groups", "--"])
    } else {
        bail!("neither runuser nor setpriv is installed, so setup cannot enroll as the {SERVICE_USER} user. Enroll by hand:\n  \
               sudo -u {SERVICE_USER} {} init --config {} --token -", exe.display(), config.display());
    };
    let mut child = Command::new(prog)
        .args(pre)
        .arg(&exe)
        .args(["init", "--config"]).arg(config).args(["--token", "-"])
        .env("RUST_LOG", "exnode=warn")
        .current_dir("/")
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {prog}"))?;
    {
        let mut stdin = child.stdin.take().context("the enrollment's stdin")?;
        stdin.write_all(token.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let status = child.wait()?;
    match status.code() {
        Some(0) => Ok(()),
        Some(c) if c == EXIT_REFUSED as i32 => Err(Refused("the safety checks refused when enrolling (see above)".into()).into()),
        _ => bail!("enrollment failed (see above). The config is saved; fix the problem and run sudo exnode setup again"),
    }
}

/// Ask for the token and enroll.  `Ok(true)` when this node has a key afterwards.
async fn step_enroll(ui: &Ui, args: &SetupArgs, config: &Path, key: &str, as_root: bool) -> Result<bool> {
    heading("Enrollment");
    let has_key = Path::new(key).exists();
    let later = || {
        println!("  Enroll later by running setup again, or:");
        println!("    sudo -u {SERVICE_USER} exnode init --config {} --token -", config.display());
    };
    let want = if ui.interactive {
        if has_key {
            println!("  This node has a key ({key}), so it enrolled before.");
            ui.confirm("Enroll again with a new token?", false)?
        } else {
            true
        }
    } else {
        args.token_stdin
    };
    if !want {
        if !has_key {
            later();
        }
        return Ok(has_key);
    }

    let token = if ui.interactive {
        println!("  Create a token on the exchange site: the \"Host a model\" page, step 6.");
        println!("  It starts with {TOKEN_PREFIX}, works once, and is not saved anywhere.");
        let mut got = None;
        for _ in 0..3 {
            let t = ui.ask_secret("Enrollment token (hidden; blank to skip)")?;
            if t.is_empty() {
                break;
            }
            match enroll::parse_token(&t) {
                Ok(t)  => { got = Some(t); break }
                Err(e) => println!("  {e}"),
            }
        }
        match got {
            Some(t) => t,
            None => {
                later();
                return Ok(has_key);
            }
        }
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        enroll::parse_token(&line)?
    };

    println!("  enrolling (safety checks, then the hub)...");
    if as_root {
        enroll_as_service_user(config, &token)?;
    } else {
        let cfg = NodeConfig::load(&config.to_string_lossy()).map_err(|e| anyhow!("config error: {e}"))?;
        let done = enroll::enroll(cfg, key, &token).await?;
        println!("  node_id:  {}", done.node_id);
        println!("  status:   {} (tier {})", done.status, done.tier);
        println!("  key file: {key} (mode 0600; never copy it off this machine)");
    }
    drop(token);
    Ok(true)
}

fn step_service(ui: &Ui, config: &Path, system: bool, enrolled: bool, changed: bool) -> Result<()> {
    heading("Service");
    if !system {
        if enrolled {
            println!("  Start the agent: exnode run --config {}", config.display());
        }
        return Ok(());
    }
    let Some(sc) = first_existing(SYSTEMCTL) else {
        println!("  systemctl is not installed. Start the agent: exnode run --config {}", config.display());
        return Ok(());
    };
    if !enrolled {
        println!("  Once the node is enrolled: sudo systemctl enable --now exnode");
        return Ok(());
    }
    let active = Command::new(sc).args(["is-active", "--quiet", "exnode.service"]).status().is_ok_and(|s| s.success());
    let (question, cmd): (&str, &[&str]) = match (active, changed) {
        (true, false) => {
            println!("  exnode is running with these settings.");
            return Ok(());
        }
        (true, true)  => ("Restart the node to use these settings?", &["restart", "exnode.service"]),
        (false, _)    => ("Start the node now?", &["enable", "--now", "exnode.service"]),
    };
    if ui.confirm(question, true)? {
        match Command::new(sc).args(cmd).status() {
            Ok(s) if s.success() => println!("  {}. Follow it: journalctl -u exnode -f", if active { "restarted" } else { "started" }),
            _ => println!("  systemctl {} failed; see: journalctl -u exnode", cmd.join(" ")),
        }
    } else {
        println!("  Start it later: sudo systemctl enable --now exnode");
    }
    println!("  Check it any time: sudo -u {SERVICE_USER} exnode doctor --config {}", config.display());
    Ok(())
}

/// Run setup.
pub async fn run(args: SetupArgs) -> Result<()> {
    let interactive = !args.yes;
    let stdin_tty = std::io::stdin().is_terminal();
    if interactive && !stdin_tty {
        bail!("setup asks its questions on a terminal. For a scripted install pass --yes and flags \
               (exnode setup --help), and --token-stdin to enroll");
    }
    if args.token_stdin && (interactive || stdin_tty) {
        bail!("--token-stdin reads the token from a pipe, with --yes; on a terminal leave both out and setup asks for it, hidden");
    }
    let root = is_root();
    let system = args.config == DEFAULT_CONFIG;
    if system && !root {
        bail!("run it as root: sudo exnode setup");
    }
    let config = std::path::absolute(&args.config).context("the config path")?;
    // the config's group, so the agent can read it; checked before anything is asked
    let owner = if root {
        let gid = std::fs::read_to_string("/etc/group").ok().and_then(|g| id_in(&g, SERVICE_USER))
            .with_context(|| format!("there is no {SERVICE_USER} group, so the agent could not read its config; run install.sh first"))?;
        let has_user = std::fs::read_to_string("/etc/passwd").ok().and_then(|p| id_in(&p, SERVICE_USER)).is_some();
        if !has_user {
            bail!("there is no {SERVICE_USER} user to run the agent; run install.sh first");
        }
        Some((0, gid))
    } else {
        None
    };
    if interactive {
        // put the terminal back if stopped while the token's input is hidden
        tokio::spawn(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                restore_tty();
                println!("\nsetup stopped.");
                std::process::exit(130);
            }
        });
    }
    let ui = Ui { interactive };
    println!("exnode {} setup.  Nothing is written until the Config step{}.", env!("CARGO_PKG_VERSION"),
             if interactive { "; Ctrl-C stops it" } else { "" });

    let existing = Existing::read(&config)?;
    let prior = existing.valid.as_ref();
    let api_key = prior.and_then(|c| c.backend_api_key());

    let hub = step_hub(&ui, &args, prior).await?;
    let registry = match fetch_registry(&hub).await {
        Ok(r) => Some(r),
        Err(e) => {
            println!("  could not read the hub's model registry ({}); models are shown unchecked", e.root_cause());
            None
        }
    };
    let engine = step_engine(&ui, &args, prior, &api_key).await?;
    step_checks(&hub, &engine, prior, &api_key).await?;
    let models = step_models(&ui, &args, prior, &engine, &api_key, registry.as_ref()).await?;
    step_level(&ui)?;
    let max_concurrency = step_capacity(&ui, &args, prior).await?;
    let serve_hours = step_hours(&ui, &args, prior)?;

    let key_file = prior.map(|c| c.key_file.clone()).unwrap_or_else(|| if system {
        DEFAULT_KEY.into()
    } else {
        config.with_file_name("node.key").to_string_lossy().to_string()
    });
    let cfg = NodeConfig {
        hub_url: hub,
        key_file: key_file.clone(),
        enroll_token: None,
        backend: engine.kind_name().into(),
        backend_url: Some(engine.url.clone()),
        ollama_url: engine.url.clone(),
        backend_api_key_env: prior.and_then(|c| c.backend_api_key_env.clone()),
        models,
        heartbeat_secs: prior.map_or(15, |c| c.heartbeat_secs),
        poll_secs: prior.map_or(20, |c| c.poll_secs),
        max_concurrency,
        serve_hours: serve_hours.map(|h| h.to_string()),
        timezone: prior.and_then(|c| c.timezone.clone()),
        schedule: if serve_hours.is_some() { Vec::new() } else { prior.map(|c| c.schedule.clone()).unwrap_or_default() },
        blackout: prior.map(|c| c.blackout.clone()).unwrap_or_default(),
    };
    let text = render_config(&cfg, system);
    toml::from_str::<NodeConfig>(&text).context("internal: the config does not parse")?
        .validate().context("internal: the config does not validate")?;

    let Some(changed) = step_write(&ui, &config, &existing, &text, owner)? else { return Ok(()) };
    let enrolled = step_enroll(&ui, &args, &config, &key_file, root).await?;
    step_service(&ui, &config, system, enrolled, changed)?;
    println!("\nSetup is done. Run sudo exnode setup again to change any answer.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const H1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn hub_urls_are_https_or_loopback_and_bare() {
        assert_eq!(check_hub_url("https://exchange.holdfast.services/").unwrap(), "https://exchange.holdfast.services");
        assert_eq!(check_hub_url(" https://hub.example.com:8443 ").unwrap(), "https://hub.example.com:8443");
        assert_eq!(check_hub_url("http://127.0.0.1:8000").unwrap(), "http://127.0.0.1:8000");
        assert_eq!(check_hub_url("http://localhost:8000/").unwrap(), "http://localhost:8000");
        for bad in ["http://hub.example.com", "ftp://hub.example.com", "hub.example.com", "https://u:p@hub.example.com",
                    "https://hub.example.com/v0", "https://hub.example.com/?x=1", "http://127.0.0.1.evil.com", ""] {
            assert!(check_hub_url(bad).is_err(), "{bad}");
        }
        assert!(check_engine_url("http://127.0.0.1:8080/").is_ok());
        assert!(check_engine_url("http://10.0.0.5:8080").is_err());
        assert!(check_engine_url("http://localhost@evil.com").is_err());
    }

    #[test]
    fn model_specs() {
        let s = |n: &str, c: Option<u32>, h: Option<&str>| ModelSpec { name: n.into(), context: c, hash: h.map(str::to_string) };
        assert_eq!(parse_model_spec("qwen3:30b").unwrap(), s("qwen3:30b", None, None));
        assert_eq!(parse_model_spec("qwen3:30b:32768").unwrap(), s("qwen3:30b", Some(32768), None));
        assert_eq!(parse_model_spec("llama3").unwrap(), s("llama3", None, None));
        assert_eq!(parse_model_spec("llama3:8192").unwrap(), s("llama3", Some(8192), None));
        let hex = &H1[7..];
        assert_eq!(parse_model_spec(&format!("Qwen3.8-Flash-Next:32768@{}", hex.to_uppercase())).unwrap(),
                   s("Qwen3.8-Flash-Next", Some(32768), Some(H1)));
        assert_eq!(parse_model_spec(&format!("org/model@{H1}")).unwrap(), s("org/model", None, Some(H1)));
        assert!(parse_model_spec("gemma:7").unwrap_err().contains("gemma:7:8192"));
        assert_eq!(parse_model_spec("gemma:7:8192").unwrap(), s("gemma:7", Some(8192), None));
        for bad in ["", "a b", "m@sha256:abc", ":8192", "m:99999999"] {
            assert!(parse_model_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn selections() {
        assert_eq!(parse_selection("1", 3).unwrap(), vec![0]);
        assert_eq!(parse_selection("3,1, 3", 3).unwrap(), vec![2, 0]);
        assert_eq!(parse_selection("2 3", 3).unwrap(), vec![1, 2]);
        assert_eq!(parse_selection("ALL", 2).unwrap(), vec![0, 1]);
        for bad in ["", "0", "4", "x", "1-2", ","] {
            assert!(parse_selection(bad, 3).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn contexts_stay_in_range_and_within_the_engine() {
        assert_eq!(check_context("8192", None), Ok(8192));
        assert_eq!(check_context("32_768", Some(32768)), Ok(32768));
        assert!(check_context("65536", Some(32768)).unwrap_err().contains("32768"));
        assert!(check_context("100", None).is_err());
        assert!(check_context("8k", None).is_err());
        let m = |running, max| EngineModel { name: "m".into(), hash: None, max_context: max, running_context: running };
        assert_eq!(engine_default_context(&m(None, None)), 8192);
        assert_eq!(engine_default_context(&m(None, Some(262144))), 8192, "a trained maximum is not a default");
        assert_eq!(engine_default_context(&m(None, Some(4096))), 4096);
        assert_eq!(engine_default_context(&m(Some(32768), Some(32768))), 32768);
    }

    fn registry() -> Registry {
        parse_registry(&json!({"require_registry": true, "entries": [
            {"entry_id": "a", "model_pattern": "qwen3:30b", "quantization": "Q4", "license": "apache-2.0",
             "status": "active", "hashes": [H1[7..].to_uppercase()]},
            {"entry_id": "b", "model_pattern": "qwen3*", "quantization": "Q8", "license": "apache-2.0",
             "status": "active", "hashes": [H1, H2]},
            {"entry_id": "c", "model_pattern": "gemma3:12b", "quantization": "Q4", "license": "gemma",
             "status": "retired", "hashes": [H2]},
            {"entry_id": "d", "model_pattern": "llama4:70b", "quantization": "FP8", "license": "llama4", "status": "active"},
        ]}))
    }

    #[test]
    fn the_registry_is_matched_as_the_hub_matches_it() {
        let r = registry();
        assert_eq!(r.entries.len(), 3, "the retired entry is dropped");
        let listed = |q: &str| Listing::Listed { quantization: q.into(), license: "apache-2.0".into() };
        assert_eq!(listing(Some(&r), "qwen3:30b", Some(H1)), listed("Q4"), "the exact name beats the prefix");
        assert_eq!(listing(Some(&r), "qwen3:30b", Some(H2)), listed("Q8"), "the prefix entry lists H2");
        assert_eq!(listing(Some(&r), "qwen3-coder:30b", Some(&H2[7..])), listed("Q8"), "bare hex is a hash too");
        let h3 = format!("sha256:{}", "3".repeat(64));
        assert_eq!(listing(Some(&r), "qwen3:30b", Some(&h3)), Listing::NameOnly { hash_known: true });
        assert_eq!(listing(Some(&r), "qwen3:30b", Some("sha256:feedface")), Listing::NameOnly { hash_known: false },
                   "not a sha256 at all");
        assert_eq!(listing(Some(&r), "qwen3:30b", None), Listing::NameOnly { hash_known: false });
        assert_eq!(listing(Some(&r), "gemma3:12b", Some(H2)), Listing::Unlisted, "retired entries allow nothing");
        assert_eq!(listing(Some(&r), "llama4:70b", Some(H1)), Listing::NameOnly { hash_known: true }, "no hashes listed");
        assert_eq!(listing(None, "qwen3:30b", Some(H1)), Listing::Unchecked);
        assert!(parse_registry(&json!({"entries": []})).required, "unsaid means required");
    }

    #[test]
    fn vram_sets_the_default_capacity() {
        assert_eq!(parse_vram_mib("24576\n24576\n"), Some((2, 49152)));
        assert_eq!(parse_vram_mib(" 40960 \n"), Some((1, 40960)));
        assert_eq!(parse_vram_mib("[N/A]\n"), None);
        assert_eq!(parse_vram_mib(""), None);
        assert_eq!(default_concurrency(Some((2, 49152))), 2);
        assert_eq!(default_concurrency(Some((1, 40960))), 1, "40 GB is not over 40 GB");
        assert_eq!(default_concurrency(None), 1);
    }

    fn sample(models: Vec<ModelConfig>, backend: &str, hours: Option<&str>) -> NodeConfig {
        NodeConfig {
            hub_url: DEFAULT_HUB.into(), key_file: DEFAULT_KEY.into(), enroll_token: None,
            backend: backend.into(), backend_url: Some("http://127.0.0.1:11434".into()), ollama_url: String::new(),
            backend_api_key_env: None, models, heartbeat_secs: 15, poll_secs: 20, max_concurrency: 1,
            serve_hours: hours.map(str::to_string),
            timezone: None,
            schedule: Vec::new(),
            blackout: Default::default(),
        }
    }

    #[test]
    fn the_written_config_loads_back_as_written() {
        let models = vec![
            ModelConfig { name: "qwen3:30b".into(), hash: String::new(), context: 8192, tools: false },
            ModelConfig { name: "we\"ird\\name".into(), hash: String::new(), context: 4096, tools: false },
        ];
        let text = render_config(&sample(models, "ollama", Some("18:00-08:00")), true);
        assert!(text.contains("root:exnode") && !text.contains("hash"), "{text}");
        let back: NodeConfig = toml::from_str(&text).unwrap();
        back.validate().unwrap();
        assert_eq!(back.hub_url, DEFAULT_HUB);
        assert_eq!(back.models[1].name, "we\"ird\\name");
        assert_eq!(back.serve_hours.as_deref(), Some("18:00-08:00"));
        assert_eq!((back.max_concurrency, back.backend_url()), (1, "http://127.0.0.1:11434".to_string()));

        let always = render_config(&sample(vec![ModelConfig { name: "m".into(), hash: H1.into(), context: 8192, tools: false }], "openai", None), false);
        let back: NodeConfig = toml::from_str(&always).unwrap();
        back.validate().unwrap();
        assert_eq!((back.serve_hours, back.models[0].hash.as_str()), (None, H1));
    }

    #[test]
    fn a_rewrite_shows_what_changes_and_never_the_token() {
        let old: toml::Table = toml::from_str(r#"
            hub_url = "https://user:pw@old.example.com/?k=v"
            key_file = "/var/lib/exnode/node.key"
            enroll_token = "qd_reg_secret"
            max_concurrency = 2
            [[models]]
            name = "qwen3:30b"
            context = 32768
            [[models]]
            name = "gone"
        "#).unwrap();
        let new: toml::Table = toml::from_str(&render_config(&sample(vec![
            ModelConfig { name: "qwen3:30b".into(), hash: String::new(), context: 8192, tools: false },
            ModelConfig { name: "llama3:8b".into(), hash: String::new(), context: 8192, tools: false },
        ], "ollama", None), true)).unwrap();
        let lines = summarize_changes(&old, &new);
        let all = lines.join("\n");
        assert!(!all.contains("secret") && !all.contains("pw"), "{all}");
        assert!(lines.contains(&"- enroll_token = (set)".to_string()), "{all}");
        assert!(lines.contains(&"~ max_concurrency: 2 -> 1".to_string()), "{all}");
        assert!(lines.contains(&"~ model qwen3:30b: context 32768 -> context 8192".to_string()), "{all}");
        assert!(lines.contains(&"- model gone = context default".to_string()), "{all}");
        assert!(lines.contains(&"+ model llama3:8b = context 8192".to_string()), "{all}");
        assert!(lines.iter().any(|l| l.starts_with("~ hub_url: \"https://old.example.com\"")), "{all}");
        assert!(summarize_changes(&new, &new).is_empty());
    }

    #[test]
    fn the_config_is_written_atomically_and_0640() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("exnode-setup-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, "new = 1\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 1\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("node.toml")], "no temp file left behind");
        // a directory where the file should go: the write fails and leaves nothing
        let blocked = dir.join("sub");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_atomic(&blocked, "x", None).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ids_come_from_the_user_and_group_files() {
        let group = "root:x:0:\nexnode:x:998:\nexnodes:x:5:\n";
        assert_eq!(id_in(group, "exnode"), Some(998));
        assert_eq!(id_in(group, "nobody"), None);
        assert_eq!(id_in("exnode:x:notanumber:", "exnode"), None);
    }
}
