use lsp::{HoverContents, LspServerLogLevel, MarkupKind};
use markdown_parser::{FormattedText, FormattedTextFragment, FormattedTextLine};
use num_traits::SaturatingSub;
use std::ops::Range;
use string_offset::CharOffset;
use warp_core::send_telemetry_from_ctx;
use warp_core::ui::appearance::Appearance;
use warp_core::ui::theme::color::internal_colors;
use warp_core::ui::theme::WarpTheme;
use warp_editor::content::buffer::InitialBufferState;
use warp_editor::render::element::VerticalExpansionBehavior;
use warp_editor::render::model::Decoration;
use warpui::elements::{
    Border, ChildView, ClippedScrollStateHandle, ClippedScrollable, ConstrainedBox, Container,
    CornerRadius, CrossAxisAlignment, Flex, FormattedTextElement, HighlightedHyperlink, Hoverable,
    MouseStateHandle, ParentElement, Radius, Rect, ScrollbarWidth,
};
use warpui::text::point::Point;
use warpui::{AppContext, Element, SingletonEntity, ViewContext};

use super::editor::view::{CodeEditorRenderOptions, CodeEditorView};
use super::lsp_telemetry::LspTelemetryEvent;
use crate::code::local_code_editor::{
    HoverContentSegment, LocalCodeEditorView, LspCompletionState, LspHoverState,
    COMPLETION_MAX_VISIBLE_ITEMS, COMPLETION_POPUP_MAX_HEIGHT, COMPLETION_POPUP_MAX_WIDTH,
    HOVER_TOOLTIP_MAX_HEIGHT, HOVER_TOOLTIP_MAX_WIDTH,
};
use crate::editor::InteractionState;
use vec1::Vec1;

/// A processed diagnostic with its converted offset range.
/// Stored on LocalCodeEditorView and used for both decoration and hover display.
#[derive(Clone)]
pub struct ProcessedDiagnostic {
    /// The diagnostic message.
    pub message: String,
    /// The severity of the diagnostic.
    pub severity: lsp_types::DiagnosticSeverity,
    /// The start offset (0-based, for rendering).
    pub start: CharOffset,
    /// The end offset (0-based, for rendering).
    pub end: CharOffset,
}

enum PendingSection {
    Markdown(Vec<FormattedTextLine>),
    Code { language: String, code: String },
}

#[derive(Default)]
struct PendingSections {
    sections: Vec<PendingSection>,
    pending: Option<PendingSection>,
    active_line_break: bool,
}

impl PendingSections {
    fn push_formatted_line(&mut self, line: FormattedTextLine) {
        match line {
            FormattedTextLine::LineBreak
                if self.active_line_break
                    || matches!(self.pending, Some(PendingSection::Code { .. }) | None) => {}
            FormattedTextLine::HorizontalRule => {
                self.active_line_break = false;
                if let Some(section) = self.pending.take() {
                    self.sections.push(section);
                }
            }
            FormattedTextLine::CodeBlock(code_block) => {
                self.active_line_break = false;
                match self.pending.take() {
                    Some(pending @ PendingSection::Markdown(_)) => {
                        self.sections.push(pending);
                    }
                    Some(PendingSection::Code { mut code, language }) => {
                        if language == code_block.lang {
                            code.push('\n');
                            code.push_str(code_block.code.trim());

                            self.pending = Some(PendingSection::Code { code, language });
                            return;
                        }
                        self.sections.push(PendingSection::Code { code, language });
                    }
                    None => (),
                };
                self.pending = Some(PendingSection::Code {
                    code: code_block.code,
                    language: code_block.lang,
                })
            }
            other => {
                self.active_line_break = matches!(other, FormattedTextLine::LineBreak);
                match self.pending.take() {
                    Some(code @ PendingSection::Code { .. }) => {
                        self.sections.push(code);
                        self.pending = Some(PendingSection::Markdown(vec![other]));
                    }
                    Some(PendingSection::Markdown(mut markdown)) => {
                        markdown.push(other);
                        self.pending = Some(PendingSection::Markdown(markdown));
                    }
                    None => self.pending = Some(PendingSection::Markdown(vec![other])),
                }
            }
        }
    }

    fn flush(self, ctx: &mut ViewContext<LocalCodeEditorView>) -> Vec<HoverContentSegment> {
        let mut segments = Vec::new();
        for section in self.sections {
            match section {
                PendingSection::Markdown(text_lines) => {
                    segments.push(HoverContentSegment::Text(FormattedText::new(text_lines)))
                }
                PendingSection::Code { language, code } => segments.push(
                    LocalCodeEditorView::create_highlighted_code_fragment(language, code, ctx),
                ),
            }
        }

        if let Some(pending) = self.pending {
            match pending {
                PendingSection::Markdown(text_lines) => {
                    segments.push(HoverContentSegment::Text(FormattedText::new(text_lines)))
                }
                PendingSection::Code { language, code } => segments.push(
                    LocalCodeEditorView::create_highlighted_code_fragment(language, code, ctx),
                ),
            }
        }
        segments
    }
}

struct ParsedCompletionInsert {
    text: String,
    cursor_offset: Option<usize>,
}

impl LocalCodeEditorView {
    /// Refresh diagnostics from the LSP server.
    /// This updates the cached processed diagnostics and creates decorations for the editor.
    pub(super) fn refresh_diagnostics(&mut self, ctx: &mut ViewContext<Self>) {
        // Update cached processed diagnostics.
        self.processed_diagnostics = self.compute_processed_diagnostics(ctx);

        // Convert processed diagnostics to decorations.
        let appearance = Appearance::as_ref(ctx);
        let error_color = appearance.theme().ui_error_color();
        let warning_color = appearance.theme().ui_warning_color();

        let decorations: Vec<Decoration> = self
            .processed_diagnostics
            .iter()
            .map(|diag| {
                let color = match diag.severity {
                    lsp_types::DiagnosticSeverity::ERROR => error_color,
                    lsp_types::DiagnosticSeverity::WARNING => warning_color,
                    _ => error_color, // Fallback, though we filter to only errors/warnings
                };
                Decoration::new(diag.start, diag.end).with_dashed_underline(color)
            })
            .collect();

        self.diagnostic_decorations = decorations;
        self.update_editor_decorations(ctx);
    }

