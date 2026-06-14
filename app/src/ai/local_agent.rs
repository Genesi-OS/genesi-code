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
use std::time::Duration;

/// Hard caps so a tool result can't blow the model's context window or jank the
/// UI thread (read tools run synchronously).
const MAX_FILE_BYTES: usize = 24_000;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_GREP_MATCHES: usize = 60;
const MAX_GREP_FILES: usize = 4_000;

/// Caps for `run_command`: a command that hangs can't wedge the agent, and its
/// output can't blow the context window.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT_CHARS: usize = 12_000;

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

/// A tool the agent can call. Reads run immediately; `run_command` is
/// side-effecting and gated by user approval (unless AUTO is on).
#[derive(Debug, Clone)]
pub enum AgentTool {
    ReadFile { path: String },
    ListFiles { path: String },
    Grep { query: String, path: String },
    RunCommand { command: String },
}

impl AgentTool {
    pub fn name(&self) -> &'static str {
        match self {
            AgentTool::ReadFile { .. } => "read_file",
            AgentTool::ListFiles { .. } => "list_files",
            AgentTool::Grep { .. } => "grep",
            AgentTool::RunCommand { .. } => "run_command",
        }
    }

    /// A compact one-line summary for the transcript, e.g. `read_file src/main.rs`.
    pub fn summary(&self) -> String {
        match self {
            AgentTool::ReadFile { path } => format!("read_file {path}"),
            AgentTool::ListFiles { path } => format!("list_files {path}"),
            AgentTool::Grep { query, path } => format!("grep \"{query}\" in {path}"),
            AgentTool::RunCommand { command } => format!("run {command}"),
        }
    }

    /// Whether running this tool needs the user's go-ahead (anything that can
    /// change the system). Reads never do.
    pub fn requires_approval(&self) -> bool {
        matches!(self, AgentTool::RunCommand { .. })
    }
}

/// The system prompt that turns the plain chat into a codebase agent. Written
/// to push even smaller models toward the tool protocol [`parse_tool_call`]
/// understands — the failure mode is a model that says "I can't access files"
/// instead of calling a tool, so the prompt forbids exactly that.
pub fn agent_system_prompt() -> String {
    "You are Genesi Code's coding agent, working inside the user's open project \
on Genesi OS.\n\n\
IMPORTANT: You have NO direct access to files, folders, or the system. The ONLY \
way to see a file, list a folder, search the code, or run anything is to use a \
tool. If the user asks about files or the project, you MUST use a tool — NEVER \
say you cannot access them.\n\n\
To use a tool, reply with ONLY the tool tag and nothing else:\n\
  <tool:list_files path=\".\"/>\n\
  <tool:read_file path=\"src/main.rs\"/>\n\
  <tool:grep query=\"some text\" path=\".\"/>\n\
  <tool:run_command>echo hello</tool>\n\n\
After a tool runs, you receive its result as the next message. Then call another \
tool, or give your final answer in plain text (no tool tag).\n\n\
Example:\n\
User: what files are in this folder?\n\
Assistant: <tool:list_files path=\".\"/>\n\
(result: index.html, test.js)\n\
Assistant: This folder has two files: index.html and test.js.\n\n\
Rules:\n\
- Use ONE tool per message. Paths are relative to the project root; use \".\" \
for the root.\n\
- Prefer list_files / read_file before answering questions about the code.\n\
- run_command runs a shell command and returns its output (the user may be \
asked to approve it first).\n\
- Be concise and reference the files and lines you actually read. Do not invent \
file contents."
        .to_string()
}

/// Parse the first tool call out of a model reply, if any. Deliberately lenient
/// — small local models produce near-miss formats — accepting the `<tool:NAME>`
/// and bare `<NAME>` tags, self-closing or block form, and quoted or unquoted
/// attributes.
pub fn parse_tool_call(text: &str) -> Option<AgentTool> {
    // Block form first (run_command's body can contain anything).
    if let Some(body) = block_tool_body(text, "run_command") {
        let command = body.trim().to_string();
        if !command.is_empty() {
            return Some(AgentTool::RunCommand { command });
        }
    }
    // Attribute/self-closing form for the rest.
    for name in ["read_file", "list_files", "grep", "run_command"] {
        let Some(attrs) = tag_attrs(text, name) else {
            continue;
        };
        return match name {
            "read_file" => attr(&attrs, "path").map(|path| AgentTool::ReadFile { path }),
            "list_files" => Some(AgentTool::ListFiles {
                path: attr(&attrs, "path").unwrap_or_else(|| ".".to_string()),
            }),
            "grep" => attr(&attrs, "query").map(|query| AgentTool::Grep {
                query,
                path: attr(&attrs, "path").unwrap_or_else(|| ".".to_string()),
            }),
            "run_command" => attr(&attrs, "command")
                .or_else(|| attr(&attrs, "cmd"))
                .map(|command| AgentTool::RunCommand { command }),
            _ => None,
        };
    }
    None
}

