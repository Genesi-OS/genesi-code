//! Tests for the pure helpers behind local model selection and error reporting.
//!
//! These cover the two failures reported on hardware: a local GGUF being
//! invisible to Code's model picker, and a Turbo request that ran and then
//! "answered nothing" because the real reason was never surfaced.

use super::*;

#[test]
fn gguf_references_are_recognised() {
    assert!(is_gguf_ref("gguf:qwen3-30b-a3b-q4_k_m"));
    assert!(is_gguf_ref("/home/u/models/mistral.gguf"));
    assert!(is_gguf_ref("MODEL.GGUF"), "the suffix check is case-insensitive");
}

#[test]
fn ollama_tags_are_not_gguf_references() {
    // Regression guard: an Ollama tag routed as a GGUF would be handed to
    // llama-server instead of Ollama and answer nothing.
    assert!(!is_gguf_ref("llama3.1:8b"));
    assert!(!is_gguf_ref("qwen2.5:7b"));
    assert!(!is_gguf_ref("gpt-oss:20b"));
    assert!(!is_gguf_ref(""));
}

#[test]
fn server_detail_unwraps_the_openai_error_shape() {
    // llama-server and the OpenAI-compatible providers answer with this shape;
    // showing the bare status code is what made the failure undiagnosable.
    let body = r#"{"error":{"message":"the request exceeds the available context size","type":"server_error"}}"#;
    assert_eq!(
        format_server_detail(body),
        ": the request exceeds the available context size"
    );
}

#[test]
fn server_detail_handles_a_plain_string_error() {
    let body = r#"{"error":"model not found"}"#;
    assert_eq!(format_server_detail(body), ": model not found");
}

#[test]
fn server_detail_passes_through_non_json() {
    assert_eq!(format_server_detail("502 Bad Gateway"), ": 502 Bad Gateway");
}

#[test]
fn server_detail_is_empty_for_an_empty_body() {
    // No body means nothing to add — the caller's own message stands alone.
    assert_eq!(format_server_detail(""), "");
    assert_eq!(format_server_detail("   \n "), "");
}

#[test]
fn server_detail_is_bounded() {
    // A stray HTML error page must not flood the chat panel.
    // ": " + 400 chars + the ellipsis = 403.
    let detail = format_server_detail(&"x".repeat(5_000));
    assert_eq!(detail.chars().count(), 403);
    assert!(detail.ends_with('…'), "truncation should be visible");
}

#[test]
fn server_detail_does_not_mark_an_exact_length_message_as_truncated() {
    // Regression guard: comparing byte lengths across two different strings made
    // a message that exactly hit the limit look truncated and gain a stray "…".
    let message = "y".repeat(400);
    let detail = format_server_detail(&format!(r#"{{"error":{{"message":"{message}"}}}}"#));
    assert_eq!(detail.chars().count(), 402, "': ' + 400 chars, no ellipsis");
    assert!(!detail.ends_with('…'));
}