    pub(super) fn clear_diagnostics(&mut self, ctx: &mut ViewContext<Self>) {
        self.processed_diagnostics.clear();
        self.diagnostic_decorations.clear();
        self.update_editor_decorations(ctx);
    }

    /// Update the editor's text decorations with diagnostic decorations.
    fn update_editor_decorations(&self, ctx: &mut ViewContext<Self>) {
        // Pass diagnostic decorations to the render state.
        let decorations = self.diagnostic_decorations.clone();
        self.editor.update(ctx, |editor, ctx| {
            editor.set_diagnostic_decorations(decorations, ctx);
        });
    }

    /// Compute processed diagnostics (errors and warnings) with their converted offset ranges.
    /// Returns an empty vec if LSP server is not available or there are no diagnostics.
    fn compute_processed_diagnostics(&self, ctx: &ViewContext<Self>) -> Vec<ProcessedDiagnostic> {
        let Some(lsp_server) = self.lsp_server.as_ref() else {
            return Vec::new();
        };
        let Some(file_path) = self.file_path() else {
            return Vec::new();
        };
        let Some(doc_diagnostics) = lsp_server
            .as_ref(ctx)
            .diagnostics_for_path(file_path)
            .ok()
            .flatten()
        else {
            return Vec::new();
        };

        // Only show diagnostics that match the current buffer version.
        let current_buffer_version = self.editor.as_ref(ctx).buffer_version(ctx).as_usize() as i32;
        let diag_count = doc_diagnostics.diagnostics.len();
        let diag_age_ms = doc_diagnostics.published_at.elapsed().as_millis();

        match doc_diagnostics.version {
            Some(version) if version != current_buffer_version => {
                lsp_server.as_ref(ctx).log_to_server_log(
                    LspServerLogLevel::Info,
                    format!(
                        "render: DROPPED (version mismatch) file={} render_version={current_buffer_version} diag_version={version} diag_count={diag_count} age_ms={diag_age_ms}",
                        file_path.display(),
                    ),
                );
                return Vec::new();
            }
            Some(version) => {
                lsp_server.as_ref(ctx).log_to_server_log(
                    LspServerLogLevel::Debug,
                    format!(
                        "render: OK file={} render_version={current_buffer_version} diag_version={version} diag_count={diag_count} age_ms={diag_age_ms}",
                        file_path.display(),
                    ),
                );
            }
            None => {
                lsp_server.as_ref(ctx).log_to_server_log(
                    LspServerLogLevel::Debug,
                    format!(
                        "render: UNVERSIONED file={} render_version={current_buffer_version} diag_count={diag_count} age_ms={diag_age_ms}",
                        file_path.display(),
                    ),
                );
            }
        }

        doc_diagnostics
            .diagnostics
            .iter()
            .filter_map(|diagnostic| {
                // Only include errors and warnings.
                let severity = diagnostic.severity?;
                if !matches!(
                    severity,
                    lsp_types::DiagnosticSeverity::ERROR | lsp_types::DiagnosticSeverity::WARNING
                ) {
                    return None;
                }

                // Convert LSP range to CharOffset range.
                let range: lsp::types::Range = diagnostic.range.into();
                let mut start_offset = self
                    .editor
                    .as_ref(ctx)
                    .lsp_location_to_offset(&range.start, ctx);
                let mut end_offset = self
                    .editor
                    .as_ref(ctx)
                    .lsp_location_to_offset(&range.end, ctx);

                // Handle zero-width ranges by expanding to at least 1 character.
                if start_offset >= end_offset {
                    end_offset = start_offset + CharOffset::from(1);
                }

                // Check if the diagnostic range only covers a newline character.
                // This happens for diagnostics like "missing semicolon" that point to
                // the end of a line. In this case, shift the range back to cover the
                // last character on the line instead, so it renders visibly.
                let is_single_char_range =
                    end_offset.saturating_sub(&start_offset) == CharOffset::from(1);
                if is_single_char_range {
                    let char_at_start = self.editor.as_ref(ctx).char_at(start_offset, ctx);
                    if let Some('\n') = char_at_start {
                        // Shift range back by 1 to cover the character before the newline.
                        if start_offset > CharOffset::from(1) {
                            start_offset = start_offset.saturating_sub(&CharOffset::from(1));
                            end_offset = end_offset.saturating_sub(&CharOffset::from(1));
                        }
                    }
                }

                // Convert to 0-based offsets (render offsets).
                let start = start_offset.saturating_sub(&CharOffset::from(1));
                let end = end_offset.saturating_sub(&CharOffset::from(1));

                Some(ProcessedDiagnostic {
                    message: diagnostic.message.clone(),
                    severity,
                    start,
                    end,
                })
            })
            .collect()
    }

    /// Get diagnostics at the given offset from the cached processed diagnostics.
    /// Returns a list of ProcessedDiagnostic for any diagnostics whose range contains the offset.
    /// The input offset and ProcessedDiagnostic ranges are both 0-based render offsets.
    pub(super) fn diagnostics_at_offset(&self, offset: CharOffset) -> Vec<ProcessedDiagnostic> {
        self.processed_diagnostics
            .iter()
            .filter(|diag| offset >= diag.start && offset < diag.end)
            .cloned()
            .collect()
    }

