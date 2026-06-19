//! Login-free local AI chat panel.
//!
//! A small side panel that talks to a local OpenAI-compatible endpoint via
//! [`super::local_chat`] — ollama on `:11434` ("Local") or `genesi-ai-turbo` on
//! `:11435` ("Turbo"). It also surfaces, and lets you drive, `genesi-ai-mode`'s
//! AI Mode (the daemon that tunes the box for inference) so the whole local-AI
//! story lives in one place: no account, no cloud.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use markdown_parser::{parse_markdown, FormattedText, FormattedTextFragment, FormattedTextLine};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, TextDiff};
use warpui::clipboard::ClipboardContent;
use warpui::color::ColorU;
use warpui::elements::{
    Border, ClippedScrollStateHandle, ClippedScrollable, ConstrainedBox, Container, CornerRadius,
    CrossAxisAlignment, DispatchEventResult, Element, Empty, EventHandler, Expanded, Fill, Flex,
    FormattedTextElement, Icon, MainAxisAlignment, MainAxisSize, ParentElement, Radius,
    ScrollbarWidth, SelectableArea, SelectionHandle, Shrinkable,
};
use warpui::keymap::FixedBinding;
use warpui::presenter::ChildView;
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::units::Pixels;
use warpui::{
    AppContext, Entity, FocusContext, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};
use warpui_extras::secure_storage::AppContextExt as _;

use super::local_agent::{self, AgentTool, MAX_AGENT_STEPS};
use super::local_chat::{
    cloud_presets, list_models, load_cloud_config, load_legacy_cloud_key, read_ai_mode_state,
    save_cloud_config, set_ai_mode_force, stream_chat, stream_chat_cloud, turbo_health_ok,
    AiModeState, ChatMessage, ChatStreamItem, CloudConfig, CloudKeyStore, CloudProviderKind,
    CodeContext, LocalEndpoint, CLOUD_KEYS_STORAGE_KEY,
};
use crate::appearance::Appearance;
use crate::code::local_code_editor::LocalCodeEditorView;
use crate::settings_view::SettingsSection;
use crate::util::bindings::CustomAction;
use crate::view_components::{SubmittableTextInput, SubmittableTextInputEvent};
use crate::workspace::WorkspaceAction;

const TITLE_FONT_SIZE: f32 = 15.;
const CHIP_FONT_SIZE: f32 = 11.;
const BODY_FONT_SIZE: f32 = 13.;
/// Slightly smaller monospace size for tool output / terminal blocks.
const MONO_FONT_SIZE: f32 = 12.;
const PANEL_PADDING: f32 = 8.;
const MODEL_LABEL_MAX_CHARS: usize = 34;
const VIBE_COLUMN_WIDTH: f32 = 760.;
const MEMPALACE_STORAGE_KEY: &str = "GenesiCodeMempalaceV1";
/// A large scroll target; `ClippedScrollable::after_layout` clamps it to the
/// real bottom, so this reliably pins the transcript to the latest message.
const SCROLL_TO_BOTTOM: f32 = 1.0e7;

const SYSTEM_PROMPT: &str =
    "You are a helpful AI assistant running locally on Genesi OS. Be concise.";

pub fn init(app: &mut AppContext) {
    use warpui::keymap::macros::*;

    app.register_fixed_bindings([FixedBinding::custom(
        CustomAction::Copy,
        LocalAiChatAction::CopySelectedText,
        "Copy",
        id!(LocalAiChatView::ui_name()) & !id!("IMEOpen"),
    )]);
}

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

fn genesi_panel_surface() -> ColorU {
    ColorU::new(20, 21, 23, 245)
}

fn genesi_card_surface() -> ColorU {
    ColorU::new(31, 32, 35, 245)
}

fn genesi_subtle_border() -> ColorU {
    ColorU::new(255, 255, 255, 24)
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let len = value.chars().count();
    if len <= max_chars {
        return value.to_string();
    }

    let head_len = (max_chars.saturating_sub(3) + 1) / 2;
    let tail_len = max_chars.saturating_sub(3 + head_len);
    let head: String = value.chars().take(head_len).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

fn ai_mode_short_label(state: Option<&AiModeState>) -> String {
    match state {
        Some(state) => format!("Mode {}", state.force_mode),
        None => "Mode n/a".to_string(),
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum StepStatus {
    Running,
    Ok,
    Error,
    Denied,
}

/// One line in the transcript. The assistant's text grows as tokens stream in.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// For a file write/edit Tool step: `(added, removed)` line counts, rendered
    /// as a `+N −N` diff card. `None` for every other step.
    diff_stat: Option<(u32, u32)>,
    diff_preview: Option<Vec<DiffPreviewLine>>,
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
            diff_stat: None,
            diff_preview: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum DiffPreviewLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiffPreviewLine {
    old_line: Option<usize>,
    new_line: Option<usize>,
    text: String,
    kind: DiffPreviewLineKind,
}

/// A file the agent created or edited this turn, tracked so the review bar can
/// summarize the changes and undo them (restore the captured original, or delete
/// a file that didn't exist before).
struct PendingEdit {
    /// Project-relative path.
    path: String,
    added: u32,
    removed: u32,
    /// The file's content before the edit, or `None` if the file was created.
    original: Option<String>,
    diff_preview: Vec<DiffPreviewLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedLocalChat {
    id: String,
    #[serde(default)]
    messages: Vec<ChatEntry>,
    #[serde(default)]
    agent_messages: Vec<ChatMessage>,
    #[serde(default)]
    updated_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MempalaceState {
    #[serde(default)]
    active_chat_id: String,
    #[serde(default)]
    chats: Vec<PersistedLocalChat>,
}

/// Count lines for a diff stat: 0 for empty, else the number of lines (a
/// trailing newline doesn't add a phantom empty line).
fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        s.lines().count() as u32
    }
}

fn build_diff_preview(original: &str, current: &str) -> Vec<DiffPreviewLine> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_lines(original, current);

    let mut old_line = 1usize;
    let mut new_line = 1usize;
    let mut lines = Vec::new();

    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Equal => DiffPreviewLineKind::Context,
            ChangeTag::Delete => DiffPreviewLineKind::Removed,
            ChangeTag::Insert => DiffPreviewLineKind::Added,
        };

        for raw_line in change.value().lines() {
            let (old_number, new_number) = match kind {
                DiffPreviewLineKind::Context => {
                    let nums = (Some(old_line), Some(new_line));
                    old_line += 1;
                    new_line += 1;
                    nums
                }
                DiffPreviewLineKind::Removed => {
                    let nums = (Some(old_line), None);
                    old_line += 1;
                    nums
                }
                DiffPreviewLineKind::Added => {
                    let nums = (None, Some(new_line));
                    new_line += 1;
                    nums
                }
            };

            lines.push(DiffPreviewLine {
                old_line: old_number,
                new_line: new_number,
                text: raw_line.to_string(),
                kind: kind.clone(),
            });
        }
    }

    lines
}

/// Events emitted to the workspace.
pub enum LocalAiChatEvent {
    /// Close the panel.
    ClosePanel,
    /// Open the app's native code review / diff panel for the active repo.
    OpenDiff,
    /// The user submitted a prompt; the workspace attaches fresh file context
    /// and calls back into [`LocalAiChatView::send_with_context`]. Routing this
    /// through the workspace is what gives the panel workspace awareness.
    SubmitPrompt(String),
    /// The persisted chat list or active chat changed.
    StateChanged,
}

#[derive(Debug, Clone)]
pub struct LocalChatSummary {
    pub id: String,
    pub title: String,
    pub is_active: bool,
}

/// Click actions dispatched by the header chips.
#[derive(Debug, Clone)]
pub enum LocalAiChatAction {
    /// Copy the text currently selected in the transcript.
    CopySelectedText,
    /// Move keyboard focus from the prompt editor to the transcript selection.
    FocusTranscript,
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
    /// Accept the agent's file changes and dismiss the review bar.
    KeepEdits,
    /// Undo the agent's file changes (restore each captured original).
    UndoEdits,
    /// Open the workspace diff / review panel for a transcript file card.
    OpenDiff(usize),
    /// Expand/collapse the modified-files review browser.
    ToggleReviewExpanded,
    /// Focus a single file inside the expanded review browser.
    SelectReviewFile(String),
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
    /// Select a specific cloud provider from the picker.
    SelectCloudProvider(CloudProviderKind),
    /// Pick a suggested cloud model for the active provider.
    PickCloudModel(String),
    /// Capture the cloud provider's API key in the compose box.
    SetKey,
    /// Capture the cloud provider's model id in the compose box.
    SetModel,
    /// Clear the active conversation and start a fresh one.
    NewChat,
    /// Submit whatever is currently typed into the compose input.
    SubmitPromptInput,
}

