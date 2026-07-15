//! Interactive graph surface used by the Genesi Project Canvas.
//!
//! The surrounding chrome is built from normal Warp UI components. This
//! element owns the sheet itself so connections, grid dots, hit testing,
//! panning, zooming, and node dragging all share the same coordinate system.

use std::collections::HashMap;

use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::{vec2f, Vector2F};
use warp_core::ui::color::coloru_with_opacity;
use warpui::color::ColorU;
use warpui::elements::{
    AfterLayoutContext, AppContext, CornerRadius, Element, EventContext, Fill, LayoutContext,
    PaintContext, Point, Radius, SizeConstraint,
};
use warpui::event::DispatchedEvent;
use warpui::ClipBounds;

use super::project_canvas::{CanvasEdge, CanvasEdgeKind, CanvasNodeKind, ProjectCanvasGraph};
use crate::workspace::WorkspaceAction;

pub const FORGE_NODE_WIDTH: f32 = 214.;
pub const FORGE_NODE_HEIGHT: f32 = 116.;

pub struct ProjectGraphNodeElement {
    pub id: String,
    pub position: Vector2F,
    pub child: Box<dyn Element>,
    size: Vector2F,
    screen_bounds: Option<RectF>,
}

impl ProjectGraphNodeElement {
    pub fn new(id: String, position: Vector2F, child: Box<dyn Element>) -> Self {
        Self {
            id,
            position,
            child,
            size: Vector2F::zero(),
            screen_bounds: None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct ProjectGraphColors {
    pub background: Fill,
    pub grid: Fill,
    pub page: ColorU,
    pub router: ColorU,
    pub endpoint: ColorU,
}

pub struct ProjectGraphCanvas {
    graph: ProjectCanvasGraph,
    nodes: Vec<ProjectGraphNodeElement>,
    pan: Vector2F,
    zoom: f32,
    drag_active: bool,
    colors: ProjectGraphColors,
    size: Vector2F,
    origin: Option<Point>,
}

impl ProjectGraphCanvas {
    pub fn new(
        graph: ProjectCanvasGraph,
        nodes: Vec<ProjectGraphNodeElement>,
        pan: Vector2F,
        zoom: f32,
        drag_active: bool,
        colors: ProjectGraphColors,
    ) -> Self {
        Self {
            graph,
            nodes,
            pan,
            zoom,
            drag_active,
            colors,
            size: Vector2F::zero(),
            origin: None,
        }
    }

    pub fn finish(self) -> Box<dyn Element> {
        Box::new(self)
    }

    fn node_accent(&self, id: &str) -> ColorU {
        match self.graph.node(id).map(|node| node.kind) {
            Some(CanvasNodeKind::Page) => self.colors.page,
            Some(CanvasNodeKind::Router) => self.colors.router,
            Some(CanvasNodeKind::Endpoint) => self.colors.endpoint,
            None => self.colors.router,
        }
    }

    fn edge_accent(&self, edge: &CanvasEdge) -> ColorU {
        match edge.kind {
            CanvasEdgeKind::PageLink => self.colors.page,
            CanvasEdgeKind::ApiCall | CanvasEdgeKind::InternalCall => self.colors.endpoint,
            CanvasEdgeKind::Defines => self.colors.router,
        }
    }

    fn sheet_to_screen(&self, origin: Vector2F, position: Vector2F) -> Vector2F {
        origin + self.pan + position * self.zoom
    }

    fn draw_segment(ctx: &mut PaintContext, from: Vector2F, to: Vector2F, fill: Fill) {
        let min_x = from.x().min(to.x());
        let min_y = from.y().min(to.y());
        let width = (from.x() - to.x()).abs().max(2.);
        let height = (from.y() - to.y()).abs().max(2.);
        ctx.scene
            .draw_rect_without_hit_recording(RectF::new(vec2f(min_x, min_y), vec2f(width, height)))
            .with_background(fill)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(1.)));
    }

    fn draw_connection(
        &self,
        edge: &CanvasEdge,
        origin: Vector2F,
        positions: &HashMap<&str, Vector2F>,
        ctx: &mut PaintContext,
    ) {
        let (Some(from), Some(to)) = (
            positions.get(edge.from.as_str()),
            positions.get(edge.to.as_str()),
        ) else {
            return;
        };
        let node_size = vec2f(FORGE_NODE_WIDTH, FORGE_NODE_HEIGHT) * self.zoom;
        let start = self.sheet_to_screen(origin, *from) + vec2f(node_size.x(), node_size.y() / 2.);
        let end = self.sheet_to_screen(origin, *to) + vec2f(0., node_size.y() / 2.);
        let middle_x = start.x() + (end.x() - start.x()) * 0.5;
        let fill = Fill::Solid(coloru_with_opacity(self.edge_accent(edge), 72));

        Self::draw_segment(ctx, start, vec2f(middle_x, start.y()), fill);
        Self::draw_segment(
            ctx,
            vec2f(middle_x, start.y()),
            vec2f(middle_x, end.y()),
            fill,
        );
        Self::draw_segment(ctx, vec2f(middle_x, end.y()), end, fill);

        let port_size = (12. * self.zoom).clamp(8., 15.);
        for (center, color) in [
            (start, self.node_accent(&edge.from)),
            (end, self.node_accent(&edge.to)),
        ] {
            ctx.scene
                .draw_rect_without_hit_recording(RectF::new(
                    center - vec2f(port_size / 2., port_size / 2.),
                    vec2f(port_size, port_size),
                ))
                .with_background(self.colors.background)
                .with_border(warpui::elements::Border::all(2.).with_border_color(color))
                .with_corner_radius(CornerRadius::with_all(Radius::Percentage(50.)));
        }
    }
}

impl Element for ProjectGraphCanvas {
    fn layout(
        &mut self,
        constraint: SizeConstraint,
        ctx: &mut LayoutContext,
        app: &AppContext,
    ) -> Vector2F {
        let width = if constraint.max.x().is_finite() {
            constraint.max.x()
        } else {
            900.
        };
        let height = if constraint.max.y().is_finite() {
            constraint.max.y()
        } else {
            560.
        };
        self.size = vec2f(
            width.max(constraint.min.x()),
            height.max(constraint.min.y()),
        );
        let node_size = vec2f(FORGE_NODE_WIDTH, FORGE_NODE_HEIGHT) * self.zoom;
        for node in &mut self.nodes {
            node.size = node
                .child
                .layout(SizeConstraint::new(node_size, node_size), ctx, app);
        }
        self.size
    }

