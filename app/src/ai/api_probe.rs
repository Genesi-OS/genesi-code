//! Discovery and request execution for Genesi Probe — the built-in API client.
//!
//! Probe answers two questions the Project Canvas raises but cannot close:
//! *which of these endpoints is actually reachable right now*, and *what does it
//! answer*. It therefore has two halves:
//!
//! 1. **Discovery** — reuses [`super::project_canvas::analyze_project`] for the
//!    route list (same regexes, one source of truth) and adds a port survey:
//!    the OS listening table where the platform exposes it, ports declared in
//!    project config/source, and a bounded sweep of well-known dev ports.
//! 2. **Execution** — a plain HTTP round trip the user drives.
//!
//! Discovery never sends an HTTP request. This is deliberate, and it is the
//! lesson Genesi Ports learned the hard way: probing app routes to find out
//! whether they exist hits real handlers, and a backend whose handler does
//! `req.json()` on an empty body *crashes*. Liveness here is a TCP connect and
//! nothing more — the only requests Probe ever makes are the ones the user
//! presses Send on.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use walkdir::{DirEntry, WalkDir};

use super::project_canvas::{analyze_project, CanvasNodeKind, ProjectCanvasGraph};

/// How long a liveness connect may block. Loopback either answers immediately or
/// is not there, so this only needs to cover a busy machine, not a network.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(120);

/// Ceiling on the request itself. Long enough for a cold dev server that
/// recompiles on first hit, short enough that a hung port does not look frozen.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on establishing the connection, as opposed to the whole exchange.
/// A server that is not there should say so immediately rather than burning the
/// request timeout first.
const CONNECT_TIMEOUT_HTTP: Duration = Duration::from_secs(5);

/// Response bodies past this are cut before they reach the view. A response
/// pane is not a file viewer: shaping megabytes of text costs a frame on every
/// repaint, and nobody reads it. [`ProbeResponse::size_bytes`] still reports the
/// true size, so a truncated body is visible as such rather than misleading.
const MAX_RESPONSE_BYTES: usize = 128 * 1024;

const MAX_CONFIG_FILES: usize = 600;
const MAX_CONFIG_FILE_BYTES: u64 = 256 * 1024;

/// Well-known dev-server ports, checked when nothing else pointed at them.
/// Kept short on purpose: every entry is a connect with a timeout, and a long
/// list turns "open Probe" into a visible pause.
const COMMON_DEV_PORTS: &[u16] = &[
    3000, 3001, 4000, 4200, 5000, 5173, 5174, 7000, 8000, 8080, 8081, 8888, 9000,
];

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

/// Methods offered in the picker, in the order they are shown.
pub const PROBE_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Why a port is in the list — which also ranks it, since a socket the OS
/// confirms is listening is worth more than one we merely guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortOrigin {
    /// Present in the OS listening table.
    Listening,
    /// Declared by the project (`PORT=3000`, `app.listen(8080)`, compose, …).
    Declared,
    /// A well-known dev port, checked on spec.
    Common,
}

impl PortOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Listening => "listening",
            Self::Declared => "declared",
            Self::Common => "common",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePort {
    pub port: u16,
    /// A TCP connect to loopback succeeded.
    pub open: bool,
    pub origin: PortOrigin,
    /// Process name, where the platform lets us read it without privileges.
    pub process: Option<String>,
    /// The address the socket is bound to, for display (`0.0.0.0`, `127.0.0.1`).
    pub bound_to: Option<String>,
    /// Where the project declared it, when that is how we found it.
    pub declared_in: Option<PathBuf>,
    pub declared_line: usize,
}

impl ProbePort {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// One-line provenance for the sidebar: what it is and how we know.
    pub fn detail(&self) -> String {
        let state = if self.open { "open" } else { "closed" };
        match (&self.process, &self.declared_in) {
            (Some(process), _) => format!("{state} · {process}"),
            (None, Some(path)) => format!("{state} · {}", path.display()),
            (None, None) => format!("{state} · {}", self.origin.label()),
        }
    }
}

/// An endpoint the user can fire at, lifted from the Canvas graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRoute {
    pub id: String,
    pub method: String,
    pub route: String,
    pub source: PathBuf,
    pub line: usize,
    /// What the handler appears to read off the request, if anything.
    pub body_hint: Option<String>,
}

