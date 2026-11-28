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
//!
//! The cached part is reported too ([`Generation::cached_tokens`]), because
//! the hub may bill it at a lower price.  It is read from Ollama's
//! `prompt_eval_cached_count`, from `usage.prompt_tokens_details.cached_tokens`
//! (vLLM when started with `--enable-prompt-tokens-details`, llama.cpp,
//! SGLang), and failing that from llama.cpp's `timings.cache_n`.  An engine
//! that says nothing reports 0, and the count never exceeds the prompt's.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Map, Value};

use ex_common::models::ToolCall;

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
    /// The part of `prompt_tokens` the backend read from its prefix cache
    /// ([`prompt_counts`]); 0 when it doesn't say.  Never above `prompt_tokens`.
    pub cached_tokens: u64,
    /// The backend stopped early because `on_delta` asked it to.
    pub cancelled: bool,
    /// The calls the model made, whole, in order.  Empty unless the job
    /// offered tools.
    pub tool_calls: Vec<ToolCall>,
}

/// The tools a job offers, as the hub sent them.
#[derive(Debug, Clone, Copy)]
pub struct ToolOffer<'a> {
    /// `[{"type":"function","function":{…}}]`.
    pub tools:  &'a Value,
    /// `"auto"`, `"required"`, or a named function.  Honoured by engines that
    /// take `tool_choice` (vLLM, llama-server); Ollama has no such field and
    /// leaves the choice to the model.
    pub choice: &'a Value,
}

/// One chat job as the backend runs it.
#[derive(Debug, Clone, Copy)]
pub struct ChatJob<'a> {
    pub model:      &'a str,
    pub messages:   &'a [Value],
    pub params:     &'a Value,
    pub tools:      Option<ToolOffer<'a>>,
    pub max_tokens: u32,
    /// The context window the model runs in.
    pub window:     u32,
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

/// The whole prompt's token count and the cached part of it, from what a
/// backend reported: ([`whole_prompt`], the cached count capped at it).  No
/// cached count is 0 cached tokens.
pub fn prompt_counts(reported: u64, cached: Option<u64>) -> (u64, u64) {
    let whole = whole_prompt(reported, cached);
    (whole, cached.unwrap_or(0).min(whole))
}

/// The prompt tokens and the cached part of them that Ollama's final
/// (`done`) chunk reports: `prompt_eval_count` and `prompt_eval_cached_count`.
fn ollama_prompt_counts(done: &Value) -> (u64, u64) {
    prompt_counts(done["prompt_eval_count"].as_u64().unwrap_or(0), done["prompt_eval_cached_count"].as_u64())
}

