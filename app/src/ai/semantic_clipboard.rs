//! Copy a selection together with what it means.
//!
//! Pasting a React component into a chat and getting back "what is `CartItem`?"
//! is the normal experience, because a selection carries names and leaves their
//! definitions behind. This resolves those names from the project before the
//! copy happens: the interfaces a snippet is typed against, the definitions it
//! imports, and the CSS rules its class names refer to.
//!
//! Resolution is convention-based rather than a type check. It follows imports
//! by path and matches declarations by name, which is what a reader does. When
//! it cannot find something it says nothing about it — a wrong definition is
//! worse than a missing one, because the reader has no way to tell.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Ceiling on definitions pulled in. Past this the paste stops being a snippet
/// with context and becomes a code dump nobody reads.
const MAX_DEFINITIONS: usize = 24;

/// Ceiling on one definition's length, in lines. A 400-line class is a
/// reference, not context.
const MAX_DEFINITION_LINES: usize = 60;

const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_SEARCHED_FILES: usize = 900;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".venv",
    "venv",
    "vendor",
    "coverage",
    "__pycache__",
    ".idea",
    ".vscode",
];

/// Names that appear in every file and mean nothing on their own. Resolving
/// them wastes the budget and buries the definitions that matter.
const NOISE: &[&str] = &[
    "true", "false", "null", "undefined", "this", "self", "super", "return", "const", "let", "var",
    "function", "class", "import", "export", "default", "from", "async", "await", "new", "typeof",
    "interface", "type", "enum", "public", "private", "string", "number", "boolean", "void", "any",
    "unknown", "never", "object", "if", "else", "for", "while", "switch", "case", "break",
    "continue", "try", "catch", "finally", "throw", "React", "console", "window", "document",
    "props", "state", "children", "div", "span", "map", "filter", "reduce", "length", "push",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefinitionKind {
    Interface,
    Type,
    Enum,
    Class,
    Function,
    Constant,
    CssRule,
}

impl DefinitionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Enum => "enum",
            Self::Class => "class",
            Self::Function => "function",
            Self::Constant => "const",
            Self::CssRule => "css",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub kind: DefinitionKind,
    /// Project-relative, so the paste says where this came from.
    pub source: PathBuf,
    pub line: usize,
    pub text: String,
    /// The definition ran past [`MAX_DEFINITION_LINES`] and was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelection {
    pub file: PathBuf,
    pub start_line: usize,
    pub selection: String,
    pub definitions: Vec<Definition>,
    /// Names that looked resolvable but were not found anywhere in the project.
    /// Reported rather than hidden, so a paste that is missing something says
    /// which something.
    pub unresolved: Vec<String>,
}

impl ResolvedSelection {
    pub fn is_enriched(&self) -> bool {
        !self.definitions.is_empty()
    }

    /// The clipboard payload: the selection, then whatever it depends on.
    ///
    /// Markdown with fenced blocks, because the destination is usually a chat
    /// or an issue, and a fence is the one format both a model and a person
    /// read correctly.
    pub fn to_clipboard_text(&self) -> String {
        if self.definitions.is_empty() {
            return self.selection.clone();
        }
        let language = language_fence(&self.file);
        let mut out = format!(
            "// {}:{}\n```{language}\n{}\n```\n",
            self.file.display(),
            self.start_line,
            self.selection.trim_end()
        );
        out.push_str("\n// Referenced definitions\n");
        for definition in &self.definitions {
            let fence = language_fence(&definition.source);
            out.push_str(&format!(
                "\n// {} {} — {}:{}\n```{fence}\n{}\n```\n",
                definition.kind.label(),
                definition.name,
                definition.source.display(),
                definition.line,
                definition.text.trim_end()
            ));
            if definition.truncated {
                out.push_str("// … definition truncated\n");
            }
        }
        if !self.unresolved.is_empty() {
            out.push_str(&format!(
                "\n// Not found in this project: {}\n",
                self.unresolved.join(", ")
            ));
        }
        out
    }
}

fn language_fence(file: &Path) -> &'static str {
    match file
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "ts" => "ts",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "js",
        "jsx" => "jsx",
        "css" | "scss" | "sass" | "less" => "css",
        "rs" => "rust",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "html" | "htm" => "html",
        "vue" => "vue",
        "svelte" => "svelte",
        _ => "",
    }
}

