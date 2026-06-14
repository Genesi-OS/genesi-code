//! Login-free local AI chat panel.
//!
//! A small side panel that talks to a local OpenAI-compatible endpoint via
//! [`super::local_chat`] — ollama on `:11434` ("Local") or `genesi-ai-turbo` on
//! `:11435` ("Turbo"). It also surfaces, and lets you drive, `genesi-ai-mode`'s
//! AI Mode (the daemon that tunes the box for inference) so the whole local-AI
//! story lives in one place: no account, no cloud.
#![allow(dead_code)]

use anyhow::Result;
use warpui::color::ColorU;
use warpui::elements::{
    Border, ClippedScrollStateHandle, ClippedScrollable, Container, CornerRadius,
    CrossAxisAlignment, DispatchEventResult, Element, Empty, EventHandler, Fill, Flex,
    MainAxisSize, ParentElement, Radius, ScrollbarWidth, Shrinkable,
};
use warpui::presenter::ChildView;
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::units::Pixels;
use warpui::{
    AppContext, Entity, FocusContext, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use super::local_chat::{
    endpoint_available, list_models, read_ai_mode_state, set_ai_mode_force, stream_chat, AiModeState,
    ChatMessage, ChatStreamItem, LocalEndpoint, TURBO_LOCAL_BASE_URL,
};
use crate::appearance::Appearance;
use crate::view_components::{SubmittableTextInput, SubmittableTextInputEvent};

const TITLE_FONT_SIZE: f32 = 15.;
const CHIP_FONT_SIZE: f32 = 11.;
const BODY_FONT_SIZE: f32 = 13.;
const PANEL_PADDING: f32 = 8.;
/// A large scroll target; `ClippedScrollable::after_layout` clamps it to the
/// real bottom, so this reliably pins the transcript to the latest message.
const SCROLL_TO_BOTTOM: f32 = 1.0e7;

const SYSTEM_PROMPT: &str =
    "You are a helpful AI assistant running locally on Genesi OS. Be concise.";

/// Who authored a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    User,
    Assistant,
}

/// One line in the transcript. The assistant's text grows as tokens stream in.
struct ChatEntry {
    role: ChatRole,
    text: String,
}

/// Events emitted to the workspace (so it can close the panel).
pub enum LocalAiChatEvent {
    ClosePanel,
}

/// Click actions dispatched by the header chips.
#[derive(Debug, Clone)]
pub enum LocalAiChatAction {
    /// Switch between the Local (ollama) and Turbo endpoints.
    CycleEndpoint,
    /// Advance to the next available model.
    CycleModel,
    /// Cycle the AI Mode override: auto -> on -> off -> auto.
    CycleAiMode,
    /// Re-probe models, endpoints, and AI Mode state.
    Refresh,
    /// Clear the transcript.
    Clear,
}

pub struct LocalAiChatView {
    input: ViewHandle<SubmittableTextInput>,
    messages: Vec<ChatEntry>,
    transcript_scroll: ClippedScrollStateHandle,

    endpoint: LocalEndpoint,
    turbo_available: bool,
    models: Vec<String>,
    selected_model: Option<usize>,
    ai_mode: Option<AiModeState>,

    in_flight: bool,
    error: Option<String>,
}