/// The cached prompt tokens one chunk of an OpenAI-compatible stream
/// reports, if it reports any: `usage.prompt_tokens_details.cached_tokens`,
/// else llama.cpp's `timings.cache_n`.
fn openai_cached(chunk: &Value) -> Option<u64> {
    chunk["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().or_else(|| chunk["timings"]["cache_n"].as_u64())
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
                let n = ollama_name(m);
                n == model || n == latest
            }))
            .and_then(ollama_digest)
            .context(format!("no digest for {model} in ollama /api/tags"))
    }

    /// A small JSON document from a POST, on the same terms as [`Self::get_json`]
    /// (Ollama's `/api/show`).
    pub async fn post_json(&self, path: &str, body: &Value) -> Result<Option<Value>> {
        let mut req = self.client.post(format!("{}{path}", self.base)).timeout(PROBE_TIMEOUT).json(body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let mut resp = req.send().await.with_context(|| format!("backend {path}"))?;
        let status = resp.status();
        if status.is_server_error() {
            bail!("backend {path} returned {status}");
        }
        if !status.is_success() {
            return Ok(None);
        }
        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await.with_context(|| format!("backend {path}"))? {
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_PROBE_BODY {
                return Ok(None);
            }
        }
        Ok(serde_json::from_slice(&buf).ok())
    }

    /// The models the engine has, with the hash the agent would advertise
    /// for each (Ollama) and what the engine says about their windows, for
    /// `exnode setup`.  Reads only the engine's API, never its files.
    pub async fn engine_models(&self) -> Result<Vec<EngineModel>> {
        match self.kind {
            BackendKind::Ollama => {
                let tags = self.get_json("/api/tags").await?
                    .context("Ollama did not list its models (/api/tags)")?;
                let ps = self.get_json("/api/ps").await.ok().flatten().unwrap_or(Value::Null);
                let mut models = ollama_models(&tags, &ps);
                for m in &mut models {
                    if let Ok(Some(show)) = self.post_json("/api/show", &json!({"model": m.name})).await {
                        m.max_context = context_from_show(&show);
                    }
                }
                Ok(models)
            }
            BackendKind::OpenAi => {
                let list = self.get_json("/v1/models").await?
                    .context("the server did not list its models (/v1/models)")?;
                let props = self.get_json("/props").await.ok().flatten().unwrap_or(Value::Null);
                Ok(openai_models(&list, &props))
            }
        }
    }

    /// Stream a chat completion in the job's context window.  `on_delta`
    /// receives each piece of output text and returns `false` to stop
    /// generation early.  While the model reasons before its answer the
    /// engine streams no text; `on_delta` is then called with an empty piece
    /// for each reasoning event, so the caller can tell work from silence.  The calls the model makes come back whole in the
    /// [`Generation`], never in pieces.  A job that overflows the window is
    /// [`GenError::ContextExceeded`], never a shortened answer.
    pub async fn chat_stream(
        &self, job: &ChatJob<'_>, on_delta: impl FnMut(&str) -> bool,
    ) -> Result<Generation, GenError> {
        match self.kind {
            BackendKind::Ollama => self.ollama_stream(job, on_delta).await,
            BackendKind::OpenAi => self.openai_stream(job, on_delta).await,
        }
    }

    /// The body of an OpenAI-compatible `/v1/chat/completions` request.
    fn openai_body(job: &ChatJob<'_>) -> Map<String, Value> {
        let mut body = Self::picked(job.params);
        body.insert("model".into(), json!(job.model));
        body.insert("messages".into(), json!(job.messages));
        body.insert("max_tokens".into(), json!(job.max_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
        if let Some(t) = &job.tools {
            body.insert("tools".into(), t.tools.clone());
            body.insert("tool_choice".into(), t.choice.clone());
        }
        body
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
    fn ollama_body(job: &ChatJob<'_>) -> Value {
        let mut options = Self::picked(job.params);
        options.insert("num_predict".into(), json!(job.max_tokens));
        options.insert("num_ctx".into(), json!(job.window));
        let mut body = json!({
            "model": job.model, "messages": ollama_messages(job.messages), "stream": true, "options": options,
            // refuse a prompt longer than the window, and stop when it is full (Ollama 0.12.6 on)
            "truncate": false, "shift": false,
        });
        if let Some(t) = &job.tools {
            body["tools"] = t.tools.clone();
        }
        body
    }

    async fn ollama_stream(
        &self, job: &ChatJob<'_>, mut on_delta: impl FnMut(&str) -> bool,
    ) -> Result<Generation, GenError> {
        let (window, max_tokens) = (job.window, job.max_tokens);
        let body = Self::ollama_body(job);
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
                } else if obj["message"]["thinking"].as_str().is_some_and(|t| !t.is_empty()) && !on_delta("") {
                    gen.cancelled = true;
                    break 'outer;
                }
                // Ollama sends a call whole, with its arguments as an object
                for call in obj["message"]["tool_calls"].as_array().into_iter().flatten() {
                    if let Some(c) = ollama_call(call) {
                        gen.tool_calls.push(c);
                    }
                }
                if obj["done"].as_bool() == Some(true) {
                    let counted = obj["eval_count"].as_u64();
                    gen.output_tokens = counted.unwrap_or(deltas);
                    // the whole prompt: prompt_eval_count includes what came from the cache
                    (gen.prompt_tokens, gen.cached_tokens) = ollama_prompt_counts(&obj);
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
        &self, job: &ChatJob<'_>, mut on_delta: impl FnMut(&str) -> bool,
    ) -> Result<Generation, GenError> {
        let (window, max_tokens) = (job.window, job.max_tokens);
        let body = Self::openai_body(job);

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
        // prompt tokens and the cached part of them, as reported; they may come in different chunks
        let (mut prompt, mut cached): (u64, Option<u64>) = (0, None);
        let (mut length_stop, mut truncated) = (false, false);
        // the calls being streamed, by index: the name from the first piece, the arguments joined
        let mut pieces: BTreeMap<usize, (String, String)> = BTreeMap::new();
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
                    prompt = n;
                }
                if let Some(c) = openai_cached(&obj) {
                    cached = Some(c);
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
                } else if is_reasoning(&obj["choices"][0]["delta"]) && !on_delta("") {
                    gen.cancelled = true;
                    break 'outer;
                }
                for piece in obj["choices"][0]["delta"]["tool_calls"].as_array().into_iter().flatten() {
                    let index = piece["index"].as_u64().unwrap_or(0) as usize;
                    let entry = pieces.entry(index).or_default();
                    // the name arrives whole in a call's first piece; some servers repeat it, so only the first counts
                    if let Some(n) = piece["function"]["name"].as_str().filter(|n| entry.0.is_empty() && !n.is_empty()) {
                        entry.0 = n.to_string();
                    }
                    if let Some(a) = piece["function"]["arguments"].as_str() {
                        entry.1.push_str(a);
                    }
                }
            }
        }
        gen.tool_calls.extend(pieces.into_values().filter(|(name, _)| !name.is_empty()).map(|(name, arguments)| ToolCall {
            name,
            // a call with no arguments is an empty object, as OpenAI sends it
            arguments: if arguments.trim().is_empty() { "{}".to_string() } else { arguments },
        }));
        gen.output_tokens = usage.unwrap_or(deltas);
        // the whole prompt; the cached tokens are a part of it
        (gen.prompt_tokens, gen.cached_tokens) = prompt_counts(prompt, cached);
        // a length stop is the window's only when the server counted the output
        if !gen.cancelled
            && overflowed(gen.prompt_tokens, gen.output_tokens, window, max_tokens, length_stop && usage.is_some(), truncated) {
            return Err(GenError::ContextExceeded);
        }
        Ok(gen)
    }
}

