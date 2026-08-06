//! Tests for the agent's tool protocol.
//!
//! The native-call mapping exists because of a hardware report: gpt-oss-20b
//! answered questions fine but produced "the model returned nothing" whenever it
//! was asked to DO something. A harmony model announces a tool on its own
//! channel, which llama-server surfaces as `tool_calls` rather than as answer
//! text, so a loop that only ever read the answer saw an empty turn.

use super::*;

#[test]
fn a_native_call_maps_onto_a_read_tool() {
    let tool = tool_from_native_call("read_file", r#"{"path":"src/main.rs"}"#)
        .expect("read_file should map");
    match tool {
        AgentTool::ReadFile { path } => assert_eq!(path, "src/main.rs"),
        other => panic!("expected ReadFile, got {other:?}"),
    }
}

#[test]
fn a_namespaced_function_keeps_its_leaf_name() {
    // Providers (and the harmony format) prefix the namespace.
    assert!(matches!(
        tool_from_native_call("functions.list_files", r#"{"path":"."}"#),
        Some(AgentTool::ListFiles { .. })
    ));
}

#[test]
fn list_files_defaults_to_the_project_root() {
    // A model that calls list_files with no arguments means "the project".
    match tool_from_native_call("list_files", "{}") {
        Some(AgentTool::ListFiles { path }) => assert_eq!(path, "."),
        other => panic!("expected ListFiles, got {other:?}"),
    }
}

#[test]
fn argument_names_are_matched_leniently() {
    // The model was never given a schema, so it invents plausible names.
    match tool_from_native_call("write_file", r#"{"filename":"a.js","text":"hi"}"#) {
        Some(AgentTool::WriteFile { path, content }) => {
            assert_eq!(path, "a.js");
            assert_eq!(content, "hi");
        }
        other => panic!("expected WriteFile, got {other:?}"),
    }
}

#[test]
fn an_edit_without_markers_becomes_a_write() {
    // Handing back the whole file is a write, whatever the model called it.
    match tool_from_native_call("edit_file", r#"{"path":"a.js","content":"new"}"#) {
        Some(AgentTool::WriteFile { path, content }) => {
            assert_eq!(path, "a.js");
            assert_eq!(content, "new");
        }
        other => panic!("expected WriteFile, got {other:?}"),
    }
}

#[test]
fn an_unknown_function_is_not_guessed_at() {
    assert!(tool_from_native_call("send_email", r#"{"to":"a@b.c"}"#).is_none());
}

#[test]
fn malformed_arguments_do_not_panic() {
    // Truncated JSON happens when a stream is cut off mid-arguments.
    assert!(tool_from_native_call("read_file", r#"{"path":"src/mai"#).is_none());
    assert!(tool_from_native_call("read_file", "").is_none());
}

#[test]
fn a_native_call_round_trips_through_the_text_protocol() {
    // The loop records native calls as tags so history stays in one shape; that
    // tag has to parse back into the same tool.
    let tool =
        tool_from_native_call("grep", r#"{"query":"TODO","path":"src"}"#).expect("grep should map");
    let reparsed = parse_tool_call(&tool.to_tag()).expect("the tag should parse back");
    match reparsed {
        AgentTool::Grep { query, path } => {
            assert_eq!(query, "TODO");
            assert_eq!(path, "src");
        }
        other => panic!("expected Grep, got {other:?}"),
    }
}

#[test]
fn a_write_tag_round_trips_with_its_body_intact() {
    let tool = AgentTool::WriteFile {
        path: "index.html".to_string(),
        content: "<h1>hi</h1>\nline two".to_string(),
    };
    match parse_tool_call(&tool.to_tag()) {
        Some(AgentTool::WriteFile { path, content }) => {
            assert_eq!(path, "index.html");
            assert_eq!(content, "<h1>hi</h1>\nline two");
        }
        other => panic!("expected WriteFile, got {other:?}"),
    }
}

#[test]
fn the_prompt_tells_the_model_where_the_tag_must_go() {
    // A thinking model that plans in its reasoning channel and never writes the
    // tag into its answer is the exact failure this line guards against.
    let prompt = agent_system_prompt();
    assert!(
        prompt.contains("not in your private reasoning"),
        "the harmony guidance should stay in the agent prompt"
    );
}