impl ProbeRoute {
    /// Path parameters (`:id`, `{id}`, `[id]`) the user still has to fill in.
    pub fn placeholders(&self) -> Vec<String> {
        self.route
            .split('/')
            .filter_map(|segment| {
                let name = segment
                    .strip_prefix(':')
                    .or_else(|| {
                        segment
                            .strip_prefix('{')
                            .and_then(|value| value.strip_suffix('}'))
                    })
                    .or_else(|| {
                        segment
                            .strip_prefix('[')
                            .and_then(|value| value.strip_suffix(']'))
                    })
                    .or_else(|| segment.strip_prefix('*'))?;
                (!name.is_empty()).then(|| name.to_string())
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSurvey {
    pub root: PathBuf,
    pub project_name: String,
    pub stacks: Vec<String>,
    pub ports: Vec<ProbePort>,
    pub routes: Vec<ProbeRoute>,
}

impl ProbeSurvey {
    /// The port a request should default to: the best open one, preferring a
    /// socket the OS confirmed over a lucky hit on a common port.
    pub fn preferred_port(&self) -> Option<&ProbePort> {
        self.ports.iter().find(|port| port.open)
    }

    pub fn base_url(&self) -> String {
        self.preferred_port()
            .map(ProbePort::base_url)
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
    }

    pub fn open_port_count(&self) -> usize {
        self.ports.iter().filter(|port| port.open).count()
    }

    pub fn route(&self, id: &str) -> Option<&ProbeRoute> {
        self.routes.iter().find(|route| route.id == id)
    }
}

/// Everything Probe needs to send one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRequest {
    pub method: String,
    pub url: String,
    /// Raw `Key: Value` lines as typed, parsed at send time.
    pub headers: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub elapsed_ms: u128,
    pub size_bytes: usize,
    pub content_type: Option<String>,
    /// The body was longer than [`MAX_RESPONSE_BYTES`] and has been cut.
    pub truncated: bool,
}

impl ProbeResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn status_line(&self) -> String {
        format!("{} {}", self.status, self.status_text)
    }

    /// `1.2 KB` / `340 B` — the size as the response panel shows it.
    pub fn size_label(&self) -> String {
        let bytes = self.size_bytes as f64;
        if self.size_bytes < 1024 {
            format!("{} B", self.size_bytes)
        } else if self.size_bytes < 1024 * 1024 {
            format!("{:.1} KB", bytes / 1024.)
        } else {
            format!("{:.1} MB", bytes / (1024. * 1024.))
        }
    }
}

/// Walk the project for routes and survey the machine for ports.
///
/// `graph` lets a caller that already analysed the project (the Canvas) hand its
/// result over instead of paying for a second walk.
pub fn survey_project(root: &Path, graph: Option<Arc<ProjectCanvasGraph>>) -> ProbeSurvey {
    let graph = graph.unwrap_or_else(|| Arc::new(analyze_project(root)));
    let routes = graph
        .nodes
        .iter()
        .filter(|node| node.kind == CanvasNodeKind::Endpoint)
        .map(|node| ProbeRoute {
            id: node.id.clone(),
            method: node.method.clone().unwrap_or_else(|| "GET".to_string()),
            route: node.route.clone().unwrap_or_else(|| "/".to_string()),
            source: node.source.clone(),
            line: node.line,
            body_hint: node.request_body.clone(),
        })
        .collect::<Vec<_>>();

    ProbeSurvey {
        root: root.to_path_buf(),
        project_name: graph.project_name.clone(),
        stacks: graph.stacks.clone(),
        ports: scan_ports(root),
        routes,
    }
}

/// Survey ports alone, for the rescan button — re-walking the whole project to
/// learn that :3000 came up is wasted work.
pub fn scan_ports(root: &Path) -> Vec<ProbePort> {
    // BTreeMap so the merge below is deterministic and the result comes out
    // ordered by port before the final ranking sort.
    let mut ports: BTreeMap<u16, ProbePort> = BTreeMap::new();

    for socket in listening_sockets() {
        ports.insert(
            socket.port,
            ProbePort {
                port: socket.port,
                // The OS says it is listening; no need to knock.
                open: true,
                origin: PortOrigin::Listening,
                process: socket.process,
                bound_to: Some(socket.bound_to),
                declared_in: None,
                declared_line: 0,
            },
        );
    }

    for declared in declared_ports(root) {
        let entry = ports.entry(declared.port).or_insert_with(|| ProbePort {
            port: declared.port,
            open: false,
            origin: PortOrigin::Declared,
            process: None,
            bound_to: None,
            declared_in: None,
            declared_line: 0,
        });
        // A port can be both listening and declared — keep the stronger origin
        // but take the provenance, since "declared in vite.config.ts" is the
        // more useful thing to show next to it.
        if entry.declared_in.is_none() {
            entry.declared_in = Some(declared.source);
            entry.declared_line = declared.line;
        }
    }

    for port in COMMON_DEV_PORTS {
        ports.entry(*port).or_insert_with(|| ProbePort {
            port: *port,
            open: false,
            origin: PortOrigin::Common,
            process: None,
            bound_to: None,
            declared_in: None,
            declared_line: 0,
        });
    }

    let mut ports = ports.into_values().collect::<Vec<_>>();
    for port in &mut ports {
        if !port.open {
            port.open = is_port_open(port.port);
        }
    }

    // Open first, then by how sure we are it belongs to this project, then by
    // number so the list does not reshuffle between scans.
    ports.sort_by(|left, right| {
        right
            .open
            .cmp(&left.open)
            .then(left.origin.cmp(&right.origin))
            .then(left.port.cmp(&right.port))
    });
    ports
}

/// Liveness by TCP connect: the connection is opened and dropped without a byte
/// being written. A server sees one empty connection per scan — harmless, and
/// the price of never touching an application route to find out it exists.
fn is_port_open(port: u16) -> bool {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok()
}

/// Join a base URL and a route into something reqwest will accept.
pub fn build_url(base: &str, route: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let route = route.trim();
    if route.is_empty() {
        return base.to_string();
    }
    if route.starts_with("http://") || route.starts_with("https://") {
        return route.to_string();
    }
    if route.starts_with('/') {
        format!("{base}{route}")
    } else {
        format!("{base}/{route}")
    }
}

/// Parse the header pane: one `Key: Value` per line, `#` comments out a line.
///
/// Returned as pairs rather than a map because duplicate headers are legal and
/// occasionally the point (`Set-Cookie`, repeated `Accept`).
pub fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Whether a method carries a body, which decides if the body pane is sent at
/// all. A GET with a body is legal but confuses enough servers to be a bad
/// default.
pub fn method_takes_body(method: &str) -> bool {
    !matches!(
        method.trim().to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS"
    )
}

/// Send the request the user composed.
pub async fn send_request(request: ProbeRequest) -> Result<ProbeResponse> {
    let url = request.url.trim();
    if url.is_empty() {
        return Err(anyhow!("Enter a URL first."));
    }
    // A bare `localhost:3000/api` is what people type; make it a URL rather
    // than failing with reqwest's "relative URL without a base".
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("http://{url}")
    };

    let method = reqwest::Method::from_bytes(request.method.trim().to_ascii_uppercase().as_bytes())
        .with_context(|| format!("`{}` is not a valid HTTP method", request.method))?;

    let client = client_for(&url);
    let mut builder = client.request(method.clone(), &url);
    let headers = parse_headers(&request.headers);
    let has_content_type = headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("content-type"));
    for (key, value) in headers {
        builder = builder.header(key, value);
    }

