//! Turns a compiled [`PreviewDocument`] into warpui elements.
//!
//! The renderer is deliberately dumb: every layout decision was already made by
//! [`super::live_preview`], so this file only maps boxes onto `Flex`/`Container`
//! and inline runs onto wrappable text. Keeping it that way is what lets the
//! whole preview be exercised from unit tests without a window.

use std::path::Path;

use markdown_parser::weight::CustomWeight;
use markdown_parser::{
    FormattedText, FormattedTextFragment, FormattedTextLine, FormattedTextStyles,
};
use warpui::assets::asset_cache::AssetSource;
use warpui::color::ColorU;
use warpui::elements::{
    Border, CacheOption, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Element,
    Empty, Expanded, Flex, FormattedTextElement, Image, MainAxisAlignment, MainAxisSize,
    ParentElement, Radius,
};
use warpui::ui_components::components::{UiComponent, UiComponentStyles};

use super::live_preview::{
    Align, ComputedStyle, Display, Edges, FlexDirection, InlineFragment, Justify, PreviewDocument,
    PreviewNode, Rgba, TextAlign,
};
use crate::appearance::Appearance;

/// A preview never renders on the app's own background — a page with no
/// `background` of its own gets a neutral sheet so it reads as a document.
pub const DEFAULT_SHEET_BACKGROUND: ColorU = ColorU {
    r: 250,
    g: 250,
    b: 251,
    a: 255,
};
pub const DEFAULT_SHEET_TEXT: ColorU = ColorU {
    r: 24,
    g: 26,
    b: 30,
    a: 255,
};

/// Scales everything a preview draws. The hover card renders the same tree at a
/// smaller scale than the full page.
#[derive(Debug, Clone, Copy)]
pub struct PreviewScale(pub f32);

impl Default for PreviewScale {
    fn default() -> Self {
        Self(1.)
    }
}

impl PreviewScale {
    fn px(self, value: f32) -> f32 {
        value * self.0
    }

    fn font(self, value: f32) -> f32 {
        (value * self.0).clamp(7., 64.)
    }
}

pub fn to_color(color: Rgba) -> ColorU {
    ColorU::new(color.r, color.g, color.b, color.a)
}

/// The background the sheet paints behind `document`.
pub fn sheet_background(document: &PreviewDocument) -> ColorU {
    document
        .background
        .filter(|color| !color.is_transparent())
        .map(to_color)
        .unwrap_or(DEFAULT_SHEET_BACKGROUND)
}

/// The color text falls back to when the page never sets one. Picked against
/// the sheet background so a dark page keeps readable copy.
pub fn sheet_text_color(document: &PreviewDocument) -> ColorU {
    if let Some(color) = document.color.filter(|color| !color.is_transparent()) {
        return to_color(color);
    }
    let background = sheet_background(document);
    if is_dark(background) {
        ColorU::new(236, 238, 242, 255)
    } else {
        DEFAULT_SHEET_TEXT
    }
}

fn is_dark(color: ColorU) -> bool {
    let luminance =
        0.2126 * color.r as f32 + 0.7152 * color.g as f32 + 0.0722 * color.b as f32;
    luminance < 128.
}

/// Renders a whole document, including its page background.
pub fn render_document(
    appearance: &Appearance,
    document: &PreviewDocument,
    scale: PreviewScale,
) -> Box<dyn Element> {
    let text_color = sheet_text_color(document);
    let body = render_node(appearance, &document.root, text_color, scale);
    Container::new(body)
        .with_background_color(sheet_background(document))
        .finish()
}

