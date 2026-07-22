//! The Live Preview workspace: pages rendered natively, a component list with
//! inline hover previews, and controls for the project's own dev server.
//!
//! State and behaviour for the preview live here rather than in
//! [`super::local_chat_panel`], which already carries the chat, the agent loop
//! and the project canvas.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use warp_core::ui::color::blend::Blend;
use warp_core::ui::icons::Icon as CoreIcon;
use warp_core::ui::theme::Fill as ThemeFill;
use warpui::color::ColorU;
use warpui::elements::{
    Border, ChildAnchor, Clipped, ClippedScrollStateHandle, ClippedScrollable, ConstrainedBox,
    Container, CornerRadius, CrossAxisAlignment, DispatchEventResult, Element, Empty, EventHandler,
    Expanded, Fill, Flex, Hoverable, MainAxisAlignment, MainAxisSize, MouseStateHandle,
    OffsetPositioning, ParentAnchor, ParentElement, ParentOffsetBounds, Radius, ScrollbarWidth,
    Stack,
};
use warpui::geometry::vector::vec2f;
use warpui::ui_components::components::{UiComponent, UiComponentStyles};
use warpui::ViewContext;

use super::live_preview::{
    compile_component, compile_html, compile_html_fragment, compile_page, html_landmarks,
    page_stylesheet, scan_preview_project, PreviewDocument, PreviewPageKind, PreviewProject,
    Stylesheet,
};
use super::live_preview_view::{render_document, sheet_background, PreviewScale};
use super::local_chat_panel::LocalAiChatView;
use crate::appearance::Appearance;
use crate::workspace::WorkspaceAction;

#[cfg(not(target_family = "wasm"))]
use super::live_preview_server::DevServerHandle;

const SIDEBAR_WIDTH: f32 = 236.;
const HOVER_CARD_WIDTH: f32 = 420.;
const HOVER_CARD_MAX_HEIGHT: f32 = 380.;

/// What the preview is currently showing.
#[derive(Debug, Default)]
pub(crate) enum LivePreviewState {
    /// No project has been opened yet.
    #[default]
    Idle,
    Scanning(PathBuf),
    Ready(Arc<PreviewProject>),
    Error(String),
}

/// One entry in the "Components" list. Static projects contribute landmark
/// sections of the current page; React projects contribute their components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewPart {
    Component(String),
    /// A landmark of the current page: its label and the markup to compile.
    Landmark { label: String, markup: String },
}

impl PreviewPart {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Component(name) => format!("component:{name}"),
            Self::Landmark { label, .. } => format!("landmark:{label}"),
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Component(name) => name,
            Self::Landmark { label, .. } => label,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::Component(_) => "Component",
            Self::Landmark { .. } => "Section",
        }
    }
}

#[derive(Default)]
pub(crate) struct LivePreviewPanelState {
    pub(crate) state: LivePreviewState,
    /// Bumped on every refresh so a stale background scan cannot overwrite a
    /// newer one.
    pub(crate) generation: u64,
    pub(crate) selected_page: usize,
    pub(crate) document: Option<Arc<PreviewDocument>>,
    /// The page's own stylesheet, kept so hovered sections compile with the
    /// exact styles they have on the page.
    pub(crate) page_sheet: Option<Arc<Stylesheet>>,
    pub(crate) parts: Vec<PreviewPart>,
    pub(crate) zoom: f32,
    pub(crate) scroll: ClippedScrollStateHandle,
    pub(crate) sidebar_scroll: ClippedScrollStateHandle,
    pub(crate) log_scroll: ClippedScrollStateHandle,
    /// Which part's inline preview card is showing, and the card itself.
    pub(crate) hovered_part: Option<String>,
    /// A clicked part stays open after the pointer leaves.
    pub(crate) pinned_part: Option<String>,
    pub(crate) hover_documents: HashMap<String, Arc<PreviewDocument>>,
    pub(crate) hover_states: HashMap<String, MouseStateHandle>,
    /// `true` renders project source; `false` renders what the dev server served.
    pub(crate) from_source: bool,
    pub(crate) server_document: Option<Arc<PreviewDocument>>,
    pub(crate) server_error: Option<String>,
    #[cfg(not(target_family = "wasm"))]
    pub(crate) server: Option<DevServerHandle>,
}

impl LivePreviewPanelState {
    fn reset_for_new_project(&mut self) {
        self.document = None;
        self.page_sheet = None;
        self.parts.clear();
        self.selected_page = 0;
        self.hovered_part = None;
        self.pinned_part = None;
        self.hover_documents.clear();
        self.hover_states.clear();
        self.server_document = None;
        self.server_error = None;
    }

    fn active_part(&self) -> Option<&String> {
        self.pinned_part.as_ref().or(self.hovered_part.as_ref())
    }
}

impl LocalAiChatView {
    // ── state transitions ───────────────────────────────────────────────────