    let body = request.body.trim();
    if !body.is_empty() && method_takes_body(&request.method) {
        // Typing JSON and getting a 415 back because nobody set the header is
        // the single most common way a hand-written request fails.
        if !has_content_type && looks_like_json(body) {
            builder = builder.header("content-type", "application/json");
        }
        builder = builder.body(request.body.clone());
    }

    let started = Instant::now();
    let response = builder.send().await.map_err(describe_transport_error)?;
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect::<Vec<_>>();
    let content_type = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone());

    let bytes = response
        .bytes()
        .await
        .context("the response ended before the body was read")?;
    let elapsed_ms = started.elapsed().as_millis();
    let size_bytes = bytes.len();
    let truncated = size_bytes > MAX_RESPONSE_BYTES;
    let visible = &bytes[..size_bytes.min(MAX_RESPONSE_BYTES)];
    let body = match std::str::from_utf8(visible) {
        Ok(text) => prettify(text, content_type.as_deref()),
        Err(_) => format!("<{size_bytes} bytes of binary response>"),
    };

    Ok(ProbeResponse {
        status: status.as_u16(),
        status_text: status
            .canonical_reason()
            .unwrap_or("Unknown")
            .to_string(),
        headers,
        body,
        elapsed_ms,
        size_bytes,
        content_type,
        truncated,
    })
}

