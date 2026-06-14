//! Login-free local AI chat panel.
//!
//! A small side panel that talks to a local OpenAI-compatible endpoint via
//! [`super::local_chat`] — ollama on `:11434` ("Local") or `genesi-ai-turbo` on
//! `:11435` ("Turbo"). It also surfaces, and lets you drive, `genesi-ai-mode`'s
//! AI Mode (the daemon that tunes the box for inference) so the whole local-AI
//! story lives in one place: no account, no cloud.
#![allow(dead_code)]

use std::path::PathBuf;

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

use super::local_agent::{self, MAX_AGENT_STEPS};
use super::local_chat::{
    list_models, read_ai_mode_state, set_ai_mode_force, stream_chat, turbo_health_ok, AiModeState,
    ChatMessage, ChatStreamItem, CodeContext, LocalEndpoint,
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

/// The Genesi brand green, used as the panel's accent.
fn genesi_green() -> ColorU {
    ColorU::new(15, 143, 106, 255)
}

/// Who authored a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    User,
    Assistant,
    /// An agent tool step (e.g. read_file) — rendered distinctly from prose.
    Tool,
}

/// One line in the transcript. The assistant's text grows as tokens stream in.
struct ChatEntry {
    role: ChatRole,
    text: String,
    /// For user turns: a short label of the code context attached (if any), e.g.
    /// `foo.rs · lines 12-40`. Shown faintly under the message.
    context_label: Option<String>,
}

/// Events emitted to the workspace.
pub enum LocalAiChatEvent {
    /// Close the panel.
    ClosePanel,
    /// The user submitted a prompt; the workspace attaches fresh file context
    /// and calls back into [`LocalAiChatView::send_with_context`]. Routing this
    /// through the workspace is what gives the panel workspace awareness.
    SubmitPrompt(String),
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
    /// Toggle auto-attaching the focused file as context.
    ToggleAttachContext,
    /// Toggle agent mode (the model can read the project via tools).
    ToggleAgent,
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

    /// When on, each send attaches the focused code editor's file (or selection)
    /// as context — like a normal AI IDE.
    attach_context: bool,

    /// When on, the model runs as a codebase agent: it can read the project via
    /// tools (read_file/list_files/grep) before answering.
    agent_mode: bool,

    // ── agent-loop state (only meaningful while an agent turn is in flight) ──
    /// Project root the agent's tools resolve paths against.
    agent_root: Option<PathBuf>,
    /// The running conversation sent to the model (diverges from the visible
    /// transcript: it carries raw tool calls and tool results).
    agent_messages: Vec<ChatMessage>,
    /// Model name pinned for the duration of the agent turn.
    agent_model: String,
    /// How many tool steps this turn has taken (bounded by [`MAX_AGENT_STEPS`]).
    agent_step: u32,
    /// Accumulates the current step's streamed tokens until it completes.
    agent_step_buffer: String,
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
            attach_context: true,
            agent_mode: false,
            agent_root: None,
            agent_messages: Vec::new(),
            agent_model: String::new(),
            agent_step: 0,
            agent_step_buffer: String::new(),
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
        // Turbo (llama-server) already has its model loaded and its `/v1/models`
        // isn't a reliable signal, so an empty/failed list there is not an error —
        // readiness comes from the `/health` probe below instead.
        let is_turbo = self.endpoint == LocalEndpoint::Turbo;
        ctx.spawn(
            async move { list_models(&base).await },
            move |me, result, ctx| {
                match result {
                    Ok(models) => {
                        me.models = models;
                        me.selected_model = if me.models.is_empty() {
                            None
                        } else {
                            Some(me.selected_model.unwrap_or(0).min(me.models.len() - 1))
                        };
                        if me.models.is_empty() && !is_turbo {
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
                        me.error = if is_turbo {
                            None
                        } else {
                            Some(format!("Can't reach the local endpoint: {e}"))
                        };
                    }
                }
                ctx.notify();
            },
        );

        ctx.spawn(
            async { turbo_health_ok().await },
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
            // Route the submit through the workspace so it can attach the
            // focused file as context before we actually send (see
            // `LocalAiChatEvent::SubmitPrompt`). The workspace calls back into
            // `send_with_context`.
            SubmittableTextInputEvent::Submit(text) => {
                ctx.emit(LocalAiChatEvent::SubmitPrompt(text.clone()));
            }
            SubmittableTextInputEvent::Escape => {}
        }
    }