/// Renders one node. `inherited_color` is what text uses when the cascade never
/// produced a color for it.
pub fn render_node(
    appearance: &Appearance,
    node: &PreviewNode,
    inherited_color: ColorU,
    scale: PreviewScale,
) -> Box<dyn Element> {
    match node {
        PreviewNode::Box(preview_box) => {
            let style = &preview_box.style;
            if style.display == Display::None {
                return Empty::new().finish();
            }
            let color = style.color.map(to_color).unwrap_or(inherited_color);
            let children: Vec<Box<dyn Element>> = preview_box
                .children
                .iter()
                .filter(|child| child.style().display != Display::None)
                .map(|child| render_node(appearance, child, color, scale))
                .collect();
            let content = lay_out_children(preview_box.children.as_slice(), children, style, scale);
            decorate(content, style, scale)
        }
        PreviewNode::Inline { fragments, style } => {
            let color = style.color.map(to_color).unwrap_or(inherited_color);
            let text = render_inline(appearance, fragments, style, color, scale);
            align_text(text, style)
        }
        PreviewNode::Image { path, alt, style } => render_image(appearance, path.as_deref(), alt, style, inherited_color, scale),
        PreviewNode::Placeholder { label, style } => {
            let color = style.color.map(to_color).unwrap_or(inherited_color);
            render_placeholder(appearance, label, color, scale)
        }
        PreviewNode::Rule { style } => {
            let color = style
                .border_color
                .or(style.background)
                .map(to_color)
                .unwrap_or(ColorU::new(inherited_color.r, inherited_color.g, inherited_color.b, 48));
            ConstrainedBox::new(
                Container::new(Empty::new().finish())
                    .with_background_color(color)
                    .finish(),
            )
            .with_height(scale.px(1.).max(1.))
            .finish()
        }
    }
}

/// Lays children out along the box's axis, inserting `gap` between them.
fn lay_out_children(
    nodes: &[PreviewNode],
    children: Vec<Box<dyn Element>>,
    style: &ComputedStyle,
    scale: PreviewScale,
) -> Box<dyn Element> {
    if children.is_empty() {
        return Empty::new().finish();
    }

    let row = style.display == Display::Flex && style.direction == FlexDirection::Row;
    let gap = scale.px(style.gap);
    let mut flex = if row { Flex::row() } else { Flex::column() };

    flex = flex.with_cross_axis_alignment(match (row, style.display, style.align) {
        (_, Display::Flex, Align::Center) => CrossAxisAlignment::Center,
        (_, Display::Flex, Align::Start) => CrossAxisAlignment::Start,
        (_, Display::Flex, Align::End) => CrossAxisAlignment::End,
        // A block box stretches its children across the content width, which is
        // what makes ordinary page sections fill the sheet.
        _ => CrossAxisAlignment::Stretch,
    });
    if style.display == Display::Flex {
        flex = flex.with_main_axis_alignment(match style.justify {
            Justify::Center => MainAxisAlignment::Center,
            Justify::End => MainAxisAlignment::End,
            Justify::SpaceBetween => MainAxisAlignment::SpaceBetween,
            Justify::SpaceAround => MainAxisAlignment::SpaceEvenly,
            Justify::Start => MainAxisAlignment::Start,
        });
        if row && style.justify != Justify::Start {
            flex = flex.with_main_axis_size(MainAxisSize::Max);
        }
    }

    // Only the nodes that survived the display filter are laid out, so walk the
    // two lists in step to recover each child's own style.
    let visible: Vec<&PreviewNode> = nodes
        .iter()
        .filter(|node| node.style().display != Display::None)
        .collect();
    for (index, child) in children.into_iter().enumerate() {
        let child_style = visible.get(index).map(|node| node.style());
        let mut wrapped = Container::new(child);
        if index > 0 && gap > 0. {
            if row {
                wrapped = wrapped.with_margin_left(gap);
            } else {
                wrapped = wrapped.with_margin_top(gap);
            }
        }
        if let Some(child_style) = child_style {
            let margin = scaled_edges(child_style.margin, scale);
            if !margin.is_zero() {
                wrapped = wrapped
                    .with_margin_top(margin.top)
                    .with_margin_bottom(margin.bottom)
                    .with_margin_left(margin.left)
                    .with_margin_right(margin.right);
            }
        }
        let element = wrapped.finish();
        let grow = child_style.map(|style| style.grow).unwrap_or(0.);
        if grow > 0. {
            flex.add_child(Expanded::new(grow, element).finish());
        } else {
            flex.add_child(element);
        }
    }
    flex.finish()
}

