use std::collections::HashMap;

use lsp_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range as LspRange, ResourceOp,
    TextDocumentEdit, Uri, WorkspaceEdit,
};

use super::*;
use crate::types::{Location, Range};

fn edit(line: usize, start: usize, end_line: usize, end: usize, text: &str) -> TextEdit {
    TextEdit {
        range: Range {
            start: Location {
                line,
                column: start,
            },
            end: Location {
                line: end_line,
                column: end,
            },
        },
        text: text.to_string(),
    }
}

fn lsp_edit(line: u32, start: u32, end: u32, text: &str) -> lsp_types::TextEdit {
    lsp_types::TextEdit {
        range: LspRange {
            start: Position {
                line,
                character: start,
            },
            end: Position {
                line,
                character: end,
            },
        },
        new_text: text.to_string(),
    }
}

#[cfg(windows)]
fn uri(name: &str) -> Uri {
    format!("file:///C:/project/{name}").parse().unwrap()
}

#[cfg(not(windows))]
fn uri(name: &str) -> Uri {
    format!("file:///project/{name}").parse().unwrap()
}

#[test]
fn a_rename_touches_every_occurrence() {
    let content = "let count = 1;\nprint(count);\n";
    let renamed = apply_text_edits(
        content,
        &[edit(0, 4, 0, 9, "total"), edit(1, 6, 1, 11, "total")],
    )
    .unwrap();
    assert_eq!(renamed, "let total = 1;\nprint(total);\n");
}

#[test]
fn edits_apply_regardless_of_the_order_they_arrive_in() {
    // Applying front to back would shift every later position; the result must
    // be the same whichever order the server listed them in.
    let content = "a b c";
    let forward = apply_text_edits(content, &[edit(0, 0, 0, 1, "xx"), edit(0, 4, 0, 5, "zz")]);
    let backward = apply_text_edits(content, &[edit(0, 4, 0, 5, "zz"), edit(0, 0, 0, 1, "xx")]);
    assert_eq!(forward.unwrap(), "xx b zz");
    assert_eq!(backward.unwrap(), "xx b zz");
}

#[test]
fn inserts_at_one_position_keep_the_servers_order() {
    // The spec: "the order in the array defines the order in which the inserted
    // strings appear in the resulting text". Applying back to front reverses
    // whatever order it applies in, which is the trap this guards.
    let content = "()";
    let result = apply_text_edits(content, &[edit(0, 1, 0, 1, "a"), edit(0, 1, 0, 1, "b")]);
    assert_eq!(result.unwrap(), "(ab)");
}

#[test]
fn columns_are_utf16_units_not_chars() {
    // The emoji is one char but two UTF-16 units, so `x` sits at UTF-16 column
    // 5 and char column 4. Counting chars would land this edit one past it.
    let content = "\"😀\" x";
    let result = apply_text_edits(content, &[edit(0, 5, 0, 6, "y")]);
    assert_eq!(result.unwrap(), "\"😀\" y");
}

#[test]
fn a_crlf_file_keeps_its_line_endings() {
    // A column at the end of a CRLF line must land before the `\r`, or the edit
    // splits the line ending in two.
    let content = "foo\r\nbar\r\n";
    let result = apply_text_edits(content, &[edit(0, 3, 0, 3, "!")]);
    assert_eq!(result.unwrap(), "foo!\r\nbar\r\n");
}

#[test]
fn a_position_past_the_end_is_clamped() {
    // Servers append to the last line by naming a column, or a line, beyond it.
    let content = "end";
    assert_eq!(
        apply_text_edits(content, &[edit(0, 99, 0, 99, ".")]).unwrap(),
        "end."
    );
    assert_eq!(
        apply_text_edits(content, &[edit(9, 0, 9, 0, "\n")]).unwrap(),
        "end\n"
    );
}

#[test]
fn overlapping_edits_are_refused() {
    // Guessing which one wins would silently mangle the file.
    let content = "abcdef";
    let result = apply_text_edits(content, &[edit(0, 0, 0, 4, "x"), edit(0, 2, 0, 6, "y")]);
    assert!(result.is_err());
}

#[test]
fn both_workspace_edit_shapes_are_read() {
    let mut changes = HashMap::new();
    changes.insert(uri("a.rs"), vec![lsp_edit(0, 0, 3, "new")]);

    let workspace_edit = WorkspaceEdit {
        changes: Some(changes),
        document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri("b.rs"),
                version: None,
            },
            edits: vec![
                OneOf::Left(lsp_edit(1, 0, 3, "one")),
                OneOf::Right(AnnotatedTextEdit {
                    text_edit: lsp_edit(2, 0, 3, "two"),
                    annotation_id: "rename".to_string(),
                }),
            ],
        }])),
        change_annotations: None,
    };

    let (files, skipped) = workspace_edit_to_file_edits(workspace_edit).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(files.len(), 2);
    let total: usize = files.iter().map(|file| file.edits.len()).sum();
    assert_eq!(total, 3, "an annotated edit must not be dropped");
}

#[test]
fn file_operations_are_counted_not_half_applied() {
    let workspace_edit = WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(vec![
            DocumentChangeOperation::Op(ResourceOp::Create(lsp_types::CreateFile {
                uri: uri("new.rs"),
                options: None,
                annotation_id: None,
            })),
            DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri("a.rs"),
                    version: None,
                },
                edits: vec![OneOf::Left(lsp_edit(0, 0, 1, "x"))],
            }),
        ])),
        change_annotations: None,
    };

    let (files, skipped) = workspace_edit_to_file_edits(workspace_edit).unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(files.len(), 1);
}