pub struct LocalAiChatView {
    input: ViewHandle<SubmittableTextInput>,
    messages: Vec<ChatEntry>,
    active_chat_id: String,
    chats: Vec<PersistedLocalChat>,
    transcript_scroll: ClippedScrollStateHandle,
    transcript_selection: SelectionHandle,
    selected_transcript_text: Arc<RwLock<Option<String>>>,
    review_sidebar_scroll: ClippedScrollStateHandle,
    vibe_mode: bool,

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
    /// The user's saved cloud provider (metadata only; keys live in secure storage).
    cloud: CloudConfig,
    /// Per-provider API keys stored in platform secure storage and mirrored in memory.
    cloud_keys: CloudKeyStore,
    /// When on, prompts go to the cloud provider instead of Local/Turbo.
    cloud_active: bool,
    /// What the compose box's next submit means (a prompt, or a key/model value).
    input_mode: InputMode,

    /// Whether the click-to-open AI model picker (the popup above the compose
    /// box) is currently expanded.
    model_picker_open: bool,

    /// Files the agent created/edited this turn, for the review bar (Undo all /
    /// Keep) — each carries its captured pre-edit original so Undo can restore it.
    pending_edits: Vec<PendingEdit>,
    review_expanded: bool,
    selected_review_path: Option<String>,
}

impl LocalAiChatView {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let input = ctx.add_typed_action_view(|ctx| SubmittableTextInput::new(ctx));
        input.update(ctx, |input, ctx| {
            input.set_placeholder_text(" Ask the local model...", ctx);
            input.set_outer_margins(0., 0., ctx);
            input.set_submit_button_visible(false, ctx);
        });
        ctx.subscribe_to_view(&input, |me, _, event, ctx| {
            me.handle_input_event(event, ctx);
        });

