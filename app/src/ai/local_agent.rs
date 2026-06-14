//! Local coding-agent: read-only tools + the text protocol the model uses to
//! call them.
//!
//! The local chat panel ([`super::local_chat_panel`]) runs an agent loop: the
//! model can ask to read the project (one tool call per turn), the panel runs
//! the tool against the project root, feeds the result back, and loops until the
//! model answers without a tool call. This module owns the protocol (how a tool
//! call is written and parsed) and the read-only tool implementations. Commands
//! and edits (which need approval) are added in later phases.
#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};

/// Hard caps so a tool result can't blow the model's context window or jank the
/// UI thread (tools run synchronously for now).
const MAX_FILE_BYTES: usize = 24_000;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_GREP_MATCHES: usize = 60;
const MAX_GREP_FILES: usize = 4_000;

/// Directories never worth walking for `grep`/`list_files` — noise that would
/// blow the caps and the context window.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
];

/// Markers that identify a project root when walking up from an open file.
const ROOT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "pom.xml",
    "build.gradle",
    "Makefile",
    "CMakeLists.txt",
    ".genesi",
];

/// The maximum number of agent steps (tool calls) per user turn, so a confused
/// model can't loop forever.
pub const MAX_AGENT_STEPS: u32 = 8;

/// A read-only tool the agent can call to explore the project.
#[derive(Debug, Clone)]
pub enum AgentTool {
    ReadFile { path: String },
    ListFiles { path: String },
    Grep { query: String, path: String },
}

impl AgentTool {
    pub fn name(&self) -> &'static str {
        match self {
            AgentTool::ReadFile { .. } => "read_file",
            AgentTool::ListFiles { .. } => "list_files",
            AgentTool::Grep { .. } => "grep",
        }
    }

    /// A compact one-line summary for the transcript, e.g. `read_file src/main.rs`.
    pub fn summary(&self) -> String {
        match self {
            AgentTool::ReadFile { path } => format!("read_file {path}"),
            AgentTool::ListFiles { path } => format!("list_files {path}"),
            AgentTool::Grep { query, path } => format!("grep \"{query}\" in {path}"),
        }
    }
}

/// The system prompt that turns the plain chat into a read-only codebase agent.
/// Describes the tool protocol the [`parse_tool_call`] parser understands.
pub fn agent_system_prompt() -> String {
    "You are Genesi Code's coding agent, working inside the user's project. You \
can read the project with tools before answering.\n\n\
To use a tool, reply with ONLY a single tool tag and nothing else:\n\
  <tool:read_file path=\"relative/path.rs\"/>\n\
  <tool:list_files path=\"relative/dir\"/>\n\
  <tool:grep query=\"some text\" path=\"relative/dir\"/>\n\n\
Rules:\n\
- Paths are relative to the project root. Use \".\" for the root.\n\
- Call ONE tool per message. After a tool runs, you get its result as the next \
message; then call another tool or give your final answer.\n\
- When you have enough information, answer the user in plain text with NO tool \
tag. Be concise and reference files/lines you read.\n\
- Do not invent file contents — read them first."
        .to_string()
}

/// Parse the first tool call out of a model reply, if present. Recognizes the
/// self-closing tags described in [`agent_system_prompt`]. Lenient: tolerates
/// extra prose around the tag and either `/>` or `>` terminators.
pub fn parse_tool_call(text: &str) -> Option<AgentTool> {
    let start = text.find("<tool:")?;
    let rest = &text[start + "<tool:".len()..];
    // The tag ends at the first `>`; allow a `/` just before it.
    let end = rest.find('>')?;
    let mut inner = &rest[..end];
    if let Some(stripped) = inner.strip_suffix('/') {
        inner = stripped;
    }
    let inner = inner.trim();

    // `name` is everything up to the first whitespace; the rest are attributes.
    let (name, attrs) = match inner.find(char::is_whitespace) {
        Some(i) => (&inner[..i], &inner[i..]),
        None => (inner, ""),
    };

    match name {
        "read_file" => Some(AgentTool::ReadFile {
            path: attr(attrs, "path")?,
        }),
        "list_files" => Some(AgentTool::ListFiles {
            path: attr(attrs, "path").unwrap_or_else(|| ".".to_string()),
        }),
        "grep" => Some(AgentTool::Grep {
            query: attr(attrs, "query")?,
            path: attr(attrs, "path").unwrap_or_else(|| ".".to_string()),
        }),
        _ => None,
    }
}

