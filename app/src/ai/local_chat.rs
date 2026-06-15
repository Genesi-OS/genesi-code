//! Login-free local AI chat.
//!
//! Talks DIRECTLY to a local OpenAI-compatible endpoint (ollama / llama-server —
//! the same one `genesi-ai-mode` runs at `http://localhost:11434`). This
//! deliberately bypasses the Warp cloud agent runtime, which is server-side and
//! event-sourced (every turn is a `POST /agent/runs` against the Warp servers
//! and requires an account). Pointing that path at `localhost` cannot work
//! because the base URL is resolved on Warp's servers, not the user's machine.
//! So for Genesi we own this small client (and the chat UI on top of it), giving
//! AI with no account and no cloud.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::stream::{Stream, StreamExt};
use reqwest_eventsource::Event;
use serde::{Deserialize, Serialize};

/// Upper bound on a single chat request. A local model that never loads (or an
/// ollama wedged on an oversized model) would otherwise leave the panel
/// spinning forever; this surfaces a clear timeout error instead. Generous
/// enough not to cut a slow-but-working generation from a small local model.
const CHAT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Default local endpoint: ollama's OpenAI-compatible API.
pub const DEFAULT_LOCAL_BASE_URL: &str = "http://localhost:11434/v1";

/// A single chat message in the OpenAI `chat/completions` shape.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }
}

/// Caps on how much of the active file is injected as context. A small local
/// model has a tight context window, so the whole-file case is bounded; a
/// selection is sent verbatim (the user picked it deliberately).
const CONTEXT_MAX_LINES: usize = 200;
const CONTEXT_MAX_CHARS: usize = 6000;

/// Snapshot of the code editor the user is focused on, attached to a chat turn
/// so the local model can answer about the code in front of the user — like a
/// normal AI IDE. The workspace builds this at send time (see
/// `Workspace::focused_code_context`) so it's always fresh.
#[derive(Debug, Clone)]
pub struct CodeContext {
    /// Display name of the file (the file name, not the full path).
    pub path: String,
    /// Language hint for the markdown code fence (from the file extension).
    pub language: String,
    /// A user selection: `(start_line, end_line, text)`, 1-indexed lines.
    pub selection: Option<(u32, u32, String)>,
    /// The full file text, used only when there's no selection.
    pub file_text: Option<String>,
}

impl CodeContext {
    /// Short label for the chat UI, e.g. `foo.rs · lines 12-40`.
    pub fn label(&self) -> String {
        match &self.selection {
            Some((start, end, _)) if start == end => format!("{} · line {start}", self.path),
            Some((start, end, _)) => format!("{} · lines {start}-{end}", self.path),
            None => self.path.clone(),
        }
    }

    /// Render the context as a system message prepended to the turn. Bounded so a
    /// large file can't blow a small local model's context window.
    pub fn to_system_message(&self) -> ChatMessage {
        let mut body = String::from(
            "The user is editing code in Genesi Code. Use the file context below to \
             answer their question; refer to it only as needed.\n\n",
        );
        match &self.selection {
            Some((start, end, text)) => {
                body.push_str(&format!(
                    "File: {} (selected lines {start}-{end})\n",
                    self.path
                ));
                body.push_str(&fenced(&self.language, &clamp_context(text)));
            }
            None => {
                body.push_str(&format!("File: {}\n", self.path));
                let text = self.file_text.as_deref().unwrap_or_default();
                body.push_str(&fenced(&self.language, &clamp_context(text)));
            }
        }
        ChatMessage::system(body)
    }
}

/// Truncate context text to the line/char caps, marking it when cut.
fn clamp_context(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let line_truncated = lines.len() > CONTEXT_MAX_LINES;
    let line_capped = lines[..lines.len().min(CONTEXT_MAX_LINES)].join("\n");

    let (mut out, char_truncated) = if line_capped.chars().count() > CONTEXT_MAX_CHARS {
        (
            line_capped
                .chars()
                .take(CONTEXT_MAX_CHARS)
                .collect::<String>(),
            true,
        )
    } else {
        (line_capped, false)
    };
    if line_truncated || char_truncated {
        out.push_str("\n… [truncated]");
    }
    out
}

/// Wrap text in a markdown code fence with a language hint.
fn fenced(language: &str, text: &str) -> String {
    format!("```{language}\n{text}\n```\n")
}