/// Resolve what a selection refers to.
///
/// `file` is project-relative; `selection` is the text as copied.
pub fn resolve(root: &Path, file: &Path, selection: &str, start_line: usize) -> ResolvedSelection {
    let mut resolved = ResolvedSelection {
        file: file.to_path_buf(),
        start_line,
        selection: selection.to_string(),
        definitions: Vec::new(),
        unresolved: Vec::new(),
    };
    if selection.trim().is_empty() {
        return resolved;
    }

    let source = fs::read_to_string(root.join(file)).unwrap_or_default();
    let wanted = referenced_symbols(selection);
    let classes = referenced_css_classes(selection);

    // Imports first: they say exactly which file a name came from, so they give
    // the right answer where a project-wide name search would only guess.
    let imports = imports_in(&source);
    let mut found: BTreeMap<String, Definition> = BTreeMap::new();
    let mut searched_files: HashSet<PathBuf> = HashSet::new();

    for name in &wanted {
        if let Some(import) = imports.get(name) {
            for candidate in resolve_module(root, file, &import.module) {
                searched_files.insert(candidate.clone());
                // Search for the name the module EXPORTS, not the one this file
                // calls it: `import { CartItem as LineItem }` means the
                // definition is spelled `CartItem` over there. Then label it
                // with the local name, because that is what the selection says.
                if let Some(mut definition) = definition_in_file(root, &candidate, &import.original)
                {
                    definition.name = name.clone();
                    found.insert(name.clone(), definition);
                    break;
                }
            }
        }
    }

    // Anything still missing may be declared in the same file, which is where a
    // component and the interface describing its props usually both live.
    for name in &wanted {
        if found.contains_key(name) {
            continue;
        }
        if let Some(definition) = definition_in_file(root, file, name) {
            found.insert(name.clone(), definition);
        }
    }

    // Then the rest of the project, for names that were used without an import
    // this pass understood (re-exports, path aliases, globals).
    let still_missing = wanted
        .iter()
        .filter(|name| !found.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !still_missing.is_empty() || !classes.is_empty() {
        for candidate in project_files(root) {
            if found.len() >= MAX_DEFINITIONS {
                break;
            }
            if searched_files.contains(&candidate) || candidate == file {
                continue;
            }
            for name in &still_missing {
                if found.contains_key(name) {
                    continue;
                }
                if let Some(definition) = definition_in_file(root, &candidate, name) {
                    found.insert(name.clone(), definition);
                }
            }
            for class in &classes {
                let key = format!(".{class}");
                if found.contains_key(&key) {
                    continue;
                }
                if let Some(definition) = css_rule_in_file(root, &candidate, class) {
                    found.insert(key, definition);
                }
            }
        }
    }

    resolved.unresolved = wanted
        .iter()
        .filter(|name| !found.contains_key(*name))
        .cloned()
        .collect();
    let mut definitions = found.into_values().collect::<Vec<_>>();
    definitions.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then(left.name.cmp(&right.name))
    });
    definitions.truncate(MAX_DEFINITIONS);
    resolved.definitions = definitions;
    resolved
}