    pub fn open_live_preview(&mut self, project_root: Option<PathBuf>, ctx: &mut ViewContext<Self>) {
        self.refresh_live_preview(project_root, ctx);
    }

    /// Re-scans the project. The walk touches the filesystem, so it runs off
    /// the UI thread; project code is never executed.
    pub fn refresh_live_preview(
        &mut self,
        project_root: Option<PathBuf>,
        ctx: &mut ViewContext<Self>,
    ) {
        self.preview.generation = self.preview.generation.wrapping_add(1);
        let generation = self.preview.generation;
        self.preview.reset_for_new_project();
        if self.preview.zoom <= 0. {
            self.preview.zoom = 1.;
        }

        let Some(root) = project_root else {
            self.preview.state = LivePreviewState::Idle;
            ctx.notify();
            return;
        };

        self.preview.state = LivePreviewState::Scanning(root.clone());
        ctx.notify();

        ctx.spawn(
            async move { tokio::task::spawn_blocking(move || scan_preview_project(&root)).await },
            move |me, result, ctx| {
                if me.preview.generation != generation {
                    return;
                }
                match result {
                    Ok(project) => {
                        me.preview.state = LivePreviewState::Ready(Arc::new(project));
                        me.compile_selected_page(ctx);
                    }
                    Err(error) => {
                        me.preview.state = LivePreviewState::Error(format!(
                            "The preview scanner stopped unexpectedly: {error}"
                        ));
                        ctx.notify();
                    }
                }
            },
        );
    }

    pub fn select_live_preview_page(&mut self, index: usize, ctx: &mut ViewContext<Self>) {
        if self.preview.selected_page == index {
            return;
        }
        self.preview.selected_page = index;
        self.preview.hovered_part = None;
        self.preview.pinned_part = None;
        self.preview.hover_documents.clear();
        self.compile_selected_page(ctx);
    }

    /// Compiles the selected page and rebuilds the part list that goes with it.
    fn compile_selected_page(&mut self, ctx: &mut ViewContext<Self>) {
        let LivePreviewState::Ready(project) = &self.preview.state else {
            ctx.notify();
            return;
        };
        let project = project.clone();
        let generation = self.preview.generation;
        let index = self.preview.selected_page.min(
            project
                .pages
                .len()
                .saturating_sub(1),
        );
        let Some(page) = project.pages.get(index).cloned() else {
            self.preview.document = None;
            self.rebuild_parts(&project, None);
            ctx.notify();
            return;
        };

        ctx.spawn(
            async move {
                tokio::task::spawn_blocking(move || {
                    let document = compile_page(&project, &page);
                    let (sheet, landmarks) = match page.kind {
                        PreviewPageKind::Html => (
                            Some(page_stylesheet(&project.root, &page.path)),
                            html_landmarks(&page.path),
                        ),
                        PreviewPageKind::Jsx => (None, Vec::new()),
                    };
                    (project, document, sheet, landmarks)
                })
                .await
            },
            move |me, result, ctx| {
                if me.preview.generation != generation {
                    return;
                }
                match result {
                    Ok((project, document, sheet, landmarks)) => {
                        me.preview.document = Some(Arc::new(document));
                        me.preview.page_sheet = sheet.map(Arc::new);
                        me.rebuild_parts(&project, Some(landmarks));
                    }
                    Err(error) => {
                        me.preview.state =
                            LivePreviewState::Error(format!("Could not render the page: {error}"));
                    }
                }
                ctx.notify();
            },
        );
    }

    /// The hover list: components for a React project, page landmarks for a
    /// static one. Every part gets a persistent mouse-state handle up front so
    /// the render pass never has to mutate state.
    fn rebuild_parts(
        &mut self,
        project: &PreviewProject,
        landmarks: Option<Vec<(String, String)>>,
    ) {
        let mut parts: Vec<PreviewPart> = project
            .components
            .iter()
            .map(|component| PreviewPart::Component(component.name.clone()))
            .collect();
        if let Some(landmarks) = landmarks {
            parts.extend(
                landmarks
                    .into_iter()
                    .map(|(label, markup)| PreviewPart::Landmark { label, markup }),
            );
        }
        self.preview.hover_states = parts
            .iter()
            .map(|part| (part.key(), MouseStateHandle::default()))
            .collect();
        self.preview.parts = parts;
    }

    /// Compiles a part's preview the first time it is hovered, then caches it —
    /// re-entering a row must not re-parse anything.
    pub fn hover_live_preview_part(&mut self, key: Option<String>, ctx: &mut ViewContext<Self>) {
        if self.preview.hovered_part == key {
            return;
        }
        self.preview.hovered_part = key.clone();
        if let Some(key) = key {
            self.ensure_part_document(&key);
        }
        ctx.notify();
    }

