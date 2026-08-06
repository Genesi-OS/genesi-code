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
/// model can't loop forever. Generous enough to read several files and then
/// create/edit several more in one turn (multi-file changes), which the old
/// budget of 8 cut short.
pub const MAX_AGENT_STEPS: u32 = 24;

/// A tool the agent can call. Reads run immediately; `run_command` is
/// side-effecting and gated by user approval (unless AUTO is on).
#[derive(Debug, Clone)]
pub enum AgentTool {
    ReadFile {
        path: String,
    },
    ListFiles {
        path: String,
    },
    Grep {
        query: String,
        path: String,
    },
    RunCommand {
        command: String,
    },
    EditFile {
        path: String,
        search: String,
        replace: String,
    },
    /// Create or overwrite a file with the full given content. Far more reliable
    /// for small local models than a SEARCH/REPLACE diff — they routinely emit
    /// the whole file, which is exactly what this takes.
    WriteFile {
        path: String,
        content: String,
    },
}

impl AgentTool {
    pub fn name(&self) -> &'static str {
        match self {
            AgentTool::ReadFile { .. } => "read_file",
            AgentTool::ListFiles { .. } => "list_files",
            AgentTool::Grep { .. } => "grep",
            AgentTool::RunCommand { .. } => "run_command",
            AgentTool::EditFile { .. } => "edit_file",
            AgentTool::WriteFile { .. } => "write_file",
        }
    }

    /// A compact one-line summary for the transcript, e.g. `read_file src/main.rs`.
    pub fn summary(&self) -> String {
        match self {
            AgentTool::ReadFile { path } => format!("read_file {path}"),
            AgentTool::ListFiles { path } => format!("list_files {path}"),
            AgentTool::Grep { query, path } => format!("grep \"{query}\" in {path}"),
            AgentTool::RunCommand { command } => format!("run {command}"),
            AgentTool::EditFile { path, .. } => format!("edit {path}"),
            AgentTool::WriteFile { path, .. } => format!("write {path}"),
        }
    }

    /// Whether running this tool needs the user's go-ahead (anything that can
    /// change the system). Reads never do.
    pub fn requires_approval(&self) -> bool {
        matches!(
            self,
            AgentTool::RunCommand { .. } | AgentTool::EditFile { .. } | AgentTool::WriteFile { .. }
        )
    }

    /// Render the call back into the text protocol, so a call that arrived in a
    /// model's NATIVE tool-call format is written into the conversation in the
    /// one shape [`parse_tool_call`] and the transcript both understand. Without
    /// this the history would describe the call in prose, and the model would
    /// drift away from the tag format on the next step.
    pub fn to_tag(&self) -> String {
        match self {
            AgentTool::ReadFile { path } => format!("<tool:read_file path=\"{path}\"/>"),
            AgentTool::ListFiles { path } => format!("<tool:list_files path=\"{path}\"/>"),
            AgentTool::Grep { query, path } => {
                format!("<tool:grep query=\"{query}\" path=\"{path}\"/>")
            }
            AgentTool::RunCommand { command } => {
                format!("<tool:run_command>{command}</tool>")
            }
            AgentTool::WriteFile { path, content } => {
                format!("<tool:write_file path=\"{path}\">\n{content}\n</tool>")
            }
            AgentTool::EditFile {
                path,
                search,
                replace,
            } => format!(
                "<tool:edit_file path=\"{path}\">\n<<<<<<< SEARCH\n{search}\n=======\n{replace}\n>>>>>>> REPLACE\n</tool>"
            ),
        }
    }
}

