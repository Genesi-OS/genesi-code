//! Inline AI code completion.
//!
//! Suggests the rest of what you are typing, drawn in grey after the cursor and
//! accepted with Tab. The suggestion comes from the same local stack the AI
//! panel uses — Turbo (llama-server) or Ollama — rather than from a second
//! engine of its own, so enabling completions does not load a second copy of a
//! model behind the one already resident.
//!
//! The rules that matter, and why:
//!
//! * A suggestion is bound to the exact buffer offset and revision it was asked
//!   for. Anything that moves the cursor or edits the text throws it away, so a
//!   late answer can never paste itself into a place it was not written for.
//! * Only one request is ever in flight. Typing cancels the previous one by
//!   bumping a generation counter — the reply still arrives, and is dropped.
//! * Requests are debounced. Asking a local model on every keystroke would keep
//!   a GPU busy producing text the user has already typed past.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ai::local_chat::{
    stream_chat, transport_for, ChatMessage, ChatStreamItem, LocalEndpoint,
};

/// How long the buffer must be still before a completion is requested.
///
/// Long enough that a burst of typing makes one request rather than ten, short
/// enough that a pause to think produces a suggestion before the thought is
/// finished.
pub const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(350);

/// Upper bound on a suggestion, in tokens.
///
/// Inline completion is meant to finish a line or a small block. Anything
/// longer is a job for the AI panel, where the user can read it before it lands
/// in the file.
const COMPLETION_MAX_TOKENS: u32 = 96;

/// How much of the file to send before and after the cursor.
///
/// Characters, not tokens, because that is what we can measure without a
/// tokenizer. The split is deliberately lopsided: what comes before the cursor
/// determines the completion, while what comes after mostly stops the model
/// repeating code that is already there.
const PREFIX_BUDGET_CHARS: usize = 4000;
const SUFFIX_BUDGET_CHARS: usize = 1000;

/// A suggestion, and the exact place it was written for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSuggestion {
    /// The text to insert at [`Self::offset`].
    pub text: String,
    /// Byte offset in the buffer the suggestion continues from.
    pub offset: usize,
    /// The generation this suggestion belongs to. A suggestion whose generation
    /// is behind the model's current one is stale and must not be shown.
    pub generation: u64,
}

/// Everything the editor needs to know about inline completion right now.
#[derive(Debug, Default)]
pub struct InlineCompletionState {
    suggestion: Option<InlineSuggestion>,
    /// Bumped by every edit and cursor move. In-flight requests carry the value
    /// they started with, and are discarded when they come back behind it.
    generation: u64,
}

impl InlineCompletionState {
    /// The suggestion to draw, if there is a live one for `offset`.
    ///
    /// Checks the offset as well as the generation: a suggestion made for the
    /// end of one line must not be drawn after the cursor has landed at the
    /// same generation somewhere else.
    pub fn visible_suggestion(&self, offset: usize) -> Option<&InlineSuggestion> {
        self.suggestion
            .as_ref()
            .filter(|suggestion| suggestion.generation == self.generation)
            .filter(|suggestion| suggestion.offset == offset)
    }