    fn after_layout(&mut self, ctx: &mut AfterLayoutContext, app: &AppContext) {
        for node in &mut self.nodes {
            node.child.after_layout(ctx, app);
        }
    }

    fn paint(&mut self, origin: Vector2F, ctx: &mut PaintContext, app: &AppContext) {
        let bounds = RectF::new(origin, self.size);
        ctx.scene
            .start_layer(ClipBounds::BoundedByActiveLayerAnd(bounds));
        self.origin = Some(Point::from_vec2f(origin, ctx.scene.z_index()));
        ctx.scene
            .draw_rect_with_hit_recording(bounds)
            .with_background(self.colors.background);

        let spacing = (28. * self.zoom).clamp(18., 42.);
        let dot = (1.6 * self.zoom).clamp(1., 2.4);
        let start_x = origin.x() + self.pan.x().rem_euclid(spacing);
        let start_y = origin.y() + self.pan.y().rem_euclid(spacing);
        let mut x = start_x;
        while x < origin.x() + self.size.x() {
            let mut y = start_y;
            while y < origin.y() + self.size.y() {
                ctx.scene
                    .draw_rect_without_hit_recording(RectF::new(vec2f(x, y), vec2f(dot, dot)))
                    .with_background(self.colors.grid)
                    .with_corner_radius(CornerRadius::with_all(Radius::Percentage(50.)));
                y += spacing;
            }
            x += spacing;
        }

        let positions = self
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node.position))
            .collect::<HashMap<_, _>>();
        for edge in &self.graph.edges {
            self.draw_connection(edge, origin, &positions, ctx);
        }

