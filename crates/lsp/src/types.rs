use std::path::PathBuf;

use lsp_types::{
    FileChangeType, FileEvent, Location as LspLocation, LocationLink, Position as LspPosition,
    Range as LspRange,
};

use crate::config::{lsp_uri_to_path, path_to_lsp_uri};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLocation {
    pub path: PathBuf,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

impl Location {
    pub fn into_lsp(self) -> LspPosition {
        LspPosition {
            line: self.line as u32,
            character: self.column as u32,
        }
    }
}

impl From<LspLocation> for Location {
    fn from(location: LspLocation) -> Self {
        Self {
            line: location.range.start.line as usize,
            column: location.range.start.character as usize,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub start: Location,
    pub end: Location,
}

impl Range {
    fn into_lsp(self) -> LspRange {
        LspRange {
            start: self.start.into_lsp(),
            end: self.end.into_lsp(),
        }
    }
}

impl From<LspRange> for Range {
    fn from(range: LspRange) -> Self {
        Self {
            start: Location {
                line: range.start.line as usize,
                column: range.start.character as usize,
            },
            end: Location {
                line: range.end.line as usize,
                column: range.end.character as usize,
            },
        }
    }
}

#[derive(Debug)]
pub struct LspDefinitionLocation {
    origin: Option<LspRange>,
    target: LspLocation,
}

impl From<LspLocation> for LspDefinitionLocation {
    fn from(location: LspLocation) -> Self {
        Self {
            origin: None,
            target: location,
        }
    }
}

impl From<LocationLink> for LspDefinitionLocation {
    fn from(location_link: LocationLink) -> Self {
        Self {
            origin: location_link.origin_selection_range,
            target: LspLocation {
                uri: location_link.target_uri,
                // Use target_selection_range (the exact identifier location) instead of
                // target_range (the full declaration range including comments/attributes)
                // to jump directly to the definition name rather than the start of the
                // declaration.
                range: location_link.target_selection_range,
            },
        }
    }
}

#[derive(Debug)]
pub struct DefinitionLocation {
    pub origin: Option<Range>,
    pub target: FileLocation,
}

impl TryFrom<LspDefinitionLocation> for DefinitionLocation {
    type Error = anyhow::Error;

    fn try_from(location: LspDefinitionLocation) -> anyhow::Result<Self> {
        let path = lsp_uri_to_path(&location.target.uri)?;

        Ok(Self {
            origin: location.origin.map(Into::into),
            target: FileLocation {
                path,
                location: location.target.into(),
            },
        })
    }
}

/// A reference location returned from the LSP textDocument/references request.
#[derive(Debug, Clone)]
pub struct ReferenceLocation {
    pub file_path: PathBuf,
    pub range: Range,
}

impl TryFrom<LspLocation> for ReferenceLocation {
    type Error = anyhow::Error;

    fn try_from(location: LspLocation) -> anyhow::Result<Self> {
        let path = lsp_uri_to_path(&location.uri)?;

        Ok(Self {
            file_path: path,
            range: location.range.into(),
        })
    }
}

/// Edit returned by the LSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub text: String,
}

impl From<lsp_types::TextEdit> for TextEdit {
    fn from(edit: lsp_types::TextEdit) -> Self {
        Self {
            range: edit.range.into(),
            text: edit.new_text,
        }
    }
}

/// The signature of the call the caret is inside, from
/// `textDocument/signatureHelp`, flattened to what the popup draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHelpInfo {
    /// The whole signature, e.g. `fn push(&mut self, value: T)`.
    pub label: String,
    /// The parameter being typed, as a char range into `label`.
    pub active_parameter: Option<std::ops::Range<usize>>,
    /// Documentation for the active parameter, when the server has it.
    pub parameter_documentation: Option<String>,
    /// Documentation for the function as a whole.
    pub documentation: Option<String>,
    /// Which overload this is, and how many there are, for `2/3`.
    pub signature_index: usize,
    pub signature_count: usize,
}

fn documentation_text(documentation: lsp_types::Documentation) -> String {
    match documentation {
        lsp_types::Documentation::String(text) => text,
        lsp_types::Documentation::MarkupContent(markup) => markup.value,
    }
}

/// Convert a UTF-16 code-unit offset into `text` into a char index.
fn utf16_to_char_index(text: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.chars().enumerate() {
        if units >= utf16_offset {
            return index;
        }
        units += ch.len_utf16();
    }
    text.chars().count()
}

impl SignatureHelpInfo {
    /// Pick the active signature and parameter out of a server response.
    ///
    /// Returns `None` when there is nothing to show, which is also how a server
    /// says the caret has left the call -- the popup should close then.
    pub fn from_lsp(help: lsp_types::SignatureHelp) -> Option<Self> {
        let signature_count = help.signatures.len();
        if signature_count == 0 {
            return None;
        }
        let signature_index =
            (help.active_signature.unwrap_or(0) as usize).min(signature_count - 1);
        let signature = help.signatures.into_iter().nth(signature_index)?;

        // The signature's own active parameter wins over the response-wide
        // one; servers that support overloads set it per signature.
        let active_index = signature
            .active_parameter
            .or(help.active_parameter)
            .map(|index| index as usize);
        let parameter = active_index.and_then(|index| {
            signature
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.get(index).cloned())
        });

        let active_parameter = parameter
            .as_ref()
            .and_then(|parameter| match &parameter.label {
                // Offsets are UTF-16 code units into the signature label.
                lsp_types::ParameterLabel::LabelOffsets([start, end]) => {
                    let start = utf16_to_char_index(&signature.label, *start as usize);
                    let end = utf16_to_char_index(&signature.label, *end as usize);
                    (start < end).then_some(start..end)
                }
                // A bare string has to be found in the label. Search after the
                // opening paren, so a parameter named like the function -- `fn
                // id(id: u32)` -- highlights the parameter and not the name.
                lsp_types::ParameterLabel::Simple(text) => {
                    if text.is_empty() {
                        return None;
                    }
                    let search_from = signature.label.find('(').map(|i| i + 1).unwrap_or(0);
                    let byte_start = signature.label[search_from..]
                        .find(text.as_str())
                        .map(|i| i + search_from)?;
                    let start = signature.label[..byte_start].chars().count();
                    Some(start..start + text.chars().count())
                }
            });

        Some(Self {
            label: signature.label,
            active_parameter,
            parameter_documentation: parameter
                .and_then(|parameter| parameter.documentation)
                .map(documentation_text),
            documentation: signature.documentation.map(documentation_text),
            signature_index,
            signature_count,
        })
    }
}