/// Applies a box's own background, border, radius, padding and sizing.
fn decorate(
    content: Box<dyn Element>,
    style: &ComputedStyle,
    scale: PreviewScale,
) -> Box<dyn Element> {
    let padding = scaled_edges(style.padding, scale);
    let mut container = Container::new(content);
    if !padding.is_zero() {
        container = container
            .with_padding_top(padding.top)
            .with_padding_bottom(padding.bottom)
            .with_padding_left(padding.left)
            .with_padding_right(padding.right);
    }
    if let Some(background) = style.background.filter(|color| !color.is_transparent()) {
        container = container.with_background_color(with_opacity(to_color(background), style.opacity));
    }
    if !style.border.is_zero() {
        let width = style
            .border
            .top
            .max(style.border.right)
            .max(style.border.bottom)
            .max(style.border.left);
        let color = style
            .border_color
            .map(to_color)
            .unwrap_or(ColorU::new(0, 0, 0, 40));
        container = container.with_border(Border::all(scale.px(width).max(1.)).with_border_color(color));
    }
    if style.radius > 0. {
        container =
            container.with_corner_radius(CornerRadius::with_all(Radius::Pixels(scale.px(style.radius))));
    }
    let element = container.finish();

    let needs_size =
        style.width.is_some() || style.height.is_some() || style.max_width.is_some() || style.min_height.is_some();
    if !needs_size {
        return element;
    }
    let mut constrained = ConstrainedBox::new(element);
    if let Some(width) = style.width {
        constrained = constrained.with_width(scale.px(width));
    } else if let Some(max_width) = style.max_width {
        constrained = constrained.with_max_width(scale.px(max_width));
    }
    if let Some(height) = style.height {
        constrained = constrained.with_height(scale.px(height));
    } else if let Some(min_height) = style.min_height {
        constrained = constrained.with_min_height(scale.px(min_height));
    }
    let sized = constrained.finish();

    // `margin: 0 auto` on a width-capped box is the centered content column
    // every landing page uses.
    if style.centered_block {
        return Flex::row()
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(sized)
            .finish();
    }
    sized
}

fn scaled_edges(edges: Edges, scale: PreviewScale) -> Edges {
    Edges {
        top: scale.px(edges.top),
        right: scale.px(edges.right),
        bottom: scale.px(edges.bottom),
        left: scale.px(edges.left),
    }
}

fn with_opacity(color: ColorU, opacity: f32) -> ColorU {
    if opacity >= 1. {
        return color;
    }
    ColorU::new(
        color.r,
        color.g,
        color.b,
        (color.a as f32 * opacity.clamp(0., 1.)) as u8,
    )
}

/// Renders one wrappable run. Bold / italic / code / underline come through as
/// formatted fragments so a paragraph keeps its emphasis and still wraps.
fn render_inline(
    appearance: &Appearance,
    fragments: &[InlineFragment],
    style: &ComputedStyle,
    color: ColorU,
    scale: PreviewScale,
) -> Box<dyn Element> {
    if fragments.is_empty() {
        return Empty::new().finish();
    }
    // `<br>` produced hard breaks; split the run into lines on them.
    let mut lines: Vec<Vec<FormattedTextFragment>> = vec![Vec::new()];
    for fragment in fragments {
        for (index, piece) in fragment.text.split('\n').enumerate() {
            if index > 0 {
                lines.push(Vec::new());
            }
            if piece.is_empty() {
                continue;
            }
            let styles = FormattedTextStyles {
                weight: (fragment.bold || style.bold).then_some(CustomWeight::Bold),
                italic: fragment.italic || style.italic,
                underline: fragment.underline,
                strikethrough: false,
                inline_code: fragment.code,
                hyperlink: None,
            };
            if let Some(last) = lines.last_mut() {
                last.push(FormattedTextFragment {
                    text: piece.to_string(),
                    styles,
                });
            }
        }
    }
    lines.retain(|line| !line.is_empty());
    if lines.is_empty() {
        return Empty::new().finish();
    }

    let formatted = FormattedText::new(
        lines
            .into_iter()
            .map(FormattedTextLine::Line)
            .collect::<Vec<_>>(),
    );
    let family = if style.monospace {
        appearance.monospace_font_family()
    } else {
        appearance.ui_font_family()
    };
    Box::new(FormattedTextElement::new(
        formatted,
        scale.font(style.font_size),
        family,
        appearance.monospace_font_family(),
        with_opacity(color, style.opacity),
        Default::default(),
    ))
}

