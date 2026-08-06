use pathfinder_color::ColorU;
use warpui::elements::{
    Border, Container, CornerRadius, CrossAxisAlignment, Flex, MainAxisAlignment, MainAxisSize,
    MouseStateHandle, ParentElement, Radius, Shrinkable,
};
use warpui::ui_components::components::{Coords, UiComponent, UiComponentStyles};
use warpui::{
    AppContext, Element, Entity, FocusContext, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use crate::appearance::Appearance;
use crate::editor::{
    EditorOptions, EditorView, Event as EditorEvent, InteractionState, TextOptions,
};
use crate::themes::theme::Fill;

const ENTER_BUTTON_SIZE: f32 = 22.;

enum ValidatorType {
    /// Validates whenever the input changes.
    OnEdit,
    /// Only validates on submission.
    OnSubmitOnly,
}

/// This View is a text input in which you can submit its contents by clicking the embedded button
/// or pressing Enter.
pub struct SubmittableTextInput {
    editor: ViewHandle<EditorView>,
    /// A closure that returns if the current editor content are valid and can be submitted.
    validator: Box<dyn Fn(&str) -> bool>,
    validator_type: ValidatorType,
    /// Whether or not the last edit made the contents valid.
    has_error: bool,
    submit_button_state: MouseStateHandle,
    show_submit_button: bool,
    outer_margin_top: f32,
    outer_margin_bottom: f32,
    /// Drop the frame around the field. A caller that already draws its own
    /// compose surface (the AI panel) otherwise gets a second box inside the
    /// first. An error still draws its border, so invalid input stays visible.
    borderless: bool,
}

impl SubmittableTextInput {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let editor = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            let options = EditorOptions {
                autogrow: true,
                soft_wrap: true,
                text: TextOptions::ui_font_size(appearance),
                ..Default::default()
            };
            EditorView::new(options, ctx)
        });

        ctx.subscribe_to_view(&editor, Self::handle_editor_event);

        Self {
            editor,
            validator: Box::new(|_| true),
            validator_type: ValidatorType::OnEdit,
            has_error: false,
            submit_button_state: Default::default(),
            show_submit_button: true,
            outer_margin_top: 10.,
            outer_margin_bottom: 10.,
            borderless: false,
        }
    }

    /// Validates the input contents using the provided `validator`
    /// on every edit action.
    pub fn validate_on_edit<F: Fn(&str) -> bool + 'static>(mut self, validator: F) -> Self {
        self.validator_type = ValidatorType::OnEdit;
        self.validator = Box::new(validator);
        self
    }

    /// Validates the input contents using the provided `validator`
    /// whenever a submit action is attempted.
    pub fn validate_on_submit<F: Fn(&str) -> bool + 'static>(mut self, validator: F) -> Self {
        self.validator_type = ValidatorType::OnSubmitOnly;
        self.validator = Box::new(validator);
        self
    }

    pub fn set_placeholder_text(&mut self, text: impl Into<String>, ctx: &mut ViewContext<Self>) {
        self.editor.update(ctx, |editor, ctx| {
            editor.set_placeholder_text(text, ctx);
        });
    }

    pub fn set_outer_margins(&mut self, top: f32, bottom: f32, ctx: &mut ViewContext<Self>) {
        self.outer_margin_top = top;
        self.outer_margin_bottom = bottom;
        ctx.notify();
    }

    /// Render without the surrounding frame — for callers that supply their own.
    pub fn set_borderless(&mut self, borderless: bool, ctx: &mut ViewContext<Self>) {
        self.borderless = borderless;
        ctx.notify();
    }

    pub fn set_submit_button_visible(&mut self, visible: bool, ctx: &mut ViewContext<Self>) {
        self.show_submit_button = visible;
        ctx.notify();
    }

    /// Returns a handle to the backing [`EditorView`].
    pub fn editor(&self) -> &ViewHandle<EditorView> {
        &self.editor
    }

    pub fn submit(&mut self, ctx: &mut ViewContext<Self>) {
        self.on_try_submit(ctx);
    }

    fn handle_editor_event(
        &mut self,
        _handle: ViewHandle<EditorView>,
        event: &EditorEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        match event {
            // Pressing Enter is the user attempting to submit the entry.
            EditorEvent::Enter => self.on_try_submit(ctx),
            // Every time the editor contents are changed, we check if the contents are now valid
            // so we can update the border and enable/disable the button.
            EditorEvent::Edited(_) => {
                let content = self.editor.read(ctx, |editor, ctx| editor.buffer_text(ctx));
                self.has_error = match self.validator_type {
                    ValidatorType::OnEdit => !(self.validator)(&content),
                    ValidatorType::OnSubmitOnly => false,
                };
                ctx.notify();
            }
            EditorEvent::Escape => ctx.emit(SubmittableTextInputEvent::Escape),
            // Surfaced so a caller that accepts attachments (the Genesi AI panel)
            // can turn a pasted FILE into a reference instead of a wall of path
            // text. With `delegate_paste_handling` on, nothing has been inserted
            // yet — the handler decides, and calls back for the text case.
            EditorEvent::Paste => ctx.emit(SubmittableTextInputEvent::Paste),
            _ => {}
        }
    }

    /// Route paste through the parent instead of inserting straight away. Only
    /// meaningful together with a handler for [`SubmittableTextInputEvent::Paste`]
    /// — without one, paste stops working.
    pub fn set_delegate_paste(&mut self, delegate: bool, ctx: &mut ViewContext<Self>) {
        self.editor.update(ctx, |editor, _| {
            editor.set_paste_to_parent(delegate);
        });
    }

    /// Insert text at the cursor, for a delegated paste the parent decided is
    /// ordinary text after all.
    pub fn insert_pasted_text(&mut self, text: &str, ctx: &mut ViewContext<Self>) {
        self.editor
            .update(ctx, |editor, ctx| editor.insert_pasted_text(text, ctx));
    }

    fn on_try_submit(&mut self, ctx: &mut ViewContext<Self>) {
        let content = self
            .editor
            .read(ctx, |editor, ctx| editor.buffer_text(ctx).trim().to_owned());
        if content.is_empty() {
            return;
        }

        if !(self.validator)(&content) {
            self.has_error = true;
            ctx.notify();
        } else {
            self.editor
                .update(ctx, |editor, ctx| editor.clear_buffer(ctx));
            ctx.emit(SubmittableTextInputEvent::Submit(content))
        }
    }
}

