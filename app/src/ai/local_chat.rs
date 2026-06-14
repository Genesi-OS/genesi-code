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
}

/// ollama's native `/api/chat` reply (with `stream: false`).
#[derive(Deserialize, Default)]
struct OllamaChatResponse {
    #[serde(default)]
    message: OllamaResponseMessage,
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
) -> futures::stream::LocalBoxStream<'static, Result<ChatStreamItem>> {
    match endpoint {
        LocalEndpoint::Ollama => {
            stream_chat_ollama_native(model.to_string(), messages).boxed_local()
        }
        LocalEndpoint::Turbo => {
            stream_chat_openai_sse(endpoint.base_url(), model, messages).boxed_local()
        }
    }
}

/// ollama's native `/api/chat` with `stream: false`: surface the whole reply as
/// a single token. This reuses the exact `send()` + `json()` call path that
/// already works for `list_models`, avoiding the SSE machinery that ollama's
/// OpenAI-compat endpoint chokes on.
fn stream_chat_ollama_native(
    model: String,
    messages: Vec<ChatMessage>,
) -> impl Stream<Item = Result<ChatStreamItem>> {
    async_stream::stream! {
        let body = OllamaChatRequest {
            model: &model,
            messages: &messages,
            stream: false,
        };
        let client = http_client::Client::new();
        let sent = client
            .post(OLLAMA_NATIVE_CHAT_URL)
            .json(&body)
            .timeout(CHAT_REQUEST_TIMEOUT)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(e) => {
                yield Err(anyhow!("failed to reach ollama at {OLLAMA_NATIVE_CHAT_URL}: {e}"));
                return;
            }
        };
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            yield Err(anyhow!("ollama returned {status}: {detail}"));
            return;
        }
        let parsed: OllamaChatResponse = match response.json().await {
            Ok(parsed) => parsed,
            Err(e) => {
                yield Err(anyhow!("failed to parse the ollama reply: {e}"));
                return;
            }
        };
        if let Some(err) = parsed.error {
            yield Err(anyhow!("ollama error: {err}"));
            return;
        }
        if !parsed.message.content.is_empty() {
            yield Ok(ChatStreamItem::Token(parsed.message.content));
        }
        yield Ok(ChatStreamItem::Done);
    }
}

/// OpenAI-compatible SSE streaming (`/v1/chat/completions`) — used for the Turbo
/// (llama-server) endpoint, whose OpenAI compat layer works correctly.
fn stream_chat_openai_sse(
    base_url: &str,
    model: &str,
    messages: Vec<ChatMessage>,
) -> impl Stream<Item = Result<ChatStreamItem>> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model,
        messages: &messages,
        stream: true,
    };

    // `json()` serializes eagerly into the builder, so `body` (and its borrow of
    // `messages`) is no longer needed once the request is built.
    let client = http_client::Client::new();
    let event_source = client
        .post(url)
        .json(&body)
        .timeout(CHAT_REQUEST_TIMEOUT)
        .eventsource();

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
            format!("AI Mode: {}", if self.activity.is_empty() { "active" } else { &self.activity })
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