impl LocalAiChatView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let input = ctx.add_typed_action_view(|ctx| SubmittableTextInput::new(ctx));
        input.update(ctx, |input, ctx| {
            input.set_placeholder_text(" Ask the local model...", ctx);
        });
        ctx.subscribe_to_view(&input, |me, _, event, ctx| {
            me.handle_input_event(event, ctx);
        });

        let mut view = Self {
            input,
            messages: Vec::new(),
            transcript_scroll: ClippedScrollStateHandle::default(),
            endpoint: LocalEndpoint::Ollama,
            turbo_available: false,
            models: Vec::new(),
            selected_model: None,
            ai_mode: None,
            in_flight: false,
            error: None,
        };
        view.refresh_ai_mode();
        view.refresh_models(ctx);
        view
    }

    /// Name of the currently selected model, if any.
    fn current_model(&self) -> Option<String> {
        self.selected_model
            .and_then(|index| self.models.get(index))
            .cloned()
    }

    /// Re-read the daemon's published AI Mode state (cheap, synchronous).
    fn refresh_ai_mode(&mut self) {
        self.ai_mode = read_ai_mode_state();
    }

    /// Ask the active endpoint for its model list and probe whether Turbo is up.
    fn refresh_models(&mut self, ctx: &mut ViewContext<Self>) {
        let base = self.endpoint.base_url().to_string();
        ctx.spawn(
            async move { list_models(&base).await },
            |me, result, ctx| {
                match result {
                    Ok(models) => {
                        me.models = models;
                        me.selected_model = if me.models.is_empty() {
                            None
                        } else {
                            Some(me.selected_model.unwrap_or(0).min(me.models.len() - 1))
                        };
                        if me.models.is_empty() {
                            me.error = Some(
                                "No local models found. Is ollama running? Try `ollama pull llama3.2`."
                                    .to_string(),
                            );
                        } else {
                            me.error = None;
                        }
                    }
                    Err(e) => {
                        me.models.clear();
                        me.selected_model = None;
                        me.error = Some(format!("Can't reach the local endpoint: {e}"));
                    }
                }
                ctx.notify();
            },
        );

        ctx.spawn(
            async { endpoint_available(TURBO_LOCAL_BASE_URL).await },
            |me, available, ctx| {
                me.turbo_available = available;
                ctx.notify();
            },
        );
    }

    fn scroll_to_bottom(&self) {
        self.transcript_scroll.scroll_to(Pixels::new(SCROLL_TO_BOTTOM));
    }

    fn handle_input_event(
        &mut self,
        event: &SubmittableTextInputEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        match event {
            SubmittableTextInputEvent::Submit(text) => self.send(text.clone(), ctx),
            SubmittableTextInputEvent::Escape => {}
        }
    }

    /// Send the prompt and start streaming the assistant's reply.
    fn send(&mut self, prompt: String, ctx: &mut ViewContext<Self>) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() || self.in_flight {
            return;
        }
        let Some(model) = self.current_model() else {
            self.error = Some(
                "No model selected. Is ollama running? Try `ollama pull llama3.2`.".to_string(),
            );
            ctx.notify();
            return;
        };

        self.error = None;
        self.messages.push(ChatEntry {
            role: ChatRole::User,
            text: prompt,
        });
        // The assistant placeholder grows as tokens arrive.
        self.messages.push(ChatEntry {
            role: ChatRole::Assistant,
            text: String::new(),
        });
        self.in_flight = true;
        self.scroll_to_bottom();

        // Build the request from the full transcript (skipping the empty
        // placeholder we just pushed), prefixed with a small system prompt.
        let mut request = vec![ChatMessage::system(SYSTEM_PROMPT)];
        for entry in &self.messages {
            if entry.text.is_empty() {
                continue;
            }
            request.push(match entry.role {
                ChatRole::User => ChatMessage::user(entry.text.clone()),
                ChatRole::Assistant => ChatMessage::assistant(entry.text.clone()),
            });
        }

        let stream = stream_chat(self.endpoint.base_url(), &model, request);
        ctx.spawn_stream_local(
            stream,
            |me, item, ctx| me.on_stream_item(item, ctx),
            |me, ctx| {
                // Stream ended without an explicit `[DONE]` — settle the UI.
                if me.in_flight {
                    me.in_flight = false;
                    ctx.notify();
                }
            },
        );
        ctx.notify();
    }

    fn on_stream_item(&mut self, item: Result<ChatStreamItem>, ctx: &mut ViewContext<Self>) {
        match item {
            Ok(ChatStreamItem::Token(token)) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == ChatRole::Assistant {
                        last.text.push_str(&token);
                    }
                }
                self.scroll_to_bottom();
                ctx.notify();
            }
            Ok(ChatStreamItem::Done) => {
                self.in_flight = false;
                // tokens/s and activity likely changed — refresh the badge.
                self.refresh_ai_mode();
                self.scroll_to_bottom();
                ctx.notify();
            }
            Err(e) => {
                self.in_flight = false;
                // Drop the empty assistant placeholder if nothing streamed.
                if let Some(last) = self.messages.last() {
                    if last.role == ChatRole::Assistant && last.text.is_empty() {
                        self.messages.pop();
                    }
                }
                self.error = Some(format!("Local model error: {e}"));
                ctx.notify();
            }
        }
    }

    // ── rendering ────────────────────────────────────────────────────────────

    fn label_text(
        &self,
        appearance: &Appearance,
        text: impl Into<String>,
        size: f32,
        color: ColorU,
        soft_wrap: bool,
    ) -> Box<dyn Element> {
        appearance
            .ui_builder()
            .wrappable_text(text.into(), soft_wrap)
            .with_style(UiComponentStyles {
                font_family_id: Some(appearance.ui_font_family()),
                font_size: Some(size),
                font_color: Some(color),
                ..Default::default()
            })
            .build()
            .finish()
    }

    fn chip(
        &self,
        appearance: &Appearance,
        label: String,
        action: LocalAiChatAction,
        enabled: bool,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let color: ColorU = if enabled {
            theme.active_ui_text_color().into()
        } else {
            theme.disabled_text_color(theme.background()).into()
        };
        let content = Container::new(self.label_text(appearance, label, CHIP_FONT_SIZE, color, false))
            .with_horizontal_padding(8.)
            .with_vertical_padding(3.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_2())
            .with_margin_right(6.)
            .finish();

        EventHandler::new(content)
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    fn render_header(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();

        let ai_mode_label = match &self.ai_mode {
            Some(state) => state.badge_text(),
            None => "AI Mode: n/a".to_string(),
        };

        let title_row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.label_text(
                appearance,
                "Local AI",
                TITLE_FONT_SIZE,
                theme.active_ui_text_color().into(),
                false,
            ))
            .with_child(Shrinkable::new(1., Empty::new().finish()).finish())
            .with_child(self.chip(
                appearance,
                ai_mode_label,
                LocalAiChatAction::CycleAiMode,
                self.ai_mode.is_some(),
            ))
            .finish();

        let endpoint_label = format!("Endpoint: {}", self.endpoint.label());
        let model_label = match self.current_model() {
            Some(model) => format!("Model: {model}"),
            None => "Model: none".to_string(),
        };

        let controls_row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.chip(
                appearance,
                endpoint_label,
                LocalAiChatAction::CycleEndpoint,
                true,
            ))
            .with_child(self.chip(
                appearance,
                model_label,
                LocalAiChatAction::CycleModel,
                !self.models.is_empty(),
            ))
            .with_child(self.chip(
                appearance,
                "Refresh".to_string(),
                LocalAiChatAction::Refresh,
                true,
            ))
            .with_child(self.chip(
                appearance,
                "Clear".to_string(),
                LocalAiChatAction::Clear,
                !self.messages.is_empty(),
            ))
            .finish();

        Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(Container::new(title_row).with_padding_bottom(6.).finish())
            .with_child(controls_row)
            .finish()
    }

    fn render_message(&self, appearance: &Appearance, entry: &ChatEntry) -> Box<dyn Element> {
        let theme = appearance.theme();
        let (background, prefix): (ColorU, &str) = match entry.role {
            ChatRole::User => (theme.surface_2().into(), "You"),
            ChatRole::Assistant => (theme.surface_1().into(), "AI"),
        };

        let body = if entry.text.is_empty() && self.in_flight {
            "…".to_string()
        } else {
            entry.text.clone()
        };

        let inner = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.label_text(
                appearance,
                prefix,
                CHIP_FONT_SIZE,
                theme.disabled_text_color(theme.background()).into(),
                false,
            ))
            .with_child(self.label_text(
                appearance,
                body,
                BODY_FONT_SIZE,
                theme.main_text_color(theme.background()).into(),
                true,
            ))
            .finish();

        Container::new(inner)
            .with_uniform_padding(8.)
            .with_margin_bottom(6.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
            .with_background(background)
            .finish()
    }

    fn render_transcript(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        if self.messages.is_empty() {
            let hint = match self.current_model() {
                Some(model) => format!("Chat with your local model. Current: {model}."),
                None => "No local model available yet. Start ollama and pull a model.".to_string(),
            };
            column.add_child(
                Container::new(self.label_text(
                    appearance,
                    hint,
                    BODY_FONT_SIZE,
                    theme.disabled_text_color(theme.background()).into(),
                    true,
                ))
                .with_uniform_padding(8.)
                .finish(),
            );
        } else {
            for entry in &self.messages {
                column.add_child(self.render_message(appearance, entry));
            }
        }

        ClippedScrollable::vertical(
            self.transcript_scroll.clone(),
            Container::new(column.finish())
                .with_horizontal_padding(PANEL_PADDING)
                .finish(),
            ScrollbarWidth::Auto,
            theme.disabled_ui_text_color().into(),
            theme.active_ui_text_color().into(),
            Fill::None,
        )
        .finish()
    }
}