        let mut view = Self {
            input,
            messages: Vec::new(),
            active_chat_id: String::new(),
            chats: Vec::new(),
            transcript_scroll: ClippedScrollStateHandle::default(),
            transcript_selection: SelectionHandle::default(),
            selected_transcript_text: Arc::new(RwLock::new(None)),
            review_sidebar_scroll: ClippedScrollStateHandle::default(),
            vibe_mode: false,
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
            agent_mode: true,
            agent_root: None,
            agent_messages: Vec::new(),
            agent_model: String::new(),
            agent_step: 0,
            agent_step_buffer: String::new(),
            agent_tool_summary: String::new(),
            auto_approve: false,
            pending_tool: None,
            cloud: load_cloud_config().unwrap_or_default().with_defaults(),
            cloud_keys: CloudKeyStore::default(),
            cloud_active: false,
            input_mode: InputMode::Chat,
            model_picker_open: false,
            pending_edits: Vec::new(),
            review_expanded: false,
            selected_review_path: None,
        };
        view.load_cloud_keys(ctx);
        view.load_mempalace(ctx);
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
        &mut self,
        model: &str,
        messages: Vec<ChatMessage>,
        ctx: &mut ViewContext<Self>,
    ) -> futures::stream::BoxStream<'static, Result<ChatStreamItem>> {
        self.load_cloud_keys(ctx);
        if self.cloud_active && self.cloud_ready() {
            stream_chat_cloud(
                self.cloud.provider,
                model,
                self.active_cloud_key().to_string(),
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
        self.cloud.provider.label().to_string()
    }

    fn active_cloud_key(&self) -> &str {
        self.cloud_keys.get(self.cloud.provider)
    }

    fn cloud_ready(&self) -> bool {
        self.cloud.is_ready(self.active_cloud_key())
    }

    fn active_backend_error_label(&self) -> String {
        if self.cloud_active {
            format!("{} error", self.cloud.provider.label())
        } else {
            "Local model error".to_string()
        }
    }

    fn save_cloud_keys(&self, ctx: &mut ViewContext<Self>) -> Result<()> {
        let json = serde_json::to_string(&self.cloud_keys)?;
        ctx.secure_storage()
            .write_value_with_owner_only_fallback(CLOUD_KEYS_STORAGE_KEY, &json)
            .map_err(|e| anyhow::anyhow!("failed to write cloud keys: {e}"))
    }

    fn load_cloud_keys(&mut self, ctx: &mut ViewContext<Self>) {
        match ctx.secure_storage().read_value(CLOUD_KEYS_STORAGE_KEY) {
            Ok(json) => match serde_json::from_str::<CloudKeyStore>(&json) {
                Ok(keys) => self.cloud_keys = keys,
                Err(err) => {
                    log::warn!("Failed to deserialize cloud keys from secure storage: {err:#}");
                }
            },
            Err(err) => {
                if !matches!(err, warpui_extras::secure_storage::Error::NotFound) {
                    log::warn!("Failed to read cloud keys from secure storage: {err:#}");
                }
            }
        }

        if let Some((provider, key)) = load_legacy_cloud_key() {
            let existing = self.cloud_keys.get(provider).trim();
            if existing.is_empty() && !key.trim().is_empty() {
                self.cloud_keys.set(provider, key);
                if let Err(err) = self.save_cloud_keys(ctx) {
                    log::warn!("Failed to migrate legacy cloud key to secure storage: {err:#}");
                }
                let _ = save_cloud_config(&self.cloud);
            }
        }
    }

    fn select_cloud_provider(
        &mut self,
        provider: CloudProviderKind,
        close_picker: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        self.load_cloud_keys(ctx);
        self.endpoint_user_chosen = true;
        self.cloud_active = true;
        self.endpoint = LocalEndpoint::Ollama;
        let old_provider = self.cloud.provider;
        let old_default = old_provider.default_model();
        self.cloud.provider = provider;
        if self.cloud.model.trim().is_empty() || self.cloud.model == old_default {
            self.cloud.model = provider.default_model().to_string();
        }
        if close_picker {
            self.model_picker_open = false;
        }
        if let Err(err) = save_cloud_config(&self.cloud) {
            self.error = Some(format!("Couldn't save cloud config: {err}"));
        } else if !self.cloud_ready() {
            self.error = Some(format!(
                "Add your {} API key in Settings to use {}.",
                self.cloud.provider.label(),
                self.cloud.model
            ));
        } else {
            self.error = None;
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
                            self.cloud.provider.default_model()
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
            InputMode::CloudKey => self.cloud_keys.set(self.cloud.provider, value),
            InputMode::CloudModel => self.cloud.model = value,
            InputMode::Chat => {}
        }
        let save_result = match which {
            InputMode::CloudKey => self.save_cloud_keys(ctx),
            InputMode::CloudModel => save_cloud_config(&self.cloud),
            InputMode::Chat => Ok(()),
        };
        match save_result {
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
        self.load_cloud_keys(ctx);
        // BYOK: a selected cloud provider needs its key before it can be used.
        if self.cloud_active && !self.cloud_ready() {
            self.error = Some(format!(
                "Add your {} API key in Genesi AI settings first.",
                self.cloud.provider.label()
            ));
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
        self.persist_mempalace(ctx, true);

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
        let stream = self.build_stream(&model, request, ctx);
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
            diff_stat: None,
            diff_preview: None,
        });
        let turn = self.current_turn;
        let agent_model = self.agent_model.clone();
        let agent_messages = self.agent_messages.clone();
        let stream = self.build_stream(&agent_model, agent_messages, ctx);
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
            Err(e) => self.finish_agent_with_error(
                turn,
                format!("{}: {e}", self.active_backend_error_label()),
                ctx,
            ),
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
                self.persist_mempalace(ctx, false);
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
        self.persist_mempalace(ctx, false);
        ctx.notify();
    }

    /// Snapshot a write/edit *before* it runs: `(path, added, removed,
    /// original_content)`. Returns `None` for read tools. Best-effort — the line
    /// stats drive the `+N −N` diff card, the original drives Undo.
    fn capture_edit(&self, tool: &AgentTool) -> Option<(String, u32, u32, Option<String>)> {
        let root = self.agent_root.as_ref()?;
        let read_original = |path: &str| {
            local_agent::resolve_in_project(root, path)
                .and_then(|p| std::fs::read_to_string(p).ok())
        };
        match tool {
            AgentTool::WriteFile { path, content } => {
                let original = read_original(path);
                let removed = original.as_deref().map(count_lines).unwrap_or(0);
                Some((path.clone(), count_lines(content), removed, original))
            }
            AgentTool::EditFile {
                path,
                search,
                replace,
            } => {
                let original = read_original(path);
                Some((
                    path.clone(),
                    count_lines(replace),
                    count_lines(search),
                    original,
                ))
            }
            _ => None,
        }
    }

    fn read_project_file_after_edit(&self, path: &str) -> Option<String> {
        let root = self.agent_root.as_ref()?;
        local_agent::resolve_in_project(root, path).and_then(|p| std::fs::read_to_string(p).ok())
    }

    /// Record a successful edit for the review bar. Re-editing the same file keeps
    /// the EARLIEST original (so Undo restores the pre-turn state) and refreshes
    /// the displayed stats.
    fn record_edit(
        &mut self,
        path: String,
        added: u32,
        removed: u32,
        original: Option<String>,
        diff_preview: Vec<DiffPreviewLine>,
        ctx: &mut ViewContext<Self>,
    ) {
        self.update_open_editor_diff(&path, Some(original.as_deref().unwrap_or("")), ctx);

        if let Some(existing) = self.pending_edits.iter_mut().find(|e| e.path == path) {
            existing.added = added;
            existing.removed = removed;
            existing.diff_preview = diff_preview;
        } else {
            self.pending_edits.push(PendingEdit {
                path,
                added,
                removed,
                original,
                diff_preview,
            });
        }

        if self.selected_review_path.is_none() {
            self.selected_review_path = self.pending_edits.first().map(|edit| edit.path.clone());
        }
    }

    fn update_open_editor_diff(
        &self,
        relative_path: &str,
        original: Option<&str>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(root) = self.agent_root.as_ref() else {
            return;
        };
        let Some(target) = local_agent::resolve_in_project(root, relative_path) else {
            return;
        };
        let Some(window_id) = ctx.windows().active_window() else {
            return;
        };
        let Some(editors) = ctx.views_of_type::<LocalCodeEditorView>(window_id) else {
            return;
        };

        for editor in editors {
            let matches = editor.as_ref(ctx).file_path().is_some_and(|path| {
                path == target
                    || path
                        .canonicalize()
                        .ok()
                        .zip(target.canonicalize().ok())
                        .is_some_and(|(path, target)| path == target)
            });
            if !matches {
                continue;
            }
            editor.update(ctx, |editor, ctx| match original {
                Some(original) => editor.show_pending_agent_diff(original, ctx),
                None => editor.keep_pending_agent_diff(ctx),
            });
        }
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
                    diff_stat: None,
                    diff_preview: None,
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
                // For a write/edit, snapshot the file first so we can show a
                // `+N −N` diff card and later undo it. Reads return None here.
                let edit_meta = self.capture_edit(&tool);
                let (title, diff_stat) = match &edit_meta {
                    Some((path, added, removed, _)) => (path.clone(), Some((*added, *removed))),
                    None => (self.agent_tool_summary.clone(), None),
                };
                // A read tool: collapsed by default, showing just its one-line
                // summary; an edit shows the file card. The detail is one click away.
                self.messages.push(ChatEntry {
                    role: ChatRole::Tool,
                    text: String::new(),
                    context_label: None,
                    tool_title: Some(title),
                    command: None,
                    collapsed: true,
                    status: StepStatus::Running,
                    diff_stat,
                    diff_preview: None,
                });
                let result = match self.agent_root.clone() {
                    Some(root) => local_agent::run_local_tool(&root, &tool),
                    None => "error: no project is open, so I can't read files.".to_string(),
                };
                // Record a successful edit for the review bar (Undo all / Keep).
                if let Some((path, added, removed, original)) = edit_meta {
                    if !result.trim_start().starts_with("error") {
                        let current = self.read_project_file_after_edit(&path).unwrap_or_default();
                        let diff_preview =
                            build_diff_preview(original.as_deref().unwrap_or(""), &current);
                        self.record_edit(
                            path.clone(),
                            added,
                            removed,
                            original,
                            diff_preview.clone(),
                            ctx,
                        );
                        if let Some(last) = self.messages.last_mut() {
                            last.diff_preview = Some(diff_preview);
                        }
                    }
                }
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
        self.persist_mempalace(ctx, false);
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
        self.persist_mempalace(ctx, false);
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
                self.persist_mempalace(ctx, false);
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
                self.error = Some(format!("{}: {e}", self.active_backend_error_label()));
                self.persist_mempalace(ctx, false);
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
            .set_selectable(true),
        )
    }

    fn chip_content(
        &self,
        appearance: &Appearance,
        label: String,
        icon_path: Option<&'static str>,
        selected: bool,
        enabled: bool,
    ) -> Box<dyn Element> {
        let text_color: ColorU = if enabled {
            if selected {
                ColorU::new(215, 248, 234, 255)
            } else {
                ColorU::new(228, 231, 236, 255)
            }
        } else {
            ColorU::new(132, 137, 145, 255)
        };
        let background = if selected {
            ColorU::new(15, 143, 106, 44)
        } else if enabled {
            ColorU::new(255, 255, 255, 14)
        } else {
            ColorU::new(255, 255, 255, 8)
        };
        let border = if selected {
            green_soft()
        } else {
            ColorU::new(255, 255, 255, 32)
        };
        let icon_color = if selected { genesi_green() } else { text_color };

        let mut row = Flex::row().with_cross_axis_alignment(CrossAxisAlignment::Center);
        if let Some(path) = icon_path {
            row.add_child(
                Container::new(
                    ConstrainedBox::new(Icon::new(path, icon_color).finish())
                        .with_width(12.)
                        .with_height(12.)
                        .finish(),
                )
                .with_margin_right(5.)
                .finish(),
            );
        }
        row.add_child(self.label_text(appearance, label, CHIP_FONT_SIZE, text_color, false));

        Container::new(row.finish())
            .with_horizontal_padding(8.)
            .with_vertical_padding(5.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(7.)))
            .with_border(Border::all(1.).with_border_color(border))
            .with_background_color(background)
            .with_margin_right(6.)
            .with_margin_top(2.)
            .finish()
    }

    fn chip_with_icon(
        &self,
        appearance: &Appearance,
        label: String,
        icon_path: Option<&'static str>,
        action: LocalAiChatAction,
        selected: bool,
        enabled: bool,
    ) -> Box<dyn Element> {
        let content = self.chip_content(appearance, label, icon_path, selected, enabled);
        if !enabled {
            return content;
        }

        EventHandler::new(content)
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    fn chip(
        &self,
        appearance: &Appearance,
        label: String,
        action: LocalAiChatAction,
        selected: bool,
    ) -> Box<dyn Element> {
        self.chip_with_icon(appearance, label, None, action, selected, true)
    }

    fn workspace_chip(
        &self,
        appearance: &Appearance,
        label: String,
        icon_path: Option<&'static str>,
        action: WorkspaceAction,
        selected: bool,
    ) -> Box<dyn Element> {
        EventHandler::new(self.chip_content(appearance, label, icon_path, selected, true))
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    pub fn current_chat_title(&self) -> String {
        Self::chat_title_from_entries(&self.messages)
    }

    pub fn start_new_chat(&mut self, ctx: &mut ViewContext<Self>) {
        self.stop_turn(ctx);
        self.persist_mempalace(ctx, false);
        if self.messages.is_empty() {
            self.reset_current_chat();
        } else {
            self.active_chat_id = Self::new_chat_id();
            self.reset_current_chat();
        }
        self.persist_mempalace(ctx, true);
        ctx.notify();
    }

    pub fn set_vibe_mode(&mut self, enabled: bool, ctx: &mut ViewContext<Self>) {
        self.vibe_mode = enabled;
        ctx.notify();
    }

    pub fn select_review_file(&mut self, path: &str, ctx: &mut ViewContext<Self>) {
        if self.pending_edits.iter().any(|edit| edit.path == path) {
            self.selected_review_path = Some(path.to_string());
            self.review_expanded = true;
            ctx.notify();
        }
    }

    pub fn keep_pending_edits(&mut self, ctx: &mut ViewContext<Self>) {
        let paths = self
            .pending_edits
            .iter()
            .map(|edit| edit.path.clone())
            .collect::<Vec<_>>();
        for path in paths {
            self.update_open_editor_diff(&path, None, ctx);
        }
        self.pending_edits.clear();
        self.review_expanded = false;
        self.selected_review_path = None;
        self.persist_mempalace(ctx, false);
        ctx.notify();
    }

    pub fn undo_pending_edits(&mut self, ctx: &mut ViewContext<Self>) {
        let edits = std::mem::take(&mut self.pending_edits);
        if let Some(root) = self.agent_root.clone() {
            for edit in edits {
                let Some(path) = local_agent::resolve_in_project(&root, &edit.path) else {
                    continue;
                };
                match edit.original {
                    Some(content) => {
                        let _ = std::fs::write(&path, content);
                    }
                    None => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        self.review_expanded = false;
        self.selected_review_path = None;
        self.persist_mempalace(ctx, false);
        ctx.notify();
    }

    pub fn render_review_sidebar(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let mut root = Flex::column()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        let header = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                Expanded::new(
                    1.,
                    self.label_text(
                        appearance,
                        "Review".to_string(),
                        TITLE_FONT_SIZE,
                        theme.main_text_color(theme.background()).into(),
                        false,
                    ),
                )
                .finish(),
            )
            .with_child(self.workspace_chip(
                appearance,
                "Files".to_string(),
                Some("folder"),
                WorkspaceAction::OpenGenesiFilesTool,
                false,
            ))
            .with_child(self.workspace_chip(
                appearance,
                "Terminal".to_string(),
                Some("terminal"),
                WorkspaceAction::OpenGenesiTerminalTool,
                false,
            ))
            .finish();
        root.add_child(
            Container::new(header)
                .with_uniform_padding(10.)
                .with_border(Border::bottom(1.).with_border_fill(theme.outline()))
                .finish(),
        );

        let selected_preview = self.selected_review_preview();
        if self.pending_edits.is_empty() && selected_preview.is_none() {
            root.add_child(
                Expanded::new(
                    1.,
                    Container::new(self.label_text(
                        appearance,
                        "No changes to review yet.".to_string(),
                        BODY_FONT_SIZE,
                        theme.disabled_text_color(theme.background()).into(),
                        true,
                    ))
                    .with_uniform_padding(16.)
                    .finish(),
                )
                .finish(),
            );
            return root.finish();
        }

        let selected_path = selected_preview
            .as_ref()
            .map(|preview| preview.0.to_string())
            .unwrap_or_default();
        let mut files = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        let file_rows: Vec<(&str, u32, u32)> = if self.pending_edits.is_empty() {
            selected_preview
                .as_ref()
                .map(|preview| vec![(preview.0, preview.1, preview.2)])
                .unwrap_or_default()
        } else {
            self.pending_edits
                .iter()
                .map(|edit| (edit.path.as_str(), edit.added, edit.removed))
                .collect()
        };
        for (edit_path, added, removed) in file_rows {
            let path = edit_path.to_string();
            let row = Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(
                    Expanded::new(
                        1.,
                        self.label_text(
                            appearance,
                            edit_path.to_string(),
                            CHIP_FONT_SIZE,
                            theme.main_text_color(theme.background()).into(),
                            false,
                        ),
                    )
                    .finish(),
                )
                .with_child(self.label_text(
                    appearance,
                    format!("+{added}"),
                    CHIP_FONT_SIZE,
                    genesi_green(),
                    false,
                ))
                .with_child(
                    Container::new(self.label_text(
                        appearance,
                        format!("-{removed}"),
                        CHIP_FONT_SIZE,
                        theme.ui_error_color().into(),
                        false,
                    ))
                    .with_margin_left(5.)
                    .finish(),
                )
                .finish();
            files.add_child(
                EventHandler::new(
                    Container::new(row)
                        .with_horizontal_padding(10.)
                        .with_vertical_padding(8.)
                        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
                        .with_background_color(if edit_path == selected_path {
                            ColorU::new(255, 255, 255, 18)
                        } else {
                            ColorU::new(0, 0, 0, 0)
                        })
                        .finish(),
                )
                .on_left_mouse_down(move |ctx, _, _| {
                    ctx.dispatch_typed_action(WorkspaceAction::SelectGenesiReviewFile(
                        path.clone(),
                    ));
                    DispatchEventResult::StopPropagation
                })
                .finish(),
            );
        }
        root.add_child(
            Container::new(files.finish())
                .with_uniform_padding(8.)
                .with_border(Border::bottom(1.).with_border_fill(theme.outline()))
                .finish(),
        );

        if let Some((_, _, _, diff_preview)) = selected_preview {
            root.add_child(
                Expanded::new(
                    1.,
                    ClippedScrollable::vertical(
                        self.review_sidebar_scroll.clone(),
                        Container::new(self.render_diff_preview(appearance, diff_preview))
                            .with_uniform_padding(8.)
                            .finish(),
                        ScrollbarWidth::Auto,
                        theme.disabled_ui_text_color().into(),
                        theme.active_ui_text_color().into(),
                        Fill::None,
                    )
                    .finish(),
                )
                .finish(),
            );
        }

        if !self.pending_edits.is_empty() {
            root.add_child(
                Container::new(
                    Flex::row()
                        .with_main_axis_alignment(MainAxisAlignment::End)
                        .with_child(self.workspace_chip(
                            appearance,
                            "Undo".to_string(),
                            None,
                            WorkspaceAction::UndoGenesiEdits,
                            false,
                        ))
                        .with_child(self.workspace_chip(
                            appearance,
                            "Keep All".to_string(),
                            None,
                            WorkspaceAction::KeepGenesiEdits,
                            true,
                        ))
                        .finish(),
                )
                .with_uniform_padding(10.)
                .with_border(Border::top(1.).with_border_fill(theme.outline()))
                .finish(),
            );
        }

        root.finish()
    }

    fn reset_current_chat(&mut self) {
        self.messages.clear();
        self.error = None;
        self.pending_tool = None;
        self.pending_edits.clear();
        self.review_expanded = false;
        self.selected_review_path = None;
        self.agent_messages.clear();
        self.agent_root = None;
        self.agent_model.clear();
        self.agent_step = 0;
        self.agent_step_buffer.clear();
        self.agent_tool_summary.clear();
        self.in_flight = false;
        self.current_turn += 1;
    }

    fn selected_review_edit(&self) -> Option<&PendingEdit> {
        self.selected_review_path
            .as_ref()
            .and_then(|path| self.pending_edits.iter().find(|edit| &edit.path == path))
            .or_else(|| self.pending_edits.first())
    }

    /// Returns the selected diff whether it is still pending or is an immutable
    /// historical snapshot stored on the tool card in the conversation.
    fn selected_review_preview(&self) -> Option<(&str, u32, u32, &[DiffPreviewLine])> {
        if let Some(edit) = self.selected_review_edit() {
            return Some((
                edit.path.as_str(),
                edit.added,
                edit.removed,
                edit.diff_preview.as_slice(),
            ));
        }

        let selected_path = self.selected_review_path.as_deref()?;
        self.messages.iter().rev().find_map(|entry| {
            let path = entry.tool_title.as_deref()?;
            if path != selected_path {
                return None;
            }
            let preview = entry.diff_preview.as_deref()?;
            let (added, removed) = entry.diff_stat.unwrap_or_default();
            Some((path, added, removed, preview))
        })
    }

    fn collapse_other_diffs(&mut self, keep_index: usize) {
        for (index, entry) in self.messages.iter_mut().enumerate() {
            if index != keep_index
                && entry.role == ChatRole::Tool
                && entry
                    .diff_preview
                    .as_ref()
                    .is_some_and(|lines| !lines.is_empty())
            {
                entry.collapsed = true;
            }
        }
    }

    fn render_diff_preview(
        &self,
        appearance: &Appearance,
        lines: &[DiffPreviewLine],
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        for line in lines {
            let (prefix, bg, fg) = match line.kind {
                DiffPreviewLineKind::Context => (
                    " ",
                    ColorU::new(0, 0, 0, 0),
                    theme.disabled_text_color(theme.background()).into(),
                ),
                DiffPreviewLineKind::Added => ("+", ColorU::new(15, 143, 106, 36), genesi_green()),
                DiffPreviewLineKind::Removed => (
                    "-",
                    ColorU::new(181, 68, 68, 36),
                    theme.ui_error_color().into(),
                ),
            };

            let old_line = line
                .old_line
                .map(|value| value.to_string())
                .unwrap_or_default();
            let new_line = line
                .new_line
                .map(|value| value.to_string())
                .unwrap_or_default();

            let row = Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(
                    ConstrainedBox::new(
                        Container::new(self.mono_text(
                            appearance,
                            format!("{old_line:>4}"),
                            MONO_FONT_SIZE,
                            theme.disabled_text_color(theme.background()).into(),
                            false,
                        ))
                        .finish(),
                    )
                    .with_width(34.)
                    .finish(),
                )
                .with_child(
                    ConstrainedBox::new(
                        Container::new(self.mono_text(
                            appearance,
                            format!("{new_line:>4}"),
                            MONO_FONT_SIZE,
                            theme.disabled_text_color(theme.background()).into(),
                            false,
                        ))
                        .finish(),
                    )
                    .with_width(34.)
                    .finish(),
                )
                .with_child(
                    ConstrainedBox::new(
                        Container::new(self.mono_text(
                            appearance,
                            prefix.to_string(),
                            MONO_FONT_SIZE,
                            fg,
                            false,
                        ))
                        .finish(),
                    )
                    .with_width(12.)
                    .finish(),
                )
                .with_child(
                    Expanded::new(
                        1.,
                        self.mono_text(appearance, line.text.clone(), MONO_FONT_SIZE, fg, true),
                    )
                    .finish(),
                )
                .finish();

            column.add_child(
                Container::new(row)
                    .with_horizontal_padding(8.)
                    .with_vertical_padding(2.)
                    .with_background_color(bg)
                    .finish(),
            );
        }

        Container::new(column.finish())
            .with_uniform_padding(6.)
            .with_margin_top(4.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
            .with_border(Border::all(1.).with_border_color(genesi_subtle_border()))
            .with_background_color(genesi_card_surface())
            .finish()
    }

    /// The panel header keeps only the assistant title and utility controls.
    /// The global Vibe/IDE switch belongs to the app title bar.
    fn render_header(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();

        let mut row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.label_text(
                appearance,
                "Genesi Code",
                TITLE_FONT_SIZE,
                theme.active_ui_text_color().into(),
                false,
            ))
            .with_child(Shrinkable::new(1., Empty::new().finish()).finish())
            .with_child(self.chip_with_icon(
                appearance,
                "Refresh".to_string(),
                Some("bundled/svg/refresh-cw-04.svg"),
                LocalAiChatAction::Refresh,
                false,
                true,
            ));
        row.add_child(self.chip_with_icon(
            appearance,
            "Clear".to_string(),
            Some("bundled/svg/trash-02.svg"),
            LocalAiChatAction::Clear,
            false,
            !self.messages.is_empty(),
        ));
        row.finish()
    }

    /// The bottom control strip, sitting just above the compose box like a real
    /// IDE: a click-to-open AI selector, the Turbo toggle next to it, and the
    /// agent toggles. The model picker itself is a popup (see
    /// [`Self::render_model_picker`]) so the user clicks once and chooses.
    fn render_control_strip(&self, appearance: &Appearance) -> Box<dyn Element> {
        let model_name = self
            .current_model()
            .unwrap_or_else(|| "no model".to_string());
        let model_name = truncate_middle(&model_name, MODEL_LABEL_MAX_CHARS);
        let selector_label = if self.cloud_active {
            format!("{}: {model_name}", self.cloud.provider.label())
        } else if self.endpoint == LocalEndpoint::Turbo {
            format!("Turbo: {model_name}")
        } else {
            format!("AI: {model_name}")
        };

        let selector_row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(
                Shrinkable::new(
                    1.,
                    self.selector_button(
                        appearance,
                        selector_label,
                        LocalAiChatAction::ToggleModelPicker,
                        self.model_picker_open,
                    ),
                )
                .finish(),
            )
            .with_child(self.chip_with_icon(
                appearance,
                "Turbo".to_string(),
                Some("bundled/svg/lightning-02.svg"),
                LocalAiChatAction::ToggleTurbo,
                self.endpoint == LocalEndpoint::Turbo,
                true,
            ))
            .finish();

        let mut toggles_row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.chip_with_icon(
                appearance,
                "Agent".to_string(),
                Some("bundled/svg/agentmode.svg"),
                LocalAiChatAction::ToggleAgent,
                self.agent_mode,
                true,
            ));
        if self.agent_mode {
            toggles_row.add_child(self.chip_with_icon(
                appearance,
                format!("AUTO {}", if self.auto_approve { "on" } else { "off" }),
                Some("bundled/svg/sparkle.svg"),
                LocalAiChatAction::ToggleAuto,
                self.auto_approve,
                true,
            ));
        }
        toggles_row.add_child(self.chip_with_icon(
            appearance,
            "Attach".to_string(),
            Some("bundled/svg/paperclip.svg"),
            LocalAiChatAction::ToggleAttachContext,
            self.attach_context,
            true,
        ));
        toggles_row.add_child(Shrinkable::new(1., Empty::new().finish()).finish());
        toggles_row.add_child(self.chip_with_icon(
            appearance,
            ai_mode_short_label(self.ai_mode.as_ref()),
            Some("bundled/svg/psychology.svg"),
            LocalAiChatAction::CycleAiMode,
            self.ai_mode.is_some(),
            true,
        ));

        Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(selector_row)
            .with_child(
                Container::new(toggles_row.finish())
                    .with_margin_top(5.)
                    .finish(),
            )
            .finish()
    }

    /// A quiet dropdown-style label used as the AI selector. It intentionally
    /// avoids the old bordered pill style so long model names do not dominate.
    fn selector_button(
        &self,
        appearance: &Appearance,
        label: String,
        action: LocalAiChatAction,
        open: bool,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let text_color: ColorU = theme.active_ui_text_color().into();
        let chevron = if open {
            "bundled/svg/chevron-up.svg"
        } else {
            "bundled/svg/chevron-down.svg"
        };
        let container = Container::new(
            Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(self.label_text(appearance, label, BODY_FONT_SIZE, text_color, false))
                .with_child(
                    Container::new(
                        ConstrainedBox::new(Icon::new(chevron, text_color).finish())
                            .with_width(12.)
                            .with_height(12.)
                            .finish(),
                    )
                    .with_margin_left(6.)
                    .finish(),
                )
                .finish(),
        )
        .with_horizontal_padding(8.)
        .with_vertical_padding(5.)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
        .with_border(Border::all(1.).with_border_color(if open {
            green_soft()
        } else {
            genesi_subtle_border()
        }))
        .with_background_color(if open {
            green_tint()
        } else {
            ColorU::new(255, 255, 255, 6)
        })
        .with_margin_right(4.)
        .with_margin_top(2.);

        EventHandler::new(container.finish())
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(action.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    fn render_prompt_action_button(&self, _appearance: &Appearance) -> Box<dyn Element> {
        let (icon_path, action, fill, icon_color) = if self.in_flight {
            (
                "bundled/svg/stop-filled.svg",
                LocalAiChatAction::Stop,
                ColorU::new(117, 34, 34, 255),
                ColorU::new(255, 214, 214, 255),
            )
        } else {
            (
                "bundled/svg/send.svg",
                LocalAiChatAction::SubmitPromptInput,
                genesi_green(),
                ColorU::white(),
            )
        };

        let button = Container::new(
            ConstrainedBox::new(Icon::new(icon_path, icon_color).finish())
                .with_width(14.)
                .with_height(14.)
                .finish(),
        )
        .with_horizontal_padding(8.)
        .with_vertical_padding(8.)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(10.)))
        .with_border(Border::all(1.).with_border_color(if self.in_flight {
            ColorU::new(181, 68, 68, 90)
        } else {
            green_soft()
        }))
        .with_background_color(fill)
        .finish();

        EventHandler::new(button)
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

        list.add_child(section(self, "Cloud providers"));
        for provider in cloud_presets() {
            let selected = self.cloud.provider == *provider;
            let mark = if self.cloud_active && selected {
                "●  "
            } else {
                "○  "
            };
            list.add_child(self.picker_row(
                appearance,
                format!("{mark}{}", provider.label()),
                LocalAiChatAction::SelectCloudProvider(*provider),
                selected,
            ));
        }

        let key_saved = !self.active_cloud_key().trim().is_empty();
        let key_status = if key_saved {
            format!("Key stored securely for {}", self.cloud.provider.label())
        } else {
            format!(
                "No API key saved for {}. Open Settings to add one.",
                self.cloud.provider.label()
            )
        };
        list.add_child(
            Container::new(self.label_text(appearance, key_status, BODY_FONT_SIZE, muted, false))
                .with_horizontal_padding(8.)
                .with_padding_top(6.)
                .finish(),
        );
        list.add_child(self.picker_row(
            appearance,
            if key_saved {
                "Manage API keys in Settings".to_string()
            } else {
                "Open API key Settings".to_string()
            },
            LocalAiChatAction::SetKey,
            key_saved,
        ));
        list.add_child(section(self, "Suggested cloud models"));
        for model in self.cloud.provider.suggested_models() {
            let selected = self.cloud.model == *model;
            let mark = if selected { "●  " } else { "○  " };
            list.add_child(self.picker_row(
                appearance,
                format!("{mark}{model}"),
                LocalAiChatAction::PickCloudModel((*model).to_string()),
                selected,
            ));
        }
        list.add_child(self.picker_row(
            appearance,
            format!(
                "Custom model: {}",
                truncate_middle(self.cloud.model.trim(), MODEL_LABEL_MAX_CHARS)
            ),
            LocalAiChatAction::SetModel,
            false,
        ));
        list.add_child(
            Container::new(self.label_text(
                appearance,
                match self.cloud.provider {
                    CloudProviderKind::Anthropic => {
                        "Anthropic uses its native Messages API; the key is sent with x-api-key."
                    }
                    CloudProviderKind::Gemini => {
                        "Gemini uses Google's official OpenAI-compatible endpoint."
                    }
                    CloudProviderKind::OpenAI => {
                        "OpenAI uses /v1/chat/completions with your own bearer token."
                    }
                    CloudProviderKind::HuggingFace => {
                        "Hugging Face uses the official router and an HF token with Inference Providers permission."
                    }
                },
                CHIP_FONT_SIZE,
                muted,
                true,
            ))
            .with_horizontal_padding(8.)
            .with_padding_top(6.)
            .with_padding_bottom(4.)
            .finish(),
        );
        if false {
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
        }
        let card = Container::new(list.finish())
            .with_uniform_padding(6.)
            .with_margin_bottom(6.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(10.)))
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_background(theme.surface_1())
            .finish();
        let wrapped = if self.vibe_mode {
            ConstrainedBox::new(card)
                .with_width(VIBE_COLUMN_WIDTH)
                .finish()
        } else {
            card
        };
        Some(
            Container::new(wrapped)
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
        let mut row =
            Container::new(self.label_text(appearance, label, BODY_FONT_SIZE, color, false))
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

    /// The review bar: a summary of the file changes the agent has made this
    /// turn, with "Undo all" / "Keep" — like the reference's "N files need
    /// review". Sits just above the compose box; `None` when nothing's pending.
    fn render_review_bar(&self, appearance: &Appearance) -> Option<Box<dyn Element>> {
        if self.pending_edits.is_empty() {
            return None;
        }
        let theme = appearance.theme();
        let n = self.pending_edits.len();
        let added: u32 = self.pending_edits.iter().map(|e| e.added).sum();
        let removed: u32 = self.pending_edits.iter().map(|e| e.removed).sum();
        let label = format!("> {n} file{} need review", if n == 1 { "" } else { "s" });

        let row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(self.label_text(
                appearance,
                label,
                CHIP_FONT_SIZE,
                theme.main_text_color(theme.background()).into(),
                false,
            ))
            .with_child(
                Container::new(self.label_text(
                    appearance,
                    format!("+{added}"),
                    CHIP_FONT_SIZE,
                    genesi_green(),
                    false,
                ))
                .with_margin_left(10.)
                .with_margin_right(4.)
                .finish(),
            )
            .with_child(self.label_text(
                appearance,
                format!("-{removed}"),
                CHIP_FONT_SIZE,
                theme.ui_error_color().into(),
                false,
            ))
            .with_child(Shrinkable::new(1., Empty::new().finish()).finish())
            .with_child(self.chip(
                appearance,
                "Undo".to_string(),
                LocalAiChatAction::UndoEdits,
                false,
            ))
            .with_child(self.chip(
                appearance,
                "Keep All".to_string(),
                LocalAiChatAction::KeepEdits,
                true,
            ))
            .finish();

        let column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(
                EventHandler::new(
                    Container::new(row)
                        .with_uniform_padding(8.)
                        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
                        .with_border(Border::all(1.).with_border_color(genesi_subtle_border()))
                        .with_background_color(genesi_panel_surface())
                        .finish(),
                )
                .on_left_mouse_down(move |ctx, _, _| {
                    ctx.dispatch_typed_action(LocalAiChatAction::ToggleReviewExpanded);
                    DispatchEventResult::StopPropagation
                })
                .finish(),
            );

        let wrapped = if self.vibe_mode {
            ConstrainedBox::new(column.finish())
                .with_width(VIBE_COLUMN_WIDTH)
                .finish()
        } else {
            column.finish()
        };
        Some(
            Container::new(wrapped)
                .with_horizontal_padding(PANEL_PADDING)
                .with_padding_bottom(4.)
                .finish(),
        )
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
            AgentTool::WriteFile { path, content } => (format!("✏ Write {path}?"), clip(content)),
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

        let content = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.label_text(appearance, title, CHIP_FONT_SIZE, genesi_green(), false))
            .with_child(detail_box)
            .with_child(Container::new(buttons).with_margin_top(6.).finish())
            .finish();
        let wrapped = if self.vibe_mode {
            ConstrainedBox::new(content)
                .with_width(VIBE_COLUMN_WIDTH)
                .finish()
        } else {
            content
        };
        Container::new(wrapped)
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
        let caret = if collapsed { ">" } else { "v" };
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
        let (prefix, role_color): (&str, ColorU) = if is_user {
            ("You", theme.disabled_text_color(theme.background()).into())
        } else {
            (
                "Genesi AI",
                theme.disabled_text_color(theme.background()).into(),
            )
        };

        let mut inner = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.label_text(appearance, prefix, CHIP_FONT_SIZE, role_color, false));

        if let Some(label) = &entry.context_label {
            let pill = Container::new(self.label_text(
                appearance,
                format!("Attach: {label}"),
                CHIP_FONT_SIZE,
                theme.disabled_text_color(theme.background()).into(),
                false,
            ))
            .with_horizontal_padding(6.)
            .with_vertical_padding(2.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .with_border(Border::all(1.).with_border_color(genesi_subtle_border()))
            .with_background_color(ColorU::new(0, 0, 0, 0))
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
            let dots = if self.in_flight { "..." } else { "" };
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
            .with_margin_bottom(10.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)));
        let bubble = if is_user {
            bubble
                .with_uniform_padding(10.)
                .with_background_color(genesi_card_surface())
                .with_border(Border::all(1.).with_border_color(genesi_subtle_border()))
        } else {
            bubble.with_vertical_padding(2.)
        };
        bubble.finish()
    }
    /// A collapsible thought, kept out of the way so the transcript stays clean.
    fn render_thought(
        &self,
        appearance: &Appearance,
        index: usize,
        entry: &ChatEntry,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();
        let title = if entry.status == StepStatus::Running {
            "Thinking...".to_string()
        } else {
            "Thought".to_string()
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
            StepStatus::Running => ("file", muted),
            StepStatus::Ok => ("file", theme.main_text_color(theme.background()).into()),
            StepStatus::Error => ("error", theme.ui_error_color().into()),
            StepStatus::Denied => ("denied", muted),
        };
        let suffix = match entry.status {
            StepStatus::Running => " - running...",
            StepStatus::Error => " - error",
            StepStatus::Denied => " - denied",
            StepStatus::Ok => "",
        };
        let path = entry.tool_title.clone().unwrap_or_default();
        let header: Box<dyn Element> = if let Some((added, removed)) = entry.diff_stat {
            let caret = if entry.collapsed { ">" } else { "v" };
            let green: ColorU = genesi_green();
            let red: ColorU = theme.ui_error_color().into();
            let row = Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_main_axis_size(MainAxisSize::Max)
                .with_child(self.label_text(
                    appearance,
                    format!("{caret} {path}{suffix}"),
                    CHIP_FONT_SIZE,
                    color,
                    false,
                ))
                .with_child(Shrinkable::new(1., Empty::new().finish()).finish())
                .with_child(
                    Container::new(self.label_text(
                        appearance,
                        format!("+{added}"),
                        CHIP_FONT_SIZE,
                        green,
                        false,
                    ))
                    .with_margin_right(4.)
                    .finish(),
                )
                .with_child(self.label_text(
                    appearance,
                    format!("-{removed}"),
                    CHIP_FONT_SIZE,
                    red,
                    false,
                ))
                .with_child(
                    Container::new(self.chip(
                        appearance,
                        "Open Diff".to_string(),
                        LocalAiChatAction::OpenDiff(index),
                        false,
                    ))
                    .with_margin_left(8.)
                    .finish(),
                )
                .finish();
            EventHandler::new(
                Container::new(row)
                    .with_horizontal_padding(8.)
                    .with_vertical_padding(6.)
                    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
                    .with_border(Border::all(1.).with_border_color(genesi_subtle_border()))
                    .with_background_color(genesi_card_surface())
                    .finish(),
            )
            .on_left_mouse_down(move |ctx, _, _| {
                ctx.dispatch_typed_action(LocalAiChatAction::ToggleCollapse(index));
                DispatchEventResult::StopPropagation
            })
            .finish()
        } else {
            let title = format!("{icon}: {path}{suffix}");
            self.step_header(appearance, index, title, color, entry.collapsed)
        };

        let mut column = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(header);

        if !entry.collapsed {
            if entry.diff_preview.is_none() && !entry.text.trim().is_empty() {
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
                .with_border(Border::all(1.).with_border_color(genesi_subtle_border()))
                .with_background_color(genesi_card_surface())
                .finish();
                column.add_child(body);
            }
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
            if self.vibe_mode {
                return Container::new(Empty::new().finish())
                    .with_horizontal_padding(PANEL_PADDING)
                    .finish();
            }
            let (title, hint) = if self.cloud_active {
                match self.current_model() {
                    Some(model) => (
                        format!("Ask {}", self.cloud.provider.label()),
                        format!(
                            "Using {model} via {}. Your API key stays on this device in secure storage. With Attach on, the file you're editing is sent as context.",
                            self.cloud.provider.label()
                        ),
                    ),
                    None => (
                        format!("Set up {}", self.cloud.provider.label()),
                        format!(
                            "Pick a model and save an API key for {} to start chatting.",
                            self.cloud.provider.label()
                        ),
                    ),
                }
            } else {
                match self.current_model() {
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
                }
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

        let selected_text = self.selected_transcript_text.clone();
        let selectable_transcript = SelectableArea::new(
            self.transcript_selection.clone(),
            move |selection, _, _| {
                *selected_text.write() = selection.selection.filter(|text| !text.is_empty());
            },
            Container::new(column.finish())
                .with_horizontal_padding(PANEL_PADDING)
                .finish(),
        )
        .on_selection_updated(|ctx, _| {
            ctx.dispatch_typed_action(LocalAiChatAction::FocusTranscript);
        })
        .finish();

        ClippedScrollable::vertical(
            self.transcript_scroll.clone(),
            selectable_transcript,
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
            LocalAiChatAction::CopySelectedText => {
                if let Some(text) = self
                    .selected_transcript_text
                    .read()
                    .clone()
                    .filter(|text| !text.is_empty())
                {
                    ctx.clipboard().write(ClipboardContent::plain_text(text));
                } else {
                    self.input.update(ctx, |input, ctx| {
                        input.editor().update(ctx, |editor, ctx| editor.copy(ctx));
                    });
                }
            }
            LocalAiChatAction::FocusTranscript => {
                ctx.focus_self();
            }
            LocalAiChatAction::CycleEndpoint => {
                self.load_cloud_keys(ctx);
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
                    if !self.cloud_ready() {
                        self.error = Some(format!(
                            "Pick a provider and add your key (🔑 Set key) to use {}.",
                            self.cloud_label()
                        ));
                    }
                }
                ctx.notify();
            }
            LocalAiChatAction::CycleProvider => {
                self.load_cloud_keys(ctx);
                let presets = cloud_presets();
                let current = presets
                    .iter()
                    .position(|provider| *provider == self.cloud.provider)
                    .unwrap_or(0);
                let next = (current + 1) % presets.len();
                // Replace the model only if it was empty or still the old preset's
                // default — never clobber a model the user typed by hand.
                let previous_provider = presets[current];
                let replace_model = self.cloud.model.trim().is_empty()
                    || self.cloud.model == previous_provider.default_model();
                self.cloud.provider = presets[next];
                if replace_model {
                    self.cloud.model = self.cloud.provider.default_model().to_string();
                }
                if let Err(err) = save_cloud_config(&self.cloud) {
                    self.error = Some(format!("Couldn't save cloud config: {err}"));
                } else if !self.cloud_ready() {
                    self.error = Some(format!(
                        "Add your {} API key to use {}.",
                        self.cloud.provider.label(),
                        self.cloud.model
                    ));
                } else {
                    self.error = None;
                }
                ctx.notify();
            }
            LocalAiChatAction::SelectCloudProvider(provider) => {
                self.select_cloud_provider(*provider, false, ctx);
                ctx.notify();
            }
            LocalAiChatAction::PickCloudModel(model) => {
                self.load_cloud_keys(ctx);
                self.cloud_active = true;
                self.cloud.model = model.clone();
                match save_cloud_config(&self.cloud) {
                    Ok(()) => self.error = None,
                    Err(err) => self.error = Some(format!("Couldn't save cloud config: {err}")),
                }
                ctx.notify();
            }
            LocalAiChatAction::SetKey => ctx.dispatch_typed_action(
                &WorkspaceAction::ShowSettingsPage(SettingsSection::WarpAgent),
            ),
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
                self.load_cloud_keys(ctx);
                self.refresh_models(ctx);
                self.refresh_ai_mode();
                ctx.notify();
            }
            LocalAiChatAction::Clear => {
                self.reset_current_chat();
                self.persist_mempalace(ctx, true);
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
                self.load_cloud_keys(ctx);
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
            LocalAiChatAction::KeepEdits => {
                let paths = self
                    .pending_edits
                    .iter()
                    .map(|edit| edit.path.clone())
                    .collect::<Vec<_>>();
                for path in paths {
                    self.update_open_editor_diff(&path, None, ctx);
                }
                self.pending_edits.clear();
                self.review_expanded = false;
                self.selected_review_path = None;
                self.persist_mempalace(ctx, false);
                ctx.notify();
            }
            LocalAiChatAction::UndoEdits => {
                // Restore each captured original (or delete a file the agent
                // created), inside the project root only.
                let edits = std::mem::take(&mut self.pending_edits);
                if let Some(root) = self.agent_root.clone() {
                    for edit in edits {
                        let Some(path) = local_agent::resolve_in_project(&root, &edit.path) else {
                            continue;
                        };
                        match edit.original {
                            Some(content) => {
                                let _ = std::fs::write(&path, content);
                            }
                            None => {
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                }
                self.pending_edits.clear();
                self.review_expanded = false;
                self.selected_review_path = None;
                self.persist_mempalace(ctx, false);
                ctx.notify();
            }
            LocalAiChatAction::OpenDiff(index) => {
                if let Some(path) = self
                    .messages
                    .get(*index)
                    .and_then(|entry| entry.tool_title.clone())
                {
                    self.review_expanded = true;
                    self.selected_review_path = Some(path);
                }
                ctx.emit(LocalAiChatEvent::OpenDiff);
                ctx.notify();
            }
            LocalAiChatAction::ToggleReviewExpanded => {
                self.review_expanded = true;
                if self.selected_review_path.is_none() {
                    self.selected_review_path =
                        self.pending_edits.first().map(|edit| edit.path.clone());
                }
                ctx.emit(LocalAiChatEvent::OpenDiff);
                ctx.notify();
            }
            LocalAiChatAction::SelectReviewFile(path) => {
                self.review_expanded = true;
                self.selected_review_path = Some(path.clone());
                ctx.emit(LocalAiChatEvent::OpenDiff);
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
                        diff_stat: None,
                        diff_preview: None,
                    };
                    self.messages.push(denied);
                    self.agent_messages.push(ChatMessage::user(format!(
                        "TOOL RESULT ({name}):\nThe user denied this action."
                    )));
                    self.agent_step += 1;
                    self.persist_mempalace(ctx, false);
                    self.run_agent_step(ctx);
                }
                ctx.notify();
            }
            LocalAiChatAction::Stop => {
                self.stop_turn(ctx);
            }
            LocalAiChatAction::ToggleCollapse(index) => {
                let expanding = self
                    .messages
                    .get(*index)
                    .is_some_and(|entry| entry.collapsed);
                if expanding {
                    self.collapse_other_diffs(*index);
                }
                if let Some(entry) = self.messages.get_mut(*index) {
                    entry.collapsed = !entry.collapsed;
                }
                ctx.notify();
            }
            LocalAiChatAction::NewChat => {
                self.start_new_chat(ctx);
            }
            LocalAiChatAction::SubmitPromptInput => {
                self.input.update(ctx, |input, ctx| input.submit(ctx));
            }
        }
    }
}

impl LocalAiChatView {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }

    fn new_chat_id() -> String {
        format!("chat-{}", Self::now_ms())
    }

    fn chat_title_from_entries(entries: &[ChatEntry]) -> String {
        entries
            .iter()
            .find(|entry| entry.role == ChatRole::User && !entry.text.trim().is_empty())
            .map(|entry| truncate_middle(entry.text.trim(), 28))
            .unwrap_or_else(|| "New Chat".to_string())
    }

    fn prune_chats(&mut self) {
        let active_chat_id = self.active_chat_id.clone();
        self.chats.retain(|chat| {
            if chat.id == active_chat_id {
                return true;
            }
            !chat.messages.is_empty()
        });
        self.chats
            .sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
    }

    fn snapshot_current_chat(&self) -> PersistedLocalChat {
        PersistedLocalChat {
            id: self.active_chat_id.clone(),
            messages: self.messages.clone(),
            agent_messages: self.agent_messages.clone(),
            updated_at_ms: Self::now_ms(),
        }
    }

    fn persist_mempalace(&mut self, ctx: &mut ViewContext<Self>, emit_state_changed: bool) {
        if self.active_chat_id.is_empty() {
            self.active_chat_id = Self::new_chat_id();
        }

        let snapshot = self.snapshot_current_chat();
        if let Some(existing) = self.chats.iter_mut().find(|chat| chat.id == snapshot.id) {
            *existing = snapshot;
        } else {
            self.chats.push(snapshot);
        }
        self.prune_chats();

        let state = MempalaceState {
            active_chat_id: self.active_chat_id.clone(),
            chats: self.chats.clone(),
        };
        match serde_json::to_string(&state) {
            Ok(json) => {
                if let Err(err) = ctx
                    .secure_storage()
                    .write_value_with_owner_only_fallback(MEMPALACE_STORAGE_KEY, &json)
                {
                    log::warn!("Failed to persist Genesi chat mempalace: {err:#}");
                }
            }
            Err(err) => {
                log::warn!("Failed to serialize Genesi chat mempalace: {err:#}");
            }
        }

        if emit_state_changed {
            ctx.emit(LocalAiChatEvent::StateChanged);
        }
    }

    fn restore_chat(&mut self, chat: &PersistedLocalChat) {
        self.messages = chat.messages.clone();
        self.agent_messages = chat.agent_messages.clone();
        self.error = None;
        self.pending_tool = None;
        self.pending_edits.clear();
        self.review_expanded = false;
        self.selected_review_path = None;
        self.agent_root = None;
        self.agent_model.clear();
        self.agent_step = 0;
        self.agent_step_buffer.clear();
        self.agent_tool_summary.clear();
        self.in_flight = false;
        self.current_turn += 1;
    }

    fn load_mempalace(&mut self, ctx: &mut ViewContext<Self>) {
        match ctx.secure_storage().read_value(MEMPALACE_STORAGE_KEY) {
            Ok(json) => match serde_json::from_str::<MempalaceState>(&json) {
                Ok(state) => {
                    self.active_chat_id = state.active_chat_id;
                    self.chats = state.chats;
                    self.prune_chats();
                    if self.active_chat_id.is_empty() {
                        self.active_chat_id = self
                            .chats
                            .first()
                            .map(|chat| chat.id.clone())
                            .unwrap_or_else(Self::new_chat_id);
                    }
                    if let Some(chat) = self
                        .chats
                        .iter()
                        .find(|chat| chat.id == self.active_chat_id)
                        .cloned()
                    {
                        self.restore_chat(&chat);
                    } else {
                        self.messages.clear();
                        self.agent_messages.clear();
                        self.persist_mempalace(ctx, false);
                    }
                }
                Err(err) => {
                    log::warn!("Failed to deserialize Genesi chat mempalace: {err:#}");
                    self.active_chat_id = Self::new_chat_id();
                    self.persist_mempalace(ctx, false);
                }
            },
            Err(err) => {
                if !matches!(err, warpui_extras::secure_storage::Error::NotFound) {
                    log::warn!("Failed to read Genesi chat mempalace: {err:#}");
                }
                self.active_chat_id = Self::new_chat_id();
                self.persist_mempalace(ctx, false);
            }
        }
    }

    pub fn chat_summaries(&self) -> Vec<LocalChatSummary> {
        let mut chats = self.chats.clone();
        if !self.active_chat_id.is_empty() {
            let snapshot = self.snapshot_current_chat();
            if let Some(existing) = chats.iter_mut().find(|chat| chat.id == snapshot.id) {
                *existing = snapshot;
            } else {
                chats.push(snapshot);
            }
        }
        chats.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
        chats
            .into_iter()
            .map(|chat| LocalChatSummary {
                id: chat.id.clone(),
                title: Self::chat_title_from_entries(&chat.messages),
                is_active: chat.id == self.active_chat_id,
            })
            .collect()
    }

    pub fn open_chat(&mut self, chat_id: &str, ctx: &mut ViewContext<Self>) {
        if chat_id == self.active_chat_id {
            return;
        }

        self.stop_turn(ctx);
        self.persist_mempalace(ctx, false);

        if let Some(chat) = self.chats.iter().find(|chat| chat.id == chat_id).cloned() {
            self.active_chat_id = chat.id.clone();
            self.restore_chat(&chat);
            self.scroll_to_bottom();
            ctx.emit(LocalAiChatEvent::StateChanged);
            ctx.notify();
        }
    }

    pub fn delete_chat(&mut self, chat_id: &str, ctx: &mut ViewContext<Self>) {
        self.stop_turn(ctx);

        let was_active = chat_id == self.active_chat_id;
        if !was_active {
            self.persist_mempalace(ctx, false);
        }

        self.chats.retain(|chat| chat.id != chat_id);

        if was_active {
            if let Some(next_chat) = self
                .chats
                .iter()
                .max_by_key(|chat| chat.updated_at_ms)
                .cloned()
            {
                self.active_chat_id = next_chat.id.clone();
                self.restore_chat(&next_chat);
                self.scroll_to_bottom();
            } else {
                self.active_chat_id = Self::new_chat_id();
                self.reset_current_chat();
            }
        }

        self.persist_mempalace(ctx, true);
        ctx.notify();
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
        if self.vibe_mode {
            // Center the chat column on the X axis (Claude-style) instead of
            // letting it hug the left edge of the wide vibe main area. The
            // transcript and the compose box below are both width-capped at 760
            // so this reads as a centered conversation column.
            root = root.with_cross_axis_alignment(CrossAxisAlignment::Center);
        }

        let header = if self.vibe_mode {
            Container::new(
                ConstrainedBox::new(self.render_header(appearance))
                    .with_width(VIBE_COLUMN_WIDTH)
                    .finish(),
            )
            .with_horizontal_padding(PANEL_PADDING)
            .with_padding_top(PANEL_PADDING + 4.)
            .with_padding_bottom(4.)
            .finish()
        } else {
            Container::new(self.render_header(appearance))
                .with_uniform_padding(PANEL_PADDING)
                .with_border(Border::bottom(1.).with_border_fill(theme.outline()))
                .finish()
        };
        root.add_child(header);

        let transcript: Box<dyn Element> = if self.vibe_mode {
            // Cap the transcript width to match the 760px compose box so the
            // centered column reads like a chat instead of stretching full-width.
            ConstrainedBox::new(self.render_transcript(appearance))
                .with_max_width(VIBE_COLUMN_WIDTH)
                .finish()
        } else {
            self.render_transcript(appearance)
        };
        root.add_child(Expanded::new(1., transcript).finish());

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

        // Review bar: a summary of the agent's file changes with Undo all / Keep,
        // sitting just above the compose box (the reference's "N files need review").
        if let Some(bar) = self.render_review_bar(appearance) {
            root.add_child(bar);
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
                Container::new(
                    Flex::row()
                        .with_cross_axis_alignment(CrossAxisAlignment::Center)
                        .with_child(
                            Shrinkable::new(1., ChildView::new(&self.input).finish()).finish(),
                        )
                        .with_child(
                            Container::new(self.render_prompt_action_button(appearance))
                                .with_margin_left(8.)
                                .finish(),
                        )
                        .finish(),
                )
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
            .with_background_color(genesi_panel_surface())
            .finish();
        let compose_container = if self.vibe_mode {
            Container::new(
                ConstrainedBox::new(compose_box)
                    .with_width(VIBE_COLUMN_WIDTH)
                    .finish(),
            )
            .with_horizontal_padding(PANEL_PADDING)
            .with_padding_top(4.)
            .with_padding_bottom(PANEL_PADDING + 8.)
            .finish()
        } else {
            Container::new(compose_box)
                .with_horizontal_padding(PANEL_PADDING)
                .with_padding_top(4.)
                .with_padding_bottom(PANEL_PADDING)
                .finish()
        };
        root.add_child(compose_container);

        root.finish()
    }
}