/// Find an opening `<tool:NAME ...>` / `<NAME ...>` tag and return its attribute
/// string (everything between the name and the closing `>`, minus a trailing
/// `/`). Returns `None` if no such tag is present.
fn tag_attrs(text: &str, name: &str) -> Option<String> {
    let opens = [format!("<tool:{name}"), format!("<{name}")];
    let after_name = opens
        .iter()
        .find_map(|open| text.find(open.as_str()).map(|i| i + open.len()))?;
    let rest = &text[after_name..];
    let end = rest.find('>')?;
    Some(rest[..end].trim_end_matches('/').trim().to_string())
}

/// Return the body of a block tool `<tool:NAME>body</tool>` (or `<NAME>…`),
/// tolerating a missing close tag by taking the rest of the text.
fn block_tool_body(text: &str, name: &str) -> Option<String> {
    let opens = [format!("<tool:{name}"), format!("<{name}")];
    let open = opens.iter().find_map(|o| text.find(o.as_str()))?;
    let rest = &text[open..];
    let gt = rest.find('>')?;
    // A self-closing `<tool:run_command/>` has no body.
    if rest[..gt].ends_with('/') {
        return None;
    }
    let after = &rest[gt + 1..];
    let end = after
        .find("</tool>")
        .or_else(|| after.find(&format!("</{name}>")))
        .or_else(|| after.find("</tool:"))
        .unwrap_or(after.len());
    Some(after[..end].to_string())
}

/// Extract an attribute value: `key="v"`, `key='v'`, or unquoted `key=v`.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let mut search = attrs;
    loop {
        let i = search.find(key)?;
        let rest = search[i + key.len()..].trim_start();
        if let Some(eq) = rest.strip_prefix('=') {
            let val = eq.trim_start();
            if let Some(q) = val.strip_prefix('"') {
                return q.find('"').map(|j| q[..j].to_string());
            }
            if let Some(q) = val.strip_prefix('\'') {
                return q.find('\'').map(|j| q[..j].to_string());
            }
            let end = val.find(char::is_whitespace).unwrap_or(val.len());
            let v = val[..end].trim_end_matches('/');
            return (!v.is_empty()).then(|| v.to_string());
        }
        // `key` was a substring of something else — keep looking past it.
        search = &search[i + key.len()..];
    }
}

/// Run a read-only tool against `root`, returning a bounded text result to feed
/// back to the model. Never reads outside `root`.
pub fn run_read_tool(root: &Path, tool: &AgentTool) -> String {
    match tool {
        AgentTool::ReadFile { path } => read_file(root, path),
        AgentTool::ListFiles { path } => list_files(root, path),
        AgentTool::Grep { query, path } => grep(root, query, path),
        // run_command is async and goes through `run_command` instead.
        AgentTool::RunCommand { .. } => {
            "error: run_command must be executed asynchronously".to_string()
        }
    }
}

/// Run a shell command in the project root and return its bounded output
/// (exit code + stdout + stderr). Races a timeout so a hanging command can't
/// wedge the agent. Uses the project's `command` crate (an `async_process`
/// wrapper — executor-agnostic, no tokio reactor needed, unlike the SSE path).
pub async fn run_command(root: &Path, shell_command: &str) -> String {
    let exec = async {
        let mut cmd = command::r#async::Command::new("sh");
        cmd.arg("-c").arg(shell_command).current_dir(root);
        match cmd.output().await {
            Ok(output) => format_command_output(&output),
            Err(e) => format!("error: failed to run command: {e}"),
        }
    };
    let timeout = async {
        warpui::r#async::Timer::after(COMMAND_TIMEOUT).await;
        format!("error: command timed out after {}s", COMMAND_TIMEOUT.as_secs())
    };
    futures_lite::future::or(exec, timeout).await
}

fn format_command_output(output: &std::process::Output) -> String {
    let code = output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "killed by signal".to_string());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut out = format!("exit code: {code}\n");
    if !stdout.trim().is_empty() {
        out.push_str("--- stdout ---\n");
        out.push_str(stdout.trim_end());
        out.push('\n');
    }
    if !stderr.trim().is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(stderr.trim_end());
        out.push('\n');
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        out.push_str("(no output)\n");
    }
    if out.chars().count() > MAX_OUTPUT_CHARS {
        out = out.chars().take(MAX_OUTPUT_CHARS).collect();
        out.push_str("\n… [output truncated]");
    }
    out
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