/// Build a tool from a model's NATIVE (OpenAI-shaped) tool call: a function name
/// plus a JSON argument object.
///
/// A harmony model such as gpt-oss announces a tool on its `commentary` channel
/// rather than in its answer, and llama-server surfaces that as `tool_calls`. It
/// was never handed a tool schema — it calls the names the system prompt taught
/// it — so match on those names and pull the arguments the text protocol uses.
/// Argument names are matched leniently because the model is inventing them.
pub fn tool_from_native_call(name: &str, arguments_json: &str) -> Option<AgentTool> {
    let args: serde_json::Value =
        serde_json::from_str(arguments_json).unwrap_or(serde_json::Value::Null);
    let arg = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| args.get(key).and_then(|value| value.as_str()))
            .map(str::to_owned)
    };
    // Providers namespace the function ("functions.read_file"); keep the leaf.
    let name = name.rsplit('.').next().unwrap_or(name).trim();
    match name {
        "read_file" | "open_file" | "cat" => Some(AgentTool::ReadFile {
            path: arg(&["path", "file", "filename"])?,
        }),
        "list_files" | "ls" | "list_dir" | "list_directory" => Some(AgentTool::ListFiles {
            path: arg(&["path", "dir", "directory"]).unwrap_or_else(|| ".".to_string()),
        }),
        "grep" | "search" | "search_files" => Some(AgentTool::Grep {
            query: arg(&["query", "pattern", "text"])?,
            path: arg(&["path", "dir", "directory"]).unwrap_or_else(|| ".".to_string()),
        }),
        "run_command" | "run" | "shell" | "bash" | "exec" => Some(AgentTool::RunCommand {
            command: arg(&["command", "cmd", "script"])?,
        }),
        "write_file" | "create_file" | "save_file" => Some(AgentTool::WriteFile {
            path: arg(&["path", "file", "filename"])?,
            content: arg(&["content", "contents", "text", "body"]).unwrap_or_default(),
        }),
        "edit_file" | "replace_in_file" | "patch_file" => {
            let path = arg(&["path", "file", "filename"])?;
            match (
                arg(&["search", "old", "old_string", "find"]),
                arg(&["replace", "new", "new_string"]),
            ) {
                (Some(search), Some(replace)) => Some(AgentTool::EditFile {
                    path,
                    search,
                    replace,
                }),
                // A model that "edits" by handing back the whole file is doing a
                // write; take it as one rather than dropping the call.
                _ => arg(&["content", "contents", "text"])
                    .map(|content| AgentTool::WriteFile { path, content }),
            }
        }
        _ => None,
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
  <tool:run_command>echo hello</tool>\n\
  <tool:write_file path=\"src/hello.js\">\n\
the COMPLETE new contents of the file go here\n\
</tool>\n\
  <tool:edit_file path=\"src/main.rs\">\n\
<<<<<<< SEARCH\n\
exact text to find\n\
=======\n\
new text\n\
>>>>>>> REPLACE\n\
</tool>\n\n\
After a tool runs, you receive its result as the next message. Then call another \
tool, or give your final answer in plain text (no tool tag).\n\n\
Example — create a file:\n\
User: create hello.js that logs hi\n\
Assistant: <tool:write_file path=\"hello.js\">\n\
console.log(\"hi\");\n\
</tool>\n\
(result: wrote hello.js)\n\
Assistant: Done — I created hello.js.\n\n\
Rules:\n\
- Write the tool tag in your ANSWER, not in your private reasoning. Thinking \
about calling a tool does not call it: the tag must appear in the message you \
actually send, or nothing happens.\n\
- Use ONE tool per message. Paths are relative to the project root; use \".\" \
for the root.\n\
- Prefer list_files / read_file before answering questions about the code.\n\
- To CREATE a new file, or to REWRITE a whole file, use write_file with the \
COMPLETE file contents in the body. This is the preferred way to write code — \
do NOT use SEARCH/REPLACE markers inside write_file.\n\
- Use edit_file ONLY for a small change to an existing file: put the EXACT text \
to find between `<<<<<<< SEARCH` and `=======`, and the new text between \
`=======` and `>>>>>>> REPLACE`. Read the file first so SEARCH matches it \
character-for-character. If you are unsure, prefer write_file with the full \
contents instead.\n\
- To change SEVERAL files, do them one at a time: one write_file/edit_file per \
message, and keep going until every file is done before giving your final \
answer.\n\
- run_command runs a shell command and returns its output.\n\
- Edits and commands may ask the user to approve them first.\n\
- Be concise and reference the files and lines you actually read. Do not invent \
file contents."
        .to_string()
}