    /// Request hover information (documentation, type info) for a given offset.
    pub fn hover_for_offset(&mut self, offset: CharOffset, ctx: &mut ViewContext<Self>) {
        if matches!(self.lsp_hover_state, LspHoverState::None) {
            return;
        }

        let lsp_position = self
            .editor()
            .as_ref(ctx)
            .offset_to_lsp_position(offset, ctx);

        let Some(file_path) = self.file_path() else {
            return;
        };

        if self.lsp_server.is_none() {
            log::warn!("No LSP server available for hover");
            return;
        }

        let future = match self
            .lsp_server
            .as_ref()
            .unwrap()
            .as_ref(ctx)
            .hover(file_path.to_path_buf(), lsp_position)
        {
            Ok(future) => future,
            Err(e) => {
                log::warn!("Failed to call lsp.hover: {e}");
                return;
            }
        };

        let abort_handle = ctx
            .spawn(future, move |me, result, ctx| {
                // Get diagnostics at the hovered offset from cached processed diagnostics.
                // We always check for diagnostics, regardless of the LSP hover result.
                let diagnostics = me.diagnostics_at_offset(offset);

                // Extract hover range and contents from the LSP result (if available).
                let (hover_range, hover_contents) = match result {
                    Ok(Some(hover_result)) => (hover_result.range, Some(hover_result.contents)),
                    _ => (None, None),
                };

                // Create hover segments if we have non-empty contents.
                let segments = match hover_contents {
                    Some(contents) if !contents.is_empty() => {
                        me.create_hover_content_segments(contents, ctx)
                    }
                    _ => Vec::new(),
                };

                // Only show the hover tooltip if there's something to display.
                if segments.is_empty() && diagnostics.is_empty() {
                    me.lsp_hover_state.clear();
                } else {
                    let had_content = !segments.is_empty();
                    let had_diagnostics = !diagnostics.is_empty();
                    if let Some(server) = me.lsp_server.as_ref() {
                        send_telemetry_from_ctx!(
                            LspTelemetryEvent::HoverShown {
                                server_type: server.as_ref(ctx).server_name(),
                                had_content,
                                had_diagnostics,
                            },
                            ctx
                        );
                    }

                    let editor = me.editor().as_ref(ctx);

                    // Determine the offset range for positioning the tooltip.
                    let offset_range = match hover_range {
                        Some(range) => {
                            let start = editor.lsp_location_to_offset(&range.start, ctx);
                            let end = editor.lsp_location_to_offset(&range.end, ctx);
                            // Rendering range is 0-based instead of 1-based.
                            start.saturating_sub(&CharOffset::from(1))
                                ..end.saturating_sub(&CharOffset::from(1))
                        }
                        None => match editor.word_range_at_offset(offset, ctx) {
                            Some(range) => {
                                range.start.saturating_sub(&CharOffset::from(1))
                                    ..range.end.saturating_sub(&CharOffset::from(1))
                            }
                            None => offset..offset + 1,
                        },
                    };

                    me.lsp_hover_state = LspHoverState::Loaded {
                        segments,
                        diagnostics,
                        hovered_offset_range: offset_range,
                        scroll_state: ClippedScrollStateHandle::default(),
                        mouse_state: MouseStateHandle::default(),
                    };
                }
                ctx.notify();
            })
            .abort_handle();

        self.lsp_hover_state = LspHoverState::Loading(Some(abort_handle));
    }

    /// Request LSP completion candidates at `request_offset` and, on success,
    /// open (or refresh) the completion popup. `anchor` is the start of the
    /// replaceable prefix (after a `.`-style trigger that's the cursor; while
    /// typing an identifier it's the start of the word). When `preserve` is
    /// true an already-open popup stays visible until the new candidates arrive
    /// — used for keystroke re-queries so the list doesn't flicker or close
    /// mid-typing.
    pub(super) fn completion_for_offset(
        &mut self,
        request_offset: CharOffset,
        anchor: CharOffset,
        trigger: lsp::CompletionTrigger,
        preserve: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(file_path) = self.file_path() else {
            return;
        };
        let Some(lsp_server) = self.lsp_server.as_ref() else {
            return;
        };

        let lsp_position = self
            .editor()
            .as_ref(ctx)
            .offset_to_lsp_position(request_offset, ctx);

        let future =
            match lsp_server
                .as_ref(ctx)
                .completion(file_path.to_path_buf(), lsp_position, trigger)
            {
                Ok(future) => future,
                Err(e) => {
                    log::warn!("Failed to call lsp.completion: {e}");
                    return;
                }
            };

        let abort_handle = ctx
            .spawn(future, move |me, result, ctx| match result {
                Ok(list) if !list.items.is_empty() => {
                    // A preserve re-query that resolves after the user dismissed
                    // the popup must not resurrect it.
                    if preserve && matches!(me.lsp_completion_state, LspCompletionState::None) {
                        return;
                    }
                    if let Some(server) = me.lsp_server.as_ref() {
                        server.as_ref(ctx).log_to_server_log(
                            LspServerLogLevel::Info,
                            format!("completion: received {} item(s)", list.items.len()),
                        );
                    }
                    me.populate_completion(anchor, list.items, list.is_incomplete, ctx);
                }
                Ok(_) => {
                    // An empty result on a keystroke re-query is often transient
                    // (server still indexing); don't tear down a popup the user is
                    // actively reading. The client-side refilter already closes it
                    // when the typed prefix truly matches nothing.
                    if !preserve {
                        me.dismiss_completion(ctx);
                    }
                }
                Err(e) => {
                    log::warn!("lsp.completion request failed: {e}");
                    if !preserve {
                        me.dismiss_completion(ctx);
                    }
                }
            })
            .abort_handle();

        // Only show the loading state (which hides any current popup) for a fresh
        // open. Re-queries keep the existing popup visible until results land.
        if !preserve {
            self.lsp_completion_state = LspCompletionState::Loading(Some(abort_handle));
        }
    }

