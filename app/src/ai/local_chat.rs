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

use anyhow::{anyhow, Context, Result};
use futures::stream::{Stream, StreamExt};
use reqwest_eventsource::Event;
use serde::{Deserialize, Serialize};

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

/// Open a streaming chat completion against `base_url` (e.g.
/// `http://localhost:11434/v1`). The returned stream yields assistant text
/// chunks (`Token`) and a terminal `Done`. No auth headers are sent.
///
/// Building the stream is cheap and synchronous; the request is driven as the
/// caller polls the stream.
pub fn stream_chat(
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
    let event_source = client.post(url).json(&body).eventsource();

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
            Err(err) => Some(Err(anyhow!("local chat stream error: {err}"))),
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