/// Parse the first tool call out of a model reply, if any. Deliberately lenient
/// — small local models produce near-miss formats — accepting the `<tool:NAME>`
/// and bare `<NAME>` tags, self-closing or block form, and quoted or unquoted
/// attributes.
pub fn parse_tool_call(text: &str) -> Option<AgentTool> {
    // write_file / create_file: a block whose body is the full file content.
    // Checked before edit_file so an explicit write is never mis-parsed as a
    // (markerless) edit.
    if let Some(tool) = parse_write_file(text) {
        return Some(tool);
    }
    // edit_file: a block with a `path` attr and a SEARCH/REPLACE body (falls
    // back to a full write when the model omits/mangles the markers).
    if let Some(tool) = parse_edit_file(text) {
        return Some(tool);
    }
    // run_command: a plain block whose body can contain anything.
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

/// Strip any tool-call markup from a model reply, leaving only the human-facing
/// prose that precedes it. The panel uses this so it never shows raw
/// `<tool:read_file …/>` tags while a step streams — the tag becomes a clean
/// collapsible tool step instead of flashing as ugly XML in the transcript.
///
/// Returns the trimmed prose before the first tool marker (often empty, since
/// the prompt tells the model to reply with ONLY the tool tag).
pub fn strip_tool_calls(text: &str) -> String {
    const OPENERS: &[&str] = &[
        "<tool:",
        "<tool ",
        "<read_file",
        "<list_files",
        "<grep",
        "<run_command",
        "<edit_file",
        "<write_file",
        "<create_file",
        "<<<<<<< SEARCH",
    ];
    let cut = OPENERS
        .iter()
        .filter_map(|opener| text.find(opener))
        .min()
        .unwrap_or(text.len());
    text[..cut].trim().to_string()
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

/// Parse a `<tool:write_file path="…">FULL CONTENT</tool>` (or `create_file`)
/// call. The body is the complete file content, taken verbatim aside from a
/// single leading/trailing newline around the markup. Small models handle this
/// far more reliably than a SEARCH/REPLACE diff, so it's the preferred writer.
fn parse_write_file(text: &str) -> Option<AgentTool> {
    for name in ["write_file", "create_file"] {
        let Some(attrs) = tag_attrs(text, name) else {
            continue;
        };
        let Some(path) = attr(&attrs, "path") else {
            continue;
        };
        let Some(body) = block_tool_body(text, name) else {
            continue;
        };
        return Some(AgentTool::WriteFile {
            path,
            content: clean_file_body(&body),
        });
    }
    None
}

/// Parse an `<tool:edit_file path="…"> … SEARCH/REPLACE … </tool>` call.
///
/// Small local models frequently get the conflict-marker format wrong — they
/// drop the `<<<<<<< SEARCH` line, or emit the whole new file with no markers at
/// all (see the create-a-file case). Rather than silently failing (which made
/// the tag render as raw text and "nothing happened"), fall back to a full
/// write of the body when a valid SEARCH/REPLACE block isn't present.
fn parse_edit_file(text: &str) -> Option<AgentTool> {
    let attrs = tag_attrs(text, "edit_file")?;
    let path = attr(&attrs, "path")?;
    let body = block_tool_body(text, "edit_file")?;
    match parse_search_replace(&body) {
        Some((search, replace)) => Some(AgentTool::EditFile {
            path,
            search,
            replace,
        }),
        None => Some(AgentTool::WriteFile {
            path,
            content: clean_file_body(&body),
        }),
    }
}

/// Tidy a tool body into file content: drop a single leading/trailing newline
/// introduced by the tags, and strip any stray conflict-marker lines a confused
/// model left behind (`<<<<<<< SEARCH`, `=======`, `>>>>>>> REPLACE`) so they
/// don't end up written into the file.
fn clean_file_body(body: &str) -> String {
    let body = body.strip_prefix('\n').unwrap_or(body);
    let body = body.strip_suffix('\n').unwrap_or(body);
    let has_marker = body.lines().any(is_conflict_marker);
    if !has_marker {
        return body.to_string();
    }
    body.lines()
        .filter(|line| !is_conflict_marker(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a line is a git-style conflict marker the model may have emitted.
fn is_conflict_marker(line: &str) -> bool {
    let t = line.trim_end();
    t.starts_with("<<<<<<<") || t == "=======" || t.starts_with(">>>>>>>")
}

/// Pull the SEARCH and REPLACE halves out of a conflict-marker block. Preserves
/// the content's indentation (only the marker lines and their newlines are
/// stripped).
fn parse_search_replace(body: &str) -> Option<(String, String)> {
    const SEARCH: &str = "<<<<<<< SEARCH";
    const DIVIDER: &str = "=======";
    const REPLACE: &str = ">>>>>>> REPLACE";

    let after_search = &body[body.find(SEARCH)? + SEARCH.len()..];
    let search_start = after_search.find('\n')? + 1;
    let after = &after_search[search_start..];

    let divider = after.find(DIVIDER)?;
    let search = &after[..divider];

    let after_divider = &after[divider + DIVIDER.len()..];
    let replace_start = after_divider.find('\n')? + 1;
    let after_replace = &after_divider[replace_start..];

    let replace_end = after_replace.find(REPLACE)?;
    let replace = &after_replace[..replace_end];

    Some((
        strip_one_trailing_newline(search),
        strip_one_trailing_newline(replace),
    ))
}

/// Remove the single trailing newline that sits before the next marker line,
/// without touching the content's own trailing whitespace.
fn strip_one_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').unwrap_or(s).to_string()
}

/// Run a read-only tool against `root`, returning a bounded text result to feed
/// back to the model. Never reads outside `root`.
pub fn run_local_tool(root: &Path, tool: &AgentTool) -> String {
    match tool {
        AgentTool::ReadFile { path } => read_file(root, path),
        AgentTool::ListFiles { path } => list_files(root, path),
        AgentTool::Grep { query, path } => grep(root, query, path),
        AgentTool::EditFile {
            path,
            search,
            replace,
        } => edit_file(root, path, search, replace),
        AgentTool::WriteFile { path, content } => write_file(root, path, content),
        // run_command is async and goes through `run_command` instead.
        AgentTool::RunCommand { .. } => {
            "error: run_command must be executed asynchronously".to_string()
        }
    }
}

/// Create or overwrite a file with the full given content, creating any missing
/// parent directories. The reliable path for small models, which emit whole
/// files far more dependably than they produce exact SEARCH/REPLACE diffs.
fn write_file(root: &Path, rel: &str, content: &str) -> String {
    let Some(path) = safe_resolve(root, rel) else {
        return format!("error: path '{rel}' is outside the project");
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("error creating directories for '{rel}': {e}");
        }
    }
    let existed = path.exists();
    match std::fs::write(&path, content) {
        Ok(()) => {
            let verb = if existed { "overwrote" } else { "wrote" };
            format!("{verb} {rel} ({} bytes)", content.len())
        }
        Err(e) => format!("error writing '{rel}': {e}"),
    }
}

/// Apply a SEARCH/REPLACE edit to a file on disk. An empty SEARCH creates (or
/// overwrites) the file with REPLACE. Otherwise the first exact match of SEARCH
/// is replaced; a miss is reported so the model can re-read and retry.
fn edit_file(root: &Path, rel: &str, search: &str, replace: &str) -> String {
    let Some(path) = safe_resolve(root, rel) else {
        return format!("error: path '{rel}' is outside the project");
    };

    if search.is_empty() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return match std::fs::write(&path, replace) {
            Ok(()) => format!("wrote {rel} ({} bytes)", replace.len()),
            Err(e) => format!("error writing '{rel}': {e}"),
        };
    }

    let original = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) => return format!("error reading '{rel}': {e}"),
    };
    if !original.contains(search) {
        return format!(
            "error: the SEARCH block was not found in {rel}. Read the file and \
             make SEARCH match it exactly."
        );
    }
    let updated = original.replacen(search, replace, 1);
    match std::fs::write(&path, &updated) {
        Ok(()) => format!("edited {rel} (1 replacement)"),
        Err(e) => format!("error writing '{rel}': {e}"),
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
        format!(
            "error: command timed out after {}s",
            COMMAND_TIMEOUT.as_secs()
        )
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
/// Public, safety-preserving path resolver for callers outside this module (the
/// chat panel snapshots a file before an edit and restores it on Undo, and must
/// never read or write outside the project root). Thin wrapper over
/// [`safe_resolve`].
pub fn resolve_in_project(root: &Path, rel: &str) -> Option<PathBuf> {
    safe_resolve(root, rel)
}

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

#[cfg(test)]
#[path = "local_agent_tests.rs"]
mod tests;
