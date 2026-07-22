//! Resolves "what is under the cursor" into a renderable preview.
//!
//! This backs the editor's hover card: rest the pointer on `<Card />` in a
//! React file, or on any element in an HTML file, and the thing under the
//! cursor is compiled and drawn with its real CSS.
//!
//! The hard constraint here is latency. A hover has a ~2s budget end to end, so
//! this never walks the project: it reads the file under the cursor and, at
//! most, the handful of files that file directly imports. Everything is plain
//! data in and out so it can run off the UI thread and be unit tested.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::compiler::{
    compile_html_fragment, compile_jsx_component, extract_jsx_components, page_stylesheet,
    JsxComponentSource, PreviewDocument, Stylesheet,
};

/// Extensions the hover card knows how to render.
const JSX_EXTENSIONS: &[&str] = &["jsx", "tsx", "js", "ts", "mjs"];
const HTML_EXTENSIONS: &[&str] = &["html", "htm"];

/// Module specifiers a component import might use, in resolution order.
const MODULE_SUFFIXES: &[&str] = &[
    ".tsx",
    ".jsx",
    ".ts",
    ".js",
    "/index.tsx",
    "/index.jsx",
    "/index.ts",
    "/index.js",
];

/// Stylesheets treated as global when they sit beside (or above) the hovered
/// file. These are the names bundlers and templates converge on.
const GLOBAL_STYLESHEET_NAMES: &[&str] = &[
    "index.css",
    "globals.css",
    "global.css",
    "styles.css",
    "style.css",
    "app.css",
    "main.css",
];

/// How far up from the hovered file to look for a global stylesheet.
const GLOBAL_STYLE_LOOKUP_DEPTH: usize = 4;

/// Markers that identify a project root when walking up from a file.
const ROOT_MARKERS: &[&str] = &["package.json", ".git", "index.html", "deno.json"];

/// The project root a hovered file belongs to. Root-relative asset and style
/// references (`/styles.css`) resolve against this, so guessing it well is
/// what makes a preview pick up the page's real CSS.
pub fn infer_project_root(file_path: &Path) -> PathBuf {
    let mut current = file_path.parent();
    let mut fallback = None;
    for _ in 0..8 {
        let Some(directory) = current else { break };
        if fallback.is_none() {
            fallback = Some(directory.to_path_buf());
        }
        if ROOT_MARKERS
            .iter()
            .any(|marker| directory.join(marker).exists())
        {
            return directory.to_path_buf();
        }
        current = directory.parent();
    }
    fallback.unwrap_or_else(|| file_path.to_path_buf())
}

/// A preview ready to be drawn in the hover card.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverPreview {
    /// What to title the card — the component name, or the element's selector.
    pub label: String,
    /// Where the thing under the cursor is defined, for the card's subtitle.
    pub origin: Option<PathBuf>,
    pub document: PreviewDocument,
}

/// What the cursor is resting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverTarget {
    /// A capitalised identifier in a JS-family file: a React component.
    Component(String),
    /// An element in an HTML file, as a byte range in the source.
    HtmlElement { start: usize, end: usize },
}

/// Resolves and compiles whatever sits at `byte_offset`. Returns `None` when
/// the cursor is not on something previewable, which is the common case — this
/// runs on every settled hover, so "nothing here" has to be cheap.
pub fn preview_at_offset(
    project_root: &Path,
    file_path: &Path,
    source: &str,
    byte_offset: usize,
) -> Option<HoverPreview> {
    let extension = file_path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if HTML_EXTENSIONS.contains(&extension.as_str()) {
        return html_preview(project_root, file_path, source, byte_offset);
    }
    if JSX_EXTENSIONS.contains(&extension.as_str()) {
        return component_preview(project_root, file_path, source, byte_offset);
    }
    None
}

/// The target at `byte_offset`, without compiling it. Split out so the editor
/// can cheaply decide whether a hover is worth acting on.
pub fn target_at_offset(file_path: &Path, source: &str, byte_offset: usize) -> Option<HoverTarget> {
    let extension = file_path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if HTML_EXTENSIONS.contains(&extension.as_str()) {
        let (start, end) = html_element_range_at(source, byte_offset)?;
        return Some(HoverTarget::HtmlElement { start, end });
    }
    if JSX_EXTENSIONS.contains(&extension.as_str()) {
        let name = identifier_at(source, byte_offset)?;
        // React components are capitalised; that is the whole signal that
        // separates a component reference from an ordinary variable.
        if name.chars().next().is_some_and(char::is_uppercase) {
            return Some(HoverTarget::Component(name));
        }
    }
    None
}

// ── React components ────────────────────────────────────────────────────────