    pub fn pin_live_preview_part(&mut self, key: String, ctx: &mut ViewContext<Self>) {
        if self.preview.pinned_part.as_deref() == Some(key.as_str()) {
            self.preview.pinned_part = None;
        } else {
            self.ensure_part_document(&key);
            self.preview.pinned_part = Some(key);
        }
        ctx.notify();
    }

    fn ensure_part_document(&mut self, key: &str) {
        if self.preview.hover_documents.contains_key(key) {
            return;
        }
        let LivePreviewState::Ready(project) = &self.preview.state else {
            return;
        };
        let Some(part) = self
            .preview
            .parts
            .iter()
            .find(|part| part.key() == key)
            .cloned()
        else {
            return;
        };
        let document = match &part {
            PreviewPart::Component(name) => compile_component(project, name),
            PreviewPart::Landmark { markup, .. } => {
                let page_path = project
                    .pages
                    .get(self.preview.selected_page)
                    .map(|page| page.path.clone())
                    .unwrap_or_else(|| project.root.clone());
                let sheet = self
                    .preview
                    .page_sheet
                    .clone()
                    .unwrap_or_else(|| Arc::new(Stylesheet::default()));
                Some(compile_html_fragment(
                    &project.root,
                    &page_path,
                    markup,
                    &sheet,
                ))
            }
        };
        if let Some(document) = document {
            self.preview
                .hover_documents
                .insert(key.to_string(), Arc::new(document));
        }
    }

    pub fn zoom_live_preview(&mut self, delta: f32, ctx: &mut ViewContext<Self>) {
        self.preview.zoom = (self.preview.zoom + delta).clamp(0.4, 2.);
        ctx.notify();
    }

    pub fn set_live_preview_from_source(&mut self, from_source: bool, ctx: &mut ViewContext<Self>) {
        self.preview.from_source = from_source;
        if !from_source && self.preview.server_document.is_none() {
            self.reload_live_preview_from_server(ctx);
            return;
        }
        ctx.notify();
    }

    // ── dev server ──────────────────────────────────────────────────────────

    #[cfg(not(target_family = "wasm"))]
    pub fn start_live_preview_server(&mut self, ctx: &mut ViewContext<Self>) {
        let LivePreviewState::Ready(project) = &self.preview.state else {
            return;
        };
        let Some(plan) = project.dev_server.clone() else {
            self.preview.server_error =
                Some("This project has no dev script in its package.json.".to_string());
            ctx.notify();
            return;
        };
        if self
            .preview
            .server
            .as_ref()
            .is_some_and(|server| server.is_running())
        {
            return;
        }
        match DevServerHandle::start(&project.root, &plan) {
            Ok(handle) => {
                self.preview.server = Some(handle);
                self.preview.server_error = None;
                self.preview.from_source = false;
            }
            Err(error) => {
                self.preview.server = None;
                self.preview.server_error = Some(error);
            }
        }
        ctx.notify();
    }

