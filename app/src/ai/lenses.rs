//! Reading lenses: two ways to look at a file that are not its source.
//!
//! Source tells you what a file does. It does not tell you where the file sits
//! in the system, or which lines will hurt at scale — both of which you learn
//! by reading around the file rather than through it. A lens answers one of
//! those questions directly.
//!
//! - **Architecture**: what this file depends on, what it offers, and who
//!   depends on it. Fan-out is in the file; fan-in only exists in the project,
//!   which is why it is the half nobody checks.
//! - **Performance**: the shapes that turn linear work quadratic — awaits and
//!   queries inside loops, nested iteration, accumulator spreads.
//!
//! Everything is static and convention-based. A finding is a place to look, not
//! a verdict, and each one says why it is suspicious so the reader can dismiss
//! it in a second when it is fine.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

const MAX_SCANNED_FILES: usize = 900;
const MAX_FILE_BYTES: u64 = 512 * 1024;

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

/// How bad a performance finding looks from the outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Grows with the size of the data.
    Scaling,
    /// Wasteful, but bounded.
    Waste,
    /// Worth a glance.
    Note,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scaling => "scales badly",
            Self::Waste => "wasteful",
            Self::Note => "note",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub line: usize,
    pub severity: Severity,
    /// What was spotted.
    pub title: String,
    /// Why it is worth a look — the part that lets the reader dismiss it.
    pub detail: String,
    /// The offending line, trimmed.
    pub snippet: String,
}

/// Where a file sits, guessed from its path. Wrong sometimes, useful often —
/// and only ever used as a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Route,
    Controller,
    Service,
    Model,
    Component,
    Hook,
    Store,
    Util,
    Test,
    Config,
    Unknown,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Controller => "controller",
            Self::Service => "service",
            Self::Model => "model / data",
            Self::Component => "component",
            Self::Hook => "hook",
            Self::Store => "store / state",
            Self::Util => "utility",
            Self::Test => "test",
            Self::Config => "config",
            Self::Unknown => "unclassified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureLens {
    pub file: PathBuf,
    pub layer: Layer,
    /// Packages from outside the project.
    pub external_dependencies: Vec<String>,
    /// Project files this one pulls in.
    pub internal_dependencies: Vec<String>,
    /// Names this file offers to the rest of the project.
    pub exports: Vec<String>,
    /// Files that import this one. The half you cannot see from inside.
    pub dependents: Vec<PathBuf>,
    /// True when the project scan stopped early, so `dependents` is a floor
    /// rather than a count.
    pub dependents_truncated: bool,
}

impl ArchitectureLens {
    pub fn fan_out(&self) -> usize {
        self.external_dependencies.len() + self.internal_dependencies.len()
    }

    pub fn fan_in(&self) -> usize {
        self.dependents.len()
    }

