#![cfg_attr(target_family = "wasm", allow(dead_code, unused_imports))]

//! Jump to a symbol by typing part of its name.
//!
//! One view serves both scopes. In a file, the server lists every symbol once
//! and the typed query filters that list here; across the project, the list is
//! too large to fetch whole, so every query goes back to the server and the
//! answer replaces the list.

use lsp::types::SymbolEntry;
use warp_editor::editor::NavigationKey;
use warpui::elements::{
    Border, ChildView, ClippedScrollStateHandle, ClippedScrollable, ConstrainedBox, Container,
    CornerRadius, CrossAxisAlignment, DispatchEventResult, DropShadow, EventHandler, Flex,
    Highlight, MouseInBehavior, ParentElement, Radius, ScrollbarWidth, Shrinkable, Text,
};
use warpui::fonts::{Properties, Weight};
use warpui::text_layout::ClipConfig;
use warpui::units::Pixels;
use warpui::{
    AppContext, Element, Entity, FocusContext, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use crate::appearance::Appearance;
use crate::editor::{
    EditorView, Event as EditorEvent, InteractionState, PropagateAndNoOpNavigationKeys,
    SingleLineEditorOptions, TextOptions,
};

const PICKER_WIDTH: f32 = 560.;
const ROW_HEIGHT: f32 = 26.;
const VISIBLE_ROWS: f32 = 12.;
/// Rows drawn at most. A project can hold tens of thousands of symbols; past a
/// few hundred, the user narrows the query rather than scrolls.
const MAX_ROWS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolScope {
    /// The symbols in the file in the editor.
    File,
    /// Every symbol the server knows about, asked for per query.
    Project,
    /// Every diagnostic the language servers have published, project-wide.
    Problems,
}

/// One row of the picker: something with a place in the project to go to.
///
/// Symbols and diagnostics both reduce to this. Keeping the row generic is
/// what lets one picker -- one set of keys, one look -- serve every "type to
/// find it, Enter to go there" list in the editor.
#[derive(Debug, Clone)]
pub struct PickerItem {
    /// The text the query is matched against, drawn prominently.
    pub label: String,
    /// Dimmed context after the label: a symbol's container, a problem's file.
    pub secondary: Option<String>,
    /// A short tag in the leading column: the symbol kind, or the severity.
    pub tag: &'static str,
    /// Colour for the tag. `None` uses the muted text colour.
    pub tag_color: Option<pathfinder_color::ColorU>,
    /// Shown at the far right, e.g. the file name in project scope.
    pub trailing: Option<String>,
    pub path: std::path::PathBuf,
    pub location: lsp::types::Location,
}

impl PickerItem {
    pub fn from_symbol(entry: SymbolEntry, show_file: bool) -> Self {
        let trailing = show_file.then(|| {
            entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        Self {
            label: entry.name,
            secondary: entry.container.filter(|container| !container.is_empty()),
            tag: kind_tag(entry.kind),
            tag_color: None,
            trailing,
            path: entry.path,
            location: entry.location,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Close,
    Confirm(PickerItem),
    /// The query changed. For project scope the host re-asks the server.
    QueryChanged(String),
}

#[derive(Debug, Clone)]
pub enum SymbolPickerAction {
    Hover(usize),
    Pick(usize),
}

pub struct SymbolPickerView {
    query_editor: ViewHandle<EditorView>,
    scope: SymbolScope,
    is_open: bool,
    entries: Vec<PickerItem>,
    /// Indices into `entries` that match the query, with the matched char
    /// positions in each name for highlighting.
    filtered: Vec<(usize, Vec<usize>)>,
    selected: usize,
    scroll_state: ClippedScrollStateHandle,
    /// Shown in place of the list: "Loading…", "No symbols", an error.
    status: Option<String>,
}

impl SymbolPickerView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let query_editor = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            EditorView::single_line(
                SingleLineEditorOptions {
                    text: TextOptions::ui_text(Some(13.), appearance),
                    select_all_on_focus: true,
                    clear_selections_on_blur: false,
                    propagate_and_no_op_vertical_navigation_keys:
                        PropagateAndNoOpNavigationKeys::Always,
                    ..Default::default()
                },
                ctx,
            )
        });
        ctx.subscribe_to_view(&query_editor, |me, _, event, ctx| {
            me.handle_editor_event(event, ctx)
        });

        Self {
            query_editor,
            scope: SymbolScope::File,
            is_open: false,
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            scroll_state: Default::default(),
            status: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn scope(&self) -> SymbolScope {
        self.scope
    }

    pub fn open(&mut self, scope: SymbolScope, ctx: &mut ViewContext<Self>) {
        self.scope = scope;
        self.is_open = true;
        self.entries.clear();
        self.filtered.clear();
        self.selected = 0;
        self.status = Some("Loading…".to_string());
        self.query_editor.update(ctx, |editor, ctx| {
            editor.set_interaction_state(InteractionState::Editable, ctx);
            editor.clear_buffer(ctx);
            editor.set_placeholder_text(
                match scope {
                    SymbolScope::File => "Go to symbol in file",
                    SymbolScope::Project => "Go to symbol in project",
                    SymbolScope::Problems => "Filter problems",
                },
                ctx,
            );
        });
        ctx.notify();
    }

    pub fn close(&mut self, ctx: &mut ViewContext<Self>) {
        self.is_open = false;
        ctx.notify();
    }

    pub fn set_status(&mut self, status: impl Into<String>, ctx: &mut ViewContext<Self>) {
        self.status = Some(status.into());
        ctx.notify();
    }

    /// Replace the candidates. For file scope this happens once; for project
    /// scope, once per query.
    pub fn set_entries(&mut self, entries: Vec<PickerItem>, ctx: &mut ViewContext<Self>) {
        self.entries = entries;
        self.status = None;
        self.refilter(ctx);
    }

    fn query(&self, ctx: &AppContext) -> String {
        self.query_editor
            .as_ref(ctx)
            .buffer_text(ctx)
            .trim()
            .to_string()
    }

    fn refilter(&mut self, ctx: &mut ViewContext<Self>) {
        let query = self.query(ctx);
        self.filtered = if query.is_empty() {
            (0..self.entries.len())
                .map(|index| (index, Vec::new()))
                .collect()
        } else {
            let mut scored: Vec<(usize, Vec<usize>, i64)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    fuzzy_match::match_indices_case_insensitive(&entry.label, &query)
                        .map(|result| (index, result.matched_indices, result.score))
                })
                .collect();
            // Best match first; ties keep source order, which for a file is
            // top to bottom.
            scored.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
            scored
                .into_iter()
                .map(|(index, matched, _)| (index, matched))
                .collect()
        };
        self.filtered.truncate(MAX_ROWS);
        self.selected = 0;
        self.scroll_state.scroll_to(Pixels::zero());
        if self.filtered.is_empty() && self.status.is_none() {
            self.status = Some(
                match (self.scope, self.entries.is_empty() && query.is_empty()) {
                    (SymbolScope::Problems, true) => "No problems".to_string(),
                    (_, true) => "No symbols".to_string(),
                    (_, false) => format!("Nothing matches \u{201c}{query}\u{201d}"),
                },
            );
        } else if !self.filtered.is_empty() {
            self.status = None;
        }
        ctx.notify();
    }

    fn move_selection(&mut self, delta: isize, ctx: &mut ViewContext<Self>) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        let top = self.selected as f32 * ROW_HEIGHT;
        let window_top = self.scroll_state.scroll_start().as_f32();
        let window_height = ROW_HEIGHT * VISIBLE_ROWS;
        if top < window_top {
            self.scroll_state.scroll_to(Pixels::new(top));
        } else if top + ROW_HEIGHT > window_top + window_height {
            self.scroll_state
                .scroll_to(Pixels::new(top + ROW_HEIGHT - window_height));
        }
        ctx.notify();
    }

    fn confirm(&mut self, row: usize, ctx: &mut ViewContext<Self>) {
        if let Some(entry) = self
            .filtered
            .get(row)
            .and_then(|(index, _)| self.entries.get(*index))
        {
            ctx.emit(Event::Confirm(entry.clone()));
        }
    }

    fn handle_editor_event(&mut self, event: &EditorEvent, ctx: &mut ViewContext<Self>) {
        match event {
            EditorEvent::Edited(_) => match self.scope {
                SymbolScope::File | SymbolScope::Problems => {
                    self.status = None;
                    self.refilter(ctx);
                }
                SymbolScope::Project => {
                    let query = self.query(ctx);
                    ctx.emit(Event::QueryChanged(query));
                }
            },
            EditorEvent::Navigate(NavigationKey::Up) => self.move_selection(-1, ctx),
            EditorEvent::Navigate(NavigationKey::Down) => self.move_selection(1, ctx),
            EditorEvent::Enter => self.confirm(self.selected, ctx),
            EditorEvent::Escape => ctx.emit(Event::Close),
            _ => {}
        }
    }
}