        for node in &mut self.nodes {
            let screen_origin = origin + self.pan + node.position * self.zoom;
            node.screen_bounds = Some(RectF::new(screen_origin, node.size));
            node.child.paint(screen_origin, ctx, app);
        }
        ctx.scene.stop_layer();
    }

    fn dispatch_event(
        &mut self,
        event: &DispatchedEvent,
        ctx: &mut EventContext,
        app: &AppContext,
    ) -> bool {
        for node in self.nodes.iter_mut().rev() {
            if node.child.dispatch_event(event, ctx, app) {
                return true;
            }
        }

        if self.drag_active {
            match event.raw_event() {
                warpui::Event::LeftMouseDragged { position, .. } => {
                    ctx.dispatch_typed_action(WorkspaceAction::UpdateGenesiCanvasDrag(*position));
                    return true;
                }
                warpui::Event::LeftMouseUp { .. } => {
                    ctx.dispatch_typed_action(WorkspaceAction::EndGenesiCanvasDrag);
                    return true;
                }
                _ => {}
            }
        }

        let Some(origin) = self.origin else {
            return false;
        };
        let bounds = RectF::new(origin.xy(), self.size);
        let Some(event) = event.at_z_index(origin.z_index(), ctx) else {
            return false;
        };
        match event {
            warpui::Event::LeftMouseDown { position, .. } if bounds.contains_point(*position) => {
                let node_id = self
                    .nodes
                    .iter()
                    .rev()
                    .find(|node| {
                        node.screen_bounds
                            .is_some_and(|node_bounds| node_bounds.contains_point(*position))
                    })
                    .map(|node| node.id.clone());
                ctx.dispatch_typed_action(WorkspaceAction::BeginGenesiCanvasDrag {
                    node_id,
                    pointer: *position,
                });
                true
            }
            warpui::Event::ScrollWheel {
                position,
                delta,
                modifiers,
                ..
            } if bounds.contains_point(*position) => {
                if modifiers.ctrl || modifiers.cmd {
                    let step = if delta.y() >= 0. { 0.1 } else { -0.1 };
                    ctx.dispatch_typed_action(WorkspaceAction::ZoomGenesiCanvas(step));
                } else {
                    ctx.dispatch_typed_action(WorkspaceAction::PanGenesiCanvas(*delta));
                }
                true
            }
            _ => false,
        }
    }

    fn size(&self) -> Option<Vector2F> {
        Some(self.size)
    }

    fn origin(&self) -> Option<Point> {
        self.origin
    }
}

pub struct ProjectGraphMinimap {
    graph: ProjectCanvasGraph,
    positions: HashMap<String, Vector2F>,
    colors: ProjectGraphColors,
    size: Vector2F,
    origin: Option<Point>,
}

impl ProjectGraphMinimap {
    pub fn new(
        graph: ProjectCanvasGraph,
        positions: HashMap<String, Vector2F>,
        colors: ProjectGraphColors,
    ) -> Self {
        Self {
            graph,
            positions,
            colors,
            size: Vector2F::zero(),
            origin: None,
        }
    }

    pub fn finish(self) -> Box<dyn Element> {
        Box::new(self)
    }
}

impl Element for ProjectGraphMinimap {
    fn layout(
        &mut self,
        constraint: SizeConstraint,
        _ctx: &mut LayoutContext,
        _app: &AppContext,
    ) -> Vector2F {
        self.size = constraint.max.min(vec2f(168., 86.));
        self.size
    }

    fn after_layout(&mut self, _ctx: &mut AfterLayoutContext, _app: &AppContext) {}

    fn paint(&mut self, origin: Vector2F, ctx: &mut PaintContext, _app: &AppContext) {
        self.origin = Some(Point::from_vec2f(origin, ctx.scene.z_index()));
        let max_x = self
            .positions
            .values()
            .map(|position| position.x())
            .fold(800., f32::max)
            + FORGE_NODE_WIDTH;
        let max_y = self
            .positions
            .values()
            .map(|position| position.y())
            .fold(500., f32::max)
            + FORGE_NODE_HEIGHT;
        let scale = (self.size.x() / max_x).min(self.size.y() / max_y).max(0.01);

        ctx.scene
            .draw_rect_with_hit_recording(RectF::new(origin, self.size))
            .with_background(self.colors.background);
        for node in &self.graph.nodes {
            let Some(position) = self.positions.get(&node.id) else {
                continue;
            };
            let accent = match node.kind {
                CanvasNodeKind::Page => self.colors.page,
                CanvasNodeKind::Router => self.colors.router,
                CanvasNodeKind::Endpoint => self.colors.endpoint,
            };
            ctx.scene
                .draw_rect_without_hit_recording(RectF::new(
                    origin + *position * scale,
                    vec2f(
                        (FORGE_NODE_WIDTH * scale).max(5.),
                        (FORGE_NODE_HEIGHT * scale).max(3.),
                    ),
                ))
                .with_background(accent);
        }
        ctx.scene
            .draw_rect_without_hit_recording(RectF::new(origin + vec2f(3., 3.), self.size * 0.55))
            .with_background(Fill::None)
            .with_border(warpui::elements::Border::all(1.).with_border_fill(self.colors.grid));
    }

    fn dispatch_event(
        &mut self,
        _event: &DispatchedEvent,
        _ctx: &mut EventContext,
        _app: &AppContext,
    ) -> bool {
        false
    }

    fn size(&self) -> Option<Vector2F> {
        Some(self.size)
    }

    fn origin(&self) -> Option<Point> {
        self.origin
    }
}
