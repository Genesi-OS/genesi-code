use super::LocalCodeEditorView;
use lsp::types::CompletionItem;
use string_offset::CharOffset;

fn completion(label: &str, sort_text: Option<&str>) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text: label.to_string(),
        insert_text_format: None,
        text_edit: None,
        detail: None,
        kind: None,
        sort_text: sort_text.map(str::to_string),
        filter_text: None,
    }
}

#[test]
fn completion_range_extends_stale_server_edit_to_live_prefix() {
    let range = LocalCodeEditorView::completion_replacement_range(
        Some(CharOffset::from(0)..CharOffset::from(3)),
        CharOffset::from(0),
        CharOffset::from(4),
    );

    assert_eq!(range, CharOffset::from(0)..CharOffset::from(4));
}

#[test]
fn completion_range_uses_live_prefix_for_empty_cursor_edit() {
    let range = LocalCodeEditorView::completion_replacement_range(
        Some(CharOffset::from(4)..CharOffset::from(4)),
        CharOffset::from(0),
        CharOffset::from(4),
    );

    assert_eq!(range, CharOffset::from(0)..CharOffset::from(4));
}

#[test]
fn completion_filter_ranks_exact_prefix_and_fuzzy_matches() {
    let items = vec![
        completion("fabulousObject", None),
        completion("format", None),
        completion("fo", None),
        completion("unrelated", None),
    ];

    let matches = LocalCodeEditorView::completion_filter(&items, "fo");

    assert_eq!(matches, vec![2, 1, 0]);
}

#[test]
fn completion_filter_uses_server_sort_text_for_ties() {
    let items = vec![
        completion("format", Some("b")),
        completion("former", Some("a")),
    ];

    let matches = LocalCodeEditorView::completion_filter(&items, "fo");

    assert_eq!(matches, vec![1, 0]);
}

#[test]
fn snippet_parser_selects_lowest_numbered_placeholder() {
    let parsed =
        LocalCodeEditorView::parse_completion_insert("fn ${2:args} ${1:name}() { $0 }", true);

    assert_eq!(parsed.text, "fn args name() {  }");
    assert_eq!(parsed.selection, Some(8..12));
}

#[test]
fn snippet_parser_uses_final_cursor_without_numbered_placeholder() {
    let parsed = LocalCodeEditorView::parse_completion_insert("return $0;", true);

    assert_eq!(parsed.text, "return ;");
    assert_eq!(parsed.selection, Some(7..7));
}