/// Extract a `key="value"` attribute value from a tag's attribute string.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let i = attrs.find(&needle)? + needle.len();
    let rest = &attrs[i..];
    let j = rest.find('"')?;
    Some(rest[..j].to_string())
}

/// Run a read-only tool against `root`, returning a bounded text result to feed
/// back to the model. Never reads outside `root`.
pub fn run_read_tool(root: &Path, tool: &AgentTool) -> String {
    match tool {
        AgentTool::ReadFile { path } => read_file(root, path),
        AgentTool::ListFiles { path } => list_files(root, path),
        AgentTool::Grep { query, path } => grep(root, query, path),
    }
}

/// Resolve a project-relative path inside `root`, refusing to escape it (no
/// `..` traversal out of the project). Purely lexical so it works for paths that
/// don't exist yet.
fn safe_resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim().trim_start_matches(['/', '\\']);
    let joined = root.join(rel);
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.starts_with(root).then_some(out)
}

fn read_file(root: &Path, rel: &str) -> String {
    let Some(path) = safe_resolve(root, rel) else {
        return format!("error: path '{rel}' is outside the project");
    };
    match std::fs::read(&path) {
        Ok(bytes) => {
            let truncated = bytes.len() > MAX_FILE_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_FILE_BYTES)];
            let text = String::from_utf8_lossy(slice);
            if truncated {
                format!("{text}\n… [file truncated at {MAX_FILE_BYTES} bytes]")
            } else {
                text.into_owned()
            }
        }
        Err(e) => format!("error reading '{rel}': {e}"),
    }
}

fn list_files(root: &Path, rel: &str) -> String {
    let Some(dir) = safe_resolve(root, rel) else {
        return format!("error: path '{rel}' is outside the project");
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => return format!("error listing '{rel}': {e}"),
    };

    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    names.sort();
    let total = names.len();
    names.truncate(MAX_LIST_ENTRIES);

    if names.is_empty() {
        return format!("(empty directory: {rel})");
    }
    let mut out = names.join("\n");
    if total > MAX_LIST_ENTRIES {
        out.push_str(&format!("\n… [{} more entries]", total - MAX_LIST_ENTRIES));
    }
    out
}

fn grep(root: &Path, query: &str, rel: &str) -> String {
    let Some(start) = safe_resolve(root, rel) else {
        return format!("error: path '{rel}' is outside the project");
    };
    let needle = query.to_lowercase();
    let mut matches: Vec<String> = Vec::new();
    let mut files_scanned = 0usize;
    let mut stack = vec![start];

    while let Some(dir) = stack.pop() {
        if matches.len() >= MAX_GREP_MATCHES || files_scanned >= MAX_GREP_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(entry.path());
                }
                continue;
            }
            if files_scanned >= MAX_GREP_FILES || matches.len() >= MAX_GREP_MATCHES {
                break;
            }
            files_scanned += 1;
            let path = entry.path();
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue; // binary or unreadable — skip
            };
            let display = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            for (line_no, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    let trimmed = line.trim();
                    let snippet = if trimmed.len() > 200 {
                        &trimmed[..200]
                    } else {
                        trimmed
                    };
                    matches.push(format!("{display}:{}: {snippet}", line_no + 1));
                    if matches.len() >= MAX_GREP_MATCHES {
                        break;
                    }
                }
            }
        }
    }

    if matches.is_empty() {
        format!("no matches for \"{query}\" under {rel}")
    } else {
        let capped = matches.len() >= MAX_GREP_MATCHES;
        let mut out = matches.join("\n");
        if capped {
            out.push_str(&format!("\n… [stopped at {MAX_GREP_MATCHES} matches]"));
        }
        out
    }
}

/// Walk up from a file to the nearest directory containing a project marker
/// (`.git`, `Cargo.toml`, …). Returns `None` if none is found.
pub fn project_root_from_file(file: &Path) -> Option<PathBuf> {
    let mut dir = file.parent();
    while let Some(current) = dir {
        for marker in ROOT_MARKERS {
            if current.join(marker).exists() {
                return Some(current.to_path_buf());
            }
        }
        dir = current.parent();
    }
    None
}