/// Messages as Ollama's `/api/chat` takes them.  The hub sends OpenAI's shape:
/// an assistant's `tool_calls` carry their arguments as a JSON string, and a
/// `tool` message names its call by id.  Ollama wants the arguments as an
/// object, and a tool result by the function's name.
fn ollama_messages(messages: &[Value]) -> Vec<Value> {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    messages.iter().map(|m| match m["role"].as_str() {
        Some("assistant") if m["tool_calls"].as_array().is_some_and(|c| !c.is_empty()) => {
            let calls: Vec<Value> = m["tool_calls"].as_array().into_iter().flatten().map(|c| {
                let name = c["function"]["name"].as_str().unwrap_or_default();
                if let Some(id) = c["id"].as_str() {
                    names.insert(id.to_string(), name.to_string());
                }
                let arguments = c["function"]["arguments"].as_str()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                json!({"function": {"name": name, "arguments": arguments}})
            }).collect();
            json!({"role": "assistant", "content": m["content"], "tool_calls": calls})
        }
        Some("tool") => {
            let name = m["tool_call_id"].as_str().and_then(|id| names.get(id)).cloned().unwrap_or_default();
            json!({"role": "tool", "content": m["content"], "tool_name": name})
        }
        _ => m.clone(),
    }).collect()
}

/// A call from an Ollama chunk's `message.tool_calls`: its name and arguments
/// (an object, written out as JSON text).
fn ollama_call(call: &Value) -> Option<ToolCall> {
    let name = call["function"]["name"].as_str().filter(|n| !n.is_empty())?;
    let arguments = match &call["function"]["arguments"] {
        Value::String(s) if !s.trim().is_empty() => s.clone(),
        Value::Object(o) => Value::Object(o.clone()).to_string(),
        _ => "{}".to_string(),
    };
    Some(ToolCall { name: name.to_string(), arguments })
}

