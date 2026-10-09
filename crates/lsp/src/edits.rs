//! Turning a server's `WorkspaceEdit` into edits the editor can apply.
//!
//! Rename, quick fixes and organise-imports all answer with the same shape: a
//! set of text edits, keyed by file, that may reach well beyond the file the
//! user is looking at. This module flattens that shape into one list per file,
//! and applies a list to a string for files that are not open in an editor.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, WorkspaceEdit};

use crate::config::lsp_uri_to_path;
use crate::types::TextEdit;

/// Every edit the server asked for in one file.
#[derive(Debug, Clone)]
pub struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

/// Flatten a `WorkspaceEdit` into edits grouped by file.
///
/// Servers use either `changes` (a plain map) or `documentChanges` (a list that
/// can also create, rename or delete files). Text edits from both are kept.
/// File operations are skipped rather than half-applied: an edit list that
/// renames a file and then edits it under the new name only makes sense
/// applied in full, and that is not something this editor does yet. The count
/// of skipped operations is returned so the caller can say so instead of
/// reporting success.
pub fn workspace_edit_to_file_edits(edit: WorkspaceEdit) -> Result<(Vec<FileEdits>, usize)> {
    // A BTreeMap keeps the files in a stable order, so the same rename applies
    // the same way every time and tests can rely on it.
    let mut by_path: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();
    let mut skipped_operations = 0usize;

    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            let path = lsp_uri_to_path(&uri)?;
            by_path
                .entry(path)
                .or_default()
                .extend(edits.into_iter().map(TextEdit::from));
        }
    }

    if let Some(document_changes) = edit.document_changes {
        let text_document_edits = match document_changes {
            DocumentChanges::Edits(edits) => edits,
            DocumentChanges::Operations(operations) => operations
                .into_iter()
                .filter_map(|operation| match operation {
                    DocumentChangeOperation::Edit(edit) => Some(edit),
                    DocumentChangeOperation::Op(_) => {
                        skipped_operations += 1;
                        None
                    }
                })
                .collect(),
        };
        for document_edit in text_document_edits {
            let path = lsp_uri_to_path(&document_edit.text_document.uri)?;
            let entry = by_path.entry(path).or_default();
            for edit in document_edit.edits {
                let edit = match edit {
                    OneOf::Left(edit) => edit,
                    OneOf::Right(annotated) => annotated.text_edit,
                };
                entry.push(TextEdit::from(edit));
            }
        }
    }

    let files = by_path
        .into_iter()
        .filter(|(_, edits)| !edits.is_empty())
        .map(|(path, edits)| FileEdits { path, edits })
        .collect();
    Ok((files, skipped_operations))
}

/// Apply `edits` to `content` and return the result.
///
/// Positions are LSP positions: a zero-based line, and a column counted in
/// UTF-16 code units, which is what servers use unless another encoding was
/// negotiated. Counting in chars instead would put every edit after an emoji
/// one column early.
///
/// Edits are applied from the end of the file backwards so each one's
/// positions still refer to the original text. Overlapping edits are refused
/// rather than guessed at — the spec forbids them, and a server sending them
/// has a bug the user should hear about instead of a silently mangled file.
pub fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    let line_starts = line_start_offsets(content);

    let mut resolved = edits
        .iter()
        .enumerate()
        .map(|(order, edit)| {
            let start = byte_offset(
                content,
                &line_starts,
                edit.range.start.line,
                edit.range.start.column,
            )?;
            let end = byte_offset(
                content,
                &line_starts,
                edit.range.end.line,
                edit.range.end.column,
            )?;
            if start > end {
                return Err(anyhow!(
                    "edit range ends before it starts ({}:{} .. {}:{})",
                    edit.range.start.line,
                    edit.range.start.column,
                    edit.range.end.line,
                    edit.range.end.column
                ));
            }
            Ok((start, end, order, edit.text.as_str()))
        })
        .collect::<Result<Vec<_>>>()?;

    // Last edit first. Two inserts at the same position must come out in the
    // order the server listed them (the spec says the array order defines the
    // text order), and applying back to front reverses whatever order they are
    // applied in -- so among equal starts, the one listed LAST goes first.
    resolved.sort_by(|a, b| b.0.cmp(&a.0).then(b.2.cmp(&a.2)));

    for pair in resolved.windows(2) {
        let (later, earlier) = (pair[0], pair[1]);
        if earlier.1 > later.0 {
            return Err(anyhow!("the server sent overlapping edits"));
        }
    }

    let mut result = content.to_string();
    for (start, end, _, text) in resolved {
        result.replace_range(start..end, text);
    }
    Ok(result)
}

/// Byte offset at which each line begins. Lines end at `\n`; a `\r` before it
/// stays part of the line's text, which keeps CRLF files intact.
fn line_start_offsets(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        content
            .char_indices()
            .filter(|(_, c)| *c == '\n')
            .map(|(index, _)| index + 1),
    );
    starts
}

/// Translate an LSP position into a byte offset into `content`.
///
/// A column past the end of its line is clamped to the end of that line, and a
/// line past the end of the file to the end of the file — servers do send
/// both, most often for an edit that appends to the last line.
fn byte_offset(content: &str, line_starts: &[usize], line: usize, column: usize) -> Result<usize> {
    let Some(&line_start) = line_starts.get(line) else {
        return Ok(content.len());
    };
    let line_end = line_starts
        .get(line + 1)
        .map(|next| next - 1)
        .unwrap_or(content.len());
    // Never let the column run into the CR of a CRLF line ending.
    let text_end = if content[line_start..line_end].ends_with('\r') {
        line_end - 1
    } else {
        line_end
    };

    let mut units = 0usize;
    for (index, ch) in content[line_start..text_end].char_indices() {
        if units >= column {
            return Ok(line_start + index);
        }
        units += ch.len_utf16();
    }
    Ok(text_end)
}

#[cfg(test)]
#[path = "edits_tests.rs"]
mod tests;