/// One streamed update from the model.
#[derive(Debug, Clone)]
pub enum ChatStreamItem {
    /// A chunk of assistant text to append.
    Token(String),
    /// The model finished this turn cleanly.
    Done,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
}

#[derive(Deserialize, Default)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
}

/// Native (non-OpenAI) request body for ollama's `/api/chat`.
#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    /// Keep the model resident this long after the reply, so the next message
    /// (and the AI Mode Monitor, which shares the same ollama model) doesn't pay
    /// a multi-second cold reload. ollama's default is only 5m, which expires
    /// during normal back-and-forth and makes Code feel far slower than it is.
    keep_alive: &'static str,
}

/// How long ollama should keep the model loaded between requests.
const OLLAMA_KEEP_ALIVE: &str = "30m";

/// One NDJSON line of ollama's native `/api/chat` stream: a `message.content`
/// token and, on the last line, `done: true`.
#[derive(Deserialize, Default)]
struct OllamaChatResponse {
    #[serde(default)]
    message: OllamaResponseMessage,
    /// True on the final streamed line.
    #[serde(default)]
    done: bool,
    /// Set when ollama itself errors (e.g. model not found).
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct OllamaResponseMessage {
    #[serde(default)]
    content: String,
}

/// ollama's native chat endpoint. Its OpenAI-compat `/v1/chat/completions`
/// hangs on some versions (a raw curl to it never responds, while the native
/// API and `ollama run` work) — the AI Mode monitor talks to the native API for
/// exactly this reason, so we mirror it.
const OLLAMA_NATIVE_CHAT_URL: &str = "http://localhost:11434/api/chat";

/// Stream a chat completion from the given local endpoint. Yields assistant text
/// chunks (`Token`) and a terminal `Done`. No auth headers are sent.
///
/// The two endpoints speak different protocols: ollama uses its native
/// `/api/chat` (its OpenAI-compat layer is unreliable), while genesi-ai-turbo's
/// llama-server uses the OpenAI-compatible `/v1/chat/completions` SSE stream.
pub fn stream_chat(
    endpoint: LocalEndpoint,
    model: &str,
    messages: Vec<ChatMessage>,
) -> futures::stream::BoxStream<'static, Result<ChatStreamItem>> {
    match endpoint {
        LocalEndpoint::Ollama => stream_chat_ollama_native(model.to_string(), messages).boxed(),
        LocalEndpoint::Turbo => {
            stream_chat_openai_sse(endpoint.base_url(), model, messages, None).boxed()
        }
    }
}

/// Stream a chat completion from a user-configured cloud provider (BYOK). Any
/// OpenAI-compatible endpoint works — OpenAI, OpenRouter, Groq, Together, or
/// Hugging Face's free router (`https://router.huggingface.co/v1`) — with the
/// user's own API key sent as a bearer token. Local stays the default; this is
/// purely opt-in for a bigger/faster model than the machine can run.
pub fn stream_chat_cloud(
    base_url: &str,
    model: &str,
    api_key: String,
    messages: Vec<ChatMessage>,
) -> futures::stream::BoxStream<'static, Result<ChatStreamItem>> {
    stream_chat_openai_sse(base_url, model, messages, Some(api_key)).boxed()
}