/// A model as the engine lists it, for `exnode setup`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineModel {
    pub name: String,
    /// The hash the agent advertises for it: Ollama's digest, exactly as
    /// [`Backend::model_hash`] gives it.  `None` for an OpenAI-compatible
    /// server, which has no digest API.
    pub hash: Option<String>,
    /// The largest window the engine allows: Ollama's trained context length
    /// (`/api/show`), or an OpenAI-compatible server's launch window (vLLM
    /// `max_model_len`, llama.cpp `n_ctx`).
    pub max_context: Option<u32>,
    /// The window the engine runs it in now, when it says: a model Ollama has
    /// loaded (`/api/ps`), or the server's launch window.
    pub running_context: Option<u32>,
}

/// A model's name in an Ollama `/api/tags` or `/api/ps` entry.
fn ollama_name(m: &Value) -> &str {
    m["name"].as_str().or(m["model"].as_str()).unwrap_or("")
}

/// `sha256:<digest>` from an Ollama `/api/tags` entry: the hash the agent advertises.
fn ollama_digest(m: &Value) -> Option<String> {
    m["digest"].as_str().map(|d| format!("sha256:{}", d.trim_start_matches("sha256:")))
}

fn as_window(v: &Value) -> Option<u32> {
    v.as_u64().filter(|n| *n > 0).map(|n| n.min(u32::MAX as u64) as u32)
}

/// Ollama's models from `/api/tags`, with the window of any it has loaded (`/api/ps`).
pub fn ollama_models(tags: &Value, ps: &Value) -> Vec<EngineModel> {
    let loaded = ps["models"].as_array().cloned().unwrap_or_default();
    tags["models"].as_array().into_iter().flatten()
        .filter(|m| !ollama_name(m).is_empty())
        .map(|m| {
            let name = ollama_name(m).to_string();
            let running_context = loaded.iter()
                .find(|p| ollama_name(p) == name)
                .and_then(|p| as_window(&p["context_length"]));
            EngineModel { hash: ollama_digest(m), name, max_context: None, running_context }
        })
        .collect()
}

/// The trained context length in an Ollama `/api/show` reply
/// (`model_info["<architecture>.context_length"]`).
pub fn context_from_show(show: &Value) -> Option<u32> {
    show["model_info"].as_object()?
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| as_window(v))
}