    /// Decide what to do with the completion popup after a user content change:
    /// open it on a trigger character or while typing an identifier, keep it in
    /// sync via client-side refilter, re-query the server when the cached list is
    /// incomplete, or dismiss it when the context is no longer valid.
    pub(super) fn handle_completion_trigger(&mut self, ctx: &mut ViewContext<Self>) {
        // Read the cursor and the character just typed without holding an editor
        // borrow across the later mutable calls.
        let (cursor, typed) = {
            let editor = self.editor().as_ref(ctx);
            let cursor = editor.cursor_head_offset(ctx);
            let typed = if cursor.as_usize() == 0 {
                None
            } else {
                let before = cursor.saturating_sub(&CharOffset::from(1));
                editor.char_at(before, ctx)
            };
            (cursor, typed)
        };

        let Some(typed) = typed else {
            // Nothing before the cursor — keep an open popup in sync, else no-op.
            self.refilter_completion(ctx);
            return;
        };

        // Trigger characters are language-specific (member access `.`, tag open
        // `<`, ...), sourced from the language registry. Default to `.` when the
        // file's language is unknown.
        const FALLBACK_TRIGGERS: &[char] = &['.'];
        let trigger_chars = self
            .file_path()
            .and_then(lsp::LanguageId::from_path)
            .map(|lang| lang.trigger_chars())
            .unwrap_or(FALLBACK_TRIGGERS);

        if trigger_chars.contains(&typed) {
            // Member access / tag open / etc. Anchor at the cursor: the prefix
            // grows from here as the user keeps typing.
            self.completion_for_offset(
                cursor,
                cursor,
                lsp::CompletionTrigger::TriggerCharacter(typed),
                false,
                ctx,
            );
            return;
        }

        let is_identifier_char = typed.is_alphanumeric() || typed == '_' || typed == '$';
        if !is_identifier_char {
            // Whitespace, punctuation, a closing bracket, ... — any open popup no
            // longer applies.
            self.dismiss_completion(ctx);
            return;
        }

        match &self.lsp_completion_state {
            LspCompletionState::Active { is_incomplete, .. } => {
                let incomplete = *is_incomplete;
                // Fast path: filter the cached candidates against the new prefix.
                self.refilter_completion(ctx);
                if matches!(self.lsp_completion_state, LspCompletionState::None) {
                    // The client-side filter eliminated everything. This is the
                    // `console.` -> `console.l` case when the cached list was
                    // stale or partial: re-ask the server for the live prefix so
                    // a valid member completion isn't lost (the popup would
                    // otherwise just vanish as the user types).
                    let anchor = self.completion_word_start(cursor, ctx);
                    self.completion_for_offset(
                        cursor,
                        anchor,
                        lsp::CompletionTrigger::Invoked,
                        false,
                        ctx,
                    );
                } else if incomplete {
                    // The server flagged the list incomplete; ask again so
                    // candidates absent from the first (partial) batch surface.
                    let anchor = self.completion_word_start(cursor, ctx);
                    self.completion_for_offset(
                        cursor,
                        anchor,
                        lsp::CompletionTrigger::Invoked,
                        true,
                        ctx,
                    );
                }
            }
            LspCompletionState::Loading(_) => {
                // A request is already in flight; it will filter to the live
                // prefix when it resolves.
            }
            LspCompletionState::None => {
                // Open a fresh popup for the identifier being typed (this is what
                // makes plain-prefix completion like `con` -> `console` work).
                let anchor = self.completion_word_start(cursor, ctx);
                self.completion_for_offset(
                    cursor,
                    anchor,
                    lsp::CompletionTrigger::Invoked,
                    false,
                    ctx,
                );
            }
        }
    }

    /// Scan backwards from `cursor` over identifier characters to find the start
    /// of the word currently being typed (the replaceable prefix anchor).
    fn completion_word_start(&self, cursor: CharOffset, ctx: &ViewContext<Self>) -> CharOffset {
        let editor = self.editor().as_ref(ctx);
        let mut start = cursor.as_usize();
        while start > 0 {
            let prev = CharOffset::from(start - 1);
            match editor.char_at(prev, ctx) {
                Some(c) if c.is_alphanumeric() || c == '_' || c == '$' => start -= 1,
                _ => break,
            }
        }
        CharOffset::from(start)
    }

    /// Build the active popup state from freshly-returned candidates, filtering
    /// by whatever prefix the user has typed since the request was issued (they
    /// may have kept typing while it was in flight).
    fn populate_completion(
        &mut self,
        anchor: CharOffset,
        items: Vec<lsp::CompletionItem>,
        is_incomplete: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        let cursor = self.editor().as_ref(ctx).cursor_head_offset(ctx);
        let Some(prefix) = self.completion_prefix(anchor, cursor, ctx) else {
            // The user moved away or typed a non-identifier char — drop it.
            self.dismiss_completion(ctx);
            return;
        };

        let filtered = Self::completion_filter(&items, &prefix);
        if filtered.is_empty() {
            if let Some(fallback_items) = self.html_tag_fallback_items(anchor, cursor, &prefix, ctx)
            {
                let filtered = (0..fallback_items.len()).collect();
                self.lsp_completion_state = LspCompletionState::Active {
                    items: fallback_items,
                    filtered,
                    selected: 0,
                    anchor,
                    is_incomplete: false,
                    scroll_state: Default::default(),
                };
                self.set_editor_completion_active(true, ctx);
                ctx.notify();
                return;
            }
            self.dismiss_completion(ctx);
            return;
        }

        log::info!(
            "completion popup: showing {} candidate(s) (anchor={anchor:?}, incomplete={is_incomplete})",
            filtered.len()
        );
        self.lsp_completion_state = LspCompletionState::Active {
            items,
            filtered,
            selected: 0,
            anchor,
            is_incomplete,
            scroll_state: Default::default(),
        };
        self.set_editor_completion_active(true, ctx);
        ctx.notify();
    }