/// One occurrence of a symbol in a file, from `textDocument/documentHighlight`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolOccurrence {
    pub range: Range,
    /// An assignment or declaration rather than a read. Drawn more strongly, so
    /// the places a value changes stand out from the places it is used.
    pub is_write: bool,
}

/// What a rename would replace, from `textDocument/prepareRename`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRename {
    /// The text the rename replaces -- usually the identifier, sometimes less
    /// (a rename on `self.x` targets just `x`).
    pub range: Range,
    /// The server's suggestion for the input's initial text, when it has one.
    pub placeholder: Option<String>,
}

/// Document version that should be tracked by the LSP.
pub struct DocumentVersion(i32);

impl DocumentVersion {
    pub fn as_i32(&self) -> i32 {
        self.0
    }
}

impl From<usize> for DocumentVersion {
    fn from(version: usize) -> Self {
        Self(version as i32)
    }
}

#[derive(Debug)]
pub struct TextDocumentContentChangeEvent {
    pub range: Option<Range>,
    pub text: String,
}

impl TextDocumentContentChangeEvent {
    pub fn into_lsp(self) -> lsp_types::TextDocumentContentChangeEvent {
        lsp_types::TextDocumentContentChangeEvent {
            range: self.range.map(|range| range.into_lsp()),
            range_length: None,
            text: self.text,
        }
    }
}

/// Result from a hover request.
#[derive(Debug, Clone)]
pub struct HoverResult {
    /// The hover contents (documentation, type info, etc.)
    pub contents: HoverContents,
    /// The range of the symbol being hovered over, if provided by the server.
    pub range: Option<Range>,
}

/// The kind of markup content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupKind {
    PlainText,
    Markdown,
}

#[derive(Debug, Clone)]
pub struct HoverContentSection {
    pub value: String,
    pub kind: MarkupKind,
}

/// The contents of a hover response.
#[derive(Debug, Clone)]
pub struct HoverContents {
    pub sections: Vec<HoverContentSection>,
}