fn component_preview(
    project_root: &Path,
    file_path: &Path,
    source: &str,
    byte_offset: usize,
) -> Option<HoverPreview> {
    let name = match target_at_offset(file_path, source, byte_offset)? {
        HoverTarget::Component(name) => name,
        HoverTarget::HtmlElement { .. } => return None,
    };

    // Components defined in the hovered file resolve without touching disk.
    let mut index: HashMap<String, JsxComponentSource> = extract_jsx_components(
        project_root,
        file_path,
        source,
    )
    .into_iter()
    .map(|component| (component.name.clone(), component))
    .collect();

    let component = match index.get(&name) {
        Some(component) => component.clone(),
        None => {
            // Not local: follow the file's own import for this name, and read
            // just that one module.
            let resolved = resolve_import(project_root, file_path, source, &name)?;
            let imported_source = fs::read_to_string(&resolved).ok()?;
            let imported = extract_jsx_components(project_root, &resolved, &imported_source);
            let component = imported
                .iter()
                .find(|component| component.name == name)?
                .clone();
            // Siblings in that module can appear inside the component's markup,
            // so keep them available for in-place expansion.
            for sibling in imported {
                index.entry(sibling.name.clone()).or_insert(sibling);
            }
            component
        }
    };

    let stylesheet = component_stylesheet(project_root, &component);
    let document = compile_jsx_component(project_root, &component, &index, &stylesheet);
    Some(HoverPreview {
        label: format!("<{name} />"),
        origin: Some(component.path.clone()),
        document,
    })
}

/// `import Card from './Card'` / `import { Card } from "../ui/Card"`.
fn resolve_import(
    project_root: &Path,
    file_path: &Path,
    source: &str,
    name: &str,
) -> Option<PathBuf> {
    let base_dir = file_path.parent().unwrap_or(project_root);
    for line in source.lines() {
        let line = line.trim();
        if !line.starts_with("import") {
            continue;
        }
        let Some((clause, specifier)) = line.split_once(" from ") else {
            continue;
        };
        if !import_clause_binds(clause, name) {
            continue;
        }
        let specifier = specifier.trim().trim_matches(|c| c == '"' || c == '\'' || c == ';');
        // Only relative specifiers point at project source; a bare specifier is
        // a package, which has no source to preview.
        if !specifier.starts_with('.') {
            return None;
        }
        return resolve_module(base_dir, specifier);
    }
    None
}

/// Whether an import clause introduces `name`, as a default or named binding.
fn import_clause_binds(clause: &str, name: &str) -> bool {
    let clause = clause.trim_start_matches("import").trim();
    for part in clause.split(['{', '}', ',']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // `Card as Renamed` binds the right-hand name.
        let bound = part
            .rsplit(" as ")
            .next()
            .unwrap_or(part)
            .trim();
        if bound == name {
            return true;
        }
    }
    false
}