/// ollama's native `/api/chat` with `stream: true`: parse the NDJSON token
/// stream and yield tokens as they arrive. The OpenAI-compat endpoint hangs on
/// some ollama versions, so we use the native API the AI Mode monitor uses.
///
/// NOTE: deliberately NO total `.timeout()`. A GPU-less 7B streams for a while,
/// and ollama emits tokens continuously once connected, so a whole-response
/// deadline (the old 120s) aborted slow-but-working generations — exactly the
/// "error sending request" failure. Streaming removes that wall; an unreachable
/// ollama still errors quickly on `send()`.
fn stream_chat_ollama_native(
    model: String,
    messages: Vec<ChatMessage>,
) -> impl Stream<Item = Result<ChatStreamItem>> {
    async_stream::stream! {
        let body = OllamaChatRequest {
            model: &model,
            messages: &messages,
            stream: true,
            keep_alive: OLLAMA_KEEP_ALIVE,
        };
        let client = http_client::Client::new();
        let sent = client.post(OLLAMA_NATIVE_CHAT_URL).json(&body).send().await;
        let response = match sent {
            Ok(response) => response,
            Err(e) => {
                yield Err(anyhow!(
                    "failed to reach ollama at {OLLAMA_NATIVE_CHAT_URL}: {}",
                    error_chain(&e)
                ));
                return;
            }
        };
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            yield Err(anyhow!("ollama returned {status}: {detail}"));
            return;
        }

        // ollama streams newline-delimited JSON, one object per token.
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    yield Err(anyhow!("ollama stream error: {}", error_chain(&e)));
                    return;
                }
            };
            buffer.extend_from_slice(&chunk);
            while let Some(newline) = buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=newline).collect();
                let line = &line[..line.len() - 1]; // drop the newline
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<OllamaChatResponse>(line) {
                    Ok(parsed) => {
                        if let Some(err) = parsed.error {
                            yield Err(anyhow!("ollama error: {err}"));
                            return;
                        }
                        if !parsed.message.content.is_empty() {
                            yield Ok(ChatStreamItem::Token(parsed.message.content));
                        }
                        if parsed.done {
                            yield Ok(ChatStreamItem::Done);
                            return;
                        }
                    }
                    Err(e) => log::warn!("ollama: skipping malformed NDJSON line: {e}"),
                }
            }
        }
        yield Ok(ChatStreamItem::Done);
    }
}

/// Walk an error's source chain into one string so callers see the real cause
/// (e.g. "connection refused") instead of reqwest's opaque "error sending
/// request for url".
fn error_chain<E: std::error::Error>(err: &E) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    out
}

/// OpenAI-compatible SSE streaming (`/v1/chat/completions`) — used for the Turbo
/// (llama-server) endpoint, whose OpenAI compat layer works correctly.
fn stream_chat_openai_sse(
    base_url: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    api_key: Option<String>,
) -> impl Stream<Item = Result<ChatStreamItem>> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model,
        messages: &messages,
        stream: true,
    };

    // `json()` serializes eagerly into the builder, so `body` (and its borrow of
    // `messages`) is no longer needed once the request is built.
    //
    // NOTE: do NOT add reqwest's `.timeout()` here. `.eventsource()` is built
    // synchronously inside the view's `send()` (no Tokio context), and a request
    // timeout arms a Tokio timer at construction → "there is no reactor running"
    // panic. The ollama path can use a timeout because it's polled lazily under
    // spawn_stream_local (which has a Tokio context); the SSE path cannot.
    let client = http_client::Client::new();
    let mut request = client.post(url);
    // BYOK: cloud providers (OpenAI / OpenRouter / Groq / HuggingFace …) need the
    // user's key as a bearer token. The local Turbo server ignores auth, so it's
    // only attached when a non-empty key is supplied.
    if let Some(key) = api_key.as_deref().filter(|k| !k.trim().is_empty()) {
        request = request.bearer_auth(key.trim());
    }
    let event_source = request.json(&body).eventsource();

    event_source.filter_map(|event| async move {
        match event {
            Ok(Event::Open) => None,
            Ok(Event::Message(message)) => {
                let data = message.data.trim();
                // OpenAI-style terminator. ollama/llama-server both send this.
                if data == "[DONE]" {
                    return Some(Ok(ChatStreamItem::Done));
                }
                match serde_json::from_str::<ChatChunk>(data) {
                    Ok(chunk) => {
                        let text: String = chunk
                            .choices
                            .into_iter()
                            .filter_map(|choice| choice.delta.content)
                            .collect();
                        if text.is_empty() {
                            None
                        } else {
                            Some(Ok(ChatStreamItem::Token(text)))
                        }
                    }
                    Err(err) => {
                        log::warn!("local chat: skipping malformed chunk: {err}");
                        None
                    }
                }
            }
            Err(err) => {
                // reqwest's Display hides the underlying cause (e.g. "connection
                // refused" / "connection reset"), so walk the source chain and
                // append it — otherwise every failure reads the same useless
                // "error sending request for url".
                use std::error::Error as _;
                let mut detail = String::new();
                let mut source = err.source();
                while let Some(cause) = source {
                    detail.push_str(&format!(": {cause}"));
                    source = cause.source();
                }
                Some(Err(anyhow!("local chat stream error: {err}{detail}")))
            }
        }
    })
}