/// The HTTP client to use for `url`, built once and reused.
///
/// Two things here were costing seconds per request. The client was rebuilt on
/// every send, and building a reqwest client loads the system proxy
/// configuration — which this codebase already knows is slow enough to be worth
/// disabling in tests (see `http_client::Client::new_for_test`). Worse, without
/// `no_proxy` a request to `127.0.0.1` is handed to whatever proxy the desktop
/// has configured, which on a box running genesi-proxy is a round trip through
/// something that was never meant to serve loopback.
///
/// So: loopback gets a client that has never heard of a proxy, everything else
/// gets one that respects the system settings, and both are built at most once
/// for the life of the process.
fn client_for(url: &str) -> &'static reqwest::Client {
    static LOOPBACK: OnceLock<reqwest::Client> = OnceLock::new();
    static EXTERNAL: OnceLock<reqwest::Client> = OnceLock::new();

    let base = || {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // Without this a dead port waits out the full request timeout
            // before admitting nothing is there.
            .connect_timeout(CONNECT_TIMEOUT_HTTP)
    };
    if is_loopback_url(url) {
        LOOPBACK.get_or_init(|| {
            base()
                .no_proxy()
                .build()
                .expect("a loopback HTTP client should always build")
        })
    } else {
        EXTERNAL.get_or_init(|| {
            base()
                .build()
                .expect("an HTTP client should always build")
        })
    }
}

/// Whether this URL points at the machine Probe is running on.
fn is_loopback_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip any userinfo and the port, leaving the host.
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = host.rsplit_once(':').map(|(host, _)| host).unwrap_or(host);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
        || host.starts_with("127.")
}

/// reqwest's `Display` stops at "error sending request"; the cause underneath is
/// the part that tells you the server is not running.
fn describe_transport_error(error: reqwest::Error) -> anyhow::Error {
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        detail = format!("{detail}: {cause}");
        source = cause.source();
    }
    if error.is_connect() {
        detail.push_str("\n\nNothing is listening there. Start the server, or pick an open port from the sidebar.");
    } else if error.is_timeout() {
        detail.push_str("\n\nThe server accepted the connection but never answered.");
    }
    anyhow!(detail)
}

fn looks_like_json(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

/// Re-indent a JSON body so the response pane is readable. Anything that is not
/// JSON, or is JSON we cannot parse, is shown exactly as it arrived.
fn prettify(text: &str, content_type: Option<&str>) -> String {
    let is_json = content_type
        .map(|value| value.to_ascii_lowercase().contains("json"))
        .unwrap_or_else(|| looks_like_json(text));
    if !is_json {
        return text.to_string();
    }
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string())
}

#[derive(Debug, Clone)]
struct DeclaredPort {
    port: u16,
    source: PathBuf,
    line: usize,
}

/// Off Linux nothing constructs one of these — the fallback sweep in
/// [`scan_ports`] covers those platforms — so the fields read as dead there.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ListeningSocket {
    port: u16,
    bound_to: String,
    process: Option<String>,
}

/// Ports the project itself names, so a dev server that is not running yet still
/// shows up with the right number instead of forcing the user to remember it.
fn declared_ports(root: &Path) -> Vec<DeclaredPort> {
    let patterns = [
        // PORT=3000 / "port": 3000 / port: 8080
        r"(?im)\bport\s*[=:]\s*[\x22\x27]?(\d{2,5})",
        // app.listen(3000) / server.listen(8080, …)
        r"(?i)\.listen\s*\(\s*[\x22\x27]?(?:[\d.]+:)?(\d{2,5})",
        // --port 5173 / --port=8000
        r"(?i)--port[=\s]+(\d{2,5})",
        // "0.0.0.0:8080" / "127.0.0.1:5000" in bind/addr strings
        r"(?i)[\x22\x27](?:[\d.]+|localhost)?:(\d{2,5})[\x22\x27]",
        // compose: - "3000:3000"
        r"(?m)^\s*-\s*[\x22\x27]?(\d{2,5}):\d{2,5}",
    ];
    let compiled = patterns
        .iter()
        .map(|pattern| Regex::new(pattern).expect("valid declared-port regex"))
        .collect::<Vec<_>>();

    let mut found = Vec::new();
    let mut seen = HashMap::new();
    for file in port_config_files(root) {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let relative = file
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| file.clone());
        for regex in &compiled {
            for captures in regex.captures_iter(&content) {
                let (Some(value), Some(whole)) = (captures.get(1), captures.get(0)) else {
                    continue;
                };
                let Ok(port) = value.as_str().parse::<u16>() else {
                    continue;
                };
                // Below 80 these are almost always something else that happens
                // to be spelled `port` — a timeout, an index, a version.
                if port < 80 {
                    continue;
                }
                if seen.insert(port, ()).is_some() {
                    continue;
                }
                found.push(DeclaredPort {
                    port,
                    source: relative.clone(),
                    line: line_number(&content, whole.start()),
                });
            }
        }
    }
    found
}

