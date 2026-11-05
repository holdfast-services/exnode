//! Local inference backends.  The agent talks to exactly one, on localhost.
//!
//! * `ollama` — Ollama's `/api/chat` (newline-delimited JSON)
//! * `openai` — any OpenAI-compatible server: vLLM, llama.cpp `llama-server`,
//!   SGLang, or a custom stack such as the QuietBox serving repo
//!
//! # Context windows: refuse, never truncate
//!
//! Every job runs with the window the node advertised for its model.  For
//! Ollama that is `options.num_ctx`, sent on every request.  The window also
//! decides how much memory the model loads with, and Ollama's own default
//! changes with the machine.  Each request also carries `truncate: false`
//! (refuse a prompt longer than the window instead of dropping its start) and
//! `shift: false` (stop when the window is full instead of discarding the
//! oldest tokens).  Ollama has honoured both since 0.12.6, and older versions
//! ignore them.  An OpenAI-compatible server has no per-request window: it is
//! the server's own launch setting (vLLM `--max-model-len`, llama.cpp
//! `--ctx-size` per slot), and node.toml must advertise no more than that.
//!
//! A job overflowed when the backend refuses it for its context (an error
//! naming the context length or size, or llama.cpp's
//! `exceed_context_size_error`), signals truncation (llama.cpp's
//! `truncated`), stops for length short of `max_tokens`, or reports prompt
//! plus output tokens that reach the window.  Any of these is
//! [`GenError::ContextExceeded`].  The agent then refuses the job
//! (`context_exceeded`) instead of sending a receipt for a cut-off answer.
//!
//! # Prompt tokens with a prefix cache
//!
//! `prompt_tokens` means the whole prompt, the part read from a prefix cache
//! included.  Ollama's `prompt_eval_count` already counts the whole prompt;
//! `prompt_eval_cached_count` (0.33.3 on) is the cached subset of it.  The
//! OpenAI-style `usage.prompt_tokens` (vLLM, llama.cpp, SGLang) also counts
//! the whole prompt, with `prompt_tokens_details.cached_tokens` its cached
//! subset.  llama.cpp's `timings.prompt_n` leaves cached tokens out, so it is
//! never read.  A server that reports more cached tokens than prompt tokens
//! can only be counting the uncached part, so the two are added
//! ([`whole_prompt`]).

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Map, Value};

/// Sampling parameters the hub forwards; both backends accept these names.
/// Anything else in a job's `params` is dropped here too.
const PARAMS: &[&str] = ex_common::models::INFERENCE_PARAMS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Ollama,
    OpenAi,
}

impl std::str::FromStr for BackendKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "ollama" => Ok(Self::Ollama),
            "openai" => Ok(Self::OpenAi),
            _ => bail!("backend must be \"ollama\" or \"openai\", got {s:?}"),
        }
    }
}

/// What a finished generation reports.
#[derive(Debug, Clone, Default)]
pub struct Generation {
    /// Output tokens as counted by the backend (falls back to delta count).
    pub output_tokens: u64,
    /// Input tokens as counted by the backend, the whole prompt including any
    /// part read from a prefix cache (`usage.prompt_tokens`, or Ollama's
    /// `prompt_eval_count`; [`whole_prompt`]); 0 when it doesn't say.
    pub prompt_tokens: u64,
    /// The backend stopped early because `on_delta` asked it to.
    pub cancelled: bool,
}

/// Why a generation produced no answer the node can sign for.
#[derive(Debug, thiserror::Error)]
pub enum GenError {
    /// The prompt and output don't fit the model's context window: the
    /// backend refused the prompt, signalled truncation, or filled the window.
    #[error("the prompt and output do not fit the context window")]
    ContextExceeded,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Most of a backend's error body read, to tell a context overflow from
/// other failures.  The body is never logged.
const MAX_ERROR_BODY: usize = 8 * 1024;

/// How long a safety-check probe waits for the backend.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The largest probe reply read (llama.cpp's `/props` carries its chat template).
const MAX_PROBE_BODY: usize = 1024 * 1024;

/// Whether a backend's error message says the request didn't fit its
/// context window: llama.cpp's `exceed_context_size_error` ("the request
/// exceeds the available context size"), vLLM's "maximum context length",
/// SGLang's and Ollama's mentions of the context length or window.
pub fn is_context_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    ["exceed_context_size_error", "context_length_exceeded", "context length", "context size", "context window",
     "maximum context"].iter().any(|p| m.contains(p))
}