/// An OpenAI-compatible server's models from `/v1/models`, with its window:
/// vLLM's `max_model_len` per model, else llama.cpp's `n_ctx` (`/props`) when
/// it serves a single model.
pub fn openai_models(list: &Value, props: &Value) -> Vec<EngineModel> {
    let data = list["data"].as_array().cloned().unwrap_or_default();
    let n_ctx = as_window(&props["default_generation_settings"]["n_ctx"]).or_else(|| as_window(&props["n_ctx"]));
    let single = data.len() == 1;
    data.iter()
        .filter_map(|m| m["id"].as_str().filter(|id| !id.is_empty()).map(|id| (id, m)))
        .map(|(id, m)| {
            let window = as_window(&m["max_model_len"]).or(if single { n_ctx } else { None });
            EngineModel { name: id.to_string(), hash: None, max_context: window, running_context: window }
        })
        .collect()
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

/// Whether a streamed OpenAI-style delta carries reasoning (vLLM and SGLang
/// name it `reasoning` or `reasoning_content`): the model at work, with no
/// answer text yet.
fn is_reasoning(delta: &Value) -> bool {
    ["reasoning", "reasoning_content"].iter().any(|f| delta[*f].as_str().is_some_and(|t| !t.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_deltas_are_told_from_text_and_from_nothing() {
        assert!(is_reasoning(&serde_json::json!({"reasoning": "weighing it"})));
        assert!(is_reasoning(&serde_json::json!({"reasoning_content": "weighing it"})));
        assert!(!is_reasoning(&serde_json::json!({"reasoning": ""})));
        assert!(!is_reasoning(&serde_json::json!({"content": "an answer"})));
        assert!(!is_reasoning(&serde_json::json!({"role": "assistant"})));
    }

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

    fn job<'a>(messages: &'a [Value], params: &'a Value, tools: Option<ToolOffer<'a>>) -> ChatJob<'a> {
        ChatJob { model: "m", messages, params, tools, max_tokens: 64, window: 32768 }
    }

    #[test]
    fn ollama_runs_in_the_advertised_window_and_never_truncates() {
        let (msgs, params) = ([json!({"role": "user", "content": "hi"})], json!({"temperature": 0.5}));
        let b = Backend::ollama_body(&job(&msgs, &params, None));
        assert_eq!(b["options"], json!({"temperature": 0.5, "num_predict": 64, "num_ctx": 32768}));
        assert_eq!((b["truncate"].clone(), b["shift"].clone()), (json!(false), json!(false)));
    }

    fn weather() -> Value {
        json!([{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object", "properties": {}}}}])
    }

    #[test]
    fn tools_reach_an_openai_server_with_their_choice_and_an_ollama_one_without_it() {
        let (msgs, params, tools, choice) = ([json!({"role": "user", "content": "hi"})], json!({}), weather(), json!("required"));
        let offer = Some(ToolOffer { tools: &tools, choice: &choice });
        let open = Backend::openai_body(&job(&msgs, &params, offer));
        assert_eq!((open["tools"].clone(), open["tool_choice"].clone()), (tools.clone(), json!("required")));
        let ollama = Backend::ollama_body(&job(&msgs, &params, offer));
        assert_eq!(ollama["tools"], tools);
        assert!(ollama.get("tool_choice").is_none(), "Ollama has no such field");
        // a plain chat sends neither
        let plain = Backend::openai_body(&job(&msgs, &params, None));
        assert!(plain.get("tools").is_none() && plain.get("tool_choice").is_none());
        assert!(Backend::ollama_body(&job(&msgs, &params, None)).get("tools").is_none());
    }

    #[test]
    fn ollama_gets_past_calls_with_object_arguments_and_results_by_function_name() {
        let msgs = [
            json!({"role": "user", "content": "weather?"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_0", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                {"id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": "not json"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "noon"}),
            json!({"role": "tool", "tool_call_id": "call_0", "content": "18C"}),
            json!({"role": "assistant", "content": "It is 18C."}),
        ];
        let out = ollama_messages(&msgs);
        assert_eq!(out[0], msgs[0]);
        assert_eq!(out[1]["tool_calls"][0], json!({"function": {"name": "get_weather", "arguments": {"city": "Paris"}}}));
        assert_eq!(out[1]["tool_calls"][1], json!({"function": {"name": "get_time", "arguments": {}}}), "arguments that aren't an object become one");
        assert_eq!(out[2], json!({"role": "tool", "content": "noon", "tool_name": "get_time"}));
        assert_eq!(out[3], json!({"role": "tool", "content": "18C", "tool_name": "get_weather"}));
        assert_eq!(out[4], msgs[4], "a plain turn is untouched");
    }

    #[test]
    fn an_ollama_call_is_its_name_and_its_arguments_as_json_text() {
        let call = |v: Value| ollama_call(&v);
        assert_eq!(call(json!({"function": {"name": "f", "arguments": {"b": 2, "a": 1}}})),
                   Some(ToolCall { name: "f".into(), arguments: "{\"a\":1,\"b\":2}".into() }));
        assert_eq!(call(json!({"function": {"name": "f", "arguments": "{\"a\": 1}"}})).unwrap().arguments, "{\"a\": 1}");
        assert_eq!(call(json!({"function": {"name": "f"}})).unwrap().arguments, "{}");
        assert_eq!(call(json!({"function": {"arguments": {}}})), None, "a call with no name is not one");
    }

    #[test]
    fn the_prompt_count_is_the_whole_prompt() {
        assert_eq!(whole_prompt(6024, Some(6016)), 6024, "cached tokens are a subset of the count");
        assert_eq!(whole_prompt(38, None), 38);
        assert_eq!(whole_prompt(8, Some(6016)), 6024, "a count below the cached part left the cache out");
    }

    #[test]
    fn the_cached_count_is_reported_and_never_exceeds_the_prompt() {
        assert_eq!(prompt_counts(6024, Some(6016)), (6024, 6016));
        assert_eq!(prompt_counts(38, None), (38, 0), "an engine that says nothing has cached nothing");
        assert_eq!(prompt_counts(38, Some(0)), (38, 0));
        assert_eq!(prompt_counts(8, Some(6016)), (6024, 6016), "the uncached part alone: the whole is the sum");
        assert_eq!(prompt_counts(0, Some(50)), (50, 50));
    }

    #[test]
    fn ollama_reports_its_cached_prompt_tokens_in_the_done_chunk() {
        let done = json!({"done": true, "eval_count": 4, "prompt_eval_count": 40, "prompt_eval_cached_count": 32});
        assert_eq!(ollama_prompt_counts(&done), (40, 32));
        // before 0.33.3 there is no cached count
        assert_eq!(ollama_prompt_counts(&json!({"done": true, "eval_count": 4, "prompt_eval_count": 40})), (40, 0));
        assert_eq!(ollama_prompt_counts(&json!({"done": true})), (0, 0));
    }

    #[test]
    fn an_openai_server_reports_its_cached_prompt_tokens_in_usage_or_in_llama_cpp_timings() {
        // vLLM, SGLang, llama.cpp: OpenAI's own shape
        let usage = json!({"choices": [], "usage": {"prompt_tokens": 40, "completion_tokens": 4,
                           "prompt_tokens_details": {"cached_tokens": 32}}});
        assert_eq!(openai_cached(&usage), Some(32));
        // a server that gives no details (vLLM without --enable-prompt-tokens-details sends null)
        assert_eq!(openai_cached(&json!({"usage": {"prompt_tokens": 40, "completion_tokens": 4}})), None);
        assert_eq!(openai_cached(&json!({"usage": {"prompt_tokens": 40, "prompt_tokens_details": null}})), None);
        // llama.cpp without the details: its timings say how much of the prompt it reused
        let timings = json!({"choices": [], "timings": {"cache_n": 30, "prompt_n": 10, "predicted_n": 4}});
        assert_eq!(openai_cached(&timings), Some(30));
        // usage wins when a chunk has both
        let both = json!({"usage": {"prompt_tokens_details": {"cached_tokens": 32}}, "timings": {"cache_n": 30}});
        assert_eq!(openai_cached(&both), Some(32));
        // a content chunk reports nothing
        assert_eq!(openai_cached(&json!({"choices": [{"delta": {"content": "hi"}}]})), None);
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
    fn engine_models_carry_the_hash_the_agent_advertises_and_the_window() {
        let tags = json!({"models": [
            {"name": "qwen3:30b", "digest": "feedface"},
            {"model": "llama3:8b", "digest": "sha256:beef"},
            {"name": ""},
        ]});
        let ps = json!({"models": [{"name": "qwen3:30b", "context_length": 16384}]});
        assert_eq!(ollama_models(&tags, &ps), vec![
            EngineModel { name: "qwen3:30b".into(), hash: Some("sha256:feedface".into()), max_context: None, running_context: Some(16384) },
            EngineModel { name: "llama3:8b".into(), hash: Some("sha256:beef".into()), max_context: None, running_context: None },
        ]);
        let show = json!({"model_info": {"general.architecture": "qwen3moe", "qwen3moe.context_length": 262144}});
        assert_eq!(context_from_show(&show), Some(262144));
        assert_eq!(context_from_show(&json!({})), None);

        let vllm = json!({"data": [{"id": "Qwen3.8-Flash-Next", "max_model_len": 32768}, {"id": "other"}]});
        let got = openai_models(&vllm, &Value::Null);
        assert_eq!((got[0].max_context, got[1].max_context), (Some(32768), None));
        assert!(got.iter().all(|m| m.hash.is_none()));
        let llama = json!({"data": [{"id": "model.gguf"}]});
        let props = json!({"default_generation_settings": {"n_ctx": 8192}});
        assert_eq!(openai_models(&llama, &props)[0].running_context, Some(8192));
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
