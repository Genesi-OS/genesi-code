#![cfg_attr(target_family = "wasm", allow(dead_code, unused_imports))]

//! The small input that asks for a symbol's new name.

use warpui::elements::{
    Border, ChildView, ConstrainedBox, Container, CornerRadius, DropShadow, Flex, ParentElement,
    Radius, Text,
};
use warpui::{
    AppContext, Element, Entity, FocusContext, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use crate::appearance::Appearance;
use crate::editor::{
    EditorView, Event as EditorEvent, InteractionState, PropagateAndNoOpNavigationKeys,
    SingleLineEditorOptions, TextOptions,
};

const RENAME_WIDTH: f32 = 280.;
const LABEL_FONT_SIZE: f32 = 11.;
const EDITOR_FONT_SIZE: f32 = 13.;
const ROW_SPACING: f32 = 6.;

#[derive(Debug)]
pub enum Event {
    Close,
    Confirm { new_name: String },
}

/// Asks for a new name, anchored over the symbol being renamed.
///
/// Opened with the current name already in the box and selected, the way every
/// editor that does this works: most renames are an edit of the old name, and
/// typing straight over the selection still replaces it outright.
pub struct RenameView {
    name_editor: ViewHandle<EditorView>,
    is_open: bool,
    /// The name being replaced, for the label and to recognise a no-op.
    original: String,
    error_message: Option<String>,
}

impl RenameView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let name_editor = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            EditorView::single_line(
                SingleLineEditorOptions {
                    text: TextOptions::ui_text(Some(EDITOR_FONT_SIZE), appearance),
                    select_all_on_focus: true,
                    clear_selections_on_blur: false,
                    propagate_and_no_op_vertical_navigation_keys:
                        PropagateAndNoOpNavigationKeys::Always,
                    ..Default::default()
                },
                ctx,
            )
        });

        ctx.subscribe_to_view(&name_editor, |me, _, event, ctx| {
            me.handle_editor_event(event, ctx);
        });

        Self {
            name_editor,
            is_open: false,
            original: String::new(),
            error_message: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn open(&mut self, current_name: &str, ctx: &mut ViewContext<Self>) {
        self.is_open = true;
        self.error_message = None;
        self.original = current_name.to_string();
        self.name_editor.update(ctx, |editor, ctx| {
            editor.set_interaction_state(InteractionState::Editable, ctx);
            editor.set_buffer_text(current_name, ctx);
            editor.select_all(ctx);
        });
        ctx.notify();
    }

    pub fn close(&mut self, ctx: &mut ViewContext<Self>) {
        self.is_open = false;
        self.error_message = None;
        ctx.notify();
    }

    pub fn set_error(&mut self, message: impl Into<String>, ctx: &mut ViewContext<Self>) {
        self.error_message = Some(message.into());
        ctx.notify();
    }

    fn handle_editor_event(&mut self, event: &EditorEvent, ctx: &mut ViewContext<Self>) {
        match event {
            EditorEvent::Enter => {
                let new_name = self
                    .name_editor
                    .as_ref(ctx)
                    .buffer_text(ctx)
                    .trim()
                    .to_string();
                if new_name.is_empty() {
                    self.set_error("A name can't be empty.", ctx);
                    return;
                }
                // Confirming the same name is a cancel, not a no-op request that
                // still rewrites every file it touches.
                if new_name == self.original {
                    ctx.emit(Event::Close);
                    return;
                }
                ctx.emit(Event::Confirm { new_name });
            }
            EditorEvent::Escape => ctx.emit(Event::Close),
            _ => {}
        }
    }
}

impl Entity for RenameView {
    type Event = Event;
}

impl TypedActionView for RenameView {
    type Action = ();

    fn handle_action(&mut self, _action: &(), _ctx: &mut ViewContext<Self>) {}
}

impl View for RenameView {
    fn ui_name() -> &'static str {
        "RenameView"
    }

    fn on_focus(&mut self, focus_ctx: &FocusContext, ctx: &mut ViewContext<Self>) {
        if focus_ctx.is_self_focused() {
            ctx.focus(&self.name_editor);
            ctx.notify();
        }
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        let label = Text::new_inline(
            format!("Rename {}", self.original),
            appearance.ui_font_family(),
            appearance.scale(LABEL_FONT_SIZE),
        )
        .with_color(theme.nonactive_ui_text_color().into())
        .finish();

        let input = Container::new(ChildView::new(&self.name_editor).finish())
            .with_horizontal_padding(8.)
            .with_vertical_padding(5.)
            .with_background(theme.surface_1())
            .with_border(Border::all(1.).with_border_fill(theme.accent()))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .finish();

        let mut content = Flex::column()
            .with_child(
                Container::new(label)
                    .with_margin_bottom(ROW_SPACING)
                    .finish(),
            )
            .with_child(input);

        if let Some(error) = &self.error_message {
            content.add_child(
                Container::new(
                    Text::new_inline(
                        error.clone(),
                        appearance.ui_font_family(),
                        appearance.scale(LABEL_FONT_SIZE),
                    )
                    .with_color(theme.ui_error_color())
                    .finish(),
                )
                .with_margin_top(ROW_SPACING)
                .finish(),
            );
        } else {
            content.add_child(
                Container::new(
                    Text::new_inline(
                        "Enter to rename everywhere · Esc to cancel",
                        appearance.ui_font_family(),
                        appearance.scale(LABEL_FONT_SIZE - 1.),
                    )
                    .with_color(theme.disabled_ui_text_color().into())
                    .finish(),
                )
                .with_margin_top(ROW_SPACING)
                .finish(),
            );
        }

        Container::new(
            ConstrainedBox::new(content.finish())
                .with_width(appearance.scale(RENAME_WIDTH))
                .finish(),
        )
        .with_uniform_padding(10.)
        .with_background(theme.surface_2())
        .with_border(Border::all(1.).with_border_fill(theme.outline()))
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
        .with_drop_shadow(DropShadow::default())
        .finish()
    }
}
