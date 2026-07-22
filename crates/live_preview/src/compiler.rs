//! Genesi Live Preview — render a frontend project's pages and components
//! natively, inside the app.
//!
//! The module has three layers, all UI-free so they can be unit tested and run
//! off the main thread:
//!
//!   * a small CSS engine ([`Stylesheet`]) covering the subset of selectors and
//!     properties real pages actually lean on;
//!   * an HTML → [`PreviewNode`] compiler that resolves the cascade and folds
//!     phrasing content into wrappable inline runs;
//!   * a JSX/TSX compiler that reaches the same [`PreviewNode`] tree, so React
//!     components can be previewed without a bundler.
//!
//! Project code is never executed while building a preview.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use html5ever::tendril::TendrilSink;
use html5ever::{parse_document, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Root font size used to resolve `rem`, and the fallback viewport used to
/// resolve `vw`/`vh` — the preview surface is a desktop-sized sheet.
const ROOT_FONT_SIZE: f32 = 16.;
const VIEWPORT_WIDTH: f32 = 1280.;
const VIEWPORT_HEIGHT: f32 = 800.;

/// How deep component-in-component resolution goes before a child component is
/// drawn as a labelled placeholder instead of being expanded.
const MAX_COMPONENT_DEPTH: usize = 3;
/// How deep element nesting may go before the tree is truncated.
///
/// Every stage here recurses once per level, so without this a pathologically
/// nested document (generated markup, or a file that is simply broken) blows
/// the stack instead of producing a preview. Real pages sit far below this.
const MAX_NESTING_DEPTH: usize = 64;


// ─────────────────────────────────────────────────────────────────────────────
// Colors and lengths
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub fn is_transparent(self) -> bool {
        self.a == 0
    }
}

/// The named colors worth carrying: CSS has 148, but pages that matter use a
/// small, predictable slice of them.
const NAMED_COLORS: &[(&str, (u8, u8, u8))] = &[
    ("black", (0, 0, 0)),
    ("white", (255, 255, 255)),
    ("red", (255, 0, 0)),
    ("green", (0, 128, 0)),
    ("lime", (0, 255, 0)),
    ("blue", (0, 0, 255)),
    ("yellow", (255, 255, 0)),
    ("cyan", (0, 255, 255)),
    ("aqua", (0, 255, 255)),
    ("magenta", (255, 0, 255)),
    ("fuchsia", (255, 0, 255)),
    ("gray", (128, 128, 128)),
    ("grey", (128, 128, 128)),
    ("silver", (192, 192, 192)),
    ("maroon", (128, 0, 0)),
    ("olive", (128, 128, 0)),
    ("navy", (0, 0, 128)),
    ("teal", (0, 128, 128)),
    ("purple", (128, 0, 128)),
    ("orange", (255, 165, 0)),
    ("gold", (255, 215, 0)),
    ("pink", (255, 192, 203)),
    ("brown", (165, 42, 42)),
    ("beige", (245, 245, 220)),
    ("coral", (255, 127, 80)),
    ("crimson", (220, 20, 60)),
    ("indigo", (75, 0, 130)),
    ("ivory", (255, 255, 240)),
    ("khaki", (240, 230, 140)),
    ("lavender", (230, 230, 250)),
    ("salmon", (250, 128, 114)),
    ("tan", (210, 180, 140)),
    ("tomato", (255, 99, 71)),
    ("turquoise", (64, 224, 208)),
    ("violet", (238, 130, 238)),
    ("wheat", (245, 222, 179)),
    ("skyblue", (135, 206, 235)),
    ("steelblue", (70, 130, 180)),
    ("slategray", (112, 128, 144)),
    ("slategrey", (112, 128, 144)),
    ("darkgray", (169, 169, 169)),
    ("darkgrey", (169, 169, 169)),
    ("lightgray", (211, 211, 211)),
    ("lightgrey", (211, 211, 211)),
    ("dimgray", (105, 105, 105)),
    ("dimgrey", (105, 105, 105)),
    ("whitesmoke", (245, 245, 245)),
    ("gainsboro", (220, 220, 220)),
    ("darkblue", (0, 0, 139)),
    ("darkgreen", (0, 100, 0)),
    ("darkred", (139, 0, 0)),
    ("darkorange", (255, 140, 0)),
    ("darkslategray", (47, 79, 79)),
    ("midnightblue", (25, 25, 112)),
    ("seagreen", (46, 139, 87)),
    ("forestgreen", (34, 139, 34)),
    ("limegreen", (50, 205, 50)),
    ("springgreen", (0, 255, 127)),
    ("royalblue", (65, 105, 225)),
    ("dodgerblue", (30, 144, 255)),
    ("deepskyblue", (0, 191, 255)),
    ("hotpink", (255, 105, 180)),
    ("orchid", (218, 112, 214)),
    ("plum", (221, 160, 221)),
    ("chocolate", (210, 105, 30)),
    ("sienna", (160, 82, 45)),
    ("peru", (205, 133, 63)),
    ("goldenrod", (218, 165, 32)),
    ("firebrick", (178, 34, 34)),
    ("mediumseagreen", (60, 179, 113)),
    ("mediumpurple", (147, 112, 219)),
    ("rebeccapurple", (102, 51, 153)),
];

/// Parses any CSS color the preview understands. Gradients resolve to their
/// first color stop, which keeps gradient-heavy pages readable instead of
/// blank.
pub fn parse_color(value: &str) -> Option<Rgba> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let lower = value.to_ascii_lowercase();

    if lower == "transparent" {
        return Some(Rgba::new(0, 0, 0, 0));
    }
    if lower == "currentcolor" || lower == "inherit" || lower == "initial" || lower == "unset" {
        return None;
    }

    if let Some(hex) = lower.strip_prefix('#') {
        return parse_hex_color(hex);
    }

    if let Some(rest) = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
    {
        let body = rest.strip_suffix(')').unwrap_or(rest);
        let parts = split_css_args(body);
        let channel = |index: usize| -> Option<u8> {
            let raw = parts.get(index)?.trim();
            if let Some(percent) = raw.strip_suffix('%') {
                let value = percent.trim().parse::<f32>().ok()?;
                Some((value / 100. * 255.).clamp(0., 255.) as u8)
            } else {
                Some(raw.parse::<f32>().ok()?.clamp(0., 255.) as u8)
            }
        };
        let alpha = parts
            .get(3)
            .and_then(|raw| parse_alpha(raw))
            .unwrap_or(255);
        return Some(Rgba::new(channel(0)?, channel(1)?, channel(2)?, alpha));
    }

    if let Some(rest) = lower
        .strip_prefix("hsla(")
        .or_else(|| lower.strip_prefix("hsl("))
    {
        let body = rest.strip_suffix(')').unwrap_or(rest);
        let parts = split_css_args(body);
        let hue = parts
            .first()?
            .trim()
            .trim_end_matches("deg")
            .parse::<f32>()
            .ok()?;
        let saturation = parts.get(1)?.trim().trim_end_matches('%').parse::<f32>().ok()? / 100.;
        let lightness = parts.get(2)?.trim().trim_end_matches('%').parse::<f32>().ok()? / 100.;
        let alpha = parts
            .get(3)
            .and_then(|raw| parse_alpha(raw))
            .unwrap_or(255);
        let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
        return Some(Rgba::new(r, g, b, alpha));
    }

    // `linear-gradient(...)`, `radial-gradient(...)`: approximate with the first
    // real color stop so gradient backgrounds still read as colored surfaces.
    if lower.contains("gradient(") {
        let body = lower.split_once('(')?.1;
        for arg in split_css_args(body.trim_end_matches(')')) {
            let candidate = arg
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            if candidate.ends_with("deg") || candidate.starts_with("to") || candidate.is_empty() {
                continue;
            }
            if let Some(color) = parse_color(&candidate) {
                return Some(color);
            }
        }
        return None;
    }

    NAMED_COLORS
        .iter()
        .find(|(name, _)| *name == lower)
        .map(|(_, (r, g, b))| Rgba::new(*r, *g, *b, 255))
}

fn parse_alpha(raw: &str) -> Option<u8> {
    let raw = raw.trim();
    if let Some(percent) = raw.strip_suffix('%') {
        let value = percent.trim().parse::<f32>().ok()?;
        return Some((value / 100. * 255.).clamp(0., 255.) as u8);
    }
    let value = raw.parse::<f32>().ok()?;
    Some((value * 255.).clamp(0., 255.) as u8)
}