/// The whole prompt's token count from what a backend reported: `reported`
/// prompt tokens and the `cached` part of them.  Every backend we support
/// reports the whole prompt with the cached part as a subset.  A count of
/// cached tokens above the reported prompt tokens can only come from a
/// server counting the uncached part alone, and then the two are added.
pub fn whole_prompt(reported: u64, cached: Option<u64>) -> u64 {
    match cached {
        Some(c) if c > reported => reported.saturating_add(c),
        _ => reported,
    }
}

/// Whether a finished generation overflowed `window`: prompt and output
/// reach it, the backend signalled truncation, or it stopped for length
/// with fewer than `max_tokens` (so the window, not `max_tokens`, stopped it).
fn overflowed(prompt: u64, output: u64, window: u32, max_tokens: u32, length_stop: bool, truncated: bool) -> bool {
    truncated || prompt.saturating_add(output) >= window as u64 || (length_stop && output < max_tokens as u64)
}

/// A backend's non-success reply as an error, reading at most
/// [`MAX_ERROR_BODY`] of it to classify it.
async fn refused(what: &str, mut resp: reqwest::Response) -> GenError {
    let status = resp.status();
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_ERROR_BODY {
            body.truncate(MAX_ERROR_BODY);
            break;
        }
    }
    if is_context_overflow(&String::from_utf8_lossy(&body)) {
        return GenError::ContextExceeded;
    }
    GenError::Other(anyhow::anyhow!("{what} returned {status}"))
}

pub struct Backend {
    kind:    BackendKind,
    client:  Client,
    base:    String,
    api_key: Option<String>,
}

impl Backend {
    pub fn new(kind: BackendKind, base_url: &str, api_key: Option<String>) -> Self {
        Self {
            kind,
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(600))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
            base: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    pub fn kind(&self) -> BackendKind { self.kind }

    fn probe(&self, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.get(format!("{}{path}", self.base)).timeout(PROBE_TIMEOUT);
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None    => req,
        }
    }

    /// A small JSON document from the backend, for the safety checks
    /// (Ollama's `/api/version`, llama.cpp's `/props`).  `Ok(None)` when the
    /// backend doesn't give it to us: no such route, auth refused, or not
    /// JSON.  An error when the backend can't be reached or fails (5xx).  The
    /// document is read only for its version fields and is never logged.
    pub async fn get_json(&self, path: &str) -> Result<Option<Value>> {
        let mut resp = self.probe(path).send().await.with_context(|| format!("backend {path}"))?;
        let status = resp.status();
        if status.is_server_error() {
            bail!("backend {path} returned {status}");
        }
        if !status.is_success() {
            return Ok(None);
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.with_context(|| format!("backend {path}"))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_PROBE_BODY {
                return Ok(None);
            }
        }
        Ok(serde_json::from_slice(&body).ok())
    }

    /// Whether the backend answers its model list (`/api/tags`, `/v1/models`).
    pub async fn ping(&self) -> Result<()> {
        let path = match self.kind {
            BackendKind::Ollama => "/api/tags",
            BackendKind::OpenAi => "/v1/models",
        };
        let resp = self.probe(path).send().await.with_context(|| format!("backend {path}"))?;
        if !resp.status().is_success() {
            bail!("backend {path} returned {}", resp.status());
        }
        Ok(())
    }

    /// The digest Ollama reports for a model.  OpenAI-compatible servers have
    /// no equivalent, so their hashes come from config.
    pub async fn model_hash(&self, model: &str) -> Result<String> {
        if self.kind != BackendKind::Ollama {
            bail!("the openai backend has no model digest API; set `hash` for {model} in node.toml");
        }
        let tags: Value = self.client.get(format!("{}/api/tags", self.base)).send().await
            .context("ollama /api/tags")?
            .json().await?;
        let latest = format!("{model}:latest");
        tags["models"].as_array()
            .and_then(|ms| ms.iter().find(|m| {
                let n = m["name"].as_str().or(m["model"].as_str()).unwrap_or("");
                n == model || n == latest
            }))
            .and_then(|m| m["digest"].as_str())
            .map(|d| format!("sha256:{}", d.trim_start_matches("sha256:")))
            .context(format!("no digest for {model} in ollama /api/tags"))
    }