impl HoverContents {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

impl From<lsp_types::Hover> for HoverResult {
    fn from(hover: lsp_types::Hover) -> Self {
        let contents = match hover.contents {
            lsp_types::HoverContents::Scalar(value) => vec![HoverContentSection {
                value: marked_string_to_string(value),
                kind: MarkupKind::Markdown,
            }],
            lsp_types::HoverContents::Array(values) => values
                .into_iter()
                .map(|value| HoverContentSection {
                    value: marked_string_to_string(value),
                    kind: MarkupKind::Markdown,
                })
                .collect(),
            lsp_types::HoverContents::Markup(content) => {
                let kind = match content.kind {
                    lsp_types::MarkupKind::PlainText => MarkupKind::PlainText,
                    lsp_types::MarkupKind::Markdown => MarkupKind::Markdown,
                };
                vec![HoverContentSection {
                    value: content.value,
                    kind,
                }]
            }
        };

        Self {
            contents: HoverContents { sections: contents },
            range: hover.range.map(Into::into),
        }
    }
}

fn marked_string_to_string(marked: lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

/// How a completion request was triggered, forwarded as the LSP
/// `CompletionContext` so servers can tailor results (e.g. member completions
/// after a `.`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// Explicitly invoked (keybinding) or while typing an identifier character.
    Invoked,
    /// A registered trigger character (e.g. `.`) was just typed.
    TriggerCharacter(char),
}

/// A single completion candidate returned from `textDocument/completion`,
/// flattened from `lsp_types::CompletionItem` into the fields the editor needs.
#[derive(Debug, Clone, Default)]
pub struct CompletionItem {
    /// The text shown in the completion list.
    pub label: String,
    /// The text actually inserted when the item is accepted, after falling back
    /// through `textEdit.newText` and finally `label` when `insertText` is
    /// absent.
    pub insert_text: String,
    /// Whether `insert_text` should be interpreted as a snippet.
    pub insert_text_format: Option<lsp_types::InsertTextFormat>,
    /// Optional server-provided edit range/new text; some languages (notably
    /// HTML/CSS) rely on this instead of a simple prefix replacement.
    pub text_edit: Option<lsp_types::CompletionTextEdit>,
    /// Extra info (type signature, source module, ...) shown alongside the label.
    pub detail: Option<String>,
    /// The semantic kind (function, variable, ...), used to pick an icon.
    pub kind: Option<lsp_types::CompletionItemKind>,
    /// Server-provided ordering key; falls back to `label` when absent.
    pub sort_text: Option<String>,
    /// Text the client should match the typed prefix against; falls back to `label`.
    pub filter_text: Option<String>,
    /// The signature fragment servers attach to the label itself (rust-analyzer
    /// sends `(&self, x: u32)` here), shown dimmed straight after the label.
    pub label_detail: Option<String>,
    /// Where the item comes from (`std::vec`, `react/hooks`, ...), shown at the
    /// far right of the row.
    pub label_description: Option<String>,
    /// The doc comment for the candidate, flattened to plain text. This is what
    /// makes the popup explain what it is offering rather than just naming it.
    pub documentation: Option<String>,
    /// The untouched item as the server sent it. `completionItem/resolve` has to
    /// be handed the original back (rust-analyzer keys off its `data` field), so
    /// we keep it rather than trying to reconstruct one from the flattened
    /// fields. Boxed because the list can hold hundreds of these.
    pub raw: Box<lsp_types::CompletionItem>,
}

impl CompletionItem {
    /// Fold a resolved item's extra content into this one. Servers answer
    /// `completionItem/resolve` with the same item plus whatever they left out
    /// of the list response -- for most servers that is exactly the
    /// documentation and the detail line.
    pub fn merge_resolved(&mut self, resolved: lsp_types::CompletionItem) {
        if let Some(detail) = resolved.detail {
            self.detail = Some(detail);
        }
        if let Some(documentation) = resolved.documentation {
            self.documentation = Some(match documentation {
                lsp_types::Documentation::String(text) => text,
                lsp_types::Documentation::MarkupContent(markup) => markup.value,
            });
        }
        if let Some(details) = resolved.label_details {
            self.label_detail = details.detail.or(self.label_detail.take());
            self.label_description = details.description.or(self.label_description.take());
        }
    }
}

/// The result of a `textDocument/completion` request.
#[derive(Debug, Clone, Default)]
pub struct CompletionList {
    /// The candidates returned by the server, in server/sort order.
    pub items: Vec<CompletionItem>,
    /// The server signalled the list is incomplete: further user input should
    /// trigger a fresh request rather than relying on purely client-side
    /// filtering of `items`.
    pub is_incomplete: bool,
}

impl From<lsp_types::CompletionItem> for CompletionItem {
    fn from(item: lsp_types::CompletionItem) -> Self {
        let raw = Box::new(item.clone());
        // Prefer an explicit insert_text, then a text edit's new_text, then the
        // label.
        let insert_text = item
            .insert_text
            .clone()
            .or_else(|| match &item.text_edit {
                Some(lsp_types::CompletionTextEdit::Edit(edit)) => Some(edit.new_text.clone()),
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(edit)) => {
                    Some(edit.new_text.clone())
                }
                None => None,
            })
            .unwrap_or_else(|| item.label.clone());

        let documentation = item.documentation.map(|documentation| match documentation {
            lsp_types::Documentation::String(text) => text,
            lsp_types::Documentation::MarkupContent(markup) => markup.value,
        });
        let (label_detail, label_description) = match item.label_details {
            Some(details) => (details.detail, details.description),
            None => (None, None),
        };

        Self {
            label: item.label,
            insert_text,
            insert_text_format: item.insert_text_format,
            text_edit: item.text_edit,
            detail: item.detail,
            kind: item.kind,
            sort_text: item.sort_text,
            filter_text: item.filter_text,
            label_detail,
            label_description,
            documentation,
            raw,
        }
    }
}

/// A file change event that can be forwarded to the language server using
/// `workspace/didChangeWatchedFiles`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedFileChangeEvent {
    pub path: PathBuf,
    pub typ: FileChangeType,
}

impl WatchedFileChangeEvent {
    pub fn into_lsp(self) -> anyhow::Result<FileEvent> {
        Ok(FileEvent {
            uri: path_to_lsp_uri(&self.path)?,
            typ: self.typ,
        })
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