    /// Send the prompt and start streaming the assistant's reply, optionally
    /// attaching the focused file as context. Called by the workspace after it
    /// gathers `context` for the current turn.
    pub fn send_with_context(
        &mut self,
        prompt: String,
        context: Option<CodeContext>,
        project_root: Option<PathBuf>,
        ctx: &mut ViewContext<Self>,
    ) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() || self.in_flight {
            return;
        }
        // Turbo serves whatever model llama-server already loaded, so the `model`
        // field is informational there — fall back to a placeholder when no model
        // is listed. Ollama needs a real model name.
        let model = match self.current_model() {
            Some(model) => model,
            None if self.endpoint == LocalEndpoint::Turbo => "local".to_string(),
            None => {
                self.error = Some(
                    "No model selected. Is ollama running? Try `ollama pull llama3.2`.".to_string(),
                );
                ctx.notify();
                return;
            }
        };

        self.error = None;

        // Attach the focused file only when the toggle is on. Recorded on the
        // user entry so the transcript shows what the model was given.
        let context = if self.attach_context { context } else { None };
        let context_label = context.as_ref().map(CodeContext::label);

        self.messages.push(ChatEntry {
            role: ChatRole::User,
            text: prompt,
            context_label,
        });

        // System prompt: the agent variant (with tool instructions) in agent
        // mode, otherwise the plain chat prompt. Then the attached file context,
        // then the visible transcript text (tool steps are UI-only).
        let system_prompt = if self.agent_mode {
            local_agent::agent_system_prompt()
        } else {
            SYSTEM_PROMPT.to_string()
        };
        let mut request = vec![ChatMessage::system(system_prompt)];
        if let Some(context) = &context {
            request.push(context.to_system_message());
        }
        for entry in &self.messages {
            if entry.text.is_empty() {
                continue;
            }
            match entry.role {
                ChatRole::User => request.push(ChatMessage::user(entry.text.clone())),
                ChatRole::Assistant => request.push(ChatMessage::assistant(entry.text.clone())),
                // Tool steps are UI-only; the model's real tool results live in
                // `agent_messages` during a turn, not the visible transcript.
                ChatRole::Tool => {}
            }
        }

        self.in_flight = true;
        self.scroll_to_bottom();

        if self.agent_mode {
            // Drive the agent loop: the model may call read tools before it
            // answers. Each step appends to `agent_messages`.
            self.agent_root = project_root;
            self.agent_model = model;
            self.agent_messages = request;
            self.agent_step = 0;
            self.run_agent_step(ctx);
            ctx.notify();
            return;
        }

        // Plain chat: one streamed reply into a placeholder bubble.
        self.messages.push(ChatEntry {
            role: ChatRole::Assistant,
            text: String::new(),
            context_label: None,
        });
        let stream = stream_chat(self.endpoint, &model, request);
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

    // ── agent loop ─────────────────────────────────────────────────────────

    /// Run one agent step: stream the model's next message into a fresh bubble,
    /// then settle it in [`Self::on_agent_step_end`] (execute a tool and loop, or
    /// finish).
    fn run_agent_step(&mut self, ctx: &mut ViewContext<Self>) {
        self.agent_step_buffer.clear();
        self.messages.push(ChatEntry {
            role: ChatRole::Assistant,
            text: String::new(),
            context_label: None,
        });
        let stream = stream_chat(self.endpoint, &self.agent_model, self.agent_messages.clone());
        ctx.spawn_stream_local(
            stream,
            |me, item, ctx| me.on_agent_token(item, ctx),
            |me, ctx| me.on_agent_step_end(ctx),
        );
    }

