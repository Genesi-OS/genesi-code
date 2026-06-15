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
use markdown_parser::{parse_markdown, FormattedText, FormattedTextFragment, FormattedTextLine};
use warpui::color::ColorU;
use warpui::elements::{
    Border, ClippedScrollStateHandle, ClippedScrollable, Container, CornerRadius,
    CrossAxisAlignment, DispatchEventResult, Element, Empty, EventHandler, Fill, Flex,
    FormattedTextElement, MainAxisSize, ParentElement, Radius, ScrollbarWidth, Shrinkable,
};
use warpui::presenter::ChildView;
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::units::Pixels;
use warpui::{
    AppContext, Entity, FocusContext, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use super::local_agent::{self, AgentTool, MAX_AGENT_STEPS};
use super::local_chat::{
    cloud_presets, list_models, load_cloud_config, read_ai_mode_state, save_cloud_config,
    set_ai_mode_force, stream_chat, stream_chat_cloud, turbo_health_ok, AiModeState, ChatMessage,
    ChatStreamItem, CloudConfig, CodeContext, LocalEndpoint,
};
use crate::appearance::Appearance;
use crate::view_components::{SubmittableTextInput, SubmittableTextInputEvent};

const TITLE_FONT_SIZE: f32 = 15.;
const CHIP_FONT_SIZE: f32 = 11.;
const BODY_FONT_SIZE: f32 = 13.;
/// Slightly smaller monospace size for tool output / terminal blocks.
const MONO_FONT_SIZE: f32 = 12.;
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

/// A very translucent Genesi green — for the subtle fill behind the user's
/// messages and the active control chips.
fn green_tint() -> ColorU {
    ColorU::new(15, 143, 106, 38)
}

/// A semi-opaque Genesi green — for the borders of active controls and the
/// user-message accent.
fn green_soft() -> ColorU {
    ColorU::new(15, 143, 106, 130)
}

/// What the compose box is currently capturing: a chat prompt, or a one-shot
/// value for the BYOK cloud provider (its API key or model id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Chat,
    CloudKey,
    CloudModel,
}

/// Who authored a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    User,
    /// The model's final, human-facing answer — rendered as markdown.
    Assistant,
    /// The model's reasoning for an agent step (the text it streamed before a
    /// tool call). Shown as a collapsible "💭 Thought" so the raw tool markup
    /// never clutters the transcript.
    Thought,
    /// A read-only agent tool step (read_file / list_files / grep / edit_file) —
    /// a collapsible one-line summary with the result tucked underneath.
    Tool,
    /// A `run_command` step — rendered as a terminal block (`$ cmd` + output).
    Command,
}

/// How an agent step (Tool / Command / Thought) is doing, for its status icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepStatus {
    Running,
    Ok,
    Error,
    Denied,
}

/// One line in the transcript. The assistant's text grows as tokens stream in.
struct ChatEntry {
    role: ChatRole,
    text: String,
    /// For user turns: a short label of the code context attached (if any), e.g.
    /// `foo.rs · lines 12-40`. Shown faintly under the message.
    context_label: Option<String>,
    /// For Tool steps: the one-line header (e.g. `read_file src/main.rs`).
    tool_title: Option<String>,
    /// For Command steps: the shell command line that was run.
    command: Option<String>,
    /// Collapsed state for the collapsible step kinds (Thought / Tool / Command).
    collapsed: bool,
    /// Status used to pick the step's icon/color.
    status: StepStatus,
}

impl ChatEntry {
    /// A plain prose entry (User / Assistant).
    fn prose(role: ChatRole, text: String, context_label: Option<String>) -> Self {
        Self {
            role,
            text,
            context_label,
            tool_title: None,
            command: None,
            collapsed: false,
            status: StepStatus::Ok,
        }
    }
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
    /// Toggle AUTO: run agent commands/edits without per-action approval.
    ToggleAuto,
    /// Open/close the AI model picker popup (above the compose box).
    ToggleModelPicker,
    /// Pick the local model at this index from the picker, and use the local
    /// (ollama) endpoint.
    PickModel(usize),
    /// Turn the Turbo (full-GPU) endpoint on or off.
    ToggleTurbo,
    /// Approve the pending side-effecting tool and run it.
    ApproveTool,
    /// Deny the pending side-effecting tool.
    DenyTool,
    /// Interrupt the in-flight generation / agent loop.
    Stop,
    /// Expand/collapse the transcript entry at this index (thought/tool/command).
    ToggleCollapse(usize),
    /// Cycle the BYOK cloud provider preset (HuggingFace / OpenAI / …).
    CycleProvider,
    /// Capture the cloud provider's API key in the compose box.
    SetKey,
    /// Capture the cloud provider's model id in the compose box.
    SetModel,
}

pub struct LocalAiChatView {
    input: ViewHandle<SubmittableTextInput>,
    messages: Vec<ChatEntry>,
    transcript_scroll: ClippedScrollStateHandle,

    endpoint: LocalEndpoint,
    turbo_available: bool,
    /// Set once the user picks an endpoint by hand, so the Turbo auto-default
    /// (see [`Self::refresh_models`]) never overrides a deliberate choice.
    endpoint_user_chosen: bool,
    models: Vec<String>,
    selected_model: Option<usize>,
    ai_mode: Option<AiModeState>,

    in_flight: bool,
    error: Option<String>,
    /// Monotonic id of the current generation. Stop bumps it so stale stream
    /// callbacks (from a turn the user interrupted) no-op instead of writing
    /// into a fresh turn — the streams are detached and can't be cancelled
    /// directly, so we guard their callbacks by id.
    current_turn: u64,

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
    /// Summary of the tool currently running, used to render its step.
    agent_tool_summary: String,

    /// When on, the agent runs commands/edits without asking. Off = approve each.
    auto_approve: bool,
    /// A side-effecting tool waiting for the user's Allow/Deny.
    pending_tool: Option<AgentTool>,

    // ── BYOK: optional cloud provider (off by default; local stays the default) ──
    /// The user's saved cloud provider (endpoint + key + model). Empty until set.
    cloud: CloudConfig,
    /// When on, prompts go to the cloud provider instead of Local/Turbo.
    cloud_active: bool,
    /// What the compose box's next submit means (a prompt, or a key/model value).
    input_mode: InputMode,