/// Best-effort `GET {base_url}/models` to populate the model picker. Returns the
/// model ids (e.g. `llama3.2:3b`). Errors bubble up so the UI can hint that the
/// local server (ollama) may not be running.
pub async fn list_models(base_url: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct ModelsResponse {
        #[serde(default)]
        data: Vec<ModelEntry>,
    }

    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
    }

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = http_client::Client::new();
    let response = client
        .get(url)
        .send()
        .await
        .context("failed to reach the local model endpoint")?;
    let parsed: ModelsResponse = response
        .json()
        .await
        .context("failed to parse the local model list")?;
    Ok(parsed.data.into_iter().map(|entry| entry.id).collect())
}

/// genesi-ai-turbo's speculative-decoding server (OpenAI API on :11435).
/// Same payload shape as ollama; faster generation, identical output.
pub const TURBO_LOCAL_BASE_URL: &str = "http://localhost:11435/v1";

/// llama-server's readiness endpoint (root, not under `/v1`). It answers 200
/// once a model is loaded — the reliable liveness signal for Turbo, unlike
/// `/v1/models`. This is what the AI Mode monitor polls too.
pub const TURBO_HEALTH_URL: &str = "http://localhost:11435/health";

/// Where `genesi-aid` publishes its live status (tmpfs, daemon-written).
pub const AI_MODE_STATE_FILE: &str = "/run/genesi-ai-mode/state.json";
/// User-writable override the daemon reads: `on` | `off` | absent (= auto).
pub const AI_MODE_FORCE_FILE: &str = "/run/genesi-ai-mode/force";

/// The local inference endpoint the chat talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalEndpoint {
    /// Ollama's OpenAI-compatible API on :11434 (genesi-ai-mode default).
    Ollama,
    /// genesi-ai-turbo (speculative decoding) on :11435.
    Turbo,
}

impl LocalEndpoint {
    pub fn base_url(self) -> &'static str {
        match self {
            LocalEndpoint::Ollama => DEFAULT_LOCAL_BASE_URL,
            LocalEndpoint::Turbo => TURBO_LOCAL_BASE_URL,
        }
    }

    /// Short label for the endpoint chip.
    pub fn label(self) -> &'static str {
        match self {
            LocalEndpoint::Ollama => "Local",
            LocalEndpoint::Turbo => "Turbo",
        }
    }

    /// The other endpoint (used by the chip that cycles between them).
    pub fn toggled(self) -> Self {
        match self {
            LocalEndpoint::Ollama => LocalEndpoint::Turbo,
            LocalEndpoint::Turbo => LocalEndpoint::Ollama,
        }
    }
}

/// Best-effort liveness probe: succeeds when the endpoint answers `GET /models`.
/// Used to know whether Turbo (:11435) is actually up before offering it.
pub async fn endpoint_available(base_url: &str) -> bool {
    list_models(base_url).await.is_ok()
}

/// Turbo (llama-server) liveness via `GET /health`. llama-server's `/v1/models`
/// isn't a reliable readiness signal, so probe `/health` like the AI Mode
/// monitor does. Returns true when it answers 200.
pub async fn turbo_health_ok() -> bool {
    let client = http_client::Client::new();
    match client
        .get(TURBO_HEALTH_URL)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// Snapshot of `genesi-aid`'s state, as published in `state.json`. Only the
/// fields the chat panel surfaces are parsed; everything else is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AiModeState {
    /// Whether AI Mode optimizations are currently applied.
    #[serde(default)]
    pub ai_mode_active: bool,
    /// Auto-detected workload: `active` | `warm` | `idle`.
    #[serde(default)]
    pub activity: String,
    /// User override in effect: `on` | `off` | `auto`.
    #[serde(default)]
    pub force_mode: String,
    /// Effective intensity profile: `max` | `balanced` | `battery` | `auto`.
    #[serde(default)]
    pub profile: String,
    /// Last measured generation speed, when AI Mode is active.
    #[serde(default)]
    pub tokens_per_second: Option<f64>,
}