    fn on_agent_token(&mut self, item: Result<ChatStreamItem>, ctx: &mut ViewContext<Self>) {
        match item {
            Ok(ChatStreamItem::Token(token)) => {
                self.agent_step_buffer.push_str(&token);
                if let Some(last) = self.messages.last_mut() {
                    if last.role == ChatRole::Assistant {
                        last.text.push_str(&token);
                    }
                }
                self.scroll_to_bottom();
                ctx.notify();
            }
            // The step is settled in `on_agent_step_end`.
            Ok(ChatStreamItem::Done) => {}
            Err(e) => self.finish_agent_with_error(format!("Local model error: {e}"), ctx),
        }
    }

    fn on_agent_step_end(&mut self, ctx: &mut ViewContext<Self>) {
        if !self.in_flight {
            return; // already errored out
        }
        let reply = self.agent_step_buffer.trim().to_string();
        self.agent_messages.push(ChatMessage::assistant(reply.clone()));

        match local_agent::parse_tool_call(&reply) {
            Some(tool) if self.agent_step < MAX_AGENT_STEPS => {
                let result = match self.agent_root.clone() {
                    Some(root) => local_agent::run_read_tool(&root, &tool),
                    None => "error: no project is open, so I can't read files.".to_string(),
                };

                // Replace the streamed bubble (which held the raw tool tag) with
                // a clean tool step.
                let preview = Self::tool_preview(&result);
                if let Some(last) = self.messages.last_mut() {
                    last.role = ChatRole::Tool;
                    last.text = format!("🔧 {}\n{preview}", tool.summary());
                }

                // Feed the full result back to the model and take another step.
                self.agent_messages.push(ChatMessage::user(format!(
                    "TOOL RESULT ({}):\n{result}",
                    tool.name()
                )));
                self.agent_step += 1;
                self.scroll_to_bottom();
                ctx.notify();
                self.run_agent_step(ctx);
            }
            _ => {
                // No tool call (or the step budget is spent): the reply is the
                // final answer, already in the last assistant bubble.
                if self.agent_step >= MAX_AGENT_STEPS {
                    if let Some(last) = self.messages.last_mut() {
                        if last.role == ChatRole::Assistant && last.text.is_empty() {
                            last.text = "(reached the tool-step limit)".to_string();
                        }
                    }
                }
                self.in_flight = false;
                self.refresh_ai_mode();
                self.scroll_to_bottom();
                ctx.notify();
            }
        }
    }

    fn finish_agent_with_error(&mut self, message: String, ctx: &mut ViewContext<Self>) {
        self.in_flight = false;
        if let Some(last) = self.messages.last() {
            if last.role == ChatRole::Assistant && last.text.is_empty() {
                self.messages.pop();
            }
        }
        self.error = Some(message);
        ctx.notify();
    }