    /// Whether the click-to-open AI model picker (the popup above the compose
    /// box) is currently expanded.
    model_picker_open: bool,
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
            endpoint_user_chosen: false,
            models: Vec::new(),
            selected_model: None,
            ai_mode: None,
            in_flight: false,
            error: None,
            current_turn: 0,
            attach_context: true,
            agent_mode: false,
            agent_root: None,
            agent_messages: Vec::new(),
            agent_model: String::new(),
            agent_step: 0,
            agent_step_buffer: String::new(),
            agent_tool_summary: String::new(),
            auto_approve: false,
            pending_tool: None,
            cloud: load_cloud_config().unwrap_or_default(),
            cloud_active: false,
            input_mode: InputMode::Chat,
            model_picker_open: false,
        };
        view.refresh_ai_mode();
        view.refresh_models(ctx);
        view
    }

    /// Name of the currently selected model, if any. In cloud mode this is the
    /// configured provider model; otherwise the picked local model.
    fn current_model(&self) -> Option<String> {
        if self.cloud_active {
            let model = self.cloud.model.trim();
            return (!model.is_empty()).then(|| model.to_string());
        }
        self.selected_model
            .and_then(|index| self.models.get(index))
            .cloned()
    }

    /// Build the chat stream for the active backend: the user's cloud provider
    /// (BYOK) when it's selected and ready, otherwise the local Ollama / Turbo
    /// endpoint. Both yield the same `ChatStreamItem` stream so the agent loop
    /// and plain chat don't care which one is in use.
    fn build_stream(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> futures::stream::BoxStream<'static, Result<ChatStreamItem>> {
        if self.cloud_active && self.cloud.is_ready() {
            stream_chat_cloud(
                &self.cloud.base_url,
                model,
                self.cloud.api_key.clone(),
                messages,
            )
        } else {
            stream_chat(self.endpoint, model, messages)
        }
    }

    /// Re-read the daemon's published AI Mode state (cheap, synchronous).
    fn refresh_ai_mode(&mut self) {
        self.ai_mode = read_ai_mode_state();
    }

    /// Ask the active endpoint for its model list and probe whether Turbo is up.
    fn refresh_models(&mut self, ctx: &mut ViewContext<Self>) {
        // In cloud (BYOK) mode there's no local server to enumerate — the model is
        // whatever the user configured, so skip the local probes entirely.
        if self.cloud_active {
            return;
        }
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

        ctx.spawn(async { turbo_health_ok().await }, |me, available, ctx| {
            me.turbo_available = available;
            // Default to the shared Turbo daemon when it's up and the user
            // hasn't deliberately picked an endpoint — Code then inherits
            // GPU offload, the q8 KV cache and the warm model for free
            // (roadmap 4.1 / 4.0). One-shot: switching sets the endpoint, so
            // the re-probe below sees `== Turbo` and won't loop.
            if available && !me.endpoint_user_chosen && me.endpoint != LocalEndpoint::Turbo {
                me.endpoint = LocalEndpoint::Turbo;
                me.refresh_models(ctx);
            }
            ctx.notify();
        });
    }

    fn scroll_to_bottom(&self) {
        self.transcript_scroll
            .scroll_to(Pixels::new(SCROLL_TO_BOTTOM));
    }

    fn handle_input_event(
        &mut self,
        event: &SubmittableTextInputEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        match event {
            // In a cloud key/model entry mode the submit is the value to save;
            // otherwise it's a chat prompt. Route the prompt through the workspace
            // so it can attach the focused file as context before we send (see
            // `LocalAiChatEvent::SubmitPrompt`).
            SubmittableTextInputEvent::Submit(text) => match self.input_mode {
                InputMode::CloudKey | InputMode::CloudModel => {
                    self.save_cloud_field(self.input_mode, text.clone(), ctx);
                }
                InputMode::Chat => ctx.emit(LocalAiChatEvent::SubmitPrompt(text.clone())),
            },
            // Escape backs out of a key/model entry without saving.
            SubmittableTextInputEvent::Escape => {
                if self.input_mode != InputMode::Chat {
                    self.set_input_mode(InputMode::Chat, ctx);
                }
            }
        }
    }

    /// A friendly name for the configured cloud provider (or "cloud").
    fn cloud_label(&self) -> String {
        if self.cloud.label.trim().is_empty() {
            "cloud".to_string()
        } else {
            self.cloud.label.clone()
        }
    }

    /// Switch what the compose box captures next, updating its placeholder.
    fn set_input_mode(&mut self, mode: InputMode, ctx: &mut ViewContext<Self>) {
        self.input_mode = mode;
        let placeholder = match mode {
            InputMode::Chat => " Ask the model...".to_string(),
            InputMode::CloudKey => {
                format!(" Paste your {} API key, then Enter", self.cloud_label())
            }
            InputMode::CloudModel => {
                format!(
                    " Model id for {} (e.g. {}), then Enter",
                    self.cloud_label(),
                    {
                        let m = self.cloud.model.trim();
                        if m.is_empty() {
                            "gpt-4o-mini"
                        } else {
                            m
                        }
                    }
                )
            }
        };
        self.input.update(ctx, |input, ctx| {
            input.set_placeholder_text(placeholder, ctx)
        });
        ctx.notify();
    }

    /// Persist a typed key or model id onto the cloud config, then return to chat.
    fn save_cloud_field(&mut self, which: InputMode, value: String, ctx: &mut ViewContext<Self>) {
        let value = value.trim().to_string();
        match which {
            InputMode::CloudKey => self.cloud.api_key = value,
            InputMode::CloudModel => self.cloud.model = value,
            InputMode::Chat => {}
        }
        // Seed sensible defaults from the first preset if the user jumped straight
        // to pasting a key without picking a provider.
        if self.cloud.label.trim().is_empty() {
            if let Some((label, base, model)) = cloud_presets().first() {
                self.cloud.label = label.to_string();
                if self.cloud.base_url.trim().is_empty() {
                    self.cloud.base_url = base.to_string();
                }
                if self.cloud.model.trim().is_empty() {
                    self.cloud.model = model.to_string();
                }
            }
        }
        match save_cloud_config(&self.cloud) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(format!("Couldn't save cloud config: {e}")),
        }
        self.set_input_mode(InputMode::Chat, ctx);
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
        // BYOK: a selected cloud provider needs its key before it can be used.
        if self.cloud_active && !self.cloud.is_ready() {
            self.error =
                Some("Add your API key for the cloud provider first (🔑 Set key).".to_string());
            ctx.notify();
            return;
        }
        // Turbo serves whatever model llama-server already loaded, so the `model`
        // field is informational there — fall back to a placeholder when no model
        // is listed. Ollama and cloud providers need a real model name.
        let model = match self.current_model() {
            Some(model) => model,
            None if self.cloud_active => {
                self.error =
                    Some("Set a model for the cloud provider (the Model chip).".to_string());
                ctx.notify();
                return;
            }
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

        self.messages
            .push(ChatEntry::prose(ChatRole::User, prompt, context_label));

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
                // Thought / Tool / Command steps are UI-only; the model's real
                // tool calls + results live in `agent_messages` during a turn.
                ChatRole::Thought | ChatRole::Tool | ChatRole::Command => {}
            }
        }

        // A fresh generation: bump the turn id so any still-running callbacks
        // from a previous (e.g. just-stopped) turn are ignored.
        self.current_turn += 1;
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
        self.messages
            .push(ChatEntry::prose(ChatRole::Assistant, String::new(), None));
        let turn = self.current_turn;
        let stream = self.build_stream(&model, request);
        ctx.spawn_stream_local(
            stream,
            move |me, item, ctx| me.on_stream_item(turn, item, ctx),
            move |me, ctx| {
                // Stream ended without an explicit `[DONE]` — settle the UI.
                if turn == me.current_turn && me.in_flight {
                    me.in_flight = false;
                    ctx.notify();
                }
            },
        );
        ctx.notify();
    }

    // ── agent loop ─────────────────────────────────────────────────────────

    /// Run one agent step: stream the model's next message into a fresh
    /// "thought" bubble, then settle it in [`Self::on_agent_step_end`] (execute
    /// a tool and loop, or promote the thought into the final answer).
    fn run_agent_step(&mut self, ctx: &mut ViewContext<Self>) {
        self.agent_step_buffer.clear();
        self.messages.push(ChatEntry {
            role: ChatRole::Thought,
            text: String::new(),
            context_label: None,
            tool_title: None,
            command: None,
            collapsed: false,
            status: StepStatus::Running,
        });
        let turn = self.current_turn;
        let stream = self.build_stream(&self.agent_model, self.agent_messages.clone());
        ctx.spawn_stream_local(
            stream,
            move |me, item, ctx| me.on_agent_token(turn, item, ctx),
            move |me, ctx| me.on_agent_step_end(turn, ctx),
        );
    }

    fn on_agent_token(
        &mut self,
        turn: u64,
        item: Result<ChatStreamItem>,
        ctx: &mut ViewContext<Self>,
    ) {
        if turn != self.current_turn {
            return; // a stale turn (the user hit Stop, or started a new send)
        }
        match item {
            Ok(ChatStreamItem::Token(token)) => {
                self.agent_step_buffer.push_str(&token);
                // Show only the human-facing prose as it streams — never the raw
                // `<tool:…>` markup (that becomes a clean tool step once parsed).
                let visible = local_agent::strip_tool_calls(&self.agent_step_buffer);
                if let Some(last) = self.messages.last_mut() {
                    if last.role == ChatRole::Thought {
                        last.text = visible;
                    }
                }
                self.scroll_to_bottom();
                ctx.notify();
            }
            // The step is settled in `on_agent_step_end`.
            Ok(ChatStreamItem::Done) => {}
            Err(e) => self.finish_agent_with_error(turn, format!("Local model error: {e}"), ctx),
        }
    }

    fn on_agent_step_end(&mut self, turn: u64, ctx: &mut ViewContext<Self>) {
        if turn != self.current_turn || !self.in_flight {
            return; // already errored out, stopped, or superseded
        }
        let reply = self.agent_step_buffer.trim().to_string();
        self.agent_messages
            .push(ChatMessage::assistant(reply.clone()));

        match local_agent::parse_tool_call(&reply) {
            Some(tool) if self.agent_step < MAX_AGENT_STEPS => {
                // Keep any reasoning the model wrote before the tag as a collapsed
                // thought; drop the thought entirely if it was only the tag.
                self.finalize_thought(&reply, true);
                // Side-effecting tools pause for the user unless AUTO is on. The
                // Allow/Deny prompt renders above the input (see `render`), so we
                // don't add a transcript entry yet — just remember the tool.
                if tool.requires_approval() && !self.auto_approve {
                    self.pending_tool = Some(tool);
                    self.scroll_to_bottom();
                    ctx.notify();
                    return;
                }
                self.start_tool(tool, ctx);
            }
            _ => {
                // No tool call (or the step budget is spent): the reply is the
                // final answer. Promote the thought bubble into the answer.
                self.finalize_thought(&reply, false);
                if self.agent_step >= MAX_AGENT_STEPS {
                    if let Some(last) = self.messages.last_mut() {
                        if last.role == ChatRole::Assistant && last.text.trim().is_empty() {
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

    /// Settle the streaming thought bubble for a finished step. When the step
    /// ended in a tool call, keep the pre-tool reasoning as a collapsed thought
    /// (or drop it if empty). Otherwise the reply *is* the final answer, so the
    /// bubble becomes a normal (markdown) assistant message.
    fn finalize_thought(&mut self, reply: &str, had_tool: bool) {
        let Some(idx) = self
            .messages
            .iter()
            .rposition(|m| m.role == ChatRole::Thought)
        else {
            return;
        };
        if had_tool {
            let visible = local_agent::strip_tool_calls(reply);
            if visible.is_empty() {
                self.messages.remove(idx);
            } else {
                let entry = &mut self.messages[idx];
                entry.text = visible;
                entry.collapsed = true;
                entry.status = StepStatus::Ok;
            }
        } else {
            let entry = &mut self.messages[idx];
            entry.role = ChatRole::Assistant;
            entry.text = reply.to_string();
            entry.collapsed = false;
            entry.status = StepStatus::Ok;
        }
    }

    fn finish_agent_with_error(&mut self, turn: u64, message: String, ctx: &mut ViewContext<Self>) {
        if turn != self.current_turn {
            return;
        }
        self.in_flight = false;
        // Drop a trailing empty thought/assistant placeholder if nothing useful
        // streamed into it.
        if let Some(last) = self.messages.last() {
            if matches!(last.role, ChatRole::Assistant | ChatRole::Thought)
                && last.text.trim().is_empty()
            {
                self.messages.pop();
            }
        }
        self.error = Some(message);
        ctx.notify();
    }

    /// Begin running an (already-approved) tool: reads run inline; run_command
    /// spawns. The loop continues in [`Self::finish_tool`].
    fn start_tool(&mut self, tool: AgentTool, ctx: &mut ViewContext<Self>) {
        self.agent_tool_summary = tool.summary();
        self.agent_step += 1;
        let turn = self.current_turn;

        match &tool {
            AgentTool::RunCommand { command } => {
                // A terminal block: the command runs in the project root and its
                // output is shown like an integrated terminal (`$ cmd` + output).
                self.messages.push(ChatEntry {
                    role: ChatRole::Command,
                    text: String::new(),
                    context_label: None,
                    tool_title: None,
                    command: Some(command.clone()),
                    collapsed: false,
                    status: StepStatus::Running,
                });
                self.scroll_to_bottom();
                ctx.notify();

                let command = command.clone();
                let root = self.agent_root.clone();
                ctx.spawn(
                    async move {
                        match root {
                            Some(root) => local_agent::run_command(&root, &command).await,
                            None => "error: no project is open.".to_string(),
                        }
                    },
                    move |me, result, ctx| me.finish_tool(turn, "run_command", result, ctx),
                );
            }
            _ => {
                // A read tool: collapsed by default, showing just its one-line
                // summary; the result is one click away.
                self.messages.push(ChatEntry {
                    role: ChatRole::Tool,
                    text: String::new(),
                    context_label: None,
                    tool_title: Some(self.agent_tool_summary.clone()),
                    command: None,
                    collapsed: true,
                    status: StepStatus::Running,
                });
                let result = match self.agent_root.clone() {
                    Some(root) => local_agent::run_local_tool(&root, &tool),
                    None => "error: no project is open, so I can't read files.".to_string(),
                };
                self.finish_tool(turn, tool.name(), result, ctx);
            }
        }
    }

    /// Settle a finished tool: show its result, feed the full result back to the
    /// model, and take the next agent step.
    fn finish_tool(
        &mut self,
        turn: u64,
        tool_name: &str,
        result: String,
        ctx: &mut ViewContext<Self>,
    ) {
        if turn != self.current_turn {
            return; // the user stopped or started a new turn while this ran
        }
        let is_error = result.trim_start().starts_with("error");
        if let Some(last) = self.messages.last_mut() {
            match last.role {
                // The terminal block shows the full (already-capped) output.
                ChatRole::Command => last.text = result.clone(),
                ChatRole::Tool => last.text = Self::tool_preview(&result),
                _ => {}
            }
            last.status = if is_error {
                StepStatus::Error
            } else {
                StepStatus::Ok
            };
        }
        self.agent_messages.push(ChatMessage::user(format!(
            "TOOL RESULT ({tool_name}):\n{result}"
        )));
        self.scroll_to_bottom();
        ctx.notify();
        self.run_agent_step(ctx);
    }

    /// Interrupt the in-flight generation / agent loop. The detached streams
    /// can't be cancelled directly, so bump the turn id (their callbacks then
    /// no-op) and settle whatever was on screen.
    fn stop_turn(&mut self, ctx: &mut ViewContext<Self>) {
        if !self.in_flight {
            return;
        }
        self.current_turn += 1;
        self.in_flight = false;
        self.pending_tool = None;

        let trailing = self
            .messages
            .last()
            .map(|m| (m.role, m.text.trim().is_empty()));
        match trailing {
            Some((ChatRole::Thought, true)) | Some((ChatRole::Assistant, true)) => {
                self.messages.pop();
            }
            Some((ChatRole::Thought, false)) => {
                if let Some(last) = self.messages.last_mut() {
                    last.role = ChatRole::Assistant;
                    last.collapsed = false;
                    last.status = StepStatus::Ok;
                }
            }
            Some((ChatRole::Command, _)) | Some((ChatRole::Tool, _)) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.status == StepStatus::Running {
                        last.status = StepStatus::Denied;
                    }
                }
            }
            _ => {}
        }
        self.error = None;
        self.refresh_ai_mode();
        ctx.notify();
    }

    /// A short preview of a read-tool result for the collapsed step; the full
    /// result is what goes back to the model.
    fn tool_preview(result: &str) -> String {
        const MAX_LINES: usize = 8;
        const MAX_CHARS: usize = 600;
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

    fn on_stream_item(
        &mut self,
        turn: u64,
        item: Result<ChatStreamItem>,
        ctx: &mut ViewContext<Self>,
    ) {
        if turn != self.current_turn {
            return; // a stopped / superseded turn
        }
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
        self.styled_text(appearance, text, size, color, soft_wrap, false)
    }

    /// Like [`Self::label_text`] but in the monospace family — for tool output
    /// and terminal blocks, where columns and code need to line up.
    fn mono_text(
        &self,
        appearance: &Appearance,
        text: impl Into<String>,
        size: f32,
        color: ColorU,
        soft_wrap: bool,
    ) -> Box<dyn Element> {
        self.styled_text(appearance, text, size, color, soft_wrap, true)
    }

    fn styled_text(
        &self,
        appearance: &Appearance,
        text: impl Into<String>,
        size: f32,
        color: ColorU,
        soft_wrap: bool,
        monospace: bool,
    ) -> Box<dyn Element> {
        let family = if monospace {
            appearance.monospace_font_family()
        } else {
            appearance.ui_font_family()
        };
        appearance
            .ui_builder()
            .wrappable_text(text.into(), soft_wrap)
            .with_style(UiComponentStyles {
                font_family_id: Some(family),
                font_size: Some(size),
                font_color: Some(color),
                ..Default::default()
            })
            .build()
            .finish()
    }

    /// Render markdown (headings, lists, **bold**, `code`, fenced blocks) into a
    /// laid-out element so the assistant's replies read like a real AI IDE
    /// instead of showing raw `#`/`*`/backtick characters.
    fn markdown_text(
        &self,
        appearance: &Appearance,
        text: &str,
        color: ColorU,
    ) -> Box<dyn Element> {
        let formatted = parse_markdown(text).unwrap_or_else(|_| {
            FormattedText::new([FormattedTextLine::Line(vec![
                FormattedTextFragment::plain_text(text.to_string()),
            ])])
        });
        Box::new(
            FormattedTextElement::new(
                formatted,
                BODY_FONT_SIZE,
                appearance.ui_font_family(),
                appearance.monospace_font_family(),
                color,
                Default::default(),
            )
            // Behave like static text in the scroll view (no selection/hyperlink
            // handling stealing scroll or clicks).
            .disable_mouse_interaction(),
        )
    }

    fn chip(
        &self,
        appearance: &Appearance,
        label: String,
        action: LocalAiChatAction,
        active: bool,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        // Active / highlighted chips wear the Genesi green (a soft tinted fill +
        // green border + green text); the rest stay quiet so the panel reads as a
        // calm, on-brand control strip instead of a wall of grey buttons.
        let text_color: ColorU = if active {
            genesi_green()
        } else {
            theme.disabled_text_color(theme.background()).into()
        };
        let mut container =
            Container::new(self.label_text(appearance, label, CHIP_FONT_SIZE, text_color, false))
                .with_horizontal_padding(10.)
                .with_vertical_padding(4.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(7.)))
                .with_margin_right(6.)
                .with_margin_top(2.);
        container = if active {
            container
                .with_background_color(green_tint())
                .with_border(Border::all(1.).with_border_color(green_soft()))
        } else {
            container
                .with_background(theme.surface_2())
                .with_border(Border::all(1.).with_border_fill(theme.outline()))
        };
        let content = container.finish();

        EventHandler::new(content)
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    /// The top bar: brand mark + AI Mode status badge, and the utility chips
    /// (Stop while generating, Refresh, Clear). The model/Turbo/agent controls
    /// live in the bottom control strip (next to the compose box) instead, so
    /// the panel reads top-down like a real IDE assistant.
    fn render_header(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();

        let ai_mode_label = match &self.ai_mode {
            Some(state) => state.badge_text(),
            None => "AI Mode: n/a".to_string(),
        };

        let mut row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.label_text(
                appearance,
                "✦ Genesi AI",
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
            ));
        if self.in_flight {
            row.add_child(self.chip(
                appearance,
                "⏹ Stop".to_string(),
                LocalAiChatAction::Stop,
                true,
            ));
        }
        row.add_child(self.chip(appearance, "Refresh".to_string(), LocalAiChatAction::Refresh, true));
        row.add_child(self.chip(
            appearance,
            "Clear".to_string(),
            LocalAiChatAction::Clear,
            !self.messages.is_empty(),
        ));
        row.finish()
    }

    /// The bottom control strip, sitting just above the compose box like a real
    /// IDE: a click-to-open AI selector, the Turbo toggle next to it, and the
    /// agent toggles. The model picker itself is a popup (see
    /// [`Self::render_model_picker`]) so the user clicks once and chooses.
    fn render_control_strip(&self, appearance: &Appearance) -> Box<dyn Element> {
        let model_name = self.current_model().unwrap_or_else(|| "no model".to_string());
        let selector_label = if self.endpoint == LocalEndpoint::Turbo {
            format!("⚡ {model_name}  ⌄")
        } else {
            format!("✦ {model_name}  ⌄")
        };

        let mut row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.selector_button(
                appearance,
                selector_label,
                LocalAiChatAction::ToggleModelPicker,
                self.model_picker_open,
            ))
            .with_child(self.chip(
                appearance,
                "⚡ Turbo".to_string(),
                LocalAiChatAction::ToggleTurbo,
                self.endpoint == LocalEndpoint::Turbo,
            ))
            .with_child(self.chip(
                appearance,
                "🤖 Agent".to_string(),
                LocalAiChatAction::ToggleAgent,
                self.agent_mode,
            ));
        // AUTO only matters in agent mode (approve-less commands/edits).
        if self.agent_mode {
            row.add_child(self.chip(
                appearance,
                format!("AUTO {}", if self.auto_approve { "on" } else { "off" }),
                LocalAiChatAction::ToggleAuto,
                self.auto_approve,
            ));
        }
        row.add_child(self.chip(
            appearance,
            "📎".to_string(),
            LocalAiChatAction::ToggleAttachContext,
            self.attach_context,
        ));
        row.finish()
    }

    /// A dropdown-style button (surface fill + border + caret) used as the AI
    /// selector. Highlights green while its popup is open.
    fn selector_button(
        &self,
        appearance: &Appearance,
        label: String,
        action: LocalAiChatAction,
        open: bool,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let text_color: ColorU = theme.active_ui_text_color().into();
        let mut container =
            Container::new(self.label_text(appearance, label, BODY_FONT_SIZE, text_color, false))
                .with_horizontal_padding(10.)
                .with_vertical_padding(6.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
                .with_margin_right(6.)
                .with_margin_top(2.);
        container = if open {
            container
                .with_background_color(green_tint())
                .with_border(Border::all(1.).with_border_color(green_soft()))
        } else {
            container
                .with_background(theme.surface_2())
                .with_border(Border::all(1.).with_border_fill(theme.outline()))
        };

        EventHandler::new(container.finish())
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    /// The AI picker popup: a card listing the on-device models (click to pick),
    /// the Turbo accelerated endpoint, and a disabled "Cloud — coming soon" slot
    /// (BYOK provider selection is on the roadmap). Returns `None` when closed.
    fn render_model_picker(&self, appearance: &Appearance) -> Option<Box<dyn Element>> {
        if !self.model_picker_open {
            return None;
        }
        let theme = appearance.theme();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();

        let section = |me: &Self, text: &str| -> Box<dyn Element> {
            Container::new(me.label_text(appearance, text, CHIP_FONT_SIZE, muted, false))
                .with_horizontal_padding(8.)
                .with_padding_top(4.)
                .with_padding_bottom(2.)
                .finish()
        };

        let mut list = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        list.add_child(section(self, "On-device models"));

        if self.models.is_empty() {
            list.add_child(
                Container::new(self.label_text(
                    appearance,
                    "No models yet — start ollama, pull one (e.g. `ollama pull llama3.2`), then Refresh.",
                    BODY_FONT_SIZE,
                    muted,
                    true,
                ))
                .with_horizontal_padding(8.)
                .with_vertical_padding(6.)
                .finish(),
            );
        } else {
            for (index, model) in self.models.iter().enumerate() {
                let selected = !self.cloud_active
                    && self.endpoint == LocalEndpoint::Ollama
                    && self.selected_model == Some(index);
                let mark = if selected { "●  " } else { "○  " };
                list.add_child(self.picker_row(
                    appearance,
                    format!("{mark}{model}"),
                    LocalAiChatAction::PickModel(index),
                    selected,
                ));
            }
        }

        list.add_child(section(self, "Accelerated"));
        let turbo_selected = self.endpoint == LocalEndpoint::Turbo;
        let turbo_label = if self.turbo_available || turbo_selected {
            format!(
                "{}⚡ Turbo — full GPU offload",
                if turbo_selected { "●  " } else { "○  " }
            )
        } else {
            "○  ⚡ Turbo — not running".to_string()
        };
        list.add_child(self.picker_row(
            appearance,
            turbo_label,
            LocalAiChatAction::ToggleTurbo,
            turbo_selected,
        ));

        // Cloud (BYOK) is deferred to the roadmap; show the slot disabled so it's
        // discoverable without being wired up yet.
        list.add_child(
            Container::new(self.label_text(
                appearance,
                "☁  Cloud providers — coming soon",
                BODY_FONT_SIZE,
                muted,
                false,
            ))
            .with_horizontal_padding(8.)
            .with_vertical_padding(6.)
            .finish(),
        );

        let card = Container::new(list.finish())
            .with_uniform_padding(6.)
            .with_margin_bottom(6.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(10.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_1())
            .finish();
        Some(
            Container::new(card)
                .with_horizontal_padding(PANEL_PADDING)
                .finish(),
        )
    }

    /// One clickable row in the model picker: a full-width hit target that tints
    /// green when it's the active selection.
    fn picker_row(
        &self,
        appearance: &Appearance,
        label: String,
        action: LocalAiChatAction,
        active: bool,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let color: ColorU = if active {
            genesi_green()
        } else {
            theme.main_text_color(theme.background()).into()
        };
        let mut row = Container::new(self.label_text(appearance, label, BODY_FONT_SIZE, color, false))
            .with_horizontal_padding(8.)
            .with_vertical_padding(6.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)));
        if active {
            row = row.with_background_color(green_tint());
        }

        EventHandler::new(row.finish())
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    /// The approval prompt shown while a side-effecting tool waits for the user.
    fn render_approval(&self, appearance: &Appearance, tool: &AgentTool) -> Box<dyn Element> {
        let theme = appearance.theme();
        let clip = |s: &str| -> String {
            if s.chars().count() > 240 {
                s.chars().take(240).collect::<String>() + "…"
            } else {
                s.to_string()
            }
        };
        let (title, detail) = match tool {
            AgentTool::RunCommand { command } => {
                ("⚠ Allow this command?".to_string(), command.clone())
            }
            AgentTool::EditFile {
                path,
                search,
                replace,
            } => (
                format!("✏ Apply this edit to {path}?"),
                format!("- {}\n+ {}", clip(search), clip(replace)),
            ),
            AgentTool::WriteFile { path, content } => {
                (format!("✏ Write {path}?"), clip(content))
            }
            other => ("⚠ Allow this action?".to_string(), other.summary()),
        };

        let detail_box = Container::new(self.mono_text(
            appearance,
            detail,
            MONO_FONT_SIZE,
            theme.main_text_color(theme.background()).into(),
            true,
        ))
        .with_uniform_padding(8.)
        .with_margin_top(4.)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
        .with_border(Border::all(1.).with_border_fill(theme.outline()))
        .with_background(theme.surface_1())
        .finish();

        let buttons = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.chip(
                appearance,
                "Allow".to_string(),
                LocalAiChatAction::ApproveTool,
                true,
            ))
            .with_child(self.chip(
                appearance,
                "Deny".to_string(),
                LocalAiChatAction::DenyTool,
                true,
            ))
            .finish();

        Container::new(
            Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
                .with_child(self.label_text(
                    appearance,
                    title,
                    CHIP_FONT_SIZE,
                    genesi_green(),
                    false,
                ))
                .with_child(detail_box)
                .with_child(Container::new(buttons).with_margin_top(6.).finish())
                .finish(),
        )
        .with_horizontal_padding(PANEL_PADDING)
        .with_padding_bottom(6.)
        .finish()
    }

    /// A clickable, collapse/expand header for the thought/tool/command steps.
    fn step_header(
        &self,
        appearance: &Appearance,
        index: usize,
        title: String,
        color: ColorU,
        collapsed: bool,
    ) -> Box<dyn Element> {
        let caret = if collapsed { "▸" } else { "▾" };
        let row = Container::new(self.label_text(
            appearance,
            format!("{caret} {title}"),
            CHIP_FONT_SIZE,
            color,
            false,
        ))
        .with_vertical_padding(2.)
        .finish();

        EventHandler::new(row)
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(LocalAiChatAction::ToggleCollapse(index));
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    /// A user or assistant message bubble (assistant text rendered as markdown).
    fn render_bubble(&self, appearance: &Appearance, entry: &ChatEntry) -> Box<dyn Element> {
        let theme = appearance.theme();
        let is_user = entry.role == ChatRole::User;
        // A small "●" dot before the name reads as a tiny avatar — green for the
        // user, neutral for the assistant.
        let (prefix, role_color): (&str, ColorU) = if is_user {
            ("●  You", genesi_green())
        } else {
            ("●  Genesi AI", theme.active_ui_text_color().into())
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

        let body_color = theme.main_text_color(theme.background()).into();
        let body_el: Box<dyn Element> = if entry.text.trim().is_empty() {
            // Streaming placeholder before the first token lands.
            let dots = if self.in_flight { "…" } else { "" };
            self.label_text(appearance, dots, BODY_FONT_SIZE, body_color, true)
        } else if entry.role == ChatRole::Assistant {
            self.markdown_text(appearance, &entry.text, body_color)
        } else {
            self.label_text(
                appearance,
                entry.text.clone(),
                BODY_FONT_SIZE,
                body_color,
                true,
            )
        };

        let inner = inner
            .with_child(Container::new(body_el).with_margin_top(4.).finish())
            .finish();

        let bubble = Container::new(inner)
            .with_uniform_padding(11.)
            .with_margin_bottom(9.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(10.)));
        // The user's turns read as a soft Genesi-green card with a green left
        // accent; the assistant's sit on the calmer surface.
        let bubble = if is_user {
            bubble
                .with_background_color(green_tint())
                .with_border(Border::left(3.).with_border_color(genesi_green()))
        } else {
            bubble.with_background(theme.surface_1())
        };
        bubble.finish()
    }

    /// A collapsible "💭 Thought" — the model's reasoning for a step, kept out of
    /// the way so the transcript stays clean.
    fn render_thought(
        &self,
        appearance: &Appearance,
        index: usize,
        entry: &ChatEntry,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();
        let title = if entry.status == StepStatus::Running {
            "💭 Thinking…".to_string()
        } else {
            "💭 Thought".to_string()
        };

        let mut column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.step_header(appearance, index, title, muted, entry.collapsed));

        if !entry.collapsed && !entry.text.trim().is_empty() {
            column.add_child(
                Container::new(self.label_text(
                    appearance,
                    entry.text.clone(),
                    BODY_FONT_SIZE,
                    muted,
                    true,
                ))
                .with_horizontal_padding(10.)
                .with_margin_top(2.)
                .finish(),
            );
        }

        Container::new(column.finish())
            .with_margin_bottom(6.)
            .finish()
    }

    /// A collapsible read-tool step: a one-line summary with the (previewed)
    /// result one click away.
    fn render_tool_step(
        &self,
        appearance: &Appearance,
        index: usize,
        entry: &ChatEntry,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();
        let (icon, color): (&str, ColorU) = match entry.status {
            StepStatus::Running => ("📄", muted),
            StepStatus::Ok => ("📄", genesi_green()),
            StepStatus::Error => ("⚠", theme.ui_error_color().into()),
            StepStatus::Denied => ("🚫", muted),
        };
        let suffix = match entry.status {
            StepStatus::Running => " · running…",
            StepStatus::Error => " · error",
            StepStatus::Denied => " · denied",
            StepStatus::Ok => "",
        };
        let title = format!(
            "{icon} {}{suffix}",
            entry.tool_title.clone().unwrap_or_default()
        );

        let mut column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.step_header(appearance, index, title, color, entry.collapsed));

        if !entry.collapsed && !entry.text.trim().is_empty() {
            let body = Container::new(self.mono_text(
                appearance,
                entry.text.clone(),
                MONO_FONT_SIZE,
                theme.main_text_color(theme.background()).into(),
                true,
            ))
            .with_uniform_padding(8.)
            .with_margin_top(2.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_1())
            .finish();
            column.add_child(body);
        }

        Container::new(column.finish())
            .with_margin_bottom(6.)
            .finish()
    }

    /// A `run_command` step, rendered like the app's integrated terminal: a dark
    /// block with a green `$ command` prompt and the captured output beneath.
    fn render_command_step(
        &self,
        appearance: &Appearance,
        index: usize,
        entry: &ChatEntry,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let green: ColorU = theme.terminal_colors().normal.green.into();
        let suffix = match entry.status {
            StepStatus::Running => " · running…",
            StepStatus::Error => " · exited with error",
            StepStatus::Denied => " · stopped",
            StepStatus::Ok => "",
        };
        let header_color: ColorU = if entry.status == StepStatus::Error {
            theme.ui_error_color().into()
        } else {
            green
        };

        let mut column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.step_header(
                appearance,
                index,
                format!("⌘ Terminal{suffix}"),
                header_color,
                entry.collapsed,
            ));

        if !entry.collapsed {
            let command = entry.command.clone().unwrap_or_default();
            let mut terminal = Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
                .with_child(self.mono_text(
                    appearance,
                    format!("$ {command}"),
                    MONO_FONT_SIZE,
                    green,
                    true,
                ));
            let body = if entry.text.trim().is_empty() && entry.status == StepStatus::Running {
                "running…".to_string()
            } else {
                entry.text.clone()
            };
            if !body.trim().is_empty() {
                terminal.add_child(
                    Container::new(self.mono_text(
                        appearance,
                        body,
                        MONO_FONT_SIZE,
                        theme.main_text_color(theme.background()).into(),
                        true,
                    ))
                    .with_margin_top(4.)
                    .finish(),
                );
            }
            let block = Container::new(terminal.finish())
                .with_uniform_padding(8.)
                .with_margin_top(2.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
                .with_border(Border::all(1.).with_border_fill(theme.outline()))
                .with_background(theme.background())
                .finish();
            column.add_child(block);
        }

        Container::new(column.finish())
            .with_margin_bottom(6.)
            .finish()
    }

    fn render_entry(
        &self,
        appearance: &Appearance,
        index: usize,
        entry: &ChatEntry,
    ) -> Box<dyn Element> {
        match entry.role {
            ChatRole::User | ChatRole::Assistant => self.render_bubble(appearance, entry),
            ChatRole::Thought => self.render_thought(appearance, index, entry),
            ChatRole::Tool => self.render_tool_step(appearance, index, entry),
            ChatRole::Command => self.render_command_step(appearance, index, entry),
        }
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
            for (index, entry) in self.messages.iter().enumerate() {
                column.add_child(self.render_entry(appearance, index, entry));
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
                // A deliberate choice — stop the Turbo auto-default from
                // overriding it on the next probe. Cycle: Local -> Turbo (if up)
                // -> Cloud (BYOK) -> Local.
                self.endpoint_user_chosen = true;
                self.error = None;
                if self.cloud_active {
                    self.cloud_active = false;
                    self.endpoint = LocalEndpoint::Ollama;
                    self.refresh_models(ctx);
                } else if self.endpoint == LocalEndpoint::Ollama && self.turbo_available {
                    self.endpoint = LocalEndpoint::Turbo;
                    self.refresh_models(ctx);
                } else {
                    // From Turbo, or from Local when Turbo is down -> Cloud.
                    // Reset the local endpoint so leaving cloud returns to Local.
                    self.endpoint = LocalEndpoint::Ollama;
                    self.cloud_active = true;
                    if !self.cloud.is_ready() {
                        self.error = Some(format!(
                            "Pick a provider and add your key (🔑 Set key) to use {}.",
                            self.cloud_label()
                        ));
                    }
                }
                ctx.notify();
            }
            LocalAiChatAction::CycleProvider => {
                let presets = cloud_presets();
                let current = presets
                    .iter()
                    .position(|(label, _, _)| *label == self.cloud.label);
                let next = current.map(|i| (i + 1) % presets.len()).unwrap_or(0);
                let (label, base_url, default_model) = presets[next];
                // Replace the model only if it was empty or still the old preset's
                // default — never clobber a model the user typed by hand.
                let replace_model = self.cloud.model.trim().is_empty()
                    || current.is_some_and(|i| self.cloud.model == presets[i].2);
                self.cloud.label = label.to_string();
                self.cloud.base_url = base_url.to_string();
                if replace_model {
                    self.cloud.model = default_model.to_string();
                }
                let _ = save_cloud_config(&self.cloud);
                self.error = None;
                ctx.notify();
            }
            LocalAiChatAction::SetKey => self.set_input_mode(InputMode::CloudKey, ctx),
            LocalAiChatAction::SetModel => self.set_input_mode(InputMode::CloudModel, ctx),
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
                self.pending_tool = None;
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
            LocalAiChatAction::ToggleAuto => {
                self.auto_approve = !self.auto_approve;
                ctx.notify();
            }
            LocalAiChatAction::ToggleModelPicker => {
                self.model_picker_open = !self.model_picker_open;
                ctx.notify();
            }
            LocalAiChatAction::PickModel(index) => {
                // A deliberate local-model choice: pin the local (ollama)
                // endpoint and stop the Turbo auto-default from overriding it.
                self.endpoint_user_chosen = true;
                self.cloud_active = false;
                self.endpoint = LocalEndpoint::Ollama;
                if *index < self.models.len() {
                    self.selected_model = Some(*index);
                }
                self.model_picker_open = false;
                self.error = None;
                ctx.notify();
            }
            LocalAiChatAction::ToggleTurbo => {
                self.endpoint_user_chosen = true;
                self.cloud_active = false;
                self.error = None;
                if self.endpoint == LocalEndpoint::Turbo {
                    // Turn Turbo off -> back to the local ollama endpoint.
                    self.endpoint = LocalEndpoint::Ollama;
                } else {
                    // Turn Turbo on. refresh_models re-probes /health, so a stale
                    // turbo_available can't pin us to a dead endpoint for long.
                    self.endpoint = LocalEndpoint::Turbo;
                    if !self.turbo_available {
                        self.error = Some(
                            "Turbo (genesi-ai-turbo) isn't up yet — start it and it'll be used \
                             automatically once /health responds."
                                .to_string(),
                        );
                    }
                }
                self.model_picker_open = false;
                self.refresh_models(ctx);
                ctx.notify();
            }
            LocalAiChatAction::ApproveTool => {
                if let Some(tool) = self.pending_tool.take() {
                    self.start_tool(tool, ctx);
                }
                ctx.notify();
            }
            LocalAiChatAction::DenyTool => {
                if let Some(tool) = self.pending_tool.take() {
                    let name = tool.name();
                    let denied = ChatEntry {
                        role: match tool {
                            AgentTool::RunCommand { .. } => ChatRole::Command,
                            _ => ChatRole::Tool,
                        },
                        text: "(denied by user)".to_string(),
                        context_label: None,
                        tool_title: Some(tool.summary()),
                        command: match &tool {
                            AgentTool::RunCommand { command } => Some(command.clone()),
                            _ => None,
                        },
                        collapsed: false,
                        status: StepStatus::Denied,
                    };
                    self.messages.push(denied);
                    // Tell the model the user declined so it can adapt or answer.
                    self.agent_messages.push(ChatMessage::user(format!(
                        "TOOL RESULT ({name}):\nThe user denied this action."
                    )));
                    self.agent_step += 1;
                    self.run_agent_step(ctx);
                }
                ctx.notify();
            }
            LocalAiChatAction::Stop => {
                self.stop_turn(ctx);
            }
            LocalAiChatAction::ToggleCollapse(index) => {
                if let Some(entry) = self.messages.get_mut(*index) {
                    entry.collapsed = !entry.collapsed;
                }
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

        // A pending command waits for the user's Allow/Deny above the input.
        if let Some(tool) = &self.pending_tool {
            root.add_child(self.render_approval(appearance, tool));
        }

        // AI model picker popup — opens ABOVE the compose box (IDE-style): the
        // user clicks the selector that lives inside the box and the list pops
        // up over it.
        if let Some(picker) = self.render_model_picker(appearance) {
            root.add_child(picker);
        }

        // The compose box: a single full-width, bordered, rounded container that
        // holds the prompt input on top and the AI controls (selector + Turbo +
        // agent toggles) along its bottom edge — like a real IDE assistant, with
        // everything in one surface instead of loose chips above a bare input.
        let compose_inner = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(
                Container::new(ChildView::new(&self.input).finish())
                    .with_horizontal_padding(4.)
                    .with_vertical_padding(2.)
                    .finish(),
            )
            .with_child(
                Container::new(self.render_control_strip(appearance))
                    .with_padding_top(6.)
                    .finish(),
            )
            .finish();
        let compose_box = Container::new(compose_inner)
            .with_horizontal_padding(8.)
            .with_vertical_padding(8.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(12.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_1())
            .finish();
        root.add_child(
            Container::new(compose_box)
                .with_horizontal_padding(PANEL_PADDING)
                .with_padding_top(4.)
                .with_padding_bottom(PANEL_PADDING)
                .finish(),
        );

        root.finish()
    }
}
