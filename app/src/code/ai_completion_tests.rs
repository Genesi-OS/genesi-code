//! Tests for the pure parts of inline completion: what gets sent, what comes
//! back, and the staleness rules that stop a late answer landing in the wrong
//! place.

use super::*;

#[test]
fn a_suggestion_is_bound_to_the_offset_it_was_written_for() {
    // The whole hazard of async completion: the answer arrives after the cursor
    // has moved, and pasting it where the cursor is now would corrupt the file.
    let mut state = InlineCompletionState::default();
    let generation = state.current_generation();
    state.accept_response(InlineSuggestion {
        text: "println!()".to_string(),
        offset: 42,
        generation,
    });

    assert!(state.visible_suggestion(42).is_some());
    assert!(
        state.visible_suggestion(43).is_none(),
        "a suggestion for offset 42 must not be drawn at 43"
    );
}

#[test]
fn typing_discards_an_answer_still_in_flight() {
    let mut state = InlineCompletionState::default();
    let generation = state.current_generation();

    // The user types while the request is out.
    state.invalidate();

    state.accept_response(InlineSuggestion {
        text: "stale".to_string(),
        offset: 10,
        generation,
    });
    assert!(
        state.visible_suggestion(10).is_none(),
        "a reply from before the edit must not be shown"
    );
}

#[test]
fn an_empty_reply_is_not_a_suggestion() {
    // Local models answer with nothing when there is nothing to add. That is a
    // valid outcome, not something to render as an empty ghost.
    let mut state = InlineCompletionState::default();
    let generation = state.current_generation();
    state.accept_response(InlineSuggestion {
        text: String::new(),
        offset: 0,
        generation,
    });
    assert!(state.visible_suggestion(0).is_none());
}

#[test]
fn accepting_consumes_the_suggestion() {
    let mut state = InlineCompletionState::default();
    let generation = state.current_generation();
    state.accept_response(InlineSuggestion {
        text: "value".to_string(),
        offset: 7,
        generation,
    });

    assert_eq!(state.take_suggestion(7).map(|s| s.text), Some("value".into()));
    assert!(
        state.visible_suggestion(7).is_none(),
        "the ghost must clear once its text is in the buffer"
    );
}

#[test]
fn fenced_replies_are_unwrapped() {
    // Small models fence code however firmly they are told not to, and the
    // fence would be pasted into the file verbatim.
    assert_eq!(
        sanitize_completion("```rust\nlet x = 1;\n```"),
        "let x = 1;"
    );
    assert_eq!(sanitize_completion("let x = 1;"), "let x = 1;");
}

#[test]
fn a_reply_that_echoes_the_marker_is_dropped() {
    // A model repeating the prompt back is producing noise; inserting it would
    // put "<CURSOR>" into the user's source.
    assert_eq!(sanitize_completion("<CURSOR>let x = 1;"), "");
}

#[test]
fn the_prompt_marks_the_cursor_between_prefix_and_suffix() {
    let messages = build_completion_messages("fn main() {\n    \n}\n", 16, Some("Rust"));
    assert_eq!(messages.len(), 2);
    assert!(messages[0].content.contains("Rust"));

    let user = &messages[1].content;
    let marker = user.find("<CURSOR>").expect("the cursor is marked");
    assert!(
        user[..marker].contains("fn main()"),
        "what precedes the cursor is sent before the marker"
    );
    assert!(
        user[marker..].contains('}'),
        "what follows the cursor is sent after the marker"
    );
}

#[test]
fn context_is_trimmed_on_character_boundaries() {
    // Budgets are measured in bytes, so a naive cut lands inside a multi-byte
    // character and panics. Accented text is ordinary in comments and strings.
    let text = "á".repeat(4000);
    let messages = build_completion_messages(&text, text.len(), None);
    assert!(messages[1].content.contains("<CURSOR>"));
}

#[test]
fn the_cursor_may_sit_at_the_very_end_of_the_buffer() {
    // Completing at end-of-file is the most common case of all.
    let text = "let x = ";
    let messages = build_completion_messages(text, text.len(), None);
    assert!(messages[1].content.ends_with("<CURSOR>"));
}

#[test]
fn infill_splits_the_buffer_at_the_cursor() {
    // `/infill` takes the two sides separately rather than a marked-up string,
    // so the split has to land exactly on the cursor.
    let (prefix, suffix) = infill_context("let x = 1;\nlet y = ", 11);
    assert_eq!(prefix, "let x = 1;\n");
    assert_eq!(suffix, "let y = ");
}

#[test]
fn infill_context_survives_multi_byte_text() {
    // Same hazard as the chat path: the budgets are byte counts, and a cut
    // inside a multi-byte character panics.
    let text = format!("{}fn ", "á".repeat(4000));
    let (prefix, suffix) = infill_context(&text, text.len());
    assert!(prefix.ends_with("fn "));
    assert!(suffix.is_empty());
}

#[test]
fn infill_context_handles_a_cursor_past_the_end() {
    // Defensive: an offset from a stale buffer read must clamp, not panic.
    let (prefix, suffix) = infill_context("abc", 99);
    assert_eq!(prefix, "abc");
    assert_eq!(suffix, "");
}

#[test]
fn a_repo_level_file_separator_ends_the_completion() {
    // The bug this guards: a code model trained on repository-level FIM finishes
    // the completion and then keeps going into the next file's header, so the
    // suggestion arrived as working code followed by a stray path.
    assert_eq!(
        sanitize_completion("    let y = 2;\n<|file_sep|>src/lib.rs\nfn other() {}"),
        "    let y = 2;"
    );
}

#[test]
fn a_completion_that_is_only_a_control_token_is_dropped() {
    // Nothing left to insert once the token is cut, which is the honest outcome
    // when the model had nothing to add in the first place.
    assert_eq!(sanitize_completion("<|endoftext|>"), "");
    assert_eq!(sanitize_completion("<|file_sep|>foo.rs"), "");
}

#[test]
fn codellama_style_markers_are_cut_too() {
    // CodeLlama spells its FIM markers differently from Qwen and DeepSeek, and
    // a user can load any of them.
    assert_eq!(sanitize_completion("done();<EOT>"), "done();");
}