/// A short tag for a symbol's kind, coloured by family, so a struct and a
/// function sharing a name can be told apart without reading further.
fn kind_tag(kind: lsp_types::SymbolKind) -> &'static str {
    use lsp_types::SymbolKind as K;
    match kind {
        K::FUNCTION => "fn",
        K::METHOD => "method",
        K::CONSTRUCTOR => "new",
        K::CLASS => "class",
        K::STRUCT => "struct",
        K::INTERFACE => "iface",
        K::ENUM => "enum",
        K::ENUM_MEMBER => "variant",
        K::FIELD | K::PROPERTY => "field",
        K::VARIABLE => "var",
        K::CONSTANT => "const",
        K::MODULE | K::NAMESPACE | K::PACKAGE => "mod",
        K::TYPE_PARAMETER => "type",
        K::FILE => "file",
        _ => "sym",
    }
}

impl Entity for SymbolPickerView {
    type Event = Event;
}

impl TypedActionView for SymbolPickerView {
    type Action = SymbolPickerAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            SymbolPickerAction::Hover(row) => {
                if *row < self.filtered.len() && *row != self.selected {
                    self.selected = *row;
                    ctx.notify();
                }
            }
            SymbolPickerAction::Pick(row) => self.confirm(*row, ctx),
        }
    }
}