    /// The generation a new request should carry.
    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    /// Invalidates whatever is on screen and any request in flight.
    ///
    /// Called on every edit and cursor move. Cheap on purpose — it is a counter
    /// bump, not a cancellation handshake, because the stream cannot be aborted
    /// mid-flight and does not need to be: its answer is dropped on arrival.
    pub fn invalidate(&mut self) {
        self.suggestion = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Records a completed request. Ignored when it belongs to a generation the
    /// user has already typed past.
    pub fn accept_response(&mut self, suggestion: InlineSuggestion) {
        if suggestion.generation != self.generation || suggestion.text.is_empty() {
            return;
        }
        self.suggestion = Some(suggestion);
    }

    /// Takes the current suggestion, for when the user accepts it.
    pub fn take_suggestion(&mut self, offset: usize) -> Option<InlineSuggestion> {
        let suggestion = self.visible_suggestion(offset)?.clone();
        self.suggestion = None;
        Some(suggestion)
    }
}

/// Builds the prompt for a completion at `offset` in `text`.
///
/// The model is asked for a raw continuation and nothing else. Local models are
/// eager to wrap code in fences and to explain themselves, and either would end
/// up pasted into the file verbatim, so the instruction is blunt and the
/// response is cleaned up afterwards by [`sanitize_completion`].
pub fn build_completion_messages(text: &str, offset: usize, language: Option<&str>) -> Vec<ChatMessage> {
    let offset = offset.min(text.len());
    let (before, after) = text.split_at(offset);

    let prefix = tail(before, PREFIX_BUDGET_CHARS);
    let suffix = head(after, SUFFIX_BUDGET_CHARS);

    let language_hint = language
        .map(|lang| format!(" The language is {lang}."))
        .unwrap_or_default();

    let system = format!(
        "You complete code inside an editor.{language_hint}\n\
         Continue the code at <CURSOR>. Reply with ONLY the characters that come \
         next -- no explanation, no markdown fences, no repetition of the code \
         that is already there. Stop as soon as the current statement or block \
         is finished. If nothing sensible follows, reply with nothing at all."
    );

    let user = format!("{prefix}<CURSOR>{suffix}");

    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// Strips what a local model adds around a completion despite being told not to.
///
/// Fences and a leading language tag are the two the small models produce most,
/// and both would be pasted into the file as-is. A reply that is only a fence,
/// or only whitespace, is treated as no suggestion.
pub fn sanitize_completion(raw: &str) -> String {
    let mut text = raw.trim_start_matches('\n').to_string();

    if let Some(rest) = text.strip_prefix("```") {
        // ```rust\n<code>\n``` -- drop the tag line and the closing fence.
        let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or("");
        text = rest.to_string();
        if let Some(end) = text.rfind("```") {
            text.truncate(end);
        }
    }

    // A model that echoes the instruction back is producing noise, not code.
    if text.contains("<CURSOR>") {
        return String::new();
    }

    text.trim_end().to_string()
}

/// Requests one completion.
///
/// `model` is the model id to use; `endpoint` is the transport the AI panel is
/// already on. The transport is resolved the same way the panel resolves it, so
/// a GGUF always lands on Turbo regardless of what is selected — otherwise a
/// completion would be posted to an Ollama that has never heard of the file.
pub fn completion_stream(
    model: &str,
    endpoint: LocalEndpoint,
    messages: Vec<ChatMessage>,
) -> futures::stream::BoxStream<'static, anyhow::Result<ChatStreamItem>> {
    stream_chat(
        transport_for(model, endpoint),
        model,
        messages,
        None,
        COMPLETION_MAX_TOKENS,
    )
}

/// The last `budget` characters of `text`, cut on a character boundary.
fn tail(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut start = text.len() - budget;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The first `budget` characters of `text`, cut on a character boundary.
fn head(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// llama-server's fill-in-the-middle endpoint.
///
/// Not under `/v1`: `/infill` is llama.cpp's own API, not part of the
/// OpenAI-compatible surface.
pub const TURBO_INFILL_URL: &str = "http://localhost:11435/infill";

#[derive(Serialize)]
struct InfillRequest<'a> {
    input_prefix: &'a str,
    input_suffix: &'a str,
    n_predict: u32,
    /// Sampling is kept cold. A completion that guesses creatively is worse than
    /// no completion: the user reads it, sees it is wrong, and stops trusting
    /// the feature.
    temperature: f32,
    stream: bool,
}

#[derive(Deserialize)]
struct InfillResponse {
    #[serde(default)]
    content: String,
}

/// Asks llama-server to fill the gap between `prefix` and `suffix`.
///
/// This is the endpoint that makes local completion work at all. Code models
/// are trained with fill-in-the-middle tokens, and `/infill` wraps the request
/// in whichever tokens the loaded GGUF declares. Asking the same model the same
/// question through `/v1/chat/completions` instead gets a chat answer -- prose,
/// apologies, a fenced block restating the function -- because that is the
/// shape the chat template puts it in.
///
/// Returns `None` when the server has no FIM support for the loaded model, so
/// the caller can fall back rather than showing an error for something the user
/// never asked for.
pub async fn infill_completion(prefix: &str, suffix: &str) -> Option<String> {
    let body = InfillRequest {
        input_prefix: prefix,
        input_suffix: suffix,
        n_predict: COMPLETION_MAX_TOKENS,
        temperature: 0.1,
        stream: false,
    };

    let client = http_client::Client::new();
    let response = match client.post(TURBO_INFILL_URL).json(&body).send().await {
        Ok(response) => response,
        Err(err) => {
            log::debug!("inline completion: /infill unreachable: {err}");
            return None;
        }
    };

    if !response.status().is_success() {
        // A model without FIM metadata answers 501 here. That is a fact about
        // the model, not a failure worth surfacing.
        log::debug!(
            "inline completion: /infill returned {} - falling back",
            response.status()
        );
        return None;
    }

    match response.json::<InfillResponse>().await {
        Ok(parsed) => Some(parsed.content),
        Err(err) => {
            log::debug!("inline completion: could not read /infill reply: {err}");
            None
        }
    }
}

/// Splits `text` at `offset` into the prefix and suffix `/infill` expects,
/// trimmed to the same budgets the chat path uses.
pub fn infill_context(text: &str, offset: usize) -> (&str, &str) {
    let offset = offset.min(text.len());
    let (before, after) = text.split_at(offset);
    (tail(before, PREFIX_BUDGET_CHARS), head(after, SUFFIX_BUDGET_CHARS))
}

#[cfg(test)]
#[path = "ai_completion_tests.rs"]
mod tests;