impl AiModeState {
    /// A compact human-readable status for the header badge.
    pub fn badge_text(&self) -> String {
        let base = if self.force_mode == "on" {
            "AI Mode: on (forced)".to_string()
        } else if self.force_mode == "off" {
            "AI Mode: off (forced)".to_string()
        } else if self.ai_mode_active {
            format!(
                "AI Mode: {}",
                if self.activity.is_empty() {
                    "active"
                } else {
                    &self.activity
                }
            )
        } else {
            "AI Mode: idle".to_string()
        };
        match self.tokens_per_second {
            Some(tps) if tps > 0.0 => format!("{base} · {tps:.0} tok/s"),
            _ => base,
        }
    }
}

/// Read `genesi-aid`'s current state. Returns `None` when the daemon isn't
/// running or its state file isn't present (e.g. on a dev box / non-Genesi host).
pub fn read_ai_mode_state() -> Option<AiModeState> {
    let raw = std::fs::read_to_string(AI_MODE_STATE_FILE).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Set the AI Mode override the daemon honors. `"on"`/`"off"` force the mode;
/// `"auto"` clears the override (removes the file) so the daemon follows its
/// automatic detection again. No-op-safe when the file is already absent.
pub fn set_ai_mode_force(value: &str) -> Result<()> {
    match value {
        "auto" => match std::fs::remove_file(AI_MODE_FORCE_FILE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow!("failed to clear AI Mode override: {e}")),
        },
        "on" | "off" => std::fs::write(AI_MODE_FORCE_FILE, value)
            .map_err(|e| anyhow!("failed to set AI Mode override: {e}")),
        other => Err(anyhow!("invalid AI Mode force value: {other}")),
    }
}

// ── BYOK: user-configured cloud provider ──────────────────────────────────────

/// A user-supplied cloud provider (Bring Your Own Key). Any OpenAI-compatible
/// endpoint works. Persisted to `~/.config/genesi-code/cloud.json`. Local stays
/// the default — this is purely opt-in, for a bigger/faster model than the
/// machine can run (or a frontier model for hard tasks). Stored on-device only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudConfig {
    /// Display label, e.g. "HuggingFace" or "OpenAI".
    #[serde(default)]
    pub label: String,
    /// OpenAI-compatible base URL (usually ending in `/v1`).
    #[serde(default)]
    pub base_url: String,
    /// The user's API key, sent as a bearer token.
    #[serde(default)]
    pub api_key: String,
    /// The model id to request, e.g. `gpt-4o-mini` or a Hugging Face repo id.
    #[serde(default)]
    pub model: String,
}

impl CloudConfig {
    /// Whether this config can actually be used (has an endpoint + key).
    pub fn is_ready(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.api_key.trim().is_empty()
    }
}

/// Provider presets: `(label, base_url, default_model)`. Picking one fills in the
/// endpoint + a sensible default model so the user only has to paste their key.
/// Hugging Face's router exposes many models' free inference API behind one key.
pub fn cloud_presets() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            "HuggingFace",
            "https://router.huggingface.co/v1",
            "meta-llama/Llama-3.1-8B-Instruct",
        ),
        ("OpenAI", "https://api.openai.com/v1", "gpt-4o-mini"),
        (
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "meta-llama/llama-3.1-8b-instruct",
        ),
        (
            "Groq",
            "https://api.groq.com/openai/v1",
            "llama-3.1-8b-instant",
        ),
        (
            "Together",
            "https://api.together.xyz/v1",
            "meta-llama/Llama-3.1-8B-Instruct-Turbo",
        ),
        (
            "Google Gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-1.5-flash",
        ),
    ]
}

/// Path to the on-device BYOK config (`$XDG_CONFIG_HOME` or `~/.config`).
pub fn cloud_config_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(dir.join("genesi-code").join("cloud.json"))
}

/// Load the saved cloud provider, if any.
pub fn load_cloud_config() -> Option<CloudConfig> {
    let path = cloud_config_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the cloud provider to disk (creating the config directory).
pub fn save_cloud_config(cfg: &CloudConfig) -> Result<()> {
    let path = cloud_config_path().ok_or_else(|| anyhow!("no config directory (HOME unset)"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| anyhow!("failed to create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| anyhow!("failed to serialize config: {e}"))?;
    std::fs::write(&path, json).map_err(|e| anyhow!("failed to write cloud config: {e}"))?;
    Ok(())
}