fn align_text(text: Box<dyn Element>, style: &ComputedStyle) -> Box<dyn Element> {
    match style.text_align {
        TextAlign::Start => text,
        TextAlign::Center => Flex::row()
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(text)
            .finish(),
        TextAlign::End => Flex::row()
            .with_main_axis_alignment(MainAxisAlignment::End)
            .with_main_axis_size(MainAxisSize::Max)
            .with_child(text)
            .finish(),
    }
}

/// A local `<img>` loads from disk; a remote or missing one falls back to its
/// alt text so the layout keeps the same shape.
fn render_image(
    appearance: &Appearance,
    path: Option<&Path>,
    alt: &str,
    style: &ComputedStyle,
    inherited_color: ColorU,
    scale: PreviewScale,
) -> Box<dyn Element> {
    let width = style.width.map(|width| scale.px(width)).unwrap_or(scale.px(160.));
    let height = style.height.map(|height| scale.px(height)).unwrap_or(scale.px(110.));

    let Some(path) = path else {
        return placeholder_surface(
            appearance,
            if alt.trim().is_empty() { "image" } else { alt },
            inherited_color,
            scale,
            Some((width, height)),
        );
    };

    let image = Image::new(
        AssetSource::LocalFile {
            path: path.to_string_lossy().to_string(),
        },
        CacheOption::BySize,
    )
    .contain()
    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(scale.px(style.radius))))
    .finish();

    ConstrainedBox::new(image)
        .with_max_width(width)
        .with_height(height)
        .finish()
}

fn render_placeholder(
    appearance: &Appearance,
    label: &str,
    color: ColorU,
    scale: PreviewScale,
) -> Box<dyn Element> {
    placeholder_surface(appearance, label, color, scale, None)
}

/// The dashed-ish chip used for JSX expressions, unresolved components, and
/// images the preview cannot load.
fn placeholder_surface(
    appearance: &Appearance,
    label: &str,
    color: ColorU,
    scale: PreviewScale,
    size: Option<(f32, f32)>,
) -> Box<dyn Element> {
    let muted = ColorU::new(color.r, color.g, color.b, 150);
    let text = appearance
        .ui_builder()
        .wrappable_text(label.to_string(), false)
        .with_style(UiComponentStyles {
            font_family_id: Some(appearance.monospace_font_family()),
            font_size: Some(scale.font(11.)),
            font_color: Some(muted),
            ..Default::default()
        })
        .build()
        .finish();
    let chip = Container::new(text)
        .with_horizontal_padding(scale.px(8.))
        .with_vertical_padding(scale.px(5.))
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(scale.px(6.))))
        .with_background_color(ColorU::new(color.r, color.g, color.b, 16))
        .with_border(Border::all(1.).with_border_color(ColorU::new(color.r, color.g, color.b, 46)))
        .finish();

    match size {
        Some((width, height)) => Flex::row()
            .with_main_axis_alignment(MainAxisAlignment::Center)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(
                ConstrainedBox::new(chip)
                    .with_max_width(width)
                    .with_height(height)
                    .finish(),
            )
            .finish(),
        None => chip,
    }
}