/// The files worth reading for a port: config by name anywhere in the tree, plus
/// ordinary source, bounded so this stays a fraction of the Canvas walk.
fn port_config_files(root: &Path) -> Vec<PathBuf> {
    let source_extensions = [
        "js", "jsx", "ts", "tsx", "mjs", "cjs", "py", "go", "rs", "rb", "java", "kt", "php",
    ];
    WalkDir::new(root)
        .follow_links(false)
        .max_depth(6)
        .into_iter()
        .filter_entry(should_visit)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .metadata()
                .map(|metadata| metadata.len() <= MAX_CONFIG_FILE_BYTES)
                .unwrap_or(false)
        })
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let is_config = name.starts_with(".env")
                || name.starts_with("docker-compose")
                || name == "compose.yml"
                || name == "compose.yaml"
                || name == "dockerfile"
                || name == "package.json"
                || name == "procfile"
                || name.starts_with("vite.config")
                || name.starts_with("next.config")
                || name.starts_with("nuxt.config")
                || name.starts_with("vue.config")
                || name.starts_with("svelte.config")
                || name.starts_with("angular.json");
            let is_source = entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| source_extensions.contains(&ext.to_ascii_lowercase().as_str()));
            is_config || is_source
        })
        .take(MAX_CONFIG_FILES)
        .map(DirEntry::into_path)
        .collect()
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !SKIP_DIRS.contains(&name.as_ref())
}

fn line_number(content: &str, byte_offset: usize) -> usize {
    content.as_bytes()[..byte_offset.min(content.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// Read the OS listening table.
///
/// Linux exposes it in procfs, which is both exact and free — it finds a server
/// on a port nobody would have guessed, and names the process behind it. Every
/// other platform falls back to the connect sweep in [`scan_ports`], which still
/// finds anything on a declared or common port.
#[cfg(target_os = "linux")]
fn listening_sockets() -> Vec<ListeningSocket> {
    const TCP_LISTEN: &str = "0A";

    let mut sockets: Vec<(u16, String, u64)> = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            // sl local_address rem_address st … inode
            if fields.len() < 10 || fields[3] != TCP_LISTEN {
                continue;
            }
            let Some((address, port)) = fields[1].rsplit_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port, 16) else {
                continue;
            };
            let Ok(inode) = fields[9].parse::<u64>() else {
                continue;
            };
            sockets.push((port, decode_bound_address(address), inode));
        }
    }

    if sockets.is_empty() {
        return Vec::new();
    }
    let processes = socket_inode_processes();
    let mut seen = HashMap::new();
    sockets
        .into_iter()
        .filter(|(port, _, _)| seen.insert(*port, ()).is_none())
        .map(|(port, bound_to, inode)| ListeningSocket {
            port,
            bound_to,
            process: processes.get(&inode).cloned(),
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn listening_sockets() -> Vec<ListeningSocket> {
    Vec::new()
}

/// procfs writes the bound address as little-endian hex. Only the common cases
/// are worth spelling out — anything else is shown as-is rather than guessed at.
#[cfg(target_os = "linux")]
fn decode_bound_address(hex: &str) -> String {
    match hex {
        "00000000" | "00000000000000000000000000000000" => "0.0.0.0".to_string(),
        "0100007F" => "127.0.0.1".to_string(),
        "00000000000000000000000001000000" => "::1".to_string(),
        _ if hex.len() == 8 => u32::from_str_radix(hex, 16)
            .map(|value| {
                let octets = value.to_le_bytes();
                format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
            })
            .unwrap_or_else(|_| hex.to_string()),
        _ => hex.to_string(),
    }
}

/// Map socket inode -> process name by walking `/proc/<pid>/fd`.
///
/// Only processes this user owns are readable, which is exactly the set that
/// matters: a dev server the user started. Anything unreadable is skipped
/// silently — a missing name costs a label, not the port.
#[cfg(target_os = "linux")]
fn socket_inode_processes() -> HashMap<u64, String> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let pid = entry.file_name();
        let pid = pid.to_string_lossy();
        if !pid.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut name = None;
        for descriptor in descriptors.flatten() {
            let Ok(target) = fs::read_link(descriptor.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_str()
                .and_then(|link| link.strip_prefix("socket:["))
                .and_then(|link| link.strip_suffix(']'))
                .and_then(|inode| inode.parse::<u64>().ok())
            else {
                continue;
            };
            let name = name.get_or_insert_with(|| {
                fs::read_to_string(entry.path().join("comm"))
                    .map(|comm| comm.trim().to_string())
                    .unwrap_or_else(|_| format!("pid {pid}"))
            });
            map.insert(inode, format!("{name} · pid {pid}"));
        }
    }
    map
}

#[cfg(test)]
#[path = "api_probe_tests.rs"]
mod tests;