impl View for SymbolPickerView {
    fn ui_name() -> &'static str {
        "SymbolPickerView"
    }

    fn on_focus(&mut self, focus_ctx: &FocusContext, ctx: &mut ViewContext<Self>) {
        if focus_ctx.is_self_focused() {
            ctx.focus(&self.query_editor);
        }
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let font_size = appearance.ui_font_size();
        let name_color = theme.active_ui_text_color().into_solid();
        let muted = theme.disabled_ui_text_color().into_solid();
        let accent = theme.accent().into_solid();
        let row_height = appearance.scale(ROW_HEIGHT);

        let input = Container::new(ChildView::new(&self.query_editor).finish())
            .with_horizontal_padding(10.)
            .with_vertical_padding(7.)
            .with_background(theme.surface_1())
            .with_border(Border::all(1.).with_border_fill(theme.accent()))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .finish();

        let mut body = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        body.add_child(Container::new(input).with_margin_bottom(6.).finish());

        if let Some(status) = &self.status {
            body.add_child(
                Container::new(
                    Text::new_inline(status.clone(), appearance.ui_font_family(), font_size)
                        .with_color(muted)
                        .finish(),
                )
                .with_horizontal_padding(10.)
                .with_vertical_padding(8.)
                .finish(),
            );
        } else {
            let mut rows = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
            for (row, (index, matched)) in self.filtered.iter().enumerate() {
                let Some(entry) = self.entries.get(*index) else {
                    continue;
                };

                let mut line = Flex::row()
                    .with_cross_axis_alignment(CrossAxisAlignment::Center)
                    .with_spacing(8.);
                line.add_child(
                    ConstrainedBox::new(
                        Text::new_inline(
                            entry.tag,
                            appearance.monospace_font_family(),
                            font_size - 2.,
                        )
                        .with_color(entry.tag_color.unwrap_or(muted))
                        .finish(),
                    )
                    .with_width(appearance.scale(52.))
                    .finish(),
                );

                let mut label = Text::new_inline(
                    entry.label.clone(),
                    appearance.monospace_font_family(),
                    font_size,
                )
                .with_color(name_color)
                .with_clip(ClipConfig::ellipsis());
                if !matched.is_empty() {
                    label = label.with_single_highlight(
                        Highlight::new()
                            .with_properties(Properties::default().weight(Weight::Bold))
                            .with_foreground_color(accent),
                        matched.clone(),
                    );
                }
                line.add_child(Shrinkable::new(1., label.finish()).finish());

                if let Some(secondary) = &entry.secondary {
                    line.add_child(
                        Text::new_inline(
                            secondary.clone(),
                            appearance.ui_font_family(),
                            font_size - 1.,
                        )
                        .with_color(muted)
                        .with_clip(ClipConfig::ellipsis())
                        .finish(),
                    );
                }
                if let Some(trailing) = &entry.trailing {
                    line.add_child(
                        Text::new_inline(
                            trailing.clone(),
                            appearance.ui_font_family(),
                            font_size - 1.,
                        )
                        .with_color(muted)
                        .with_clip(ClipConfig::ellipsis())
                        .finish(),
                    );
                }

                let mut container = Container::new(
                    ConstrainedBox::new(line.finish())
                        .with_height(row_height)
                        .finish(),
                )
                .with_horizontal_padding(10.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)));
                if row == self.selected {
                    container = container.with_background(theme.surface_3());
                }

                rows.add_child(
                    EventHandler::new(container.finish())
                        .on_mouse_in(
                            move |ctx, _, _| {
                                ctx.dispatch_typed_action(SymbolPickerAction::Hover(row));
                                DispatchEventResult::PropagateToParent
                            },
                            Some(MouseInBehavior {
                                fire_on_synthetic_events: false,
                                fire_when_covered: false,
                            }),
                        )
                        .on_left_mouse_down(move |ctx, _, _| {
                            ctx.dispatch_typed_action(SymbolPickerAction::Pick(row));
                            DispatchEventResult::StopPropagation
                        })
                        .finish(),
                );
            }

            body.add_child(
                ConstrainedBox::new(
                    ClippedScrollable::vertical(
                        self.scroll_state.clone(),
                        rows.finish(),
                        ScrollbarWidth::Auto,
                        theme.disabled_ui_text_color().into(),
                        theme.active_ui_text_color().into(),
                        warpui::elements::Fill::None,
                    )
                    .finish(),
                )
                .with_max_height(row_height * VISIBLE_ROWS)
                .finish(),
            );
        }

        Container::new(
            ConstrainedBox::new(body.finish())
                .with_width(appearance.scale(PICKER_WIDTH))
                .finish(),
        )
        .with_uniform_padding(8.)
        .with_background(theme.surface_2())
        .with_border(Border::all(1.).with_border_fill(theme.outline()))
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
        .with_drop_shadow(DropShadow::default())
        .finish()
    }
}