/// Identifiers in the selection worth resolving.
///
/// Capitalised names and type positions only. Lowercase locals are the bulk of
/// any snippet and almost never the thing a reader is missing, so including
/// them would spend the whole budget before reaching the interface that matters.
fn referenced_symbols(selection: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();

    // `: Type`, `<Type>`, `as Type`, `extends Type`, `implements Type`
    let typed = Regex::new(
        r"(?::\s*|<|\bas\s+|\bextends\s+|\bimplements\s+|\bsatisfies\s+)([A-Z][A-Za-z0-9_]*)",
    )
    .expect("valid type-position regex");
    // Any capitalised identifier: components, classes, enums.
    let capitalised = Regex::new(r"\b([A-Z][A-Za-z0-9_]*)\b").expect("valid identifier regex");

    for pattern in [typed, capitalised] {
        for captures in pattern.captures_iter(selection) {
            let Some(name) = captures.get(1) else {
                continue;
            };
            let name = name.as_str();
            if NOISE.contains(&name) || name.len() < 2 {
                continue;
            }
            if seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Class names the selection uses, from `className="a b"` / `class="a b"` and
/// their template-literal forms.
fn referenced_css_classes(selection: &str) -> Vec<String> {
    let pattern = Regex::new(r#"(?i)\bclass(?:Name)?\s*=\s*[\x22\x27`\{]+([^\x22\x27`\}]+)"#)
        .expect("valid class attribute regex");
    let mut classes = Vec::new();
    let mut seen = HashSet::new();
    for captures in pattern.captures_iter(selection) {
        let Some(value) = captures.get(1) else {
            continue;
        };
        for class in value.as_str().split_whitespace() {
            let class = class.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
            if class.is_empty() || class.len() < 2 {
                continue;
            }
            if seen.insert(class.to_string()) {
                classes.push(class.to_string());
            }
        }
    }
    classes
}

/// One imported name, as this file sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Import {
    module: String,
    /// The name the module exports, which differs from the local name under
    /// `as` and is what the definition is actually spelled as.
    original: String,
}

/// Map every locally-visible imported name to where it came from.
fn imports_in(source: &str) -> BTreeMap<String, Import> {
    let mut map = BTreeMap::new();
    let named =
        Regex::new(r#"(?s)import\s+(?:type\s+)?\{([^}]*)\}\s*from\s*[\x22\x27]([^\x22\x27]+)"#)
            .expect("valid named import regex");
    for captures in named.captures_iter(source) {
        let (Some(names), Some(module)) = (captures.get(1), captures.get(2)) else {
            continue;
        };
        for piece in names.as_str().split(',') {
            let piece = piece.trim().trim_start_matches("type ").trim();
            let mut words = piece.split_whitespace();
            let Some(original) = words.next() else {
                continue;
            };
            // `Foo as Bar`: the selection uses `Bar`, the module exports `Foo`.
            let local = match (words.next(), words.next()) {
                (Some("as"), Some(alias)) => alias.to_string(),
                _ => original.to_string(),
            };
            map.insert(
                local,
                Import {
                    module: module.as_str().to_string(),
                    original: original.to_string(),
                },
            );
        }
    }

    let default_import = Regex::new(
        r#"import\s+(?:type\s+)?([A-Za-z_$][\w$]*)\s*(?:,|\s+from)\s*.*?[\x22\x27]([^\x22\x27]+)"#,
    )
    .expect("valid default import regex");
    for captures in default_import.captures_iter(source) {
        let (Some(name), Some(module)) = (captures.get(1), captures.get(2)) else {
            continue;
        };
        let name = name.as_str().to_string();
        map.insert(
            name.clone(),
            Import {
                module: module.as_str().to_string(),
                original: name,
            },
        );
    }
    map
}

/// Candidate files a module specifier could mean, most likely first.
///
/// Only relative specifiers are followed. A bare `react` is a dependency, and
/// pasting node_modules into a chat helps nobody.
fn resolve_module(root: &Path, from_file: &Path, module: &str) -> Vec<PathBuf> {
    if !module.starts_with('.') {
        return Vec::new();
    }
    let base = from_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(module);
    let normalized = normalize_path(&base);
    let extensions = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "css", "scss"];

    let mut candidates = Vec::new();
    // An explicit extension is already the answer.
    if normalized.extension().is_some() && root.join(&normalized).is_file() {
        candidates.push(normalized.clone());
    }
    for extension in extensions {
        let with_extension = normalized.with_extension(extension);
        if root.join(&with_extension).is_file() {
            candidates.push(with_extension);
        }
    }
    for extension in extensions {
        let index = normalized.join(format!("index.{extension}"));
        if root.join(&index).is_file() {
            candidates.push(index);
        }
    }
    candidates
}

/// Resolve `.` and `..` lexically. The path may not exist yet, so this cannot
/// go through the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    parts.iter().collect()
}

/// The declaration of `name` in `file`, with its body.
fn definition_in_file(root: &Path, file: &Path, name: &str) -> Option<Definition> {
    let source = fs::read_to_string(root.join(file)).ok()?;
    let escaped = regex::escape(name);
    // Ordered so the most specific declaration wins: `interface Foo` should not
    // lose to a `const Foo` somewhere else in the same file.
    // Anchored with `[ 	]*` rather than `\s*`: `\s` matches newlines, so the
    // match would begin on a preceding blank line and the block extractor would
    // then read THAT line — finding the declaration but capturing nothing.
    let patterns: [(&str, DefinitionKind); 6] = [
        (r"(?m)^[ \t]*(?:export\s+)?(?:declare\s+)?interface\s+NAME\b", DefinitionKind::Interface),
        (r"(?m)^[ \t]*(?:export\s+)?(?:declare\s+)?type\s+NAME\s*[=<]", DefinitionKind::Type),
        (r"(?m)^[ \t]*(?:export\s+)?(?:declare\s+)?(?:const\s+)?enum\s+NAME\b", DefinitionKind::Enum),
        (r"(?m)^[ \t]*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+NAME\b", DefinitionKind::Class),
        (
            r"(?m)^[ \t]*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s+NAME\b",
            DefinitionKind::Function,
        ),
        (
            r"(?m)^[ \t]*(?:export\s+)?(?:const|let|var)\s+NAME\b",
            DefinitionKind::Constant,
        ),
    ];

    for (pattern, kind) in patterns {
        let regex = Regex::new(&pattern.replace("NAME", &escaped)).ok()?;
        if let Some(found) = regex.find(&source) {
            let line = source[..found.start()].matches('\n').count();
            let (text, truncated) = block_at(&source, line);
            return Some(Definition {
                name: name.to_string(),
                kind,
                source: file.to_path_buf(),
                line: line + 1,
                text,
                truncated,
            });
        }
    }
    None
}

/// The CSS rule for `class` in `file`.
fn css_rule_in_file(root: &Path, file: &Path, class: &str) -> Option<Definition> {
    let extension = file
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "css" | "scss" | "sass" | "less") {
        return None;
    }
    let source = fs::read_to_string(root.join(file)).ok()?;
    let pattern = Regex::new(&format!(
        r"(?m)^[^\n]*\.{}\b[^\n]*\{{",
        regex::escape(class)
    ))
    .ok()?;
    let found = pattern.find(&source)?;
    let line = source[..found.start()].matches('\n').count();
    let (text, truncated) = block_at(&source, line);
    Some(Definition {
        name: format!(".{class}"),
        kind: DefinitionKind::CssRule,
        source: file.to_path_buf(),
        line: line + 1,
        text,
        truncated,
    })
}

/// The declaration starting at `line`, up to its closing brace.
///
/// Brace counting rather than a parse: it handles the shapes that matter
/// (`interface … {}`, `class … {}`, `.rule {}`) and degrades to a single line
/// for one-liners like `type Id = string;` — which is the whole definition
/// anyway.
fn block_at(source: &str, line: usize) -> (String, bool) {
    let lines = source.lines().collect::<Vec<_>>();
    if line >= lines.len() {
        return (String::new(), false);
    }
    let first = lines[line];
    if !first.contains('{') {
        // A statement-shaped declaration ends at its semicolon, which for the
        // one-line forms is the same line.
        return (first.trim_end().to_string(), false);
    }

    let mut depth = 0_i32;
    let mut collected = Vec::new();
    for (offset, text) in lines[line..].iter().enumerate() {
        collected.push(*text);
        for character in text.chars() {
            match character {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 && offset > 0 || (depth == 0 && offset == 0) {
            break;
        }
        if collected.len() >= MAX_DEFINITION_LINES {
            return (collected.join("\n"), true);
        }
    }
    (collected.join("\n"), false)
}

/// Source files worth searching, bounded.
fn project_files(root: &Path) -> Vec<PathBuf> {
    let extensions = [
        "ts", "tsx", "js", "jsx", "mjs", "cjs", "css", "scss", "sass", "less", "vue", "svelte",
    ];
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .metadata()
                .map(|metadata| metadata.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
        })
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.contains(&ext.to_ascii_lowercase().as_str()))
        })
        .take(MAX_SEARCHED_FILES)
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .ok()
        })
        .collect()
}

#[cfg(test)]
#[path = "semantic_clipboard_tests.rs"]
mod tests;