    /// Reads the typed prefix between `anchor` and `cursor`. Returns `None` when
    /// the cursor is before the anchor or the gap contains a non-identifier
    /// character (both mean the completion is no longer valid and should close).
    fn completion_prefix(
        &self,
        anchor: CharOffset,
        cursor: CharOffset,
        ctx: &ViewContext<Self>,
    ) -> Option<String> {
        if cursor < anchor {
            return None;
        }
        let editor = self.editor().as_ref(ctx);
        let mut prefix = String::new();
        for i in anchor.as_usize()..cursor.as_usize() {
            match editor.char_at(CharOffset::from(i), ctx) {
                Some(c) if c.is_alphanumeric() || c == '_' || c == '$' => prefix.push(c),
                _ => return None,
            }
        }
        Some(prefix)
    }

    /// Case-insensitive prefix filter over the candidate list, capped so the
    /// rendered/scrolled list stays bounded. Preserves the server's order.
    fn completion_filter(items: &[lsp::CompletionItem], prefix: &str) -> Vec<usize> {
        if prefix.is_empty() {
            return (0..items.len().min(COMPLETION_MAX_VISIBLE_ITEMS)).collect();
        }
        let needle = prefix.to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let haystack = item.filter_text.as_deref().unwrap_or(&item.label);
                let normalized = haystack
                    .trim_start_matches('<')
                    .trim_start_matches('/')
                    .trim_start_matches("</");
                haystack.to_lowercase().starts_with(&needle)
                    || normalized.to_lowercase().starts_with(&needle)
            })
            .map(|(index, _)| index)
            .take(COMPLETION_MAX_VISIBLE_ITEMS)
            .collect()
    }

    fn set_editor_completion_active(&mut self, active: bool, ctx: &mut ViewContext<Self>) {
        self.editor.update(ctx, |editor, ctx| {
            editor.set_completion_active(active, ctx);
        });
    }

    /// Re-filter the open popup against the current typed prefix. Called on every
    /// user content change while a popup is active; closes the popup when the
    /// prefix becomes invalid or matches nothing.
    pub(super) fn refilter_completion(&mut self, ctx: &mut ViewContext<Self>) {
        let anchor = match &self.lsp_completion_state {
            LspCompletionState::Active { anchor, .. } => *anchor,
            _ => return,
        };
        let cursor = self.editor().as_ref(ctx).cursor_head_offset(ctx);
        let Some(prefix) = self.completion_prefix(anchor, cursor, ctx) else {
            self.dismiss_completion(ctx);
            return;
        };
        let new_filtered = match &self.lsp_completion_state {
            LspCompletionState::Active { items, .. } => Self::completion_filter(items, &prefix),
            _ => return,
        };
        if new_filtered.is_empty() {
            if let Some(fallback_items) = self.html_tag_fallback_items(anchor, cursor, &prefix, ctx)
            {
                let filtered = (0..fallback_items.len()).collect();
                self.lsp_completion_state = LspCompletionState::Active {
                    items: fallback_items,
                    filtered,
                    selected: 0,
                    anchor,
                    is_incomplete: false,
                    scroll_state: Default::default(),
                };
                ctx.notify();
                return;
            }
            self.dismiss_completion(ctx);
            return;
        }
        if let LspCompletionState::Active {
            filtered, selected, ..
        } = &mut self.lsp_completion_state
        {
            *filtered = new_filtered;
            *selected = 0;
        }
        ctx.notify();
    }

    pub(super) fn completion_select_next(&mut self, ctx: &mut ViewContext<Self>) {
        if let LspCompletionState::Active {
            filtered, selected, ..
        } = &mut self.lsp_completion_state
        {
            if !filtered.is_empty() {
                *selected = (*selected + 1) % filtered.len();
                ctx.notify();
            }
        }
    }

    pub(super) fn completion_select_prev(&mut self, ctx: &mut ViewContext<Self>) {
        if let LspCompletionState::Active {
            filtered, selected, ..
        } = &mut self.lsp_completion_state
        {
            if !filtered.is_empty() {
                *selected = if *selected == 0 {
                    filtered.len() - 1
                } else {
                    *selected - 1
                };
                ctx.notify();
            }
        }
    }

    /// Apply the selected candidate: replace the typed prefix `anchor..cursor`
    /// with the item's insert text, then close the popup.
    pub(super) fn accept_completion(&mut self, ctx: &mut ViewContext<Self>) {
        let (anchor, item) = match &self.lsp_completion_state {
            LspCompletionState::Active {
                items,
                filtered,
                selected,
                anchor,
                ..
            } => {
                let Some(&item_index) = filtered.get(*selected) else {
                    return;
                };
                let Some(item) = items.get(item_index) else {
                    return;
                };
                (*anchor, item.clone())
            }
            _ => return,
        };

        let cursor = self.editor().as_ref(ctx).cursor_head_offset(ctx);
        let parsed = Self::parse_completion_insert(
            &item.insert_text,
            item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET),
        );

        self.editor.update(ctx, move |editor, ctx| {
            let to_location = |position: &lsp_types::Position| lsp::types::Location {
                line: position.line as usize,
                column: position.character as usize,
            };
            let server_replacement_range = match &item.text_edit {
                Some(lsp_types::CompletionTextEdit::Edit(edit)) => {
                    let start = editor.lsp_location_to_offset(&to_location(&edit.range.start), ctx);
                    let end = editor.lsp_location_to_offset(&to_location(&edit.range.end), ctx);
                    Some(start..end)
                }
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(edit)) => {
                    let start =
                        editor.lsp_location_to_offset(&to_location(&edit.replace.start), ctx);
                    let end = editor.lsp_location_to_offset(&to_location(&edit.replace.end), ctx);
                    Some(start..end)
                }
                None => None,
            };
            let replacement_range =
                Self::completion_replacement_range(server_replacement_range, anchor, cursor);

            if replacement_range.start > replacement_range.end {
                log::warn!(
                    "completion accept: invalid replacement range {:?}..{:?}",
                    replacement_range.start,
                    replacement_range.end
                );
                return;
            }

            let before_start = replacement_range.start.saturating_sub(&CharOffset::from(1));
            let char_before = editor.char_at(before_start, ctx);
            let mut insert_text = parsed.text.clone();
            let mut cursor_offset = parsed.cursor_offset;

            if let Some(first) = insert_text.chars().next() {
                if !first.is_alphanumeric() && first != '_' && char_before == Some(first) {
                    insert_text = insert_text[first.len_utf8()..].to_string();
                    if let Some(offset) = &mut cursor_offset {
                        *offset = offset.saturating_sub(1);
                    }
                }
            }

            log::info!(
                "completion accept: anchor={anchor:?} cursor={cursor:?} char_before={char_before:?} insert={insert_text:?}"
            );

            let edit_end = replacement_range.start + CharOffset::from(insert_text.chars().count());
            let edits = vec![(insert_text, replacement_range.clone())];
            if let Ok(edits) = Vec1::try_from_vec(edits) {
                editor.apply_edits(edits, ctx);
                let move_cursor_to =
                    |offset: CharOffset, editor: &mut CodeEditorView, ctx: &mut ViewContext<CodeEditorView>| {
                        let location = editor.offset_to_lsp_position(offset, ctx);
                        let point = Point::new((location.line + 1) as u32, location.column as u32);
                        editor.cursor_at(point, ctx);
                    };
                if let Some(offset) = cursor_offset {
                    move_cursor_to(replacement_range.start + CharOffset::from(offset), editor, ctx);
                } else {
                    move_cursor_to(edit_end, editor, ctx);
                }
            }
        });

        self.dismiss_completion(ctx);
    }

    fn completion_replacement_range(
        server_range: Option<Range<CharOffset>>,
        anchor: CharOffset,
        cursor: CharOffset,
    ) -> Range<CharOffset> {
        let typed_prefix = anchor..cursor;
        let Some(server_range) = server_range else {
            return typed_prefix;
        };

        if server_range.start > server_range.end || anchor > cursor {
            return typed_prefix;
        }

        // Completion responses can arrive after more characters were typed.
        // Merge a touching/overlapping server edit with the live prefix so
        // accepting `console` after typing `cons` replaces `cons` instead of
        // producing `consolecons`.
        if server_range.start <= cursor && server_range.end >= anchor {
            server_range.start.min(anchor)..server_range.end.max(cursor)
        } else {
            server_range
        }
    }

    fn parse_completion_insert(insert_text: &str, is_snippet: bool) -> ParsedCompletionInsert {
        if !is_snippet {
            return ParsedCompletionInsert {
                text: insert_text.to_string(),
                cursor_offset: None,
            };
        }

        let chars: Vec<char> = insert_text.chars().collect();
        let mut output = String::new();
        let mut final_cursor = None;
        let mut first_tabstop = None;
        let mut index = 0usize;

        while index < chars.len() {
            match chars[index] {
                '\\' if index + 1 < chars.len() => {
                    output.push(chars[index + 1]);
                    index += 2;
                }
                '$' => {
                    if let Some((consumed, placeholder, default_text)) =
                        Self::parse_snippet_placeholder(&chars[index..])
                    {
                        let cursor_here = output.chars().count();
                        if placeholder == 0 {
                            final_cursor = Some(cursor_here);
                        } else if first_tabstop.is_none() {
                            first_tabstop = Some(cursor_here);
                        }
                        output.push_str(&default_text);
                        index += consumed;
                    } else {
                        output.push('$');
                        index += 1;
                    }
                }
                ch => {
                    output.push(ch);
                    index += 1;
                }
            }
        }

        ParsedCompletionInsert {
            text: output,
            cursor_offset: final_cursor.or(first_tabstop),
        }
    }

    fn parse_snippet_placeholder(chars: &[char]) -> Option<(usize, usize, String)> {
        if chars.first().copied()? != '$' {
            return None;
        }

        if chars.get(1).is_some_and(|c| c.is_ascii_digit()) {
            let mut end = 1usize;
            while chars.get(end).is_some_and(|c| c.is_ascii_digit()) {
                end += 1;
            }
            let placeholder = chars[1..end].iter().collect::<String>().parse().ok()?;
            return Some((end, placeholder, String::new()));
        }

        if chars.get(1) != Some(&'{') {
            return None;
        }

        let mut digit_end = 2usize;
        while chars.get(digit_end).is_some_and(|c| c.is_ascii_digit()) {
            digit_end += 1;
        }
        if digit_end == 2 {
            return None;
        }

        let placeholder = chars[2..digit_end]
            .iter()
            .collect::<String>()
            .parse()
            .ok()?;

        match chars.get(digit_end) {
            Some('}') => Some((digit_end + 1, placeholder, String::new())),
            Some(':') => {
                let mut default_text = String::new();
                let mut cursor = digit_end + 1;
                while cursor < chars.len() {
                    match chars[cursor] {
                        '\\' if cursor + 1 < chars.len() => {
                            default_text.push(chars[cursor + 1]);
                            cursor += 2;
                        }
                        '}' => return Some((cursor + 1, placeholder, default_text)),
                        other => {
                            default_text.push(other);
                            cursor += 1;
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn html_tag_fallback_items(
        &self,
        anchor: CharOffset,
        cursor: CharOffset,
        prefix: &str,
        ctx: &ViewContext<Self>,
    ) -> Option<Vec<lsp::CompletionItem>> {
        if prefix.is_empty()
            || self.file_path().and_then(lsp::LanguageId::from_path) != Some(lsp::LanguageId::Html)
        {
            return None;
        }

        let editor = self.editor().as_ref(ctx);
        let before_anchor = if anchor.as_usize() == 0 {
            None
        } else {
            editor.char_at(anchor.saturating_sub(&CharOffset::from(1)), ctx)
        };
        let after_cursor = editor.char_at(cursor, ctx);
        let safe_context = match before_anchor {
            None => true,
            Some(ch) => ch.is_whitespace() || ch == '>' || ch == '/',
        } && !after_cursor
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_');
        if !safe_context {
            return None;
        }

        const HTML_TAGS: &[&str] = &[
            "div",
            "span",
            "section",
            "article",
            "main",
            "header",
            "footer",
            "nav",
            "aside",
            "button",
            "form",
            "label",
            "input",
            "textarea",
            "select",
            "option",
            "ul",
            "ol",
            "li",
            "a",
            "img",
            "figure",
            "figcaption",
            "p",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "table",
            "thead",
            "tbody",
            "tr",
            "td",
            "th",
            "script",
            "style",
            "template",
            "dialog",
            "details",
            "summary",
            "canvas",
            "video",
            "audio",
        ];
        const VOID_TAGS: &[&str] = &[
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
            "source", "track", "wbr",
        ];

        let needle = prefix.to_ascii_lowercase();
        let items = HTML_TAGS
            .iter()
            .filter(|tag| tag.starts_with(&needle))
            .take(COMPLETION_MAX_VISIBLE_ITEMS)
            .map(|tag| {
                let insert_text = if VOID_TAGS.contains(tag) {
                    format!("<{tag}>$0")
                } else {
                    format!("<{tag}>$0</{tag}>")
                };
                lsp::CompletionItem {
                    label: (*tag).to_string(),
                    insert_text,
                    insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
                    text_edit: None,
                    detail: Some("HTML tag".to_string()),
                    kind: Some(lsp_types::CompletionItemKind::SNIPPET),
                    sort_text: None,
                    filter_text: Some((*tag).to_string()),
                }
            })
            .collect::<Vec<_>>();

        (!items.is_empty()).then_some(items)
    }

    /// Close the completion popup (if any) and stop the editor from intercepting
    /// navigation keys. Returns whether anything was dismissed.
    pub(super) fn dismiss_completion(&mut self, ctx: &mut ViewContext<Self>) -> bool {
        if self.lsp_completion_state.clear() {
            self.set_editor_completion_active(false, ctx);
            ctx.notify();
            true
        } else {
            false
        }
    }

    /// Render the completion popup if candidates are available.
    pub(super) fn render_completion_popup(&self, app: &AppContext) -> Option<Box<dyn Element>> {
        let (items, filtered, selected, scroll_state) = match &self.lsp_completion_state {
            LspCompletionState::Active {
                items,
                filtered,
                selected,
                scroll_state,
                ..
            } => (items, filtered, *selected, scroll_state.clone()),
            _ => return None,
        };
        if filtered.is_empty() {
            return None;
        }

        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        for (row_index, &item_index) in filtered.iter().enumerate() {
            let Some(item) = items.get(item_index) else {
                continue;
            };
            let is_selected = row_index == selected;

            let text = FormattedText::new([FormattedTextLine::Line(vec![
                FormattedTextFragment::plain_text(item.label.clone()),
            ])]);
            let label_element = FormattedTextElement::new(
                text,
                appearance.monospace_font_size(),
                appearance.ui_font_family(),
                appearance.monospace_font_family(),
                theme.active_ui_text_color().into(),
                HighlightedHyperlink::default(),
            )
            .finish();

            let mut row = Container::new(label_element)
                .with_horizontal_padding(8.)
                .with_vertical_padding(2.);
            if is_selected {
                row = row.with_background(warpui::elements::Fill::Solid(
                    internal_colors::neutral_3(theme),
                ));
            }
            column.add_child(row.finish());
        }

        let scrollable_content = ClippedScrollable::vertical(
            scroll_state,
            column.finish(),
            ScrollbarWidth::Auto,
            theme.disabled_ui_text_color().into(),
            theme.active_ui_text_color().into(),
            warpui::elements::Fill::None,
        )
        .finish();

        let constrained_content = ConstrainedBox::new(scrollable_content)
            .with_width(COMPLETION_POPUP_MAX_WIDTH)
            .with_max_height(COMPLETION_POPUP_MAX_HEIGHT)
            .finish();

        let popup = Container::new(constrained_content)
            .with_vertical_padding(4.)
            .with_background(theme.background())
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .with_border(Border::all(1.).with_border_fill(internal_colors::neutral_4(theme)))
            .finish();

        Some(popup)
    }

    pub(super) fn create_highlighted_code_fragment(
        language: String,
        code: String,
        ctx: &mut ViewContext<Self>,
    ) -> HoverContentSegment {
        let view = ctx.add_typed_action_view(|ctx| {
            CodeEditorView::new(
                None,
                None,
                CodeEditorRenderOptions::new(VerticalExpansionBehavior::InfiniteHeight),
                ctx,
            )
            .with_can_show_diff_ui(false)
            .with_show_line_numbers(false)
        });

        view.update(ctx, |view, ctx| {
            view.set_show_current_line_highlights(false, ctx);
            view.set_interaction_state(InteractionState::Selectable, ctx);
            let state = InitialBufferState::plain_text(code.trim());
            view.reset(state, ctx);
            view.set_language_with_name(&language, ctx);
        });

        HoverContentSegment::CodeBlock { view }
    }

    /// Creates hover content segments from parsed FormattedText lines.
    /// Code blocks are converted to CodeEditorViews for syntax highlighting,
    /// while other content is grouped into FormattedText segments.
    /// Consecutive code blocks with the same language are merged into a single view.
    pub(super) fn create_hover_content_segments(
        &mut self,
        content: HoverContents,
        ctx: &mut ViewContext<Self>,
    ) -> Vec<HoverContentSegment> {
        let mut pending = PendingSections::default();

        for section in content.sections {
            let text = match section.kind {
                MarkupKind::Markdown => match markdown_parser::parse_markdown(&section.value) {
                    Ok(text) => text,
                    Err(_) => FormattedText::new([FormattedTextLine::Line(vec![
                        FormattedTextFragment::plain_text(section.value),
                    ])]),
                },
                MarkupKind::PlainText => FormattedText::new([FormattedTextLine::Line(vec![
                    FormattedTextFragment::plain_text(section.value),
                ])]),
            };

            for line in text.lines {
                pending.push_formatted_line(line);
            }
        }

        pending.flush(ctx)
    }

    /// Render the LSP hover tooltip if hover state is available.
    pub(super) fn render_hover_tooltip(&self, app: &AppContext) -> Option<Box<dyn Element>> {
        let (segments, diagnostics, scroll_state, mouse_state) = match &self.lsp_hover_state {
            LspHoverState::Loaded {
                segments,
                diagnostics,
                scroll_state,
                mouse_state,
                ..
            } => (
                segments,
                diagnostics,
                scroll_state.clone(),
                mouse_state.clone(),
            ),
            _ => return None,
        };

        // Don't show tooltip if there's no content.
        if segments.is_empty() && diagnostics.is_empty() {
            return None;
        }

        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        // Build content column with diagnostics first, then hover info.
        let mut content_column =
            Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        let mut is_first = true;

        // Render diagnostics first (if any).
        for diagnostic in diagnostics {
            if !is_first {
                content_column.add_child(Self::render_separator(theme));
            } else {
                is_first = false;
            }

            content_column.add_child(Self::render_diagnostic(diagnostic, appearance));
        }

        // Render hover info segments after diagnostics.
        for segment in segments {
            if !is_first {
                content_column.add_child(Self::render_separator(theme));
            } else {
                is_first = false;
            }
            match segment {
                HoverContentSegment::Text(formatted_text) => {
                    // Render text content using FormattedTextElement.
                    let text_element = FormattedTextElement::new(
                        formatted_text.clone(),
                        appearance.monospace_font_size(),
                        appearance.ui_font_family(),
                        appearance.monospace_font_family(),
                        theme.active_ui_text_color().into(),
                        HighlightedHyperlink::default(),
                    )
                    .finish();
                    content_column.add_child(text_element);
                }
                HoverContentSegment::CodeBlock { view, .. } => {
                    // Render code block using the embedded CodeEditorView.
                    let code_element = Container::new(ChildView::new(view).finish())
                        .with_padding_top(4.)
                        .with_horizontal_padding(8.)
                        .finish();
                    content_column.add_child(code_element);
                }
            }
        }

        // Make content scrollable if it exceeds max height.
        let scrollable_content = ClippedScrollable::vertical(
            scroll_state,
            content_column.finish(),
            ScrollbarWidth::Auto,
            theme.disabled_ui_text_color().into(),
            theme.active_ui_text_color().into(),
            warpui::elements::Fill::None,
        )
        .finish();

        let constrained_content = ConstrainedBox::new(scrollable_content)
            .with_width(HOVER_TOOLTIP_MAX_WIDTH)
            .with_max_height(HOVER_TOOLTIP_MAX_HEIGHT)
            .finish();

        let tooltip = Container::new(constrained_content)
            .with_horizontal_padding(8.)
            .with_vertical_padding(6.)
            .with_background(theme.background())
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .with_border(Border::all(1.).with_border_fill(internal_colors::neutral_4(theme)))
            .finish();

        // Wrap in Hoverable so we can track whether the mouse is over the tooltip.
        // This is used by LocalCodeEditorView to avoid clearing hover state when
        // the mouse moves over the tooltip itself.
        let hoverable_tooltip = Hoverable::new(mouse_state, |_| tooltip).finish();

        Some(hoverable_tooltip)
    }

    /// Render a separator line between hover card sections.
    fn render_separator(theme: &WarpTheme) -> Box<dyn Element> {
        Container::new(
            ConstrainedBox::new(
                Rect::new()
                    .with_background(internal_colors::neutral_2(theme))
                    .finish(),
            )
            .with_height(1.)
            .finish(),
        )
        .with_vertical_padding(4.)
        .finish()
    }

    /// Render a diagnostic message with severity prefix.
    fn render_diagnostic(
        diagnostic: &ProcessedDiagnostic,
        appearance: &Appearance,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();

        // Create the diagnostic text with bold severity prefix.
        let severity_text = match diagnostic.severity {
            lsp_types::DiagnosticSeverity::ERROR => "Error",
            lsp_types::DiagnosticSeverity::WARNING => "Warning",
            lsp_types::DiagnosticSeverity::INFORMATION => "Info",
            lsp_types::DiagnosticSeverity::HINT => "Hint",
            _ => "Diagnostic",
        };

        let text = FormattedText::new([FormattedTextLine::Line(vec![
            FormattedTextFragment::bold(format!("{severity_text}: ")),
            FormattedTextFragment::plain_text(&diagnostic.message),
        ])]);

        // Use error or warning color for the entire diagnostic text.
        let text_color = match diagnostic.severity {
            lsp_types::DiagnosticSeverity::ERROR => theme.ui_error_color(),
            lsp_types::DiagnosticSeverity::WARNING => theme.ui_warning_color(),
            _ => theme.active_ui_text_color().into_solid(),
        };

        FormattedTextElement::new(
            text,
            appearance.monospace_font_size(),
            appearance.ui_font_family(),
            appearance.monospace_font_family(),
            text_color,
            HighlightedHyperlink::default(),
        )
        .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::LocalCodeEditorView;
    use string_offset::CharOffset;

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
}