impl View for SubmittableTextInput {
    fn ui_name() -> &'static str {
        "SubmittableTextInput"
    }

    fn on_focus(&mut self, focus_ctx: &FocusContext, ctx: &mut ViewContext<Self>) {
        if focus_ctx.is_self_focused() {
            ctx.focus(&self.editor);
        }
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let border_fill = if self.has_error {
            appearance.theme().ui_error_color().into()
        } else {
            appearance.theme().outline()
        };

        let mut submit_button = appearance
            .ui_builder()
            .enter_button(ENTER_BUTTON_SIZE, self.submit_button_state.clone())
            .with_style(UiComponentStyles {
                padding: Some(Coords::uniform(4.)),
                ..Default::default()
            })
            .build();

        if self.has_error
            || self.editor.as_ref(app).interaction_state(app) == InteractionState::Disabled
        {
            submit_button = submit_button.disable();
        }

        let mut row = Flex::row()
            .with_main_axis_alignment(MainAxisAlignment::SpaceBetween)
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                Shrinkable::new(
                    1.,
                    appearance
                        .ui_builder()
                        .text_input(self.editor.clone())
                        .with_style(UiComponentStyles {
                            background: Some(Fill::Solid(ColorU::transparent_black()).into()),
                            border_color: Some(Fill::Solid(ColorU::transparent_black()).into()),
                            padding: Some(Coords::uniform(8.)),
                            ..Default::default()
                        })
                        .build()
                        .finish(),
                )
                .finish(),
            );

        if self.show_submit_button {
            row.add_child(
                submit_button
                    .on_click(|ctx, _, _| {
                        ctx.dispatch_typed_action(SubmittableTextInputAction::Submit)
                    })
                    .finish(),
            );
        }

        let mut outer = Container::new(row.finish());
        if !self.borderless || self.has_error {
            outer = outer
                .with_border(Border::all(1.).with_border_fill(border_fill))
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)));
        }
        outer
            .with_margin_top(self.outer_margin_top)
            .with_margin_bottom(self.outer_margin_bottom)
            .with_padding_left(4.)
            .with_padding_right(8.)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubmittableTextInputEvent {
    /// Notify the subscribers (parent view) of the submission.
    Submit(String),
    Escape,
    /// The user pasted. Only emitted usefully when the caller turned on
    /// [`SubmittableTextInput::set_delegate_paste`], in which case the clipboard
    /// has NOT been inserted and the handler owns what happens next.
    Paste,
}

impl Entity for SubmittableTextInput {
    type Event = SubmittableTextInputEvent;
}

#[derive(Debug)]
pub enum SubmittableTextInputAction {
    /// The user expressing the intent to submit. Only follow through with propagating this if the
    /// input is valid as determined by the validator closure.
    Submit,
}

impl TypedActionView for SubmittableTextInput {
    type Action = SubmittableTextInputAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            SubmittableTextInputAction::Submit => self.on_try_submit(ctx),
        }
    }
}