fn parse_hex_color(hex: &str) -> Option<Rgba> {
    let digits = hex.trim();
    let expand = |c: char| -> Option<u8> { c.to_digit(16).map(|value| (value * 17) as u8) };
    let pair = |slice: &str| -> Option<u8> { u8::from_str_radix(slice, 16).ok() };
    match digits.len() {
        3 => {
            let mut chars = digits.chars();
            Some(Rgba::new(
                expand(chars.next()?)?,
                expand(chars.next()?)?,
                expand(chars.next()?)?,
                255,
            ))
        }
        4 => {
            let mut chars = digits.chars();
            Some(Rgba::new(
                expand(chars.next()?)?,
                expand(chars.next()?)?,
                expand(chars.next()?)?,
                expand(chars.next()?)?,
            ))
        }
        6 => Some(Rgba::new(
            pair(&digits[0..2])?,
            pair(&digits[2..4])?,
            pair(&digits[4..6])?,
            255,
        )),
        8 => Some(Rgba::new(
            pair(&digits[0..2])?,
            pair(&digits[2..4])?,
            pair(&digits[4..6])?,
            pair(&digits[6..8])?,
        )),
        _ => None,
    }
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let hue = hue.rem_euclid(360.) / 360.;
    let saturation = saturation.clamp(0., 1.);
    let lightness = lightness.clamp(0., 1.);
    if saturation == 0. {
        let value = (lightness * 255.).round() as u8;
        return (value, value, value);
    }
    let q = if lightness < 0.5 {
        lightness * (1. + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2. * lightness - q;
    let channel = |mut t: f32| -> u8 {
        if t < 0. {
            t += 1.;
        }
        if t > 1. {
            t -= 1.;
        }
        let value = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 1. / 2. {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        (value * 255.).round().clamp(0., 255.) as u8
    };
    (
        channel(hue + 1. / 3.),
        channel(hue),
        channel(hue - 1. / 3.),
    )
}

/// Splits a comma-or-space separated CSS argument list, respecting nested
/// parentheses so `rgb(1, 2, 3)` inside a gradient stays one argument.
fn split_css_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in input.chars() {
        match character {
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' | '/' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            ' ' if depth == 0 && !current.trim().is_empty() && args.len() < 3 => {
                // `rgb(0 0 0 / 50%)` space syntax.
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        args.push(current.trim().to_string());
    }
    args
}

/// Resolves a CSS length to device-independent pixels. `percent_basis` is what
/// a `%` resolves against, when the preview knows it.
pub fn parse_length(value: &str, font_size: f32, percent_basis: Option<f32>) -> Option<f32> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value == "auto" || value == "inherit" || value == "initial" {
        return None;
    }
    if let Some(rest) = value.strip_suffix("px") {
        return rest.trim().parse::<f32>().ok();
    }
    if let Some(rest) = value.strip_suffix("rem") {
        return rest.trim().parse::<f32>().ok().map(|n| n * ROOT_FONT_SIZE);
    }
    if let Some(rest) = value.strip_suffix("em") {
        return rest.trim().parse::<f32>().ok().map(|n| n * font_size);
    }
    if let Some(rest) = value.strip_suffix("vw") {
        return rest
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n / 100. * VIEWPORT_WIDTH);
    }
    if let Some(rest) = value.strip_suffix("vh") {
        return rest
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n / 100. * VIEWPORT_HEIGHT);
    }
    if let Some(rest) = value.strip_suffix("pt") {
        return rest.trim().parse::<f32>().ok().map(|n| n * 4. / 3.);
    }
    if let Some(rest) = value.strip_suffix('%') {
        let percent = rest.trim().parse::<f32>().ok()?;
        return percent_basis.map(|basis| percent / 100. * basis);
    }
    // Unitless zero (and bare numbers in `line-height`) land here.
    value.parse::<f32>().ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Stylesheets
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompoundSelector {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

impl CompoundSelector {
    fn is_empty(&self) -> bool {
        self.tag.is_none() && self.id.is_none() && self.classes.is_empty()
    }

    fn matches(&self, element: &ElementSnapshot) -> bool {
        if let Some(tag) = &self.tag {
            if tag != "*" && !tag.eq_ignore_ascii_case(&element.tag) {
                return false;
            }
        }
        if let Some(id) = &self.id {
            if element.id.as_deref() != Some(id.as_str()) {
                return false;
            }
        }
        self.classes
            .iter()
            .all(|class| element.classes.iter().any(|candidate| candidate == class))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Selector {
    /// Right-most compound last; ancestors precede it.
    parts: Vec<CompoundSelector>,
    specificity: u32,
}

#[derive(Debug, Clone)]
struct StyleRule {
    selector: Selector,
    declarations: Vec<(String, String)>,
    order: usize,
}

/// A parsed stylesheet: only the selector shapes and properties the preview can
/// honour survive parsing, so matching stays cheap.
#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    rules: Vec<StyleRule>,
}

/// What the cascade needs to know about one element to match selectors.
#[derive(Debug, Clone, Default)]
pub struct ElementSnapshot {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

impl Stylesheet {
    pub fn parse(source: &str) -> Self {
        let mut sheet = Self::default();
        sheet.apply_block(&strip_comments(source), &[], 0);
        sheet
    }

    /// Merges `other` after `self`, so later sheets win ties — the same order
    /// the browser applies `<link>`ed sheets in.
    pub fn extend(&mut self, other: Stylesheet) {
        let offset = self.rules.len();
        for mut rule in other.rules {
            rule.order += offset;
            self.rules.push(rule);
        }
    }

    /// Applies a rule body to `selectors`: its own declarations first, then any
    /// rules nested inside it.
    ///
    /// Nesting is what makes `.scss` files usable at all, and plain CSS has it
    /// now too. Expressing the top level as a body with no selectors lets one
    /// routine cover the whole grammar.
    fn apply_block(&mut self, block: &str, selectors: &[String], depth: usize) {
        if depth > 8 {
            return;
        }
        let (declaration_text, nested) = split_block(block);
        let declarations = parse_declarations(&declaration_text);
        if !declarations.is_empty() && !selectors.is_empty() {
            for text in selectors {
                if let Some(selector) = parse_selector(text) {
                    let order = self.rules.len();
                    self.rules.push(StyleRule {
                        selector,
                        declarations: declarations.clone(),
                        order,
                    });
                }
            }
        }

        for (prelude, inner) in nested {
            if prelude.starts_with('@') {
                // The block holds ordinary rules (or, nested inside a rule, the
                // parent's own declarations), so carry the selectors through.
                if at_rule_applies(&prelude) {
                    self.apply_block(&inner, selectors, depth + 1);
                }
            } else {
                let expanded = expand_selectors(&prelude, selectors);
                self.apply_block(&inner, &expanded, depth + 1);
            }
        }
    }

    /// Collects every declaration that applies to the element at the end of
    /// `ancestry`, ordered weakest-first so a plain `extend` applies the cascade.
    fn declarations_for(&self, ancestry: &[ElementSnapshot]) -> Vec<(String, String)> {
        let Some(element) = ancestry.last() else {
            return Vec::new();
        };
        let mut matched: Vec<&StyleRule> = self
            .rules
            .iter()
            .filter(|rule| selector_matches(&rule.selector, ancestry, element))
            .collect();
        matched.sort_by_key(|rule| (rule.selector.specificity, rule.order));
        matched
            .into_iter()
            .flat_map(|rule| rule.declarations.iter().cloned())
            .collect()
    }
}

fn selector_matches(
    selector: &Selector,
    ancestry: &[ElementSnapshot],
    element: &ElementSnapshot,
) -> bool {
    let Some(subject) = selector.parts.last() else {
        return false;
    };
    if !subject.matches(element) {
        return false;
    }
    // Walk the remaining compounds right-to-left across the ancestor chain.
    let mut remaining = &selector.parts[..selector.parts.len() - 1];
    let mut index = ancestry.len().saturating_sub(1);
    while let Some(part) = remaining.last() {
        let mut found = false;
        while index > 0 {
            index -= 1;
            if part.matches(&ancestry[index]) {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
        remaining = &remaining[..remaining.len() - 1];
    }
    true
}

/// Splits a rule body into its own declaration text and the rules nested in it.
fn split_block(block: &str) -> (String, Vec<(String, String)>) {
    let mut declarations = String::new();
    let mut nested = Vec::new();
    let mut cursor = 0usize;

    while cursor < block.len() {
        let Some(relative) = block[cursor..].find('{') else {
            declarations.push_str(&block[cursor..]);
            break;
        };
        let brace = cursor + relative;
        let segment = &block[cursor..brace];
        // Whatever precedes the last `;` belongs to this rule; the tail is the
        // nested rule's prelude.
        let split = segment.rfind(';').map(|index| index + 1).unwrap_or(0);
        declarations.push_str(&segment[..split]);
        let prelude = segment[split..].trim().to_string();
        let Some(end) = matching_brace(block, brace + 1) else {
            break;
        };
        nested.push((prelude, block[brace + 1..end].to_string()));
        cursor = end + 1;
    }
    (declarations, nested)
}

/// Resolves a nested prelude against its parents, honouring `&`.
fn expand_selectors(prelude: &str, parents: &[String]) -> Vec<String> {
    let parts = split_top_level(prelude, ',');
    if parents.is_empty() {
        return parts;
    }
    let mut out = Vec::new();
    for parent in parents {
        for part in &parts {
            out.push(if part.contains('&') {
                part.replace('&', parent)
            } else {
                format!("{parent} {part}")
            });
        }
    }
    out
}

/// Whether an at-rule's body is in effect for the preview. The sheet is a
/// desktop-width screen, so print-only blocks would fight the screen styles.
fn at_rule_applies(prelude: &str) -> bool {
    let lower = prelude.to_ascii_lowercase();
    let name = lower.split_whitespace().next().unwrap_or_default();
    match name {
        "@media" => !lower.contains("print"),
        "@supports" | "@layer" | "@scope" | "@container" => true,
        _ => false,
    }
}

fn matching_brace(source: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, character) in source[start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn parse_declarations(block: &str) -> Vec<(String, String)> {
    split_top_level(block, ';')
        .into_iter()
        .filter_map(|declaration| {
            let (property, value) = declaration.split_once(':')?;
            let property = property.trim().to_ascii_lowercase();
            let value = value.trim().trim_end_matches("!important").trim();
            if property.is_empty() || value.is_empty() {
                return None;
            }
            Some((property, value.to_string()))
        })
        .collect()
}

/// Splits on `separator`, ignoring separators nested inside parentheses,
/// brackets, or quotes — `font-family: "A, B", sans-serif` must stay one value.
fn split_top_level(input: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for character in input.chars() {
        match character {
            '"' | '\'' => {
                match quote {
                    Some(open) if open == character => quote = None,
                    None => quote = Some(character),
                    _ => {}
                }
                current.push(character);
            }
            '(' | '[' if quote.is_none() => {
                depth += 1;
                current.push(character);
            }
            ')' | ']' if quote.is_none() => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            character if character == separator && depth == 0 && quote.is_none() => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

/// Pseudo-classes that describe a state the preview can't be in. Rules gated on
/// them are dropped so a hover style never leaks into the resting render.
const STATEFUL_PSEUDOS: &[&str] = &[
    "hover",
    "focus",
    "focus-within",
    "focus-visible",
    "active",
    "visited",
    "target",
    "checked",
    "disabled",
    "before",
    "after",
    "placeholder",
    "selection",
    "backdrop",
    "marker",
];

fn parse_selector(text: &str) -> Option<Selector> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut specificity = 0u32;
    for token in text.split_whitespace() {
        // Combinators carry no compound of their own; descendant matching is a
        // safe superset of child/sibling matching for preview purposes.
        if matches!(token, ">" | "+" | "~") {
            continue;
        }
        let token = token.trim_start_matches(['>', '+', '~']);
        if token.is_empty() {
            continue;
        }
        let compound = parse_compound(token, &mut specificity)?;
        if !compound.is_empty() {
            parts.push(compound);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(Selector {
        parts,
        specificity,
    })
}

fn parse_compound(token: &str, specificity: &mut u32) -> Option<CompoundSelector> {
    let mut compound = CompoundSelector::default();
    let mut chars = token.chars().peekable();
    let mut buffer = String::new();
    let mut kind = '\0';

    let flush = |kind: char, buffer: &mut String, compound: &mut CompoundSelector| {
        if buffer.is_empty() {
            return;
        }
        let value = std::mem::take(buffer);
        match kind {
            '.' => compound.classes.push(value),
            '#' => compound.id = Some(value),
            _ => compound.tag = Some(value.to_ascii_lowercase()),
        }
    };

    while let Some(character) = chars.next() {
        match character {
            '.' | '#' => {
                flush(kind, &mut buffer, &mut compound);
                kind = character;
                *specificity += if character == '#' { 100 } else { 10 };
            }
            ':' => {
                flush(kind, &mut buffer, &mut compound);
                kind = '\0';
                // Consume the pseudo name (and any `::`), then decide.
                let mut name = String::new();
                if chars.peek() == Some(&':') {
                    chars.next();
                }
                while let Some(next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || *next == '-' {
                        name.push(*next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // A trailing functional pseudo like `:not(.a)` — drop its args.
                if chars.peek() == Some(&'(') {
                    let mut depth = 0usize;
                    for next in chars.by_ref() {
                        match next {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if STATEFUL_PSEUDOS.contains(&name.as_str()) {
                    return None;
                }
                if name == "root" && compound.tag.is_none() {
                    compound.tag = Some("html".to_string());
                }
            }
            '[' => {
                flush(kind, &mut buffer, &mut compound);
                kind = '\0';
                // Attribute selectors are matched loosely: skip the predicate and
                // keep whatever tag/class context came with it.
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                }
                *specificity += 10;
            }
            _ => {
                if kind == '\0' && buffer.is_empty() && character != '*' {
                    *specificity += 1;
                }
                buffer.push(character);
            }
        }
    }
    flush(kind, &mut buffer, &mut compound);
    Some(compound)
}

// ─────────────────────────────────────────────────────────────────────────────
// Computed style
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Flex,
    Inline,
    InlineBlock,
    Grid,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    fn uniform(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    pub fn is_zero(self) -> bool {
        self.top == 0. && self.right == 0. && self.bottom == 0. && self.left == 0.
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub direction: FlexDirection,
    pub justify: Justify,
    pub align: Align,
    pub gap: f32,
    pub padding: Edges,
    pub margin: Edges,
    pub background: Option<Rgba>,
    pub color: Option<Rgba>,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub monospace: bool,
    pub uppercase: bool,
    pub text_align: TextAlign,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub max_width: Option<f32>,
    pub min_height: Option<f32>,
    pub border: Edges,
    pub border_color: Option<Rgba>,
    pub radius: f32,
    pub opacity: f32,
    pub grow: f32,
    pub centered_block: bool,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::Block,
            direction: FlexDirection::Row,
            justify: Justify::Start,
            align: Align::Stretch,
            gap: 0.,
            padding: Edges::default(),
            margin: Edges::default(),
            background: None,
            color: None,
            font_size: ROOT_FONT_SIZE,
            bold: false,
            italic: false,
            underline: false,
            monospace: false,
            uppercase: false,
            text_align: TextAlign::Start,
            width: None,
            height: None,
            max_width: None,
            min_height: None,
            border: Edges::default(),
            border_color: None,
            radius: 0.,
            opacity: 1.,
            grow: 0.,
            centered_block: false,
        }
    }
}

impl ComputedStyle {
    /// Everything a child starts from: the inherited properties survive, the
    /// box properties reset.
    fn inherited(&self) -> Self {
        Self {
            color: self.color,
            font_size: self.font_size,
            bold: self.bold,
            italic: self.italic,
            monospace: self.monospace,
            uppercase: self.uppercase,
            text_align: self.text_align,
            ..Self::default()
        }
    }

}

/// The user-agent defaults the preview applies before author styles, so a page
/// with no CSS at all still reads like a document.
fn user_agent_declarations(tag: &str) -> Vec<(&'static str, &'static str)> {
    match tag {
        "h1" => vec![
            ("font-size", "2em"),
            ("font-weight", "700"),
            ("margin-bottom", "12px"),
            ("margin-top", "12px"),
        ],
        "h2" => vec![
            ("font-size", "1.5em"),
            ("font-weight", "700"),
            ("margin-bottom", "10px"),
            ("margin-top", "10px"),
        ],
        "h3" => vec![
            ("font-size", "1.25em"),
            ("font-weight", "700"),
            ("margin-bottom", "8px"),
            ("margin-top", "8px"),
        ],
        "h4" => vec![
            ("font-size", "1.1em"),
            ("font-weight", "700"),
            ("margin-bottom", "6px"),
            ("margin-top", "6px"),
        ],
        "h5" | "h6" => vec![("font-weight", "700"), ("margin-bottom", "6px")],
        "p" => vec![("margin-bottom", "10px")],
        "ul" | "ol" => vec![("padding-left", "18px"), ("margin-bottom", "10px")],
        "li" => vec![("margin-bottom", "4px")],
        "strong" | "b" => vec![("font-weight", "700")],
        "th" => vec![("font-weight", "700"), ("padding", "6px 10px")],
        "em" | "i" => vec![("font-style", "italic")],
        "code" | "kbd" | "samp" | "pre" => vec![("font-family", "monospace")],
        "small" => vec![("font-size", "0.85em")],
        "a" => vec![("text-decoration", "underline")],
        "button" => vec![
            ("padding", "8px 14px"),
            ("border-radius", "6px"),
            ("display", "inline-block"),
        ],
        "input" | "textarea" | "select" => vec![
            ("padding", "8px 10px"),
            ("border", "1px solid #8a8f98"),
            ("border-radius", "6px"),
        ],
        "blockquote" => vec![("padding-left", "12px"), ("margin-bottom", "10px")],
        "table" => vec![("margin-bottom", "10px")],
        "td" => vec![("padding", "6px 10px")],
        "hr" => vec![("height", "1px"), ("margin", "10px 0")],
        "header" | "footer" | "section" | "article" | "nav" | "main" | "aside" | "div" | "form" => {
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Applies one declaration onto `style`. `variables` resolves `var(--x)`.
fn apply_declaration(
    style: &mut ComputedStyle,
    property: &str,
    raw_value: &str,
    variables: &HashMap<String, String>,
    parent_font_size: f32,
) {
    let value = resolve_variables(raw_value, variables);
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let lower = value.to_ascii_lowercase();
    let length = |input: &str| parse_length(input, parent_font_size, None);

    match property {
        "display" => {
            style.display = match lower.as_str() {
                "none" => Display::None,
                "flex" | "inline-flex" => Display::Flex,
                "grid" | "inline-grid" => Display::Grid,
                "inline" => Display::Inline,
                "inline-block" => Display::InlineBlock,
                _ => Display::Block,
            }
        }
        "flex-direction" => {
            style.direction = if lower.starts_with("column") {
                FlexDirection::Column
            } else {
                FlexDirection::Row
            }
        }
        "justify-content" => {
            style.justify = match lower.as_str() {
                "center" => Justify::Center,
                "flex-end" | "end" | "right" => Justify::End,
                "space-between" => Justify::SpaceBetween,
                "space-around" | "space-evenly" => Justify::SpaceAround,
                _ => Justify::Start,
            }
        }
        "align-items" => {
            style.align = match lower.as_str() {
                "center" => Align::Center,
                "flex-end" | "end" => Align::End,
                "flex-start" | "start" => Align::Start,
                _ => Align::Stretch,
            }
        }
        "gap" | "row-gap" | "column-gap" | "grid-gap" => {
            if let Some(gap) = length(lower.split_whitespace().next().unwrap_or(&lower)) {
                style.gap = gap;
            }
        }
        "padding" => style.padding = parse_edges(&lower, parent_font_size),
        "padding-top" => style.padding.top = length(&lower).unwrap_or(style.padding.top),
        "padding-right" => style.padding.right = length(&lower).unwrap_or(style.padding.right),
        "padding-bottom" => style.padding.bottom = length(&lower).unwrap_or(style.padding.bottom),
        "padding-left" => style.padding.left = length(&lower).unwrap_or(style.padding.left),
        "margin" => {
            // `margin: 0 auto` is the classic centered content column.
            if lower.split_whitespace().any(|part| part == "auto") {
                style.centered_block = true;
            }
            style.margin = parse_edges(&lower, parent_font_size);
        }
        "margin-top" => style.margin.top = length(&lower).unwrap_or(style.margin.top),
        "margin-right" | "margin-left" if lower == "auto" => style.centered_block = true,
        "margin-right" => style.margin.right = length(&lower).unwrap_or(style.margin.right),
        "margin-bottom" => style.margin.bottom = length(&lower).unwrap_or(style.margin.bottom),
        "margin-left" => style.margin.left = length(&lower).unwrap_or(style.margin.left),
        "background" | "background-color" | "background-image" => {
            if let Some(color) = parse_color(value) {
                style.background = Some(color);
            }
        }
        "color" => {
            if let Some(color) = parse_color(value) {
                style.color = Some(color);
            }
        }
        "font-size" => {
            if let Some(size) = parse_length(&lower, parent_font_size, Some(parent_font_size)) {
                style.font_size = size.clamp(6., 96.);
            }
        }
        "font-weight" => {
            style.bold = matches!(lower.as_str(), "bold" | "bolder")
                || lower.parse::<u32>().is_ok_and(|weight| weight >= 600);
        }
        "font-style" => style.italic = lower == "italic" || lower == "oblique",
        "font-family" => {
            style.monospace = lower.contains("monospace")
                || lower.contains("mono")
                || lower.contains("courier")
                || lower.contains("consolas")
        }
        "text-decoration" | "text-decoration-line" => style.underline = lower.contains("underline"),
        "text-transform" => style.uppercase = lower == "uppercase",
        "text-align" => {
            style.text_align = match lower.as_str() {
                "center" => TextAlign::Center,
                "right" | "end" => TextAlign::End,
                _ => TextAlign::Start,
            }
        }
        "width" => style.width = parse_length(&lower, parent_font_size, Some(VIEWPORT_WIDTH)),
        "height" => style.height = parse_length(&lower, parent_font_size, Some(VIEWPORT_HEIGHT)),
        "max-width" => {
            style.max_width = parse_length(&lower, parent_font_size, Some(VIEWPORT_WIDTH))
        }
        "min-height" => {
            style.min_height = parse_length(&lower, parent_font_size, Some(VIEWPORT_HEIGHT))
        }
        "border" => {
            let (width, color) = parse_border_shorthand(&lower, parent_font_size);
            if let Some(width) = width {
                style.border = Edges::uniform(width);
            }
            if color.is_some() {
                style.border_color = color;
            }
        }
        "border-width" => {
            if let Some(width) = length(&lower) {
                style.border = Edges::uniform(width);
            }
        }
        "border-color" => style.border_color = parse_color(value).or(style.border_color),
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let (width, color) = parse_border_shorthand(&lower, parent_font_size);
            if let Some(width) = width {
                match property {
                    "border-top" => style.border.top = width,
                    "border-right" => style.border.right = width,
                    "border-bottom" => style.border.bottom = width,
                    _ => style.border.left = width,
                }
            }
            if color.is_some() {
                style.border_color = color;
            }
        }
        "border-radius" => {
            if let Some(radius) =
                parse_length(lower.split_whitespace().next().unwrap_or(&lower), parent_font_size, Some(40.))
            {
                style.radius = radius.clamp(0., 64.);
            }
        }
        "opacity" => {
            if let Ok(opacity) = lower.parse::<f32>() {
                style.opacity = opacity.clamp(0., 1.);
            }
        }
        "flex" | "flex-grow" => {
            if let Some(grow) = lower
                .split_whitespace()
                .next()
                .and_then(|part| part.parse::<f32>().ok())
            {
                style.grow = grow;
            } else if lower == "1" || lower.starts_with("1 ") {
                style.grow = 1.;
            }
        }
        _ => {}
    }
}

fn parse_edges(value: &str, font_size: f32) -> Edges {
    let parts: Vec<f32> = value
        .split_whitespace()
        .map(|part| parse_length(part, font_size, Some(0.)).unwrap_or(0.))
        .collect();
    match parts.len() {
        0 => Edges::default(),
        1 => Edges::uniform(parts[0]),
        2 => Edges {
            top: parts[0],
            right: parts[1],
            bottom: parts[0],
            left: parts[1],
        },
        3 => Edges {
            top: parts[0],
            right: parts[1],
            bottom: parts[2],
            left: parts[1],
        },
        _ => Edges {
            top: parts[0],
            right: parts[1],
            bottom: parts[2],
            left: parts[3],
        },
    }
}

fn parse_border_shorthand(value: &str, font_size: f32) -> (Option<f32>, Option<Rgba>) {
    if value == "none" || value == "0" {
        return (Some(0.), None);
    }
    let mut width = None;
    let mut color = None;
    for part in split_css_args(value).into_iter().flat_map(|arg| {
        arg.split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    }) {
        if matches!(
            part.as_str(),
            "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset"
        ) {
            continue;
        }
        if width.is_none() {
            if let Some(parsed) = parse_length(&part, font_size, None) {
                width = Some(parsed);
                continue;
            }
        }
        if color.is_none() {
            color = parse_color(&part);
        }
    }
    (width.or(Some(1.)), color)
}

/// Substitutes `var(--name, fallback)` recursively (bounded, so a cyclic
/// definition can't hang the preview).
fn resolve_variables(value: &str, variables: &HashMap<String, String>) -> String {
    let mut current = value.to_string();
    for _ in 0..4 {
        let Some(start) = current.find("var(") else {
            return current;
        };
        let Some(end) = find_close_paren(&current, start + 4) else {
            return current;
        };
        let inner = &current[start + 4..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((name, fallback)) => (name.trim(), fallback.trim()),
            None => (inner.trim(), ""),
        };
        let replacement = variables
            .get(name)
            .cloned()
            .unwrap_or_else(|| fallback.to_string());
        current = format!("{}{}{}", &current[..start], replacement, &current[end + 1..]);
    }
    current
}

fn find_close_paren(input: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, character) in input[start..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// The renderable tree
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineFragment {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewBox {
    pub tag: String,
    pub style: ComputedStyle,
    pub children: Vec<PreviewNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PreviewNode {
    /// A laid-out box: block, flex row/column, or grid approximated as a column.
    Box(PreviewBox),
    /// A wrappable run of phrasing content sharing one block style.
    Inline {
        fragments: Vec<InlineFragment>,
        style: ComputedStyle,
    },
    /// `<img>` — resolved against the page's directory when it is a local file.
    Image {
        path: Option<PathBuf>,
        alt: String,
        style: ComputedStyle,
    },
    /// A `{expression}` in JSX, or an unresolvable child component: rendered as
    /// a labelled placeholder so the layout keeps its shape.
    Placeholder { label: String, style: ComputedStyle },
    /// `<hr>` and friends.
    Rule { style: ComputedStyle },
}

impl PreviewNode {
    pub fn style(&self) -> &ComputedStyle {
        match self {
            Self::Box(node) => &node.style,
            Self::Inline { style, .. } => style,
            Self::Image { style, .. } => style,
            Self::Placeholder { style, .. } => style,
            Self::Rule { style } => style,
        }
    }

    /// Total nodes in this subtree — used to keep a runaway page from turning
    /// into a multi-thousand-element frame.
    pub fn node_count(&self) -> usize {
        match self {
            Self::Box(node) => 1 + node.children.iter().map(Self::node_count).sum::<usize>(),
            _ => 1,
        }
    }
}

/// A fully compiled preview: the tree plus what the surface around it needs.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewDocument {
    pub title: String,
    pub root: PreviewNode,
    pub background: Option<Rgba>,
    pub color: Option<Rgba>,
    pub stylesheets: Vec<PathBuf>,
    pub notes: Vec<String>,
}

impl PreviewDocument {
    /// Whether this would draw nothing at all. A target the compiler discards
    /// (SVG internals, an empty wrapper) otherwise yields a card that is
    /// titled but blank, which reads as a bug.
    pub fn is_blank(&self) -> bool {
        !has_visible_content(&self.root)
    }

    fn empty(title: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            root: PreviewNode::Box(PreviewBox {
                tag: "body".to_string(),
                style: ComputedStyle::default(),
                children: Vec::new(),
            }),
            background: None,
            color: None,
            stylesheets: Vec::new(),
            notes: vec![note.into()],
        }
    }
}

/// Whether a node paints anything a reader would notice.
fn has_visible_content(node: &PreviewNode) -> bool {
    match node {
        PreviewNode::Inline { fragments, .. } => {
            fragments.iter().any(|fragment| !fragment.text.trim().is_empty())
        }
        PreviewNode::Image { .. } | PreviewNode::Placeholder { .. } | PreviewNode::Rule { .. } => {
            true
        }
        PreviewNode::Box(preview_box) => {
            if preview_box.style.display == Display::None {
                return false;
            }
            let painted = preview_box
                .style
                .background
                .is_some_and(|color| !color.is_transparent())
                || !preview_box.style.border.is_zero();
            painted || preview_box.children.iter().any(has_visible_content)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HTML → PreviewNode
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DomElement {
    tag: String,
    id: Option<String>,
    classes: Vec<String>,
    attrs: HashMap<String, String>,
    children: Vec<DomNode>,
}

#[derive(Debug, Clone)]
enum DomNode {
    Element(DomElement),
    Text(String),
}

/// Tags whose content is not part of the rendered page.
const DROPPED_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "head", "meta", "link", "title", "base", "svg",
    "path", "iframe", "canvas",
];

/// Tags that participate in an inline run rather than opening a box.
const PHRASING_TAGS: &[&str] = &[
    "span", "a", "strong", "b", "em", "i", "u", "s", "code", "kbd", "samp", "small", "abbr", "sub",
    "sup", "mark", "time", "cite", "q", "var", "del", "ins",
];

fn parse_html_document(html: &str) -> Option<DomElement> {
    let dom = parse_document(RcDom::default(), ParseOpts::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .ok()?;
    let root = convert_handle(&dom.document)?;
    Some(root)
}

fn convert_handle(handle: &Handle) -> Option<DomElement> {
    match &handle.data {
        NodeData::Document => {
            let children: Vec<DomNode> = handle
                .children
                .borrow()
                .iter()
                .filter_map(|child| convert_node(child, 0))
                .collect();
            Some(DomElement {
                tag: "#document".to_string(),
                id: None,
                classes: Vec::new(),
                attrs: HashMap::new(),
                children,
            })
        }
        _ => match convert_node(handle, 0)? {
            DomNode::Element(element) => Some(element),
            DomNode::Text(_) => None,
        },
    }
}

fn convert_node(handle: &Handle, depth: usize) -> Option<DomNode> {
    if depth > MAX_NESTING_DEPTH {
        return None;
    }
    match &handle.data {
        NodeData::Text { contents } => {
            let text = contents.borrow().to_string();
            if text.trim().is_empty() {
                // Keep a single space so `<b>a</b> <b>b</b>` does not collapse.
                if text.contains(' ') || text.contains('\n') {
                    return Some(DomNode::Text(" ".to_string()));
                }
                return None;
            }
            Some(DomNode::Text(text))
        }
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.to_string().to_ascii_lowercase();
            let mut map = HashMap::new();
            for attr in attrs.borrow().iter() {
                map.insert(
                    attr.name.local.to_string().to_ascii_lowercase(),
                    attr.value.to_string(),
                );
            }
            let id = map.get("id").map(|value| value.trim().to_string());
            let classes = map
                .get("class")
                .map(|value| {
                    value
                        .split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let children = handle
                .children
                .borrow()
                .iter()
                .filter_map(|child| convert_node(child, depth + 1))
                .collect();
            Some(DomNode::Element(DomElement {
                tag,
                id,
                classes,
                attrs: map,
                children,
            }))
        }
        _ => None,
    }
}

fn find_element<'a>(root: &'a DomElement, tag: &str) -> Option<&'a DomElement> {
    if root.tag == tag {
        return Some(root);
    }
    for child in &root.children {
        if let DomNode::Element(element) = child {
            if let Some(found) = find_element(element, tag) {
                return Some(found);
            }
        }
    }
    None
}

fn collect_style_sources(root: &DomElement, out: &mut Vec<StyleSource>) {
    if root.tag == "style" {
        let text = root
            .children
            .iter()
            .filter_map(|child| match child {
                DomNode::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if !text.trim().is_empty() {
            out.push(StyleSource::Inline(text));
        }
    }
    if root.tag == "link" {
        let rel = root
            .attrs
            .get("rel")
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        if rel.contains("stylesheet") {
            if let Some(href) = root.attrs.get("href") {
                out.push(StyleSource::Href(href.clone()));
            }
        }
    }
    for child in &root.children {
        if let DomNode::Element(element) = child {
            collect_style_sources(element, out);
        }
    }
}

enum StyleSource {
    Inline(String),
    Href(String),
}

/// Resolves a document-relative URL to a file on disk, or `None` for remote and
/// unresolvable references.
fn resolve_local_asset(base_dir: &Path, project_root: &Path, href: &str) -> Option<PathBuf> {
    let href = href.split(['?', '#']).next().unwrap_or(href).trim();
    if href.is_empty()
        || href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("//")
        || href.starts_with("data:")
    {
        return None;
    }
    let candidate = if let Some(rooted) = href.strip_prefix('/') {
        project_root.join(rooted)
    } else {
        base_dir.join(href)
    };
    let normalized = normalize_path(&candidate);
    normalized.is_file().then_some(normalized)
}

/// Splits a CDN URL into the npm package it names and the path inside it.
///
/// Pages routinely pull Bootstrap, Bulma or an icon font off jsDelivr or
/// unpkg. Skipping those left the page without its reset and its utility
/// classes, which is most of why a preview could look nothing like the real
/// page — links kept the browser's underline, and utility classes did nothing.
pub fn cdn_package_and_path(href: &str) -> Option<(String, String)> {
    const CDN_PREFIXES: &[&str] = &[
        "https://cdn.jsdelivr.net/npm/",
        "http://cdn.jsdelivr.net/npm/",
        "https://unpkg.com/",
        "http://unpkg.com/",
        "https://cdn.skypack.dev/",
        "https://esm.sh/",
    ];
    let specifier = CDN_PREFIXES
        .iter()
        .find_map(|prefix| href.strip_prefix(prefix))?;
    let specifier = specifier.split(['?', '#']).next().unwrap_or(specifier);

    // `@scope/name@version/path` or `name@version/path`.
    let (package, path) = if let Some(scoped) = specifier.strip_prefix('@') {
        let mut parts = scoped.splitn(3, '/');
        let scope = parts.next()?;
        let name = parts.next()?;
        let path = parts.next().unwrap_or_default();
        (format!("@{scope}/{}", strip_version(name)), path.to_string())
    } else {
        let mut parts = specifier.splitn(2, '/');
        let name = parts.next()?;
        let path = parts.next().unwrap_or_default();
        (strip_version(name).to_string(), path.to_string())
    };
    (!path.is_empty()).then_some((package, path))
}

/// `bootstrap@5.3.3` → `bootstrap`. A leading `@` is a scope, not a version.
fn strip_version(name: &str) -> &str {
    match name.rfind('@') {
        Some(0) | None => name,
        Some(index) => &name[..index],
    }
}

/// The `node_modules` copy of a CDN stylesheet, read from disk rather than
/// fetched — the package is almost always installed locally.
fn resolve_cdn_stylesheet(project_root: &Path, href: &str) -> Option<PathBuf> {
    let (package, path) = cdn_package_and_path(href)?;
    // node_modules may sit above the page's own directory in a workspace.
    let mut current = Some(project_root);
    for _ in 0..4 {
        let directory = current?;
        let candidate = normalize_path(&directory.join("node_modules").join(&package).join(&path));
        if candidate.is_file() {
            return Some(candidate);
        }
        current = directory.parent();
    }
    None
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Compiles an HTML file (and the stylesheets it references) into a preview.
pub fn compile_html_file(project_root: &Path, path: &Path) -> PreviewDocument {
    let Ok(html) = fs::read_to_string(path) else {
        return PreviewDocument::empty(
            file_title(path),
            format!("Could not read {}.", path.display()),
        );
    };
    compile_html(project_root, path, &html)
}

/// Compiles HTML text as if it had been loaded from `path` — the same entry
/// point the dev-server fetch uses, so served markup renders identically.
pub fn compile_html(project_root: &Path, path: &Path, html: &str) -> PreviewDocument {
    let base_dir = path.parent().unwrap_or(project_root).to_path_buf();
    let Some(document) = parse_html_document(html) else {
        return PreviewDocument::empty(file_title(path), "The HTML could not be parsed.");
    };

    let mut style_sources = Vec::new();
    collect_style_sources(&document, &mut style_sources);

    let mut sheet = Stylesheet::default();
    let mut stylesheet_paths = Vec::new();
    let mut notes = Vec::new();
    for source in style_sources {
        match source {
            StyleSource::Inline(text) => sheet.extend(Stylesheet::parse(&text)),
            StyleSource::Href(href) => {
                // A CDN link resolves to the installed copy, so a page that
                // gets Bootstrap (or its icon font) off jsDelivr still previews
                // with its reset and utilities applied.
                let resolved = resolve_local_asset(&base_dir, project_root, &href)
                    .or_else(|| resolve_cdn_stylesheet(project_root, &href));
                match resolved {
                    Some(resolved) => {
                        if let Ok(text) = fs::read_to_string(&resolved) {
                            sheet.extend(Stylesheet::parse(&text));
                            stylesheet_paths.push(resolved);
                        }
                    }
                    None => notes.push(format!("Stylesheet not found locally: {href}")),
                }
            }
        }
    }

    let title = find_element(&document, "title")
        .map(|element| text_content(element).trim().to_string())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| file_title(path));

    let body = find_element(&document, "body").cloned().unwrap_or(document);

    let mut compiler = Compiler {
        sheet: &sheet,
        base_dir,
        project_root: project_root.to_path_buf(),
        components: None,
        depth: 0,
        // The whole tree is rebuilt every frame, and the app crate is not
        // optimised in dev builds, so this bound is what keeps a large page
        // from dominating the frame. Pages past it are truncated, not dropped.
        budget: 1_500,
        nesting: 0,
    };
    let ancestry = Vec::new();
    // Compilation starts at <body>, so anything the cascade put on the root
    // element has to be lifted in by hand. `:root { --brand: … }` is how nearly
    // every modern stylesheet declares its palette, and dropping it left every
    // `var()` unresolved.
    let (root_style, root_variables) = resolve_root(&sheet);
    let root = compiler.compile_element(&body, &root_style, &ancestry, &root_variables);

    let background = root.style().background;
    let color = root.style().color;

    PreviewDocument {
        title,
        root,
        background,
        color,
        stylesheets: stylesheet_paths,
        notes,
    }
}

/// The style and custom properties carried by the document's root element.
fn resolve_root(sheet: &Stylesheet) -> (ComputedStyle, HashMap<String, String>) {
    let root = ElementSnapshot {
        tag: "html".to_string(),
        id: None,
        classes: Vec::new(),
    };
    let declarations = sheet.declarations_for(std::slice::from_ref(&root));

    let mut variables = HashMap::new();
    for (property, value) in &declarations {
        if let Some(name) = property.strip_prefix("--") {
            variables.insert(format!("--{name}"), resolve_variables(value, &variables));
        }
    }

    let mut style = ComputedStyle {
        display: Display::Block,
        ..ComputedStyle::default()
    };
    for (property, value) in &declarations {
        if property.starts_with("--") {
            continue;
        }
        apply_declaration(&mut style, property, value, &variables, ROOT_FONT_SIZE);
    }
    (style, variables)
}

fn file_title(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn text_content(element: &DomElement) -> String {
    let mut out = String::new();
    for child in &element.children {
        match child {
            DomNode::Text(text) => out.push_str(text),
            DomNode::Element(child) => out.push_str(&text_content(child)),
        }
    }
    out
}

struct Compiler<'a> {
    sheet: &'a Stylesheet,
    base_dir: PathBuf,
    project_root: PathBuf,
    /// Component index, when compiling JSX: lets `<Card />` expand in place.
    components: Option<&'a HashMap<String, JsxComponentSource>>,
    depth: usize,
    /// Remaining node budget; a page that blows past it is truncated rather
    /// than allowed to stall the frame.
    budget: usize,
    /// Current element nesting, bounded by [`MAX_NESTING_DEPTH`]. The budget
    /// caps how many nodes are produced but says nothing about how deep the
    /// recursion goes, which is a stack overflow rather than a slow frame.
    nesting: usize,
}

impl Compiler<'_> {
    fn compile_element(
        &mut self,
        element: &DomElement,
        parent_style: &ComputedStyle,
        ancestry: &[ElementSnapshot],
        parent_variables: &HashMap<String, String>,
    ) -> PreviewNode {
        let snapshot = ElementSnapshot {
            tag: element.tag.clone(),
            id: element.id.clone(),
            classes: element.classes.clone(),
        };
        let mut chain = ancestry.to_vec();
        chain.push(snapshot);

        let (style, variables) = self.resolve_style(element, parent_style, &chain, parent_variables);

        if style.display == Display::None {
            return PreviewNode::Box(PreviewBox {
                tag: element.tag.clone(),
                style: ComputedStyle {
                    display: Display::None,
                    ..ComputedStyle::default()
                },
                children: Vec::new(),
            });
        }

        match element.tag.as_str() {
            "img" => {
                let src = element.attrs.get("src").cloned().unwrap_or_default();
                return PreviewNode::Image {
                    path: resolve_local_asset(&self.base_dir, &self.project_root, &src),
                    alt: element
                        .attrs
                        .get("alt")
                        .cloned()
                        .filter(|alt| !alt.trim().is_empty())
                        .unwrap_or_else(|| file_title(Path::new(&src))),
                    style,
                };
            }
            "hr" => return PreviewNode::Rule { style },
            "input" => {
                let label = element
                    .attrs
                    .get("placeholder")
                    .or_else(|| element.attrs.get("value"))
                    .cloned()
                    .unwrap_or_else(|| {
                        element
                            .attrs
                            .get("type")
                            .cloned()
                            .unwrap_or_else(|| "text".to_string())
                    });
                return PreviewNode::Inline {
                    fragments: vec![InlineFragment {
                        text: label,
                        ..InlineFragment::default()
                    }],
                    style,
                };
            }
            "textarea" => {
                let label = element
                    .attrs
                    .get("placeholder")
                    .cloned()
                    .unwrap_or_else(|| text_content(element));
                return PreviewNode::Inline {
                    fragments: vec![InlineFragment {
                        text: label,
                        ..InlineFragment::default()
                    }],
                    style: ComputedStyle {
                        min_height: style.min_height.or(Some(64.)),
                        ..style
                    },
                };
            }
            _ => {}
        }

        let children = self.compile_children(element, &style, &chain, &variables);
        PreviewNode::Box(PreviewBox {
            tag: element.tag.clone(),
            style,
            children,
        })
    }

    fn resolve_style(
        &self,
        element: &DomElement,
        parent_style: &ComputedStyle,
        chain: &[ElementSnapshot],
        parent_variables: &HashMap<String, String>,
    ) -> (ComputedStyle, HashMap<String, String>) {
        let mut style = parent_style.inherited();
        let mut variables = parent_variables.clone();
        let parent_font_size = parent_style.font_size;

        let mut declarations: Vec<(String, String)> = user_agent_declarations(&element.tag)
            .into_iter()
            .map(|(property, value)| (property.to_string(), value.to_string()))
            .collect();
        // Utility classes come before the project's own rules, so a real
        // stylesheet (including a compiled Tailwind output) always wins.
        for class in &element.classes {
            declarations.extend(crate::tailwind::declarations_for_class(class));
        }
        declarations.extend(self.sheet.declarations_for(chain));
        if let Some(inline) = element.attrs.get("style") {
            declarations.extend(parse_declarations(inline));
        }

        // Custom properties first: a `--brand` defined on this element must be
        // visible to the `var(--brand)` sitting next to it.
        for (property, value) in &declarations {
            if let Some(name) = property.strip_prefix("--") {
                variables.insert(
                    format!("--{name}"),
                    resolve_variables(value, &variables),
                );
            }
        }
        for (property, value) in &declarations {
            if property.starts_with("--") {
                continue;
            }
            apply_declaration(&mut style, property, value, &variables, parent_font_size);
        }

        if element.tag == "li" && style.display == Display::Block {
            style.padding.left = style.padding.left.max(2.);
        }
        (style, variables)
    }

    fn compile_children(
        &mut self,
        element: &DomElement,
        style: &ComputedStyle,
        chain: &[ElementSnapshot],
        variables: &HashMap<String, String>,
    ) -> Vec<PreviewNode> {
        if self.nesting >= MAX_NESTING_DEPTH {
            return Vec::new();
        }
        let mut children: Vec<PreviewNode> = Vec::new();
        let mut inline_run: Vec<InlineFragment> = Vec::new();

        for child in &element.children {
            if self.budget == 0 {
                break;
            }
            match child {
                DomNode::Text(text) => {
                    push_text_fragment(&mut inline_run, text, style, false, false, false, false);
                }
                DomNode::Element(child_element) => {
                    if DROPPED_TAGS.contains(&child_element.tag.as_str()) {
                        continue;
                    }
                    if child_element.tag == "br" {
                        push_text_fragment(
                            &mut inline_run,
                            "\n",
                            style,
                            false,
                            false,
                            false,
                            false,
                        );
                        continue;
                    }
                    if self.is_phrasing(child_element) {
                        self.collect_inline(child_element, style, chain, variables, &mut inline_run);
                        continue;
                    }
                    if !inline_run.is_empty() {
                        children.push(finish_inline_run(&mut inline_run, style));
                    }
                    self.budget = self.budget.saturating_sub(1);
                    self.nesting += 1;
                    let compiled =
                        self.compile_element(child_element, style, chain, variables);
                    self.nesting -= 1;
                    if compiled.style().display != Display::None {
                        children.push(compiled);
                    }
                }
            }
        }
        if !inline_run.is_empty() {
            children.push(finish_inline_run(&mut inline_run, style));
        }
        children
    }

    /// A phrasing element only stays inline while everything under it is also
    /// phrasing — a `<a>` wrapping a `<div>` has to become a box.
    fn is_phrasing(&self, element: &DomElement) -> bool {
        if !PHRASING_TAGS.contains(&element.tag.as_str()) {
            return false;
        }
        // A styled inline (its own background/padding/border) reads better as a
        // box: that is how badges and pills are built.
        if element.attrs.contains_key("style") || !element.classes.is_empty() {
            return false;
        }
        element.children.iter().all(|child| match child {
            DomNode::Text(_) => true,
            DomNode::Element(child) => self.is_phrasing(child),
        })
    }

    fn collect_inline(
        &mut self,
        element: &DomElement,
        style: &ComputedStyle,
        chain: &[ElementSnapshot],
        variables: &HashMap<String, String>,
        out: &mut Vec<InlineFragment>,
    ) {
        let bold = matches!(element.tag.as_str(), "strong" | "b");
        let italic = matches!(element.tag.as_str(), "em" | "i" | "cite" | "var");
        let code = matches!(element.tag.as_str(), "code" | "kbd" | "samp");
        let underline = matches!(element.tag.as_str(), "a" | "u" | "ins");
        for child in &element.children {
            match child {
                DomNode::Text(text) => {
                    push_text_fragment(out, text, style, bold, italic, code, underline)
                }
                DomNode::Element(child_element) => {
                    if child_element.tag == "br" {
                        push_text_fragment(out, "\n", style, false, false, false, false);
                        continue;
                    }
                    if DROPPED_TAGS.contains(&child_element.tag.as_str()) {
                        continue;
                    }
                    if self.nesting >= MAX_NESTING_DEPTH {
                        continue;
                    }
                    let mut nested = Vec::new();
                    self.nesting += 1;
                    self.collect_inline(child_element, style, chain, variables, &mut nested);
                    self.nesting -= 1;
                    for mut fragment in nested {
                        fragment.bold |= bold;
                        fragment.italic |= italic;
                        fragment.code |= code;
                        fragment.underline |= underline;
                        out.push(fragment);
                    }
                }
            }
        }
    }
}

fn push_text_fragment(
    out: &mut Vec<InlineFragment>,
    text: &str,
    style: &ComputedStyle,
    bold: bool,
    italic: bool,
    code: bool,
    underline: bool,
) {
    let collapsed = collapse_whitespace(text);
    if collapsed.is_empty() {
        return;
    }
    // A lone separator space is only worth keeping between two runs.
    if collapsed == " " && out.is_empty() {
        return;
    }
    let text = if style.uppercase {
        collapsed.to_uppercase()
    } else {
        collapsed
    };
    out.push(InlineFragment {
        text,
        bold: bold || style.bold,
        italic: italic || style.italic,
        code: code || style.monospace,
        underline: underline || style.underline,
    });
}

/// HTML whitespace collapsing: runs of whitespace become one space, and a
/// literal `\n` marker (from `<br>`) survives as a hard break.
fn collapse_whitespace(text: &str) -> String {
    if text == "\n" {
        return "\n".to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(character);
            last_was_space = false;
        }
    }
    out
}

fn finish_inline_run(run: &mut Vec<InlineFragment>, style: &ComputedStyle) -> PreviewNode {
    let mut fragments = std::mem::take(run);
    // Trim the edges of the run so wrapped paragraphs do not start with a space.
    if let Some(first) = fragments.first_mut() {
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = fragments.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    fragments.retain(|fragment| !fragment.text.is_empty());
    PreviewNode::Inline {
        fragments,
        style: ComputedStyle {
            // The run is drawn inside the parent box, so its own box decoration
            // has already been applied by that box.
            padding: Edges::default(),
            margin: Edges::default(),
            background: None,
            border: Edges::default(),
            display: Display::Block,
            ..style.clone()
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JSX / TSX → PreviewNode
// ─────────────────────────────────────────────────────────────────────────────

/// A component the preview can compile: its name, where it lives, and the JSX
/// markup it returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsxComponentSource {
    pub name: String,
    pub path: PathBuf,
    pub line: usize,
    pub markup: String,
    /// Stylesheets `import`ed by the file the component lives in.
    pub stylesheets: Vec<PathBuf>,
}

/// Finds every component in a `.jsx`/`.tsx`/`.js`/`.ts` file.
///
/// Components are located by their *declaration* rather than by walking back
/// from a `return`: a component whose body opens with a hook (`const [x, setX]
/// = useState()`) would otherwise resolve its name to the nearest preceding
/// `const`, find `[x,` where a name should be, and be dropped — which is to say
/// almost every real component.
pub fn extract_jsx_components(
    project_root: &Path,
    path: &Path,
    source: &str,
) -> Vec<JsxComponentSource> {
    let stylesheets = jsx_imported_stylesheets(project_root, path, source);
    let declarations = component_declarations(source);
    let mut components: Vec<JsxComponentSource> = Vec::new();

    for (index, (body_start, name)) in declarations.iter().enumerate() {
        if components.iter().any(|existing| existing.name == *name) {
            continue;
        }
        // Search only up to the next component declaration, so one component
        // cannot claim the markup of the one after it.
        let end = declarations
            .get(index + 1)
            .map(|(next, _)| *next)
            .unwrap_or(source.len());
        // `const Button = styled.button` has no JSX at all; its markup is the
        // base element carrying the template literal's declarations.
        if let Some((tag, css)) = styled_component_at(source, *body_start) {
            let declarations = styled_declarations(&css);
            components.push(JsxComponentSource {
                name: name.clone(),
                path: path.to_path_buf(),
                line: line_of(source, *body_start),
                markup: format!("<{tag} style=\"{}\">{name}</{tag}>", declarations.replace('"', "'")),
                stylesheets: stylesheets.clone(),
            });
            continue;
        }

        let Some(window) = source.get(*body_start..end) else {
            continue;
        };
        let Some(markup) = first_jsx_element(window) else {
            continue;
        };
        components.push(JsxComponentSource {
            name: name.clone(),
            path: path.to_path_buf(),
            line: line_of(source, *body_start),
            markup,
            stylesheets: stylesheets.clone(),
        });
    }
    components
}

/// 1-based line containing `offset`.
fn line_of(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())].lines().count().max(1)
}

/// A `styled.button` / `styled(Card)` declaration, as its base tag and the CSS
/// in its template literal.
///
/// CSS-in-JS keeps a component's styles in the source rather than in any
/// stylesheet, so without this a styled-components or emotion project previews
/// completely unstyled.
fn styled_component_at(source: &str, from: usize) -> Option<(String, String)> {
    let rest = source.get(from..)?;
    let head: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    let after_equals = rest.trim_start().strip_prefix('=')?.trim_start();
    let _ = head;

    let styled = after_equals.strip_prefix("styled")?;
    // `styled.button` names the tag; `styled(Card)` wraps another component,
    // which has no tag of its own to draw.
    let (tag, remainder) = if let Some(dotted) = styled.strip_prefix('.') {
        let tag: String = dotted
            .chars()
            .take_while(|c| c.is_alphanumeric())
            .collect();
        if tag.is_empty() {
            return None;
        }
        let remainder = &dotted[tag.len()..];
        (tag, remainder)
    } else if styled.starts_with('(') {
        ("div".to_string(), styled)
    } else {
        return None;
    };

    // Skip `.attrs(...)` and the wrapped-component parentheses.
    let backtick = remainder.find('`')?;
    let body = &remainder[backtick + 1..];
    let end = body.find('`')?;
    Some((tag, body[..end].to_string()))
}

/// Flattens a CSS-in-JS template literal into inline declarations: `${…}`
/// interpolations depend on props the preview does not have, and nested blocks
/// are states or descendants that the resting render does not show.
fn styled_declarations(css: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut chars = css.chars().peekable();
    let mut buffer = String::new();

    while let Some(character) = chars.next() {
        match character {
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                let mut nested = 1usize;
                for next in chars.by_ref() {
                    match next {
                        '{' => nested += 1,
                        '}' => {
                            nested -= 1;
                            if nested == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                // Drop the declaration this interpolation was part of.
                buffer.clear();
            }
            '{' => {
                depth += 1;
                buffer.clear();
            }
            '}' => {
                depth = depth.saturating_sub(1);
                buffer.clear();
            }
            ';' if depth == 0 => {
                if buffer.contains(':') {
                    out.push_str(buffer.trim());
                    out.push(';');
                }
                buffer.clear();
            }
            _ => buffer.push(character),
        }
    }
    if depth == 0 && buffer.contains(':') {
        out.push_str(buffer.trim());
        out.push(';');
    }
    out
}

/// Every capitalised declaration that could be a component, as
/// `(offset just past the name, name)`, in source order.
fn component_declarations(source: &str) -> Vec<(usize, String)> {
    const KEYWORDS: &[&str] = &["function ", "const ", "let ", "var ", "class "];
    let bytes = source.as_bytes();
    let mut found = Vec::new();

    for keyword in KEYWORDS {
        for (index, _) in source.match_indices(keyword) {
            // Must start a token: `myconst Foo` is not a declaration.
            if index > 0 {
                let previous = bytes[index - 1] as char;
                if previous.is_alphanumeric() || previous == '_' || previous == '$' {
                    continue;
                }
            }
            let name_start = index + keyword.len();
            let name: String = source[name_start..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            // Capitalisation is the convention that separates a component from
            // an ordinary helper or variable.
            if name.is_empty() || !name.chars().next().is_some_and(char::is_uppercase) {
                continue;
            }
            let after = name_start + name.len();
            // `const Card =` and `const Card: FC<P> =` are components;
            // `const Card` alone (or a destructuring pattern) is not.
            if *keyword != "function " && *keyword != "class " {
                let next = source[after..]
                    .chars()
                    .find(|c| !c.is_whitespace())
                    .unwrap_or(' ');
                if next != '=' && next != ':' {
                    continue;
                }
            }
            found.push((after, name));
        }
    }
    found.sort_by_key(|(offset, _)| *offset);
    found
}

/// The first JSX element in a declaration body, whether it is returned from a
/// block or is the concise body of an arrow function.
fn first_jsx_element(window: &str) -> Option<String> {
    let bytes = window.as_bytes();
    let mut anchors: Vec<usize> = Vec::new();
    for (index, _) in window.match_indices("return") {
        if index > 0 {
            let previous = bytes[index - 1] as char;
            if previous.is_alphanumeric() || previous == '_' || previous == '.' {
                continue;
            }
        }
        anchors.push(index + "return".len());
    }
    for (index, _) in window.match_indices("=>") {
        anchors.push(index + "=>".len());
    }
    anchors.sort_unstable();

    for anchor in anchors {
        let Some(open) = find_jsx_open(window, anchor) else {
            continue;
        };
        let Some((markup, _)) = read_jsx_element(window, open) else {
            continue;
        };
        if !markup.trim().is_empty() {
            return Some(markup);
        }
    }
    None
}

/// After `return`, skip whitespace and an optional wrapping `(` to find the
/// `<` that opens the JSX element.
fn find_jsx_open(source: &str, from: usize) -> Option<usize> {
    let mut index = from;
    let bytes = source.as_bytes();
    while index < bytes.len() {
        match bytes[index] as char {
            character if character.is_whitespace() => index += 1,
            '(' => index += 1,
            '<' => return Some(index),
            _ => return None,
        }
    }
    None
}

/// Reads one balanced JSX element starting at `<`, returning its source text.
fn read_jsx_element(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut depth = 0i32;
    let mut braces = 0i32;
    let mut quote: Option<char> = None;

    while index < bytes.len() {
        let character = bytes[index] as char;
        match character {
            '"' | '\'' | '`' if braces >= 0 => match quote {
                Some(open) if open == character => quote = None,
                None => quote = Some(character),
                _ => {}
            },
            _ if quote.is_some() => {}
            '{' => braces += 1,
            '}' => braces -= 1,
            '<' if braces == 0 => {
                if bytes.get(index + 1) == Some(&b'/') {
                    depth -= 1;
                    // Consume through the matching `>`.
                    while index < bytes.len() && bytes[index] != b'>' {
                        index += 1;
                    }
                    if depth <= 0 {
                        return Some((source[start..=index.min(bytes.len() - 1)].to_string(), index));
                    }
                } else {
                    // Read the tag; a `/>` closes it immediately.
                    let tag_start = index;
                    let mut self_closing = false;
                    let mut inner_quote: Option<char> = None;
                    let mut inner_braces = 0i32;
                    index += 1;
                    while index < bytes.len() {
                        let inner = bytes[index] as char;
                        match inner {
                            '"' | '\'' => match inner_quote {
                                Some(open) if open == inner => inner_quote = None,
                                None => inner_quote = Some(inner),
                                _ => {}
                            },
                            _ if inner_quote.is_some() => {}
                            '{' => inner_braces += 1,
                            '}' => inner_braces -= 1,
                            '/' if inner_braces == 0 && bytes.get(index + 1) == Some(&b'>') => {
                                self_closing = true;
                            }
                            '>' if inner_braces == 0 => break,
                            _ => {}
                        }
                        index += 1;
                    }
                    if !self_closing {
                        depth += 1;
                    } else if depth == 0 {
                        return Some((source[start..=index.min(bytes.len() - 1)].to_string(), index));
                    }
                    let _ = tag_start;
                }
            }
            _ => {}
        }
        index += 1;
        if index.saturating_sub(start) > 200_000 {
            return None;
        }
    }
    None
}

/// `import './App.css'` / `import styles from "./x.module.css"`.
fn jsx_imported_stylesheets(project_root: &Path, path: &Path, source: &str) -> Vec<PathBuf> {
    let base_dir = path.parent().unwrap_or(project_root);
    let mut out = Vec::new();
    for line in source.lines().take(200) {
        let line = line.trim();
        if !line.starts_with("import") {
            continue;
        }
        let Some(start) = line.find(['"', '\'']) else {
            continue;
        };
        let quote = line.as_bytes()[start] as char;
        let Some(end) = line[start + 1..].find(quote) else {
            continue;
        };
        let specifier = &line[start + 1..start + 1 + end];
        if !specifier.ends_with(".css") && !specifier.ends_with(".scss") {
            continue;
        }
        if let Some(resolved) = resolve_local_asset(base_dir, project_root, specifier) {
            out.push(resolved);
        }
    }
    out
}

/// Converts JSX markup into the same DOM shape HTML uses, so both paths share
/// one cascade and one compiler.
fn parse_jsx_markup(markup: &str) -> Option<DomElement> {
    let mut cursor = 0usize;
    parse_jsx_node(markup, &mut cursor)
}

fn parse_jsx_node(markup: &str, cursor: &mut usize) -> Option<DomElement> {
    let bytes = markup.as_bytes();
    while *cursor < bytes.len() && (bytes[*cursor] as char).is_whitespace() {
        *cursor += 1;
    }
    if *cursor >= bytes.len() || bytes[*cursor] != b'<' {
        return None;
    }
    *cursor += 1;

    // Fragment shorthand `<>` / `</>`.
    let mut tag = String::new();
    while *cursor < bytes.len() {
        let character = bytes[*cursor] as char;
        if character.is_alphanumeric() || character == '_' || character == '.' || character == '-' {
            tag.push(character);
            *cursor += 1;
        } else {
            break;
        }
    }
    let tag = if tag.is_empty() {
        "fragment".to_string()
    } else {
        tag
    };

    let mut attrs = HashMap::new();
    let mut self_closing = false;
    while *cursor < bytes.len() {
        while *cursor < bytes.len() && (bytes[*cursor] as char).is_whitespace() {
            *cursor += 1;
        }
        if *cursor >= bytes.len() {
            return None;
        }
        match bytes[*cursor] {
            b'>' => {
                *cursor += 1;
                break;
            }
            b'/' => {
                self_closing = true;
                *cursor += 1;
                continue;
            }
            _ => {}
        }
        let mut name = String::new();
        while *cursor < bytes.len() {
            let character = bytes[*cursor] as char;
            if character.is_alphanumeric() || character == '-' || character == '_' {
                name.push(character);
                *cursor += 1;
            } else {
                break;
            }
        }
        if name.is_empty() {
            *cursor += 1;
            continue;
        }
        if *cursor < bytes.len() && bytes[*cursor] == b'=' {
            *cursor += 1;
            let value = read_jsx_attribute_value(markup, cursor);
            attrs.insert(name.to_ascii_lowercase(), value);
        } else {
            attrs.insert(name.to_ascii_lowercase(), "true".to_string());
        }
    }

    let mut element = DomElement {
        tag: normalize_jsx_tag(&tag),
        id: attrs.get("id").cloned(),
        classes: attrs
            .get("classname")
            .or_else(|| attrs.get("class"))
            .map(|value| class_names_from_expression(value))
            .unwrap_or_default(),
        attrs: normalize_jsx_attrs(attrs, &tag),
        children: Vec::new(),
    };
    if self_closing {
        return Some(element);
    }

    // Children, until the matching close tag.
    let mut text = String::new();
    while *cursor < bytes.len() {
        match bytes[*cursor] {
            b'<' if bytes.get(*cursor + 1) == Some(&b'/') => {
                while *cursor < bytes.len() && bytes[*cursor] != b'>' {
                    *cursor += 1;
                }
                *cursor += 1;
                break;
            }
            b'<' => {
                if !text.trim().is_empty() {
                    element.children.push(DomNode::Text(std::mem::take(&mut text)));
                } else {
                    text.clear();
                }
                match parse_jsx_node(markup, cursor) {
                    Some(child) => element.children.push(DomNode::Element(child)),
                    None => break,
                }
            }
            b'{' => {
                if !text.trim().is_empty() {
                    element.children.push(DomNode::Text(std::mem::take(&mut text)));
                } else {
                    text.clear();
                }
                let expression = read_braced_expression(markup, cursor);
                if let Some(literal) = string_literal_value(&expression) {
                    element.children.push(DomNode::Text(literal));
                } else if !expression.trim().is_empty() {
                    element.children.push(DomNode::Element(DomElement {
                        tag: "genesi-expression".to_string(),
                        id: None,
                        classes: Vec::new(),
                        attrs: HashMap::from([("data-expression".to_string(), expression)]),
                        children: Vec::new(),
                    }));
                }
            }
            _ => {
                text.push(bytes[*cursor] as char);
                *cursor += 1;
            }
        }
    }
    if !text.trim().is_empty() {
        element.children.push(DomNode::Text(text));
    }
    Some(element)
}

/// The class names a `className` yields.
///
/// A plain list is the easy case. The rest is CSS Modules and its neighbours:
/// `styles.card`, `styles['card']`, template literals combining several, and
/// `clsx(...)`-style calls. The rule is that quoted and template text
/// contributes its words, and a member access contributes the member — while a
/// bare identifier (`clsx`, `styles`, `props`) contributes nothing.
///
/// A module's source selector is the plain name; the build step's hashing
/// happens well after anything the preview reads.
fn class_names_from_expression(value: &str) -> Vec<String> {
    let looks_like_code = value.contains(['.', '$', '(', '`', '\'', '"', '[']);
    if !looks_like_code {
        return value.split_whitespace().map(str::to_string).collect();
    }

    let mut names: Vec<String> = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0usize;

    let push_words = |names: &mut Vec<String>, text: &str| {
        names.extend(text.split_whitespace().map(str::to_string));
    };

    while index < bytes.len() {
        match bytes[index] {
            b'"' | b'\'' => {
                let quote = bytes[index];
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                push_words(&mut names, value.get(start..index).unwrap_or_default());
                index += 1;
            }
            b'`' => {
                index += 1;
                let mut text_start = index;
                while index < bytes.len() && bytes[index] != b'`' {
                    if bytes[index] == b'$' && bytes.get(index + 1) == Some(&b'{') {
                        push_words(&mut names, value.get(text_start..index).unwrap_or_default());
                        index += 2;
                        let start = index;
                        let mut depth = 1usize;
                        while index < bytes.len() {
                            match bytes[index] {
                                b'{' => depth += 1,
                                b'}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            index += 1;
                        }
                        // The substitution is an expression in its own right.
                        names.extend(class_names_from_expression(
                            value.get(start..index).unwrap_or_default(),
                        ));
                        index += 1;
                        text_start = index;
                        continue;
                    }
                    index += 1;
                }
                push_words(&mut names, value.get(text_start..index).unwrap_or_default());
                index += 1;
            }
            b'.' | b'[' => {
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'\'' | b'"' | b' ')
                {
                    index += 1;
                }
                let start = index;
                while index < bytes.len() {
                    let next = bytes[index] as char;
                    if next.is_alphanumeric() || next == '_' || next == '-' {
                        index += 1;
                    } else {
                        break;
                    }
                }
                if let Some(name) = value.get(start..index) {
                    if !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) {
                        names.push(name.to_string());
                    }
                }
            }
            _ => index += 1,
        }
    }

    if names.is_empty() {
        return value.split_whitespace().map(str::to_string).collect();
    }
    names.dedup();
    names
}

fn read_jsx_attribute_value(markup: &str, cursor: &mut usize) -> String {
    let bytes = markup.as_bytes();
    if *cursor >= bytes.len() {
        return String::new();
    }
    match bytes[*cursor] {
        b'"' | b'\'' => {
            let quote = bytes[*cursor];
            *cursor += 1;
            let start = *cursor;
            while *cursor < bytes.len() && bytes[*cursor] != quote {
                *cursor += 1;
            }
            let value = markup[start..*cursor].to_string();
            *cursor += 1;
            value
        }
        b'{' => {
            let expression = read_braced_expression(markup, cursor);
            // `style={{ color: 'red' }}` and `className={'a'}` are the two forms
            // worth unwrapping; anything else stays as the raw expression.
            let trimmed = expression.trim();
            if let Some(object) = trimmed.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
                jsx_style_object_to_css(object)
            } else if let Some(literal) = string_literal_value(trimmed) {
                literal
            } else {
                trimmed.to_string()
            }
        }
        _ => String::new(),
    }
}

/// Reads `{ ... }` with balanced braces, returning the inside.
fn read_braced_expression(markup: &str, cursor: &mut usize) -> String {
    let bytes = markup.as_bytes();
    if *cursor >= bytes.len() || bytes[*cursor] != b'{' {
        return String::new();
    }
    let start = *cursor + 1;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    while *cursor < bytes.len() {
        let character = bytes[*cursor] as char;
        match character {
            '"' | '\'' | '`' => match quote {
                Some(open) if open == character => quote = None,
                None => quote = Some(character),
                _ => {}
            },
            _ if quote.is_some() => {}
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let value = markup[start..*cursor].to_string();
                    *cursor += 1;
                    return value;
                }
            }
            _ => {}
        }
        *cursor += 1;
    }
    String::new()
}

fn string_literal_value(expression: &str) -> Option<String> {
    let trimmed = expression.trim();
    for quote in ['"', '\'', '`'] {
        if trimmed.starts_with(quote) && trimmed.ends_with(quote) && trimmed.len() >= 2 {
            let inner = &trimmed[1..trimmed.len() - 1];
            // A template literal with a substitution is not a plain string.
            if quote == '`' && inner.contains("${") {
                return None;
            }
            if !inner.contains(quote) {
                return Some(inner.to_string());
            }
        }
    }
    None
}

/// `{ backgroundColor: '#fff', fontSize: 12 }` → `background-color:#fff;font-size:12px`
fn jsx_style_object_to_css(object: &str) -> String {
    let mut declarations = Vec::new();
    for entry in split_top_level(object, ',') {
        let Some((key, value)) = entry.split_once(':') else {
            continue;
        };
        let property = camel_to_kebab(key.trim().trim_matches(['"', '\'']));
        let value = value.trim();
        let value = match string_literal_value(value) {
            Some(literal) => literal,
            None => {
                if value.parse::<f32>().is_ok() {
                    format!("{value}px")
                } else {
                    continue;
                }
            }
        };
        declarations.push(format!("{property}:{value}"));
    }
    declarations.join(";")
}

fn camel_to_kebab(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    for character in input.chars() {
        if character.is_uppercase() {
            out.push('-');
            out.extend(character.to_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// React tags map onto HTML tags; a capitalised tag is a component reference
/// the compiler will try to expand.
fn normalize_jsx_tag(tag: &str) -> String {
    if tag == "fragment" {
        return "div".to_string();
    }
    if tag.chars().next().is_some_and(|c| c.is_uppercase()) {
        return format!("genesi-component:{tag}");
    }
    tag.to_ascii_lowercase()
}

fn normalize_jsx_attrs(attrs: HashMap<String, String>, tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (key, value) in attrs {
        let key = match key.as_str() {
            "classname" => "class".to_string(),
            "htmlfor" => "for".to_string(),
            other => other.to_string(),
        };
        out.insert(key, value);
    }
    if tag.chars().next().is_some_and(|c| c.is_uppercase()) {
        out.insert("data-component".to_string(), tag.to_string());
    }
    out
}

/// Compiles one JSX component (expanding child components it can resolve) into
/// a preview document.
pub fn compile_jsx_component(
    project_root: &Path,
    component: &JsxComponentSource,
    index: &HashMap<String, JsxComponentSource>,
    extra_css: &Stylesheet,
) -> PreviewDocument {
    let Some(dom) = parse_jsx_markup(&component.markup) else {
        return PreviewDocument::empty(
            component.name.clone(),
            "The component's JSX could not be parsed.",
        );
    };

    let mut sheet = extra_css.clone();
    let mut stylesheet_paths = Vec::new();
    for path in &component.stylesheets {
        if let Ok(text) = fs::read_to_string(path) {
            sheet.extend(Stylesheet::parse(&text));
            stylesheet_paths.push(path.clone());
        }
    }

    let mut compiler = Compiler {
        sheet: &sheet,
        base_dir: component
            .path
            .parent()
            .unwrap_or(project_root)
            .to_path_buf(),
        project_root: project_root.to_path_buf(),
        components: Some(index),
        depth: 0,
        budget: 2_000,
        nesting: 0,
    };
    let root_style = ComputedStyle::default();
    let root = compiler.compile_component_tree(&dom, &root_style, &[], &HashMap::new());

    PreviewDocument {
        title: component.name.clone(),
        root,
        background: None,
        color: None,
        stylesheets: stylesheet_paths,
        notes: Vec::new(),
    }
}

impl Compiler<'_> {
    /// Like [`Self::compile_element`] but aware of `<Component />` references:
    /// they expand in place while there is depth budget left, and otherwise
    /// become a labelled placeholder.
    fn compile_component_tree(
        &mut self,
        element: &DomElement,
        parent_style: &ComputedStyle,
        ancestry: &[ElementSnapshot],
        variables: &HashMap<String, String>,
    ) -> PreviewNode {
        if element.tag == "genesi-expression" {
            let label = element
                .attrs
                .get("data-expression")
                .cloned()
                .unwrap_or_default();
            return PreviewNode::Placeholder {
                label: shorten_expression(&label),
                style: parent_style.inherited(),
            };
        }

        if let Some(name) = element.tag.strip_prefix("genesi-component:") {
            let resolved = self
                .components
                .and_then(|index| index.get(name))
                .filter(|_| self.depth < MAX_COMPONENT_DEPTH)
                .cloned();
            if let Some(child_component) = resolved {
                if let Some(child_dom) = parse_jsx_markup(&child_component.markup) {
                    self.depth += 1;
                    let node = self.compile_component_tree(
                        &child_dom,
                        parent_style,
                        ancestry,
                        variables,
                    );
                    self.depth -= 1;
                    return node;
                }
            }
            return PreviewNode::Placeholder {
                label: format!("<{name} />"),
                style: parent_style.inherited(),
            };
        }

        if self.nesting >= MAX_NESTING_DEPTH {
            return PreviewNode::Placeholder {
                label: "…".to_string(),
                style: parent_style.inherited(),
            };
        }

        // Plain element: same cascade as HTML, then recurse with component
        // awareness so nested `<Card />` still expands.
        let snapshot = ElementSnapshot {
            tag: element.tag.clone(),
            id: element.id.clone(),
            classes: element.classes.clone(),
        };
        let mut chain = ancestry.to_vec();
        chain.push(snapshot);
        let (style, variables) = self.resolve_style(element, parent_style, &chain, variables);
        if style.display == Display::None {
            return PreviewNode::Box(PreviewBox {
                tag: element.tag.clone(),
                style: ComputedStyle {
                    display: Display::None,
                    ..ComputedStyle::default()
                },
                children: Vec::new(),
            });
        }
        if element.tag == "img" {
            let src = element.attrs.get("src").cloned().unwrap_or_default();
            return PreviewNode::Image {
                path: resolve_local_asset(&self.base_dir, &self.project_root, &src),
                alt: element.attrs.get("alt").cloned().unwrap_or_default(),
                style,
            };
        }

        let mut children: Vec<PreviewNode> = Vec::new();
        let mut inline_run: Vec<InlineFragment> = Vec::new();
        for child in &element.children {
            if self.budget == 0 {
                break;
            }
            match child {
                DomNode::Text(text) => {
                    push_text_fragment(&mut inline_run, text, &style, false, false, false, false)
                }
                DomNode::Element(child_element) => {
                    if DROPPED_TAGS.contains(&child_element.tag.as_str()) {
                        continue;
                    }
                    if child_element.tag == "br" {
                        push_text_fragment(&mut inline_run, "\n", &style, false, false, false, false);
                        continue;
                    }
                    if child_element.tag != "genesi-expression"
                        && !child_element.tag.starts_with("genesi-component:")
                        && self.is_phrasing(child_element)
                    {
                        self.collect_inline(
                            child_element,
                            &style,
                            &chain,
                            &variables,
                            &mut inline_run,
                        );
                        continue;
                    }
                    if !inline_run.is_empty() {
                        children.push(finish_inline_run(&mut inline_run, &style));
                    }
                    self.budget = self.budget.saturating_sub(1);
                    self.nesting += 1;
                    let compiled =
                        self.compile_component_tree(child_element, &style, &chain, &variables);
                    self.nesting -= 1;
                    if compiled.style().display != Display::None {
                        children.push(compiled);
                    }
                }
            }
        }
        if !inline_run.is_empty() {
            children.push(finish_inline_run(&mut inline_run, &style));
        }

        PreviewNode::Box(PreviewBox {
            tag: element.tag.clone(),
            style,
            children,
        })
    }
}

fn shorten_expression(expression: &str) -> String {
    let collapsed = collapse_whitespace(expression);
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= 40 {
        format!("{{{trimmed}}}")
    } else {
        let head: String = trimmed.chars().take(37).collect();
        format!("{{{head}…}}")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fragment and stylesheet entry points
// ─────────────────────────────────────────────────────────────────────────────

pub fn compile_html_fragment(
    project_root: &Path,
    page_path: &Path,
    fragment: &str,
    sheet: &Stylesheet,
) -> PreviewDocument {
    let wrapper = format!("<html><body>{fragment}</body></html>");
    let Some(document) = parse_html_document(&wrapper) else {
        return PreviewDocument::empty("Fragment", "The fragment could not be parsed.");
    };
    let body = find_element(&document, "body").cloned().unwrap_or(document);
    let mut compiler = Compiler {
        sheet,
        base_dir: page_path
            .parent()
            .unwrap_or(project_root)
            .to_path_buf(),
        project_root: project_root.to_path_buf(),
        components: None,
        depth: 0,
        budget: 600,
        nesting: 0,
    };
    let root = compiler.compile_element(&body, &ComputedStyle::default(), &[], &HashMap::new());
    PreviewDocument {
        title: "Fragment".to_string(),
        root,
        background: None,
        color: None,
        stylesheets: Vec::new(),
        notes: Vec::new(),
    }
}

/// Rebuilds just the stylesheet an HTML page pulls in — its inline `<style>`
/// blocks plus every local `<link rel="stylesheet">`. Fragment previews need it
/// so a hovered section is styled exactly as it is on the page.
pub fn page_stylesheet(project_root: &Path, path: &Path) -> Stylesheet {
    let mut sheet = Stylesheet::default();
    let Ok(html) = fs::read_to_string(path) else {
        return sheet;
    };
    let Some(document) = parse_html_document(&html) else {
        return sheet;
    };
    let base_dir = path.parent().unwrap_or(project_root).to_path_buf();
    let mut sources = Vec::new();
    collect_style_sources(&document, &mut sources);
    for source in sources {
        match source {
            StyleSource::Inline(text) => sheet.extend(Stylesheet::parse(&text)),
            StyleSource::Href(href) => {
                let resolved = resolve_local_asset(&base_dir, project_root, &href)
                    .or_else(|| resolve_cdn_stylesheet(project_root, &href));
                if let Some(resolved) = resolved {
                    if let Ok(text) = fs::read_to_string(&resolved) {
                        sheet.extend(Stylesheet::parse(&text));
                    }
                }
            }
        }
    }
    sheet
}

/// Extracts the named landmark sections of an HTML page so a static project has
/// something to hover over — header, nav, main, footer, and top-level sections.
pub fn html_landmarks(path: &Path) -> Vec<(String, String)> {
    let Ok(html) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(document) = parse_html_document(&html) else {
        return Vec::new();
    };
    let Some(body) = find_element(&document, "body") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_landmarks(body, &mut out, 0);
    out
}

fn collect_landmarks(element: &DomElement, out: &mut Vec<(String, String)>, depth: usize) {
    if depth > 3 || out.len() >= 24 {
        return;
    }
    for child in &element.children {
        let DomNode::Element(child) = child else {
            continue;
        };
        let is_landmark = matches!(
            child.tag.as_str(),
            "header" | "nav" | "main" | "footer" | "section" | "article" | "aside" | "form"
        );
        if is_landmark {
            let label = child
                .id
                .clone()
                .or_else(|| child.classes.first().cloned())
                .map(|hint| format!("{} .{hint}", child.tag))
                .unwrap_or_else(|| child.tag.clone());
            out.push((label, serialize_element(child, 0)));
            if out.len() >= 24 {
                return;
            }
        }
        collect_landmarks(child, out, depth + 1);
    }
}

/// Re-serializes a parsed element back to HTML so it can be recompiled on its
/// own as a fragment.
fn serialize_element(element: &DomElement, depth: usize) -> String {
    if depth > MAX_NESTING_DEPTH {
        return String::new();
    }
    let mut out = format!("<{}", element.tag);
    for (name, value) in &element.attrs {
        out.push_str(&format!(" {name}=\"{}\"", value.replace('"', "&quot;")));
    }
    out.push('>');
    for child in &element.children {
        match child {
            DomNode::Text(text) => out.push_str(text),
            DomNode::Element(child) => out.push_str(&serialize_element(child, depth + 1)),
        }
    }
    out.push_str(&format!("</{}>", element.tag));
    out
}

#[cfg(test)]
#[path = "compiler_tests.rs"]
mod tests;