impl Entity for LocalAiChatView {
    type Event = LocalAiChatEvent;
}

impl TypedActionView for LocalAiChatView {
    type Action = LocalAiChatAction;

    fn handle_action(&mut self, action: &LocalAiChatAction, ctx: &mut ViewContext<Self>) {
        match action {
            LocalAiChatAction::CycleEndpoint => {
                let next = self.endpoint.toggled();
                if next == LocalEndpoint::Turbo && !self.turbo_available {
                    self.error = Some(
                        "Turbo (:11435) isn't running. Start it with `genesi-ai-turbo serve <model>`."
                            .to_string(),
                    );
                } else {
                    self.endpoint = next;
                    self.error = None;
                    self.refresh_models(ctx);
                }
                ctx.notify();
            }
            LocalAiChatAction::CycleModel => {
                if !self.models.is_empty() {
                    let next = self
                        .selected_model
                        .map(|index| (index + 1) % self.models.len())
                        .unwrap_or(0);
                    self.selected_model = Some(next);
                }
                ctx.notify();
            }
            LocalAiChatAction::CycleAiMode => {
                let current = self
                    .ai_mode
                    .as_ref()
                    .map(|state| state.force_mode.as_str())
                    .unwrap_or("auto");
                let next = match current {
                    "auto" => "on",
                    "on" => "off",
                    _ => "auto",
                };
                match set_ai_mode_force(next) {
                    Ok(()) => {
                        self.refresh_ai_mode();
                        self.error = None;
                    }
                    Err(e) => self.error = Some(format!("Couldn't change AI Mode: {e}")),
                }
                ctx.notify();
            }
            LocalAiChatAction::Refresh => {
                self.refresh_models(ctx);
                self.refresh_ai_mode();
                ctx.notify();
            }
            LocalAiChatAction::Clear => {
                self.messages.clear();
                self.error = None;
                ctx.notify();
            }
        }
    }
}

impl View for LocalAiChatView {
    fn ui_name() -> &'static str {
        "LocalAiChatView"
    }

    fn on_focus(&mut self, focus_ctx: &FocusContext, ctx: &mut ViewContext<Self>) {
        if focus_ctx.is_self_focused() {
            ctx.focus(&self.input);
        }
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();

        let mut root = Flex::column().with_main_axis_size(MainAxisSize::Max);

        root.add_child(
            Container::new(self.render_header(appearance))
                .with_uniform_padding(PANEL_PADDING)
                .finish(),
        );

        root.add_child(Shrinkable::new(1., self.render_transcript(appearance)).finish());

        if let Some(error) = &self.error {
            root.add_child(
                Container::new(self.label_text(
                    appearance,
                    error.clone(),
                    CHIP_FONT_SIZE,
                    theme.ui_error_color().into(),
                    true,
                ))
                .with_horizontal_padding(PANEL_PADDING)
                .with_padding_bottom(4.)
                .finish(),
            );
        }

        root.add_child(
            Container::new(ChildView::new(&self.input).finish())
                .with_horizontal_padding(PANEL_PADDING)
                .with_padding_bottom(PANEL_PADDING)
                .finish(),
        );

        root.finish()
    }
}