    /// Stream a chat completion in a context window of `window` tokens.
    /// `on_delta` receives each piece of output text and returns `false` to
    /// stop generation early.  A job that overflows the window is
    /// [`GenError::ContextExceeded`], never a shortened answer.
    pub async fn chat_stream(
        &self,
        model: &str,
        messages: &[Value],
        params: &Value,
        max_tokens: u32,
        window: u32,
        on_delta: impl FnMut(&str) -> bool,
    ) -> Result<Generation, GenError> {
        match self.kind {
            BackendKind::Ollama => self.ollama_stream(model, messages, params, max_tokens, window, on_delta).await,
            BackendKind::OpenAi => self.openai_stream(model, messages, params, max_tokens, window, on_delta).await,
        }
    }

    fn picked(params: &Value) -> Map<String, Value> {
        let mut out = Map::new();
        for k in PARAMS {
            if let Some(v) = params.get(*k).filter(|v| !v.is_null()) {
                out.insert((*k).to_string(), v.clone());
            }
        }
        out
    }

    /// The body of an Ollama `/api/chat` request: the model's window as
    /// `num_ctx`, and neither truncation nor context shift.
    fn ollama_body(model: &str, messages: &[Value], params: &Value, max_tokens: u32, window: u32) -> Value {
        let mut options = Self::picked(params);
        options.insert("num_predict".into(), json!(max_tokens));
        options.insert("num_ctx".into(), json!(window));
        json!({
            "model": model, "messages": messages, "stream": true, "options": options,
            // refuse a prompt longer than the window, and stop when it is full (Ollama 0.12.6 on)
            "truncate": false, "shift": false,
        })
    }