    /// A short preview of a tool result for the transcript; the full result is
    /// what goes back to the model.
    fn tool_preview(result: &str) -> String {
        const MAX_LINES: usize = 6;
        const MAX_CHARS: usize = 400;
        let mut preview: String = result
            .lines()
            .take(MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        if preview.chars().count() > MAX_CHARS {
            preview = preview.chars().take(MAX_CHARS).collect();
        }
        if result.lines().count() > MAX_LINES || result.chars().count() > MAX_CHARS {
            preview.push_str("\n…");
        }
        preview
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

        // The panel is only ~380px wide, so the controls live on two rows: the
        // endpoint/model selectors, then the action chips. Cramming all five into
        // one row overflowed and clipped the last button.
        let selectors_row = Flex::row()
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
            .finish();

        let actions_row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.chip(
                appearance,
                format!("🤖 Agent: {}", if self.agent_mode { "On" } else { "Off" }),
                LocalAiChatAction::ToggleAgent,
                self.agent_mode,
            ))
            .with_child(self.chip(
                appearance,
                format!("📎 {}", if self.attach_context { "On" } else { "Off" }),
                LocalAiChatAction::ToggleAttachContext,
                self.attach_context,
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
            .with_child(Container::new(selectors_row).with_padding_bottom(4.).finish())
            .with_child(actions_row)
            .finish()
    }

    fn render_message(&self, appearance: &Appearance, entry: &ChatEntry) -> Box<dyn Element> {
        let theme = appearance.theme();
        let is_user = entry.role == ChatRole::User;
        let background = if is_user {
            theme.surface_2()
        } else {
            theme.surface_1()
        };
        let (prefix, role_color): (&str, ColorU) = match entry.role {
            ChatRole::User => ("You", genesi_green()),
            ChatRole::Assistant => ("Genesi AI", theme.active_ui_text_color().into()),
            ChatRole::Tool => ("Tool", theme.disabled_text_color(theme.background()).into()),
        };

        let body = if entry.text.is_empty() && self.in_flight {
            "…".to_string()
        } else {
            entry.text.clone()
        };

        let mut inner = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.label_text(appearance, prefix, CHIP_FONT_SIZE, role_color, false));

        // Show the attached file context (if any) as a small left-aligned pill.
        if let Some(label) = &entry.context_label {
            let pill = Container::new(self.label_text(
                appearance,
                format!("📎 {label}"),
                CHIP_FONT_SIZE,
                theme.disabled_text_color(theme.background()).into(),
                false,
            ))
            .with_horizontal_padding(6.)
            .with_vertical_padding(2.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_2())
            .finish();

            inner.add_child(
                Container::new(
                    Flex::row()
                        .with_main_axis_size(MainAxisSize::Max)
                        .with_child(pill)
                        .with_child(Shrinkable::new(1., Empty::new().finish()).finish())
                        .finish(),
                )
                .with_margin_top(4.)
                .finish(),
            );
        }

        let inner = inner
            .with_child(
                Container::new(self.label_text(
                    appearance,
                    body,
                    BODY_FONT_SIZE,
                    theme.main_text_color(theme.background()).into(),
                    true,
                ))
                .with_margin_top(4.)
                .finish(),
            )
            .finish();

        let mut bubble = Container::new(inner)
            .with_uniform_padding(10.)
            .with_margin_bottom(8.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
            .with_background(background);

        // Genesi-green left accent on the user's turns for a clear visual rhythm.
        if is_user {
            bubble = bubble.with_border(Border::left(2.).with_border_color(genesi_green()));
        }
        bubble.finish()
    }

    fn render_transcript(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        if self.messages.is_empty() {
            let (title, hint) = match self.current_model() {
                Some(model) => (
                    "Ask your local model".to_string(),
                    format!(
                        "Running {model} on-device — no account, no cloud. With 📎 on, \
                         the file you're editing is sent as context."
                    ),
                ),
                None => (
                    "No local model yet".to_string(),
                    "Start ollama and pull a model (e.g. `ollama pull llama3.2`), then hit Refresh."
                        .to_string(),
                ),
            };
            column.add_child(
                Container::new(
                    Flex::column()
                        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
                        .with_child(self.label_text(
                            appearance,
                            title,
                            TITLE_FONT_SIZE,
                            theme.active_ui_text_color().into(),
                            false,
                        ))
                        .with_child(
                            Container::new(self.label_text(
                                appearance,
                                hint,
                                BODY_FONT_SIZE,
                                theme.disabled_text_color(theme.background()).into(),
                                true,
                            ))
                            .with_margin_top(6.)
                            .finish(),
                        )
                        .finish(),
                )
                .with_uniform_padding(12.)
                .with_margin_top(8.)
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
            LocalAiChatAction::ToggleAttachContext => {
                self.attach_context = !self.attach_context;
                ctx.notify();
            }
            LocalAiChatAction::ToggleAgent => {
                self.agent_mode = !self.agent_mode;
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
                .with_border(Border::bottom(1.).with_border_fill(theme.outline()))
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

        // The input as a bordered, rounded compose box so it reads as an input
        // rather than floating text.
        let input_box = Container::new(ChildView::new(&self.input).finish())
            .with_horizontal_padding(8.)
            .with_vertical_padding(6.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_1())
            .finish();
        root.add_child(
            Container::new(input_box)
                .with_horizontal_padding(PANEL_PADDING)
                .with_padding_top(4.)
                .with_padding_bottom(PANEL_PADDING)
                .finish(),
        );

        root.finish()
    }
}