    /// A one-line read on the file's position.
    pub fn summary(&self) -> String {
        match (self.fan_in(), self.fan_out()) {
            (0, 0) => "Isolated — nothing imports this, and it imports nothing".to_string(),
            (0, _) => "Nothing in the project imports this file".to_string(),
            (fan_in, _) if fan_in >= 8 => format!(
                "Widely depended on: {fan_in} files import this, so changing its shape is a project-wide edit"
            ),
            (fan_in, fan_out) => format!("{fan_in} files depend on this; it depends on {fan_out}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformanceLens {
    pub file: PathBuf,
    pub findings: Vec<Finding>,
}

impl PerformanceLens {
    pub fn summary(&self) -> String {
        if self.findings.is_empty() {
            return "Nothing worth flagging in this file".to_string();
        }
        let scaling = self
            .findings
            .iter()
            .filter(|finding| finding.severity == Severity::Scaling)
            .count();
        match scaling {
            0 => format!("{} things to glance at", self.findings.len()),
            1 => "1 place that scales badly".to_string(),
            many => format!("{many} places that scale badly"),
        }
    }
}

fn is_source(file: &Path) -> bool {
    matches!(
        file.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "ts" | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "rs"
            | "py"
            | "go"
            | "rb"
            | "java"
            | "kt"
            | "php"
            | "vue"
            | "svelte"
    )
}

pub fn layer_of(file: &Path) -> Layer {
    let path = file.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    // Tests first: a file under `services/` that is also a test is a test.
    if path.contains("/test") || path.contains("/spec") || name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.go")
    {
        return Layer::Test;
    }
    for (needle, layer) in [
        ("route", Layer::Route),
        ("controller", Layer::Controller),
        ("handler", Layer::Controller),
        ("service", Layer::Service),
        ("usecase", Layer::Service),
        ("repositor", Layer::Model),
        ("model", Layer::Model),
        ("entit", Layer::Model),
        ("schema", Layer::Model),
        ("component", Layer::Component),
        ("hook", Layer::Hook),
        ("store", Layer::Store),
        ("reducer", Layer::Store),
        ("context", Layer::Store),
        ("util", Layer::Util),
        ("helper", Layer::Util),
        ("config", Layer::Config),
    ] {
        if path.contains(needle) {
            return layer;
        }
    }
    // React components are capitalised by convention even outside a
    // `components/` directory.
    if matches!(
        file.extension().and_then(|ext| ext.to_str()).unwrap_or(""),
        "tsx" | "jsx"
    ) && file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.chars().next())
        .is_some_and(char::is_uppercase)
    {
        return Layer::Component;
    }
    Layer::Unknown
}

/// Read the architecture lens for one file.
pub fn architecture(root: &Path, file: &Path) -> ArchitectureLens {
    let source = fs::read_to_string(root.join(file)).unwrap_or_default();
    let (external, internal) = dependencies_in(&source, file);
    let (dependents, truncated) = dependents_of(root, file);

    ArchitectureLens {
        file: file.to_path_buf(),
        layer: layer_of(file),
        external_dependencies: external,
        internal_dependencies: internal,
        exports: exports_in(&source),
        dependents,
        dependents_truncated: truncated,
    }
}

/// Imports, split by whether they leave the project.
///
/// The patterns are chosen by language rather than all being tried at once.
/// Python's bare `import x` has the same shape as JavaScript's `import X from
/// "pkg"`, so running both over a `.ts` file reports the local binding `React`
/// as a dependency alongside the package `react`.
fn dependencies_in(source: &str, file: &Path) -> (Vec<String>, Vec<String>) {
    let mut external = BTreeSet::new();
    let mut internal = BTreeSet::new();

    let extension = file
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let patterns: &[&str] = match extension.as_str() {
        "py" => &[r#"(?m)^\s*(?:from\s+([A-Za-z0-9_.]+)\s+import|import\s+([A-Za-z0-9_.]+))"#],
        "rs" => &[r#"(?m)^\s*use\s+([A-Za-z0-9_:]+)"#],
        _ => &[
            r#"(?m)^\s*import\s+.*?from\s*[\x22\x27]([^\x22\x27]+)"#,
            r#"(?m)^\s*import\s*[\x22\x27]([^\x22\x27]+)"#,
            r#"(?m)\brequire\s*\(\s*[\x22\x27]([^\x22\x27]+)"#,
        ],
    };

    for pattern in patterns {
        let regex = Regex::new(pattern).expect("valid dependency regex");
        for captures in regex.captures_iter(source) {
            let Some(value) = captures.get(1).or_else(|| captures.get(2)) else {
                continue;
            };
            let value = value.as_str().trim();
            if value.is_empty() {
                continue;
            }
            // A leading `.` (or `crate`/`super`/`self` in Rust) means it stays
            // inside the project.
            if value.starts_with('.')
                || value.starts_with("crate")
                || value.starts_with("super")
                || value.starts_with("self")
            {
                internal.insert(value.to_string());
            } else {
                // Only the package name is interesting: `lodash/debounce` and
                // `lodash/merge` are one dependency, not two.
                let package = if value.starts_with('@') {
                    value.splitn(3, '/').take(2).collect::<Vec<_>>().join("/")
                } else {
                    value.split('/').next().unwrap_or(value).to_string()
                };
                external.insert(package);
            }
        }
    }
    (
        external.into_iter().collect(),
        internal.into_iter().collect(),
    )
}

fn exports_in(source: &str) -> Vec<String> {
    let mut exports = BTreeSet::new();
    let patterns = [
        r"(?m)^\s*export\s+(?:default\s+)?(?:async\s+)?(?:function|class|const|let|var|interface|type|enum)\s+([A-Za-z_$][\w$]*)",
        r"(?m)^\s*pub\s+(?:async\s+)?(?:fn|struct|enum|trait|type|const)\s+([A-Za-z_][\w]*)",
        r"(?m)^\s*func\s+([A-Z][A-Za-z0-9_]*)\s*\(",
    ];
    for pattern in patterns {
        let regex = Regex::new(pattern).expect("valid export regex");
        for captures in regex.captures_iter(source) {
            if let Some(name) = captures.get(1) {
                exports.insert(name.as_str().to_string());
            }
        }
    }
    exports.into_iter().collect()
}

/// Files that import `file`.
///
/// Matched on the module's stem rather than a resolved path, because the same
/// module is written `./cart`, `../cart`, `@/models/cart` and `models/cart`
/// depending on where it is imported from. That over-matches for a stem like
/// `index`, which is why the stem is required to be distinctive.
fn dependents_of(root: &Path, file: &Path) -> (Vec<PathBuf>, bool) {
    let Some(stem) = file.file_stem().and_then(|stem| stem.to_str()) else {
        return (Vec::new(), false);
    };
    // `index` names half the files in a JS project; matching on it would report
    // every one of them as a dependent.
    if stem.len() < 3 || stem.eq_ignore_ascii_case("index") || stem.eq_ignore_ascii_case("mod") {
        return (Vec::new(), false);
    }

    let pattern = match Regex::new(&format!(
        r#"[\x22\x27][^\x22\x27]*\b{}(?:\.[A-Za-z]+)?[\x22\x27]"#,
        regex::escape(stem)
    )) {
        Ok(pattern) => pattern,
        Err(_) => return (Vec::new(), false),
    };

    let mut dependents = Vec::new();
    let mut seen = HashSet::new();
    let mut scanned = 0_usize;
    let mut truncated = false;
    for candidate in project_files(root) {
        if scanned >= MAX_SCANNED_FILES {
            truncated = true;
            break;
        }
        scanned += 1;
        if candidate == file {
            continue;
        }
        let Ok(source) = fs::read_to_string(root.join(&candidate)) else {
            continue;
        };
        if pattern.is_match(&source) && seen.insert(candidate.clone()) {
            dependents.push(candidate);
        }
    }
    dependents.sort();
    (dependents, truncated)
}

fn project_files(root: &Path) -> Vec<PathBuf> {
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
        .filter(|entry| is_source(entry.path()))
        .take(MAX_SCANNED_FILES)
        .filter_map(|entry| entry.path().strip_prefix(root).map(Path::to_path_buf).ok())
        .collect()
}

/// Read the performance lens for one file.
pub fn performance(root: &Path, file: &Path) -> PerformanceLens {
    let source = fs::read_to_string(root.join(file)).unwrap_or_default();
    PerformanceLens {
        file: file.to_path_buf(),
        findings: scan_performance(&source),
    }
}

/// Whether a line opens a loop.
fn opens_loop(line: &str) -> bool {
    let trimmed = line.trim_start();
    let loop_start = Regex::new(
        r"^(?:\}\s*)?(?:for\b|while\b|loop\b|do\b|\.forEach\s*\(|\.map\s*\(|\.filter\s*\(|\.reduce\s*\()",
    )
    .expect("valid loop regex");
    loop_start.is_match(trimmed)
        || Regex::new(r"\.(?:forEach|map|flatMap)\s*\(\s*(?:async\s*)?\(?[A-Za-z_$]")
            .expect("valid iterator regex")
            .is_match(trimmed)
}

/// Findings for a file, in line order.
///
/// Loop depth is tracked by braces (and by indentation for Python), which is
/// enough to answer "is this line inside a loop" — the question every finding
/// here depends on.
pub fn scan_performance(source: &str) -> Vec<Finding> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut findings = Vec::new();

    // Depth of the brace nesting at which each currently-open loop began.
    let mut loop_stack: Vec<i32> = Vec::new();
    let mut depth = 0_i32;

    let await_call = Regex::new(r"\bawait\b").expect("valid await regex");
    let query_call = Regex::new(
        r"(?i)\b(?:query|findOne|findMany|findAll|fetch|select|execute|get|save|insert|update|delete)\s*\(",
    )
    .expect("valid query regex");
    let json_call = Regex::new(r"\bJSON\.(?:parse|stringify)\s*\(").expect("valid json regex");
    let regex_build =
        Regex::new(r"(?:new\s+RegExp\s*\(|Regex::new\s*\()").expect("valid regex-build regex");
    let spread_accumulator =
        Regex::new(r"\.reduce\s*\(|\[\s*\.\.\.[A-Za-z_$][\w$]*\s*,").expect("valid spread regex");
    let sync_io = Regex::new(
        r"(?:readFileSync|writeFileSync|execSync|\.unwrap\(\)\s*;?\s*$|time\.sleep\s*\(|thread::sleep)",
    )
    .expect("valid sync-io regex");

    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let number = index + 1;

        // A line's own loop-opening is not "inside a loop" for its own sake,
        // so evaluate membership BEFORE pushing this line's loop.
        let inside_loop = !loop_stack.is_empty();

        if inside_loop {
            if await_call.is_match(line) {
                findings.push(Finding {
                    line: number,
                    severity: Severity::Scaling,
                    title: "await inside a loop".to_string(),
                    detail:
                        "Each iteration waits for the last, so the cost is the sum rather than the \
                         slowest. If the calls are independent, collect the promises and await them \
                         together."
                            .to_string(),
                    snippet: line.to_string(),
                });
            } else if query_call.is_match(line) && line.contains('(') {
                findings.push(Finding {
                    line: number,
                    severity: Severity::Scaling,
                    title: "query inside a loop".to_string(),
                    detail:
                        "One round trip per item — the N+1 shape. A single query with an `in` \
                         clause, or a join, replaces the whole loop."
                            .to_string(),
                    snippet: line.to_string(),
                });
            }
            if regex_build.is_match(line) {
                findings.push(Finding {
                    line: number,
                    severity: Severity::Waste,
                    title: "regex compiled inside a loop".to_string(),
                    detail: "Compiling is the expensive half; matching is not. Build it once above \
                             the loop."
                        .to_string(),
                    snippet: line.to_string(),
                });
            }
            if json_call.is_match(line) {
                findings.push(Finding {
                    line: number,
                    severity: Severity::Waste,
                    title: "JSON work inside a loop".to_string(),
                    detail: "Serialising per iteration is usually avoidable by doing it once \
                             outside, or not at all."
                        .to_string(),
                    snippet: line.to_string(),
                });
            }
            if loop_stack.len() >= 2 && opens_loop(line) {
                findings.push(Finding {
                    line: number,
                    severity: Severity::Scaling,
                    title: "loop nested three deep".to_string(),
                    detail: "Cost multiplies with each level. Fine over small fixed collections, \
                             expensive over anything that grows."
                        .to_string(),
                    snippet: line.to_string(),
                });
            } else if opens_loop(line) {
                findings.push(Finding {
                    line: number,
                    severity: Severity::Note,
                    title: "nested loop".to_string(),
                    detail: "Quadratic over the two collections. Worth a look if either can be \
                             large; a lookup map often flattens it."
                        .to_string(),
                    snippet: line.to_string(),
                });
            }
        }

        if sync_io.is_match(line) && line.contains("Sync") {
            findings.push(Finding {
                line: number,
                severity: Severity::Scaling,
                title: "blocking I/O".to_string(),
                detail: "A synchronous read blocks the whole thread, which on a server means every \
                         other request waits behind it."
                    .to_string(),
                snippet: line.to_string(),
            });
        }
        if spread_accumulator.is_match(line) && line.contains("...") {
            findings.push(Finding {
                line: number,
                severity: Severity::Scaling,
                title: "spread into an accumulator".to_string(),
                detail: "Copying the accumulator each iteration makes an O(n) reduce O(n²). Push \
                         into the accumulator and return it instead."
                    .to_string(),
                snippet: line.to_string(),
            });
        }

        // Track nesting for the next line.
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if opens_loop(line) {
            loop_stack.push(depth);
        }
        depth += opens - closes;
        while loop_stack.last().is_some_and(|start| depth <= *start) {
            loop_stack.pop();
        }
    }

    findings.sort_by(|left, right| {
        left.severity
            .cmp(&right.severity)
            .then(left.line.cmp(&right.line))
    });
    findings
}

#[cfg(test)]
#[path = "lenses_tests.rs"]
mod tests;