    async fn ollama_stream(
        &self, model: &str, messages: &[Value], params: &Value, max_tokens: u32, window: u32,
        mut on_delta: impl FnMut(&str) -> bool,
    ) -> Result<Generation, GenError> {
        let body = Self::ollama_body(model, messages, params, max_tokens, window);
        let resp = self.client.post(format!("{}/api/chat", self.base)).json(&body).send().await
            .context("ollama /api/chat")?;
        if !resp.status().is_success() {
            return Err(refused("ollama", resp).await);
        }

        let mut gen = Generation::default();
        let mut deltas = 0u64;
        let mut lines = LineReader::default();
        let mut stream = resp.bytes_stream();
        'outer: while let Some(chunk) = stream.next().await {
            for line in lines.push(&chunk.context("ollama stream")?) {
                let Ok(obj) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(e) = obj["error"].as_str() {
                    if is_context_overflow(e) {
                        return Err(GenError::ContextExceeded);
                    }
                    return Err(anyhow::anyhow!("ollama reported an error").into());
                }
                if let Some(c) = obj["message"]["content"].as_str().filter(|c| !c.is_empty()) {
                    deltas += 1;
                    if !on_delta(c) {
                        gen.cancelled = true;
                        break 'outer;
                    }
                }
                if obj["done"].as_bool() == Some(true) {
                    let counted = obj["eval_count"].as_u64();
                    gen.output_tokens = counted.unwrap_or(deltas);
                    // the whole prompt: prompt_eval_count includes what came from the cache
                    gen.prompt_tokens = whole_prompt(
                        obj["prompt_eval_count"].as_u64().unwrap_or(0), obj["prompt_eval_cached_count"].as_u64());
                    // "length" with fewer than num_predict tokens: the window stopped it
                    let length_stop = obj["done_reason"].as_str() == Some("length") && counted.is_some();
                    if overflowed(gen.prompt_tokens, gen.output_tokens, window, max_tokens, length_stop, false) {
                        return Err(GenError::ContextExceeded);
                    }
                    return Ok(gen);
                }
            }
        }
        gen.output_tokens = deltas;
        Ok(gen)
    }

    async fn openai_stream(
        &self, model: &str, messages: &[Value], params: &Value, max_tokens: u32, window: u32,
        mut on_delta: impl FnMut(&str) -> bool,
    ) -> Result<Generation, GenError> {
        let mut body = Self::picked(params);
        body.insert("model".into(), json!(model));
        body.insert("messages".into(), json!(messages));
        body.insert("max_tokens".into(), json!(max_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));

        let mut req = self.client.post(format!("{}/v1/chat/completions", self.base)).json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.context("backend /v1/chat/completions")?;
        if !resp.status().is_success() {
            return Err(refused("backend", resp).await);
        }

        let mut gen = Generation::default();
        let mut deltas = 0u64;
        let mut usage: Option<u64> = None;
        let (mut length_stop, mut truncated) = (false, false);
        let mut lines = LineReader::default();
        let mut stream = resp.bytes_stream();
        'outer: while let Some(chunk) = stream.next().await {
            for line in lines.push(&chunk.context("backend stream")?) {
                let Some(data) = line.strip_prefix("data:").map(str::trim) else { continue };
                if data == "[DONE]" { break 'outer; }
                let Ok(obj) = serde_json::from_str::<Value>(data) else { continue };
                let error = &obj["error"];
                if !error.is_null() {
                    if is_context_overflow(&error.to_string()) {
                        return Err(GenError::ContextExceeded);
                    }
                    return Err(anyhow::anyhow!("backend reported an error").into());
                }
                if let Some(n) = obj["usage"]["completion_tokens"].as_u64() {
                    usage = Some(n);
                }
                if let Some(n) = obj["usage"]["prompt_tokens"].as_u64() {
                    // the whole prompt; cached_tokens is a part of it
                    gen.prompt_tokens = whole_prompt(n, obj["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64());
                }
                if obj["choices"][0]["finish_reason"].as_str() == Some("length") {
                    length_stop = true;
                }
                // llama.cpp: the prompt and output passed its context size
                if obj["truncated"].as_bool() == Some(true) {
                    truncated = true;
                }
                if let Some(c) = obj["choices"][0]["delta"]["content"].as_str().filter(|c| !c.is_empty()) {
                    deltas += 1;
                    if !on_delta(c) {
                        gen.cancelled = true;
                        break 'outer;
                    }
                }
            }
        }
        gen.output_tokens = usage.unwrap_or(deltas);
        // a length stop is the window's only when the server counted the output
        if !gen.cancelled
            && overflowed(gen.prompt_tokens, gen.output_tokens, window, max_tokens, length_stop && usage.is_some(), truncated) {
            return Err(GenError::ContextExceeded);
        }
        Ok(gen)
    }
}

/// Splits a byte stream into complete UTF-8 lines without corrupting
/// multi-byte characters that straddle chunk boundaries.
#[derive(Default)]
struct LineReader {
    buf: Vec<u8>,
}

impl LineReader {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let s = String::from_utf8_lossy(&line).trim().to_string();
            if !s.is_empty() { out.push(s); }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reader_keeps_split_utf8_intact() {
        let mut r = LineReader::default();
        let text = "data: {\"c\":\"héllo\"}\n".as_bytes();
        let (a, b) = text.split_at(14); // splits inside 'é'
        assert!(r.push(a).is_empty());
        assert_eq!(r.push(b), vec!["data: {\"c\":\"héllo\"}".to_string()]);
    }

    #[test]
    fn only_known_params_pass() {
        let p = Backend::picked(&json!({"temperature": 0.2, "tools": [1], "stream": true, "seed": null}));
        assert_eq!(Value::Object(p), json!({"temperature": 0.2}));
    }

    #[test]
    fn ollama_runs_in_the_advertised_window_and_never_truncates() {
        let b = Backend::ollama_body("m", &[json!({"role": "user", "content": "hi"})], &json!({"temperature": 0.5}), 64, 32768);
        assert_eq!(b["options"], json!({"temperature": 0.5, "num_predict": 64, "num_ctx": 32768}));
        assert_eq!((b["truncate"].clone(), b["shift"].clone()), (json!(false), json!(false)));
    }

    #[test]
    fn the_prompt_count_is_the_whole_prompt() {
        assert_eq!(whole_prompt(6024, Some(6016)), 6024, "cached tokens are a subset of the count");
        assert_eq!(whole_prompt(38, None), 38);
        assert_eq!(whole_prompt(8, Some(6016)), 6024, "a count below the cached part left the cache out");
    }

    #[test]
    fn overflow_is_the_window_reached_truncation_or_a_short_length_stop() {
        assert!(!overflowed(100, 64, 4096, 64, true, false), "a length stop at max_tokens is max_tokens");
        assert!(overflowed(100, 30, 4096, 64, true, false), "a length stop short of max_tokens is the window");
        assert!(overflowed(4000, 96, 4096, 512, false, false), "prompt and output reach the window");
        assert!(overflowed(10, 1, 4096, 64, false, true), "the backend said it truncated");
        assert!(!overflowed(4000, 95, 4096, 512, false, false));
    }

    #[test]
    fn context_refusals_are_told_apart_from_other_errors() {
        for e in [
            r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":5000,"n_ctx":4096}}"#,
            r#"{"object":"error","message":"This model's maximum context length is 4096 tokens. However, you requested 5000 tokens","type":"BadRequestError"}"#,
            r#"{"error":"input length exceeds the context length"}"#,
        ] {
            assert!(is_context_overflow(e), "{e}");
        }
        assert!(!is_context_overflow(r#"{"error":"model 'x' not found"}"#));
    }
}