/// Collapses `.` and `..` so a specifier like `../ui/Card` becomes a real path.
fn normalize(path: &Path) -> PathBuf {
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

fn resolve_module(base_dir: &Path, specifier: &str) -> Option<PathBuf> {
    let base = normalize(&base_dir.join(specifier));
    if base.is_file() {
        return Some(base);
    }
    for suffix in MODULE_SUFFIXES {
        let candidate = PathBuf::from(format!("{}{suffix}", base.display()));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The CSS a component preview should be styled with: whatever its own file
/// imports, plus the nearest global stylesheet above it.
fn component_stylesheet(project_root: &Path, component: &JsxComponentSource) -> Stylesheet {
    let mut sheet = Stylesheet::default();
    for path in nearest_global_stylesheets(project_root, &component.path) {
        if let Ok(text) = fs::read_to_string(&path) {
            sheet.extend(Stylesheet::parse(&text));
        }
    }
    // The component's own imports come last so they win over the globals.
    for path in &component.stylesheets {
        if let Ok(text) = fs::read_to_string(path) {
            sheet.extend(Stylesheet::parse(&text));
        }
    }
    sheet
}

/// Walks up from the file toward the project root collecting global-looking
/// stylesheets, outermost first so nearer ones override.
fn nearest_global_stylesheets(project_root: &Path, file_path: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut current = file_path.parent();
    for _ in 0..GLOBAL_STYLE_LOOKUP_DEPTH {
        let Some(directory) = current else { break };
        directories.push(directory.to_path_buf());
        if directory == project_root {
            break;
        }
        current = directory.parent();
    }

    let mut found = Vec::new();
    // Outermost directory first: a stylesheet next to the component should be
    // able to override one at the project root.
    for directory in directories.into_iter().rev() {
        for name in GLOBAL_STYLESHEET_NAMES {
            let candidate = directory.join(name);
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    found
}

/// The identifier containing `byte_offset`, if any.
fn identifier_at(source: &str, byte_offset: usize) -> Option<String> {
    if byte_offset > source.len() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    // Land inside the token even when the offset sits on its trailing edge.
    let anchor = source
        .get(byte_offset..)
        .and_then(|rest| rest.chars().next())
        .filter(|c| is_ident(*c))
        .map(|_| byte_offset)
        .or_else(|| {
            let previous = source.get(..byte_offset)?.chars().next_back()?;
            is_ident(previous).then(|| byte_offset - previous.len_utf8())
        })?;

    let start = source[..anchor]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map(|(index, _)| index)
        .unwrap_or(anchor);
    let end = source[anchor..]
        .char_indices()
        .find(|(_, c)| !is_ident(*c))
        .map(|(index, _)| anchor + index)
        .unwrap_or(source.len());

    let identifier = source.get(start..end)?.to_string();
    (!identifier.is_empty()).then_some(identifier)
}

// ── HTML elements ───────────────────────────────────────────────────────────

fn html_preview(
    project_root: &Path,
    file_path: &Path,
    source: &str,
    byte_offset: usize,
) -> Option<HoverPreview> {
    let (start, end) = html_element_range_at(source, byte_offset)?;
    let markup = source.get(start..end)?;
    let sheet = page_stylesheet(project_root, file_path);
    let document = compile_html_fragment(project_root, file_path, markup, &sheet);
    Some(HoverPreview {
        label: element_label(markup),
        origin: Some(file_path.to_path_buf()),
        document,
    })
}

/// HTML elements with no closing tag; they never nest, so the stack must not
/// wait for a `</…>` that will never arrive.
const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements too broad to be a useful hover target — previewing the whole
/// document is what the page view is for.
const UNINTERESTING_TAGS: &[&str] = &["html", "head", "body", "script", "style", "title"];

/// The byte range of the innermost element containing `byte_offset`.
///
/// Walks the source keeping a stack of open tags; the first element to close
/// while still containing the offset is by construction the innermost one.
fn html_element_range_at(source: &str, byte_offset: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        let tag_start = index;
        let is_closing = bytes.get(index + 1) == Some(&b'/');
        let name_start = index + if is_closing { 2 } else { 1 };
        let mut name_end = name_start;
        while name_end < bytes.len() {
            let character = bytes[name_end] as char;
            if character.is_alphanumeric() || character == '-' {
                name_end += 1;
            } else {
                break;
            }
        }
        let name = source
            .get(name_start..name_end)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.is_empty() {
            index += 1;
            continue;
        }

        // Skip to the end of this tag, honouring quoted attribute values.
        let mut cursor = name_end;
        let mut quote: Option<u8> = None;
        let mut self_closing = false;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            match quote {
                Some(open) if byte == open => quote = None,
                Some(_) => {}
                None => match byte {
                    b'"' | b'\'' => quote = Some(byte),
                    b'/' if bytes.get(cursor + 1) == Some(&b'>') => self_closing = true,
                    b'>' => break,
                    _ => {}
                },
            }
            cursor += 1;
        }
        let tag_end = (cursor + 1).min(bytes.len());

        if is_closing {
            // Unwind to the matching open tag; malformed markup just discards
            // the unmatched entries rather than aborting the walk.
            if let Some(position) = stack.iter().rposition(|(open, _)| *open == name) {
                let (_, open_start) = stack[position];
                stack.truncate(position);
                if open_start <= byte_offset
                    && byte_offset < tag_end
                    && !UNINTERESTING_TAGS.contains(&name.as_str())
                {
                    return Some((open_start, tag_end));
                }
            }
        } else if self_closing || VOID_TAGS.contains(&name.as_str()) {
            if tag_start <= byte_offset
                && byte_offset < tag_end
                && !UNINTERESTING_TAGS.contains(&name.as_str())
            {
                return Some((tag_start, tag_end));
            }
        } else {
            stack.push((name, tag_start));
        }

        index = tag_end.max(index + 1);
    }
    None
}

/// `<section class="hero">` → `section.hero`, which reads like the selector a
/// developer would have written.
fn element_label(markup: &str) -> String {
    let open_end = markup.find('>').unwrap_or(markup.len());
    let open = &markup[..open_end];
    let name: String = open
        .trim_start_matches('<')
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-')
        .collect();

    for (attribute, prefix) in [("id=", "#"), ("class=", ".")] {
        let Some(position) = open.find(attribute) else {
            continue;
        };
        let rest = &open[position + attribute.len()..];
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let value = &rest[1..1 + end];
        if let Some(first) = value.split_whitespace().next() {
            return format!("{name}{prefix}{first}");
        }
    }
    name
}

#[cfg(test)]
#[path = "hover_tests.rs"]
mod tests;