    #[cfg(target_family = "wasm")]
    pub fn start_live_preview_server(&mut self, ctx: &mut ViewContext<Self>) {
        self.preview.server_error =
            Some("Dev servers can only run in the desktop app.".to_string());
        ctx.notify();
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn stop_live_preview_server(&mut self, ctx: &mut ViewContext<Self>) {
        if let Some(server) = self.preview.server.take() {
            server.stop();
        }
        self.preview.server_document = None;
        self.preview.from_source = true;
        ctx.notify();
    }

    #[cfg(target_family = "wasm")]
    pub fn stop_live_preview_server(&mut self, ctx: &mut ViewContext<Self>) {
        ctx.notify();
    }

    /// Fetches the served page and renders the markup that came back. For an
    /// SSR framework this is the real page; for a client-rendered app it is the
    /// mount shell, which is why "From source" stays the default.
    #[cfg(not(target_family = "wasm"))]
    pub fn reload_live_preview_from_server(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(server) = self.preview.server.as_ref() else {
            self.preview.server_error =
                Some("Start the dev server first.".to_string());
            ctx.notify();
            return;
        };
        let snapshot = server.snapshot();
        let Some(url) = snapshot.url else {
            self.preview.server_error =
                Some("The dev server has not reported a URL yet.".to_string());
            ctx.notify();
            return;
        };
        let LivePreviewState::Ready(project) = &self.preview.state else {
            return;
        };
        let root = project.root.clone();
        let generation = self.preview.generation;
        let request_url = url.clone();

        ctx.spawn(
            async move {
                let response = reqwest::Client::new()
                    .get(&request_url)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                let body = response
                    .text()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>((root, body))
            },
            move |me, result, ctx| {
                if me.preview.generation != generation {
                    return;
                }
                match result {
                    Ok((root, body)) => {
                        let entry = root.join("index.html");
                        me.preview.server_document =
                            Some(Arc::new(compile_html(&root, &entry, &body)));
                        me.preview.server_error = None;
                        me.preview.from_source = false;
                    }
                    Err(error) => {
                        me.preview.server_error = Some(format!("Could not load {url}: {error}"));
                    }
                }
                ctx.notify();
            },
        );
    }

    #[cfg(target_family = "wasm")]
    pub fn reload_live_preview_from_server(&mut self, ctx: &mut ViewContext<Self>) {
        ctx.notify();
    }

    #[cfg(not(target_family = "wasm"))]
    fn server_is_running(&self) -> bool {
        self.preview
            .server
            .as_ref()
            .is_some_and(|server| server.is_running())
    }

    #[cfg(target_family = "wasm")]
    fn server_is_running(&self) -> bool {
        false
    }

    #[cfg(not(target_family = "wasm"))]
    fn server_url(&self) -> Option<String> {
        let snapshot = self.preview.server.as_ref()?.snapshot();
        // A server that already exited has no URL worth showing.
        snapshot.exited.is_none().then_some(snapshot.url).flatten()
    }

    /// The exit code of a dev server that stopped on its own, so the panel can
    /// say so instead of leaving a dead "running" state on screen.
    #[cfg(not(target_family = "wasm"))]
    fn server_exit_code(&self) -> Option<i32> {
        self.preview.server.as_ref()?.snapshot().exited
    }

    #[cfg(target_family = "wasm")]
    fn server_exit_code(&self) -> Option<i32> {
        None
    }

    #[cfg(target_family = "wasm")]
    fn server_url(&self) -> Option<String> {
        None
    }

    #[cfg(not(target_family = "wasm"))]
    fn server_log_tail(&self, lines: usize) -> Vec<String> {
        let Some(server) = self.preview.server.as_ref() else {
            return Vec::new();
        };
        let snapshot = server.snapshot();
        let start = snapshot.lines.len().saturating_sub(lines);
        snapshot.lines[start..].to_vec()
    }

    #[cfg(target_family = "wasm")]
    fn server_log_tail(&self, _lines: usize) -> Vec<String> {
        Vec::new()
    }

    // ── rendering ───────────────────────────────────────────────────────────

    pub fn render_live_preview_workspace(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let mut root = Flex::column()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(
                Container::new(self.render_live_preview_header(appearance))
                    .with_horizontal_padding(16.)
                    .with_vertical_padding(12.)
                    .finish(),
            );

        let body = match &self.preview.state {
            LivePreviewState::Idle => self.render_preview_message(
                appearance,
                "No project open",
                "Open a project folder or focus a source file, then reopen Live Preview.",
            ),
            LivePreviewState::Scanning(path) => self.render_preview_message(
                appearance,
                "Scanning project…",
                &format!(
                    "Looking through {} for pages, components, and stylesheets.",
                    path.display()
                ),
            ),
            LivePreviewState::Error(error) => {
                self.render_preview_message(appearance, "Preview failed", error)
            }
            LivePreviewState::Ready(project) if project.is_empty() => self.render_preview_message(
                appearance,
                "Nothing to preview",
                &format!(
                    "Scanned {} files in {} but found no HTML pages or React components.",
                    project.files_scanned,
                    project.root.display()
                ),
            ),
            LivePreviewState::Ready(project) => self.render_preview_body(appearance, project),
        };
        root.add_child(Expanded::new(1., body).finish());
        root.add_child(self.render_live_preview_status(appearance));

        Container::new(root.finish())
            .with_background(theme.background())
            .finish()
    }

    fn render_live_preview_header(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let accent: ColorU = theme.terminal_colors().normal.green.into();
        let subtitle = match &self.preview.state {
            LivePreviewState::Ready(project) => {
                let pages = project.pages.len();
                let components = project.components.len();
                format!(
                    "{} · {pages} pages · {components} components · rendered from source",
                    project.name
                )
            }
            _ => "Render this project's pages and components in the app".to_string(),
        };

        let mut row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                ConstrainedBox::new(
                    Container::new(
                        CoreIcon::LayoutAlt01
                            .to_warpui_icon(ThemeFill::Solid(accent))
                            .finish(),
                    )
                    .with_uniform_padding(8.)
                    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(9.)))
                    .with_background(
                        theme
                            .surface_1()
                            .blend(&ThemeFill::Solid(accent).with_opacity(42)),
                    )
                    .finish(),
                )
                .with_width(34.)
                .with_height(34.)
                .finish(),
            )
            .with_child(
                Container::new(
                    Flex::column()
                        .with_child(self.preview_label(
                            appearance,
                            "Live Preview".to_string(),
                            18.,
                            theme.main_text_color(theme.background()).into(),
                        ))
                        .with_child(self.preview_label(
                            appearance,
                            subtitle,
                            11.,
                            theme.disabled_text_color(theme.background()).into(),
                        ))
                        .finish(),
                )
                .with_margin_left(12.)
                .finish(),
            )
            .with_child(Expanded::new(1., Empty::new().finish()).finish());

        row.add_child(self.preview_chip(
            appearance,
            format!("{}%", (self.preview.zoom * 100.).round() as i32),
            WorkspaceAction::ZoomGenesiPreview(0.),
            false,
        ));
        row.add_child(self.preview_chip(
            appearance,
            "−".to_string(),
            WorkspaceAction::ZoomGenesiPreview(-0.1),
            false,
        ));
        row.add_child(self.preview_chip(
            appearance,
            "+".to_string(),
            WorkspaceAction::ZoomGenesiPreview(0.1),
            false,
        ));
        row.add_child(self.preview_chip(
            appearance,
            "Refresh".to_string(),
            WorkspaceAction::RefreshGenesiPreview,
            false,
        ));
        row.add_child(self.preview_chip(
            appearance,
            "Close".to_string(),
            WorkspaceAction::CloseGenesiPreview,
            false,
        ));
        row.finish()
    }

    fn render_preview_body(
        &self,
        appearance: &Appearance,
        project: &Arc<PreviewProject>,
    ) -> Box<dyn Element> {
        let content = Flex::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(
                ConstrainedBox::new(self.render_preview_sidebar(appearance, project))
                    .with_width(SIDEBAR_WIDTH)
                    .finish(),
            )
            .with_child(
                Expanded::new(
                    1.,
                    Container::new(self.render_preview_sheet(appearance))
                        .with_margin_left(12.)
                        .finish(),
                )
                .finish(),
            )
            .finish();

        // The inline preview card floats over the sheet, anchored just past the
        // sidebar, and stays up until the pointer leaves the row (or, when
        // pinned by a click, until the row is clicked again).
        let Some(card) = self.render_inline_preview_card(appearance) else {
            return Container::new(content)
                .with_horizontal_padding(16.)
                .finish();
        };

        let mut stack = Stack::new();
        stack.add_child(content);
        stack.add_positioned_child(
            card,
            OffsetPositioning::offset_from_parent(
                vec2f(SIDEBAR_WIDTH + 24., 12.),
                ParentOffsetBounds::WindowByPosition,
                ParentAnchor::TopLeft,
                ChildAnchor::TopLeft,
            ),
        );
        Container::new(stack.finish())
            .with_horizontal_padding(16.)
            .finish()
    }

    fn render_preview_sidebar(
        &self,
        appearance: &Appearance,
        project: &Arc<PreviewProject>,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();

        let mut list = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        if !project.pages.is_empty() {
            list.add_child(self.preview_section_label(appearance, "PAGES"));
            for (index, page) in project.pages.iter().enumerate() {
                list.add_child(
                    Container::new(self.render_preview_page_row(appearance, index, page))
                        .with_margin_bottom(6.)
                        .finish(),
                );
            }
        }
        if !self.preview.parts.is_empty() {
            list.add_child(self.preview_section_label(appearance, "COMPONENTS"));
            list.add_child(
                Container::new(self.preview_label(
                    appearance,
                    "Hover a row to render it · click to pin".to_string(),
                    10.,
                    muted,
                ))
                .with_margin_bottom(6.)
                .finish(),
            );
            for part in &self.preview.parts {
                list.add_child(
                    Container::new(self.render_preview_part_row(appearance, part))
                        .with_margin_bottom(6.)
                        .finish(),
                );
            }
        }

        Container::new(
            Flex::column()
                .with_main_axis_size(MainAxisSize::Max)
                .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
                .with_child(
                    Expanded::new(
                        1.,
                        ClippedScrollable::vertical(
                            self.preview.sidebar_scroll.clone(),
                            list.finish(),
                            ScrollbarWidth::Auto,
                            theme.disabled_ui_text_color().into(),
                            theme.active_ui_text_color().into(),
                            Fill::None,
                        )
                        .finish(),
                    )
                    .finish(),
                )
                .with_child(self.render_dev_server_controls(appearance, project))
                .finish(),
        )
        .with_uniform_padding(14.)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(12.)))
        .with_background(theme.surface_1())
        .with_border(Border::all(1.).with_border_fill(theme.outline()))
        .finish()
    }

    fn render_preview_page_row(
        &self,
        appearance: &Appearance,
        index: usize,
        page: &super::live_preview::PreviewPage,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let accent: ColorU = theme.terminal_colors().normal.blue.into();
        let selected = index == self.preview.selected_page;
        let row = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_child(self.preview_label(
                appearance,
                page.title.clone(),
                12.,
                theme.main_text_color(theme.background()).into(),
            ))
            .with_child(self.preview_label(
                appearance,
                page.route.clone(),
                9.,
                theme.disabled_text_color(theme.background()).into(),
            ))
            .finish();

        EventHandler::new(
            Container::new(row)
                .with_uniform_padding(8.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(9.)))
                .with_background(if selected {
                    theme
                        .surface_1()
                        .blend(&ThemeFill::Solid(accent).with_opacity(30))
                } else {
                    theme.surface_1()
                })
                .with_border(Border::all(1.).with_border_fill(if selected {
                    ThemeFill::Solid(accent).with_opacity(63)
                } else {
                    theme.outline()
                }))
                .finish(),
        )
        .on_left_mouse_down(move |ctx, _, _| {
            ctx.dispatch_typed_action(WorkspaceAction::SelectGenesiPreviewPage(index));
            DispatchEventResult::StopPropagation
        })
        .finish()
    }

    fn render_preview_part_row(
        &self,
        appearance: &Appearance,
        part: &PreviewPart,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let accent: ColorU = theme.terminal_colors().normal.magenta.into();
        let key = part.key();
        let pinned = self.preview.pinned_part.as_deref() == Some(key.as_str());
        let active = self.preview.active_part().map(String::as_str) == Some(key.as_str());
        let label = part.label().to_string();
        let kind = part.kind_label().to_string();

        let Some(mouse_state) = self.preview.hover_states.get(&key).cloned() else {
            return Empty::new().finish();
        };

        let text_color: ColorU = theme.main_text_color(theme.background()).into();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();
        let hover_key = key.clone();
        let click_key = key;

        // `Hoverable::new` calls its builder immediately, so the row can be
        // built straight from the borrowed appearance.
        Hoverable::new(mouse_state, |_state| {
            let content = Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(
                    Expanded::new(
                        1.,
                        Flex::column()
                            .with_child(self.preview_label(appearance, label, 12., text_color))
                            .with_child(self.preview_label(appearance, kind, 9., muted))
                            .finish(),
                    )
                    .finish(),
                )
                .finish();

            Container::new(content)
                .with_uniform_padding(8.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(9.)))
                .with_background(if active {
                    theme
                        .surface_1()
                        .blend(&ThemeFill::Solid(accent).with_opacity(30))
                } else {
                    theme.surface_1()
                })
                .with_border(Border::all(1.).with_border_fill(if pinned {
                    ThemeFill::Solid(accent).with_opacity(90)
                } else if active {
                    ThemeFill::Solid(accent).with_opacity(63)
                } else {
                    theme.outline()
                }))
                .finish()
        })
        .on_hover(move |is_hovered, ctx, _, _| {
            ctx.dispatch_typed_action(WorkspaceAction::HoverGenesiPreviewComponent(
                is_hovered.then(|| hover_key.clone()),
            ));
        })
        .on_click(move |ctx, _, _| {
            ctx.dispatch_typed_action(WorkspaceAction::SelectGenesiPreviewComponent(
                click_key.clone(),
            ));
        })
        .finish()
    }

    /// The floating card that renders whatever part is hovered or pinned.
    fn render_inline_preview_card(&self, appearance: &Appearance) -> Option<Box<dyn Element>> {
        let theme = appearance.theme();
        let key = self.preview.active_part()?;
        let document = self.preview.hover_documents.get(key)?;
        let part = self
            .preview
            .parts
            .iter()
            .find(|part| &part.key() == key)?;
        let pinned = self.preview.pinned_part.as_deref() == Some(key.as_str());

        let header = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                Expanded::new(
                    1.,
                    self.preview_label(
                        appearance,
                        part.label().to_string(),
                        12.,
                        theme.main_text_color(theme.background()).into(),
                    ),
                )
                .finish(),
            )
            .with_child(self.preview_label(
                appearance,
                if pinned { "pinned" } else { "preview" }.to_string(),
                10.,
                theme.disabled_text_color(theme.background()).into(),
            ))
            .finish();

        // The card renders the same tree the full page does, just smaller.
        let sheet = Container::new(render_document(
            appearance,
            document,
            PreviewScale(0.85 * self.preview.zoom.clamp(0.6, 1.2)),
        ))
        .with_uniform_padding(10.)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
        .with_background_color(sheet_background(document))
        .finish();

        Some(
            ConstrainedBox::new(
                Container::new(
                    Flex::column()
                        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
                        .with_child(Container::new(header).with_margin_bottom(8.).finish())
                        .with_child(
                            ConstrainedBox::new(Clipped::new(sheet).finish())
                                .with_max_height(HOVER_CARD_MAX_HEIGHT)
                                .finish(),
                        )
                        .finish(),
                )
                .with_uniform_padding(12.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(12.)))
                .with_background(theme.surface_1())
                .with_border(Border::all(1.).with_border_fill(theme.outline()))
                .finish(),
            )
            .with_width(HOVER_CARD_WIDTH)
            .finish(),
        )
    }

    /// The page sheet itself: either the compiled source, or what the dev
    /// server served.
    fn render_preview_sheet(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let document = if self.preview.from_source {
            self.preview.document.as_ref()
        } else {
            self.preview
                .server_document
                .as_ref()
                .or(self.preview.document.as_ref())
        };

        let Some(document) = document else {
            return Container::new(self.render_preview_message(
                appearance,
                "Rendering…",
                "Compiling the page's markup and styles.",
            ))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(12.)))
            .with_background(theme.surface_1())
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .finish();
        };

        let page = render_document(appearance, document, PreviewScale(self.preview.zoom));
        Container::new(
            Clipped::new(
                ClippedScrollable::vertical(
                    self.preview.scroll.clone(),
                    page,
                    ScrollbarWidth::Auto,
                    theme.disabled_ui_text_color().into(),
                    theme.active_ui_text_color().into(),
                    Fill::None,
                )
                .finish(),
            )
            .finish(),
        )
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(12.)))
        .with_background_color(sheet_background(document))
        .with_border(Border::all(1.).with_border_fill(theme.outline()))
        .finish()
    }

    fn render_dev_server_controls(
        &self,
        appearance: &Appearance,
        project: &Arc<PreviewProject>,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let muted: ColorU = theme.disabled_text_color(theme.background()).into();
        let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        let Some(plan) = project.dev_server.as_ref() else {
            column.add_child(self.preview_section_label(appearance, "DEV SERVER"));
            column.add_child(self.preview_label(
                appearance,
                "No dev script found — the preview renders straight from source.".to_string(),
                10.,
                muted,
            ));
            return Container::new(column.finish()).with_margin_top(10.).finish();
        };

        let running = self.server_is_running();
        column.add_child(self.preview_section_label(appearance, "DEV SERVER"));
        column.add_child(self.preview_label(
            appearance,
            format!("{} · {}", plan.framework, plan.display_command()),
            10.,
            muted,
        ));

        let mut buttons = Flex::row().with_cross_axis_alignment(CrossAxisAlignment::Center);
        if running {
            buttons.add_child(self.preview_chip(
                appearance,
                "Stop".to_string(),
                WorkspaceAction::StopGenesiPreviewServer,
                false,
            ));
            buttons.add_child(self.preview_chip(
                appearance,
                "Reload".to_string(),
                WorkspaceAction::ReloadGenesiPreviewFromServer,
                false,
            ));
        } else {
            buttons.add_child(self.preview_chip(
                appearance,
                "Start".to_string(),
                WorkspaceAction::StartGenesiPreviewServer,
                false,
            ));
        }
        column.add_child(Container::new(buttons.finish()).with_margin_top(6.).finish());

        let mut sources = Flex::row().with_cross_axis_alignment(CrossAxisAlignment::Center);
        sources.add_child(self.preview_chip(
            appearance,
            "From source".to_string(),
            WorkspaceAction::SetGenesiPreviewFromSource(true),
            self.preview.from_source,
        ));
        sources.add_child(self.preview_chip(
            appearance,
            "From server".to_string(),
            WorkspaceAction::SetGenesiPreviewFromSource(false),
            !self.preview.from_source,
        ));
        column.add_child(Container::new(sources.finish()).with_margin_top(4.).finish());

        if let Some(url) = self.server_url().filter(|_| running) {
            column.add_child(
                Container::new(self.preview_label(appearance, url, 10., muted))
                    .with_margin_top(6.)
                    .finish(),
            );
        }
        let failure = self
            .preview
            .server_error
            .clone()
            .or_else(|| self.server_exit_code().map(|code| {
                format!("The dev server exited with code {code} — check the log below.")
            }));
        if let Some(failure) = failure {
            column.add_child(
                Container::new(self.preview_label(
                    appearance,
                    failure,
                    10.,
                    theme.ui_error_color().into(),
                ))
                .with_margin_top(6.)
                .finish(),
            );
        }

        let log = self.server_log_tail(6);
        if !log.is_empty() {
            column.add_child(
                Container::new(
                    ClippedScrollable::vertical(
                        self.preview.log_scroll.clone(),
                        self.preview_mono(appearance, log.join("\n"), 9., muted),
                        ScrollbarWidth::Auto,
                        theme.disabled_ui_text_color().into(),
                        theme.active_ui_text_color().into(),
                        Fill::None,
                    )
                    .finish(),
                )
                .with_margin_top(6.)
                .with_uniform_padding(6.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
                .with_background_color(ColorU::new(0, 0, 0, 60))
                .finish(),
            );
        }

        Container::new(column.finish()).with_margin_top(10.).finish()
    }

    fn render_live_preview_status(&self, appearance: &Appearance) -> Box<dyn Element> {
        let theme = appearance.theme();
        let running = self.server_is_running();
        let (color, label): (ColorU, String) = match &self.preview.state {
            LivePreviewState::Ready(_) if running => (
                theme.terminal_colors().normal.green.into(),
                "Dev server running".to_string(),
            ),
            LivePreviewState::Ready(_) => (
                theme.terminal_colors().normal.green.into(),
                "Preview ready".to_string(),
            ),
            LivePreviewState::Scanning(_) => (
                theme.terminal_colors().normal.yellow.into(),
                "Scanning…".to_string(),
            ),
            LivePreviewState::Error(error) => (theme.ui_error_color().into(), error.clone()),
            LivePreviewState::Idle => (
                theme.disabled_text_color(theme.background()).into(),
                "Idle".to_string(),
            ),
        };

        let detail = match &self.preview.state {
            LivePreviewState::Ready(project) => {
                let elements = self
                    .preview
                    .document
                    .as_ref()
                    .map(|document| document.root.node_count())
                    .unwrap_or(0);
                format!(
                    "{} files scanned  ·  {} stylesheets  ·  {elements} elements",
                    project.files_scanned,
                    project.global_css.len()
                )
            }
            _ => String::new(),
        };

        Container::new(
            Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_child(
                    ConstrainedBox::new(
                        Container::new(Empty::new().finish())
                            .with_background_color(color)
                            .with_corner_radius(CornerRadius::with_all(Radius::Percentage(50.)))
                            .finish(),
                    )
                    .with_width(8.)
                    .with_height(8.)
                    .finish(),
                )
                .with_child(
                    Container::new(self.preview_label(appearance, label, 12., color))
                        .with_margin_left(8.)
                        .finish(),
                )
                .with_child(Expanded::new(1., Empty::new().finish()).finish())
                .with_child(self.preview_label(
                    appearance,
                    detail,
                    11.,
                    theme.disabled_text_color(theme.background()).into(),
                ))
                .finish(),
        )
        .with_uniform_padding(14.)
        .finish()
    }

    fn render_preview_message(
        &self,
        appearance: &Appearance,
        title: &str,
        body: &str,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        Container::new(
            Flex::column()
                .with_main_axis_alignment(MainAxisAlignment::Center)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_main_axis_size(MainAxisSize::Max)
                .with_child(self.preview_label(
                    appearance,
                    title.to_string(),
                    16.,
                    theme.main_text_color(theme.background()).into(),
                ))
                .with_child(
                    Container::new(
                        ConstrainedBox::new(self.preview_label_wrapped(
                            appearance,
                            body.to_string(),
                            12.,
                            theme.disabled_text_color(theme.background()).into(),
                        ))
                        .with_max_width(460.)
                        .finish(),
                    )
                    .with_margin_top(8.)
                    .finish(),
                )
                .finish(),
        )
        .with_uniform_padding(40.)
        .finish()
    }

    // ── small styling helpers ───────────────────────────────────────────────

    fn preview_section_label(&self, appearance: &Appearance, text: &str) -> Box<dyn Element> {
        let theme = appearance.theme();
        Container::new(self.preview_label(
            appearance,
            text.to_string(),
            9.,
            theme.disabled_text_color(theme.background()).into(),
        ))
        .with_margin_top(10.)
        .with_margin_bottom(5.)
        .finish()
    }

    fn preview_label(
        &self,
        appearance: &Appearance,
        text: String,
        size: f32,
        color: ColorU,
    ) -> Box<dyn Element> {
        self.preview_text(appearance, text, size, color, false, false)
    }

    fn preview_label_wrapped(
        &self,
        appearance: &Appearance,
        text: String,
        size: f32,
        color: ColorU,
    ) -> Box<dyn Element> {
        self.preview_text(appearance, text, size, color, true, false)
    }

    fn preview_mono(
        &self,
        appearance: &Appearance,
        text: String,
        size: f32,
        color: ColorU,
    ) -> Box<dyn Element> {
        self.preview_text(appearance, text, size, color, true, true)
    }

    fn preview_text(
        &self,
        appearance: &Appearance,
        text: String,
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
            .wrappable_text(text, soft_wrap)
            .with_style(UiComponentStyles {
                font_family_id: Some(family),
                font_size: Some(size),
                font_color: Some(color),
                ..Default::default()
            })
            .build()
            .finish()
    }

    fn preview_chip(
        &self,
        appearance: &Appearance,
        label: String,
        action: WorkspaceAction,
        selected: bool,
    ) -> Box<dyn Element> {
        let text_color = if selected {
            ColorU::new(215, 248, 234, 255)
        } else {
            ColorU::new(228, 231, 236, 255)
        };
        let background = if selected {
            ColorU::new(15, 143, 106, 44)
        } else {
            ColorU::new(255, 255, 255, 14)
        };
        let border = if selected {
            ColorU::new(15, 143, 106, 140)
        } else {
            ColorU::new(255, 255, 255, 32)
        };

        EventHandler::new(
            Container::new(self.preview_label(appearance, label, 11., text_color))
                .with_horizontal_padding(8.)
                .with_vertical_padding(5.)
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(7.)))
                .with_border(Border::all(1.).with_border_color(border))
                .with_background_color(background)
                .with_margin_left(6.)
                .finish(),
        )
        .on_left_mouse_down(move |ctx, _, _| {
            ctx.dispatch_typed_action(action.clone());
            DispatchEventResult::StopPropagation
        })
        .finish()
    }
}
