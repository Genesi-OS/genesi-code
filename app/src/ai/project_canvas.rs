//! Static, read-only project analysis for the Genesi Project Canvas.
//!
//! The analyzer deliberately never executes project code. It follows the same
//! source-first idea as Genesi Ports: walk a bounded set of text files, infer
//! framework routes from their conventions, then connect pages, links, API
//! calls, routers, and endpoints.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::{DirEntry, WalkDir};

const MAX_FILES: usize = 1_800;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_NODES: usize = 300;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Frontend,
    Backend,
    FullStack,
    Unknown,
}

impl ProjectKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Frontend => "Frontend",
            Self::Backend => "Backend",
            Self::FullStack => "Full stack",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasNodeKind {
    Page,
    Router,
    Endpoint,
}

impl CanvasNodeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Page => "Page",
            Self::Router => "Router",
            Self::Endpoint => "Endpoint",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasEdgeKind {
    PageLink,
    ApiCall,
    Defines,
    InternalCall,
}

impl CanvasEdgeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::PageLink => "links to",
            Self::ApiCall => "calls",
            Self::Defines => "defines",
            Self::InternalCall => "calls internally",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasNode {
    pub id: String,
    pub kind: CanvasNodeKind,
    pub title: String,
    pub route: Option<String>,
    pub method: Option<String>,
    pub source: PathBuf,
    pub line: usize,
    pub dependencies: Vec<String>,
    pub request_body: Option<String>,
    pub estimated_load_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasEdge {
    pub from: String,
    pub to: String,
    pub kind: CanvasEdgeKind,
    pub source: PathBuf,
    pub line: usize,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCanvasGraph {
    pub root: PathBuf,
    pub project_name: String,
    pub kind: ProjectKind,
    pub stacks: Vec<String>,
    pub nodes: Vec<CanvasNode>,
    pub edges: Vec<CanvasEdge>,
    pub files_scanned: usize,
}

impl ProjectCanvasGraph {
    pub fn page_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.kind == CanvasNodeKind::Page)
            .count()
    }

    pub fn endpoint_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.kind == CanvasNodeKind::Endpoint)
            .count()
    }

    pub fn node(&self, id: &str) -> Option<&CanvasNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

#[derive(Debug, Clone)]
struct SourceFile {
    relative: PathBuf,
    normalized: String,
    content: String,
    bytes: usize,
    dependencies: Vec<String>,
}

#[derive(Debug, Clone)]
struct DiscoveredEndpoint {
    method: String,
    route: String,
    line: usize,
    body: Option<String>,
}

#[derive(Debug, Clone)]
struct SourceReference {
    target: String,
    method: Option<String>,
    line: usize,
}

pub fn analyze_project(root: &Path) -> ProjectCanvasGraph {
    let project_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Project")
        .to_string();
    let files = collect_source_files(root);
    let stacks = detect_stacks(root, &files);
    let has_frontend_stack = stacks.iter().any(|stack| {
        matches!(
            stack.as_str(),
            "Next.js" | "React" | "Vue" | "Nuxt" | "SvelteKit" | "Angular" | "Vite"
        )
    });

    let mut nodes = Vec::new();
    let mut seen_nodes = HashSet::new();
    let mut endpoints_by_source: HashMap<String, Vec<String>> = HashMap::new();

    for file in &files {
        if nodes.len() >= MAX_NODES {
            break;
        }

        if let Some(route) = page_route_from_path(&file.normalized) {
            push_page_node(&mut nodes, &mut seen_nodes, file, route, 1);
        }

        if has_frontend_stack || is_explicit_frontend_file(&file.normalized, &file.content) {
            for (route, line) in declared_page_routes(&file.content) {
                push_page_node(&mut nodes, &mut seen_nodes, file, route, line);
            }
        }

        let mut endpoints = endpoint_route_from_path(&file.normalized, &file.content);
        endpoints.extend(declared_endpoints(&file.content));
        for endpoint in endpoints {
            let route = normalize_route(&endpoint.route);
            if !route.starts_with('/') || route.len() > 240 {
                continue;
            }
            let method = endpoint.method.to_ascii_uppercase();
            let id = format!(
                "endpoint:{}:{}:{}",
                method,
                route,
                file.normalized.to_ascii_lowercase()
            );
            if !seen_nodes.insert(id.clone()) {
                continue;
            }
            endpoints_by_source
                .entry(file.normalized.clone())
                .or_default()
                .push(id.clone());
            nodes.push(CanvasNode {
                id,
                kind: CanvasNodeKind::Endpoint,
                title: route.clone(),
                route: Some(route),
                method: Some(method),
                source: file.relative.clone(),
                line: endpoint.line,
                dependencies: file.dependencies.clone(),
                request_body: endpoint.body,
                estimated_load_ms: None,
            });
        }
    }

    let mut edges = Vec::new();
    let mut seen_edges = HashSet::new();

    // Backend routers/controllers are meaningful graph nodes: they show which
    // source unit owns each endpoint even when the backend has no internal HTTP
    // calls to connect one endpoint directly to another.
    for (source, endpoint_ids) in &endpoints_by_source {
        let Some(file) = files.iter().find(|file| &file.normalized == source) else {
            continue;
        };
        let router_id = format!("router:{}", source.to_ascii_lowercase());
        if seen_nodes.insert(router_id.clone()) {
            let title = file
                .relative
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("Router")
                .to_string();
            nodes.push(CanvasNode {
                id: router_id.clone(),
                kind: CanvasNodeKind::Router,
                title,
                route: None,
                method: None,
                source: file.relative.clone(),
                line: 1,
                dependencies: file.dependencies.clone(),
                request_body: None,
                estimated_load_ms: None,
            });
        }
        for endpoint_id in endpoint_ids {
            push_edge(
                &mut edges,
                &mut seen_edges,
                CanvasEdge {
                    from: router_id.clone(),
                    to: endpoint_id.clone(),
                    kind: CanvasEdgeKind::Defines,
                    source: file.relative.clone(),
                    line: 1,
                    label: "route handler".to_string(),
                },
            );
        }
    }

    let pages = nodes
        .iter()
        .filter(|node| node.kind == CanvasNodeKind::Page)
        .cloned()
        .collect::<Vec<_>>();
    let endpoints = nodes
        .iter()
        .filter(|node| node.kind == CanvasNodeKind::Endpoint)
        .cloned()
        .collect::<Vec<_>>();

    for page in &pages {
        let source_key = page.source.to_string_lossy().replace('\\', "/");
        let Some(file) = files.iter().find(|file| file.normalized == source_key) else {
            continue;
        };
        for link in page_links(&file.content) {
            if let Some(target) = find_page_target(&pages, &link.target) {
                push_edge(
                    &mut edges,
                    &mut seen_edges,
                    CanvasEdge {
                        from: page.id.clone(),
                        to: target.id.clone(),
                        kind: CanvasEdgeKind::PageLink,
                        source: file.relative.clone(),
                        line: link.line,
                        label: link.target,
                    },
                );
            }
        }
        for call in api_calls(&file.content) {
            if let Some(target) =
                find_endpoint_target(&endpoints, &call.target, call.method.as_deref())
            {
                push_edge(
                    &mut edges,
                    &mut seen_edges,
                    CanvasEdge {
                        from: page.id.clone(),
                        to: target.id.clone(),
                        kind: CanvasEdgeKind::ApiCall,
                        source: file.relative.clone(),
                        line: call.line,
                        label: format!(
                            "{} {}",
                            call.method.as_deref().unwrap_or("GET"),
                            normalize_route(&call.target)
                        ),
                    },
                );
            }
        }
    }

    for (source, endpoint_ids) in &endpoints_by_source {
        let Some(file) = files.iter().find(|file| &file.normalized == source) else {
            continue;
        };
        let router_id = format!("router:{}", source.to_ascii_lowercase());
        for call in api_calls(&file.content) {
            if let Some(target) =
                find_endpoint_target(&endpoints, &call.target, call.method.as_deref())
            {
                if endpoint_ids.contains(&target.id) {
                    continue;
                }
                push_edge(
                    &mut edges,
                    &mut seen_edges,
                    CanvasEdge {
                        from: router_id.clone(),
                        to: target.id.clone(),
                        kind: CanvasEdgeKind::InternalCall,
                        source: file.relative.clone(),
                        line: call.line,
                        label: format!(
                            "{} {}",
                            call.method.as_deref().unwrap_or("GET"),
                            normalize_route(&call.target)
                        ),
                    },
                );
            }
        }
    }

    let has_pages = nodes.iter().any(|node| node.kind == CanvasNodeKind::Page);
    let has_endpoints = nodes
        .iter()
        .any(|node| node.kind == CanvasNodeKind::Endpoint);
    let kind = match (has_pages, has_endpoints) {
        (true, true) => ProjectKind::FullStack,
        (true, false) => ProjectKind::Frontend,
        (false, true) => ProjectKind::Backend,
        (false, false) => ProjectKind::Unknown,
    };

    ProjectCanvasGraph {
        root: root.to_path_buf(),
        project_name,
        kind,
        stacks,
        nodes,
        edges,
        files_scanned: files.len(),
    }
}

fn collect_source_files(root: &Path) -> Vec<SourceFile> {
    let supported = [
        "js", "jsx", "ts", "tsx", "mjs", "cjs", "html", "htm", "vue", "svelte", "py", "java", "kt",
        "kts", "rb", "go", "rs", "php",
    ];
    WalkDir::new(root)
        .follow_links(false)
        .max_depth(10)
        .into_iter()
        .filter_entry(should_visit)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| supported.contains(&ext.to_ascii_lowercase().as_str()))
        })
        .filter(|entry| {
            entry
                .metadata()
                .map(|metadata| metadata.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
        })
        .take(MAX_FILES)
        .filter_map(|entry| {
            let absolute = entry.into_path();
            let content = fs::read_to_string(&absolute).ok()?;
            let relative = absolute.strip_prefix(root).ok()?.to_path_buf();
            let normalized = relative.to_string_lossy().replace('\\', "/");
            let dependencies = source_dependencies(&content);
            Some(SourceFile {
                relative,
                normalized,
                bytes: content.len(),
                content,
                dependencies,
            })
        })
        .collect()
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !SKIP_DIRS.contains(&name.as_ref())
}

fn detect_stacks(root: &Path, files: &[SourceFile]) -> Vec<String> {
    let mut stacks = Vec::new();
    let package = fs::read_to_string(root.join("package.json")).unwrap_or_default();
    let lower_package = package.to_ascii_lowercase();
    for (needle, stack) in [
        ("\"next\"", "Next.js"),
        ("\"react\"", "React"),
        ("\"nuxt\"", "Nuxt"),
        ("\"vue\"", "Vue"),
        ("\"@angular/core\"", "Angular"),
        ("\"@sveltejs/kit\"", "SvelteKit"),
        ("\"vite\"", "Vite"),
        ("\"express\"", "Express"),
        ("\"fastify\"", "Fastify"),
        ("\"hono\"", "Hono"),
    ] {
        if lower_package.contains(needle) {
            push_unique(&mut stacks, stack.to_string());
        }
    }

    let combined = files
        .iter()
        .take(200)
        .map(|file| file.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    for (needle, stack) in [
        ("from fastapi", "FastAPI"),
        ("from flask", "Flask"),
        ("from django", "Django"),
        ("@springbootapplication", "Spring"),
        ("rails.application.routes", "Rails"),
        ("github.com/gin-gonic/gin", "Gin"),
        ("axum::", "Axum"),
        ("actix_web", "Actix Web"),
        ("route::get", "Laravel"),
    ] {
        if combined.contains(needle) {
            push_unique(&mut stacks, stack.to_string());
        }
    }
    stacks
}

fn push_page_node(
    nodes: &mut Vec<CanvasNode>,
    seen_nodes: &mut HashSet<String>,
    file: &SourceFile,
    route: String,
    line: usize,
) {
    let route = normalize_route(&route);
    if !route.starts_with('/') || route.len() > 240 {
        return;
    }
    let id = format!("page:{}", route.to_ascii_lowercase());
    if !seen_nodes.insert(id.clone()) {
        return;
    }
    let estimated_load_ms = 35_u32
        .saturating_add((file.bytes as u32 / 1024).min(900))
        .saturating_add((file.dependencies.len() as u32).saturating_mul(15))
        .min(2_000);
    nodes.push(CanvasNode {
        id,
        kind: CanvasNodeKind::Page,
        title: route.clone(),
        route: Some(route),
        method: None,
        source: file.relative.clone(),
        line,
        dependencies: file.dependencies.clone(),
        request_body: None,
        estimated_load_ms: Some(estimated_load_ms),
    });
}

fn page_route_from_path(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        let without_ext = path.rsplit_once('.').map(|(base, _)| base).unwrap_or(path);
        let mut route = without_ext.to_string();
        if route.ends_with("/index") {
            route.truncate(route.len() - "/index".len());
        } else if route == "index" {
            route.clear();
        }
        return Some(format!("/{}", route.trim_matches('/')));
    }

    for (marker, file_prefix) in [
        ("src/app/", "page."),
        ("app/", "page."),
        ("src/routes/", "+page."),
        ("routes/", "+page."),
    ] {
        if let Some(rest) = lower.strip_prefix(marker) {
            let file = rest.rsplit('/').next().unwrap_or(rest);
            if file.starts_with(file_prefix) {
                let parent = rest.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
                return Some(url_from_segments(parent));
            }
        }
    }

    for marker in ["src/pages/", "pages/"] {
        if let Some(rest) = lower.strip_prefix(marker) {
            if rest.starts_with("api/") || rest.starts_with('_') {
                return None;
            }
            let without_ext = rest.rsplit_once('.').map(|(base, _)| base).unwrap_or(rest);
            let route = without_ext.strip_suffix("/index").unwrap_or(without_ext);
            return Some(url_from_segments(route));
        }
    }
    None
}

fn endpoint_route_from_path(path: &str, content: &str) -> Vec<DiscoveredEndpoint> {
    let lower = path.to_ascii_lowercase();
    for marker in ["src/app/", "app/"] {
        if let Some(rest) = lower.strip_prefix(marker) {
            let file = rest.rsplit('/').next().unwrap_or(rest);
            if file.starts_with("route.") {
                let parent = rest.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
                let route = url_from_segments(parent);
                let method_regex = Regex::new(
                    r"(?m)export\s+(?:(?:async\s+)?function|const)\s+(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b",
                )
                .expect("valid Next route regex");
                return method_regex
                    .captures_iter(content)
                    .filter_map(|captures| {
                        let method = captures.get(1)?.as_str().to_string();
                        let start = captures.get(0)?.start();
                        Some(DiscoveredEndpoint {
                            body: infer_request_body(content, start, &method, &route),
                            method,
                            route: route.clone(),
                            line: line_number(content, start),
                        })
                    })
                    .collect();
            }
        }
    }
    Vec::new()
}

fn declared_page_routes(content: &str) -> Vec<(String, usize)> {
    let mut routes = Vec::new();
    let patterns = [
        r#"(?i)<Route\b[^>]*\bpath\s*=\s*[\"']([^\"']+)[\"']"#,
        r#"(?i)\bpath\s*:\s*[\"']([^\"']+)[\"']"#,
    ];
    for pattern in patterns {
        let regex = Regex::new(pattern).expect("valid page route regex");
        for captures in regex.captures_iter(content) {
            let Some(route_match) = captures.get(1) else {
                continue;
            };
            let route = route_match.as_str();
            if route.starts_with('/') || !route.contains(':') {
                routes.push((
                    if route.starts_with('/') {
                        route.to_string()
                    } else {
                        format!("/{route}")
                    },
                    line_number(content, route_match.start()),
                ));
            }
        }
    }
    routes
}

fn declared_endpoints(content: &str) -> Vec<DiscoveredEndpoint> {
    let mut found = Vec::new();
    let patterns = [
        r#"(?i)\b(?:app|router|server|fastify)\s*\.\s*(get|post|put|patch|delete|head|options|all)\s*\(\s*[\"'`]([^\"'`]+)"#,
        r#"(?im)^\s*@(?:app|router|bp|blueprint)\s*\.\s*(get|post|put|patch|delete|head|options)\s*\(\s*r?[\"']([^\"']+)"#,
        r#"(?i)Route::(get|post|put|patch|delete|options)\s*\(\s*[\"']([^\"']+)"#,
        r#"(?im)^\s*(get|post|put|patch|delete)\s+[\"']([^\"']+)[\"']"#,
        r#"(?i)\b(?:router|r|e|app)\s*\.\s*(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s*\(\s*[\"']([^\"']+)"#,
        r#"(?im)^\s*#\[(get|post|put|patch|delete|head)\s*\(\s*[\"']([^\"']+)"#,
    ];
    for pattern in patterns {
        let regex = Regex::new(pattern).expect("valid endpoint regex");
        for captures in regex.captures_iter(content) {
            let (Some(method), Some(route), Some(whole)) =
                (captures.get(1), captures.get(2), captures.get(0))
            else {
                continue;
            };
            let method = method.as_str().to_ascii_uppercase();
            found.push(DiscoveredEndpoint {
                body: infer_request_body(content, whole.start(), &method, route.as_str()),
                method,
                route: route.as_str().to_string(),
                line: line_number(content, whole.start()),
            });
        }
    }

    let spring = Regex::new(
        r#"(?i)@(Get|Post|Put|Patch|Delete)Mapping\s*\(\s*(?:value\s*=\s*)?[\"']([^\"']+)"#,
    )
    .expect("valid Spring endpoint regex");
    for captures in spring.captures_iter(content) {
        let (Some(prefix), Some(route), Some(whole)) =
            (captures.get(1), captures.get(2), captures.get(0))
        else {
            continue;
        };
        let method = prefix.as_str().to_ascii_uppercase();
        found.push(DiscoveredEndpoint {
            body: infer_request_body(content, whole.start(), &method, route.as_str()),
            method,
            route: route.as_str().to_string(),
            line: line_number(content, whole.start()),
        });
    }

    let axum = Regex::new(
        r#"(?i)\.route\s*\(\s*[\"']([^\"']+)[\"']\s*,\s*(get|post|put|patch|delete)\s*\("#,
    )
    .expect("valid Axum endpoint regex");
    for captures in axum.captures_iter(content) {
        let (Some(route), Some(method), Some(whole)) =
            (captures.get(1), captures.get(2), captures.get(0))
        else {
            continue;
        };
        let method = method.as_str().to_ascii_uppercase();
        found.push(DiscoveredEndpoint {
            body: infer_request_body(content, whole.start(), &method, route.as_str()),
            method,
            route: route.as_str().to_string(),
            line: line_number(content, whole.start()),
        });
    }
    found
}

fn infer_request_body(content: &str, start: usize, method: &str, route: &str) -> Option<String> {
    if matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return None;
    }
    let end = (start + 4_000).min(content.len());
    let window = &content[start..end];
    let destructure = Regex::new(
        r"(?is)(?:const|let)\s*\{([^}]+)\}\s*=\s*(?:await\s+)?(?:(?:req|request)\.body|(?:req|request)\.json\s*\(\s*\))",
    )
    .expect("valid request destructuring regex");
    if let Some(fields) = destructure
        .captures(window)
        .and_then(|captures| captures.get(1))
    {
        let fields = fields
            .as_str()
            .split(',')
            .map(|field| field.split(':').next().unwrap_or(field).trim())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if !fields.is_empty() {
            return Some(format!("JSON fields: {}", fields.join(", ")));
        }
    }

    let body_field = Regex::new(r"(?i)(?:req|request)\.body\.([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid request body field regex");
    let mut fields = body_field
        .captures_iter(window)
        .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()))
        .collect::<Vec<_>>();
    fields.sort();
    fields.dedup();
    if !fields.is_empty() {
        return Some(format!("JSON fields: {}", fields.join(", ")));
    }

    let typed_body = Regex::new(
        r"(?i)(?:@RequestBody\s+([A-Za-z0-9_<>?, .]+)\s+\w+|Json\s*<\s*([A-Za-z0-9_:<>]+)\s*>)",
    )
    .expect("valid typed body regex");
    if let Some(captures) = typed_body.captures(window) {
        if let Some(body_type) = captures.get(1).or_else(|| captures.get(2)) {
            return Some(format!("Body type: {}", body_type.as_str().trim()));
        }
    }

    let python_fn = Regex::new(r"(?m)(?:async\s+)?def\s+\w+\s*\(([^)]*)\)")
        .expect("valid Python signature regex");
    if let Some(params) = python_fn
        .captures(window)
        .and_then(|captures| captures.get(1))
    {
        let path_params = route
            .split('/')
            .filter_map(|segment| {
                segment
                    .strip_prefix('{')
                    .and_then(|value| value.strip_suffix('}'))
                    .or_else(|| segment.strip_prefix(':'))
            })
            .collect::<HashSet<_>>();
        let params = params
            .as_str()
            .split(',')
            .map(str::trim)
            .filter(|param| !param.is_empty())
            .filter(|param| {
                let name = param.split([':', '=']).next().unwrap_or(param).trim();
                !matches!(name, "self" | "request" | "db" | "session")
                    && !path_params.contains(name)
            })
            .collect::<Vec<_>>();
        if !params.is_empty() {
            return Some(format!("Body parameters: {}", params.join(", ")));
        }
    }

    Some("Body schema is inferred at runtime; inspect the handler.".to_string())
}

fn source_dependencies(content: &str) -> Vec<String> {
    let patterns = [
        r#"(?m)\bfrom\s+[\"']([^\"']+)[\"']"#,
        r#"(?m)\brequire\s*\(\s*[\"']([^\"']+)[\"']\s*\)"#,
        r#"(?m)^\s*(?:from\s+([A-Za-z0-9_.]+)\s+import|import\s+([A-Za-z0-9_.]+))"#,
        r#"(?m)^\s*use\s+([A-Za-z0-9_:]+)"#,
    ];
    let mut dependencies = Vec::new();
    for pattern in patterns {
        let regex = Regex::new(pattern).expect("valid dependency regex");
        for captures in regex.captures_iter(content) {
            if let Some(value) = captures.get(1).or_else(|| captures.get(2)) {
                let value = value.as_str().trim().to_string();
                if !value.is_empty() {
                    push_unique(&mut dependencies, value);
                }
            }
            if dependencies.len() >= 12 {
                return dependencies;
            }
        }
    }
    dependencies
}

fn page_links(content: &str) -> Vec<SourceReference> {
    let patterns = [
        r#"(?i)\b(?:href|to)\s*=\s*[\"']([^\"']+)[\"']"#,
        r#"(?i)\b(?:navigate|push|replace)\s*\(\s*[\"']([^\"']+)[\"']"#,
    ];
    let mut links = Vec::new();
    for pattern in patterns {
        let regex = Regex::new(pattern).expect("valid page link regex");
        for captures in regex.captures_iter(content) {
            let (Some(target), Some(whole)) = (captures.get(1), captures.get(0)) else {
                continue;
            };
            let target = target.as_str();
            if target.starts_with('/') && !target.starts_with("//") {
                links.push(SourceReference {
                    target: target.to_string(),
                    method: None,
                    line: line_number(content, whole.start()),
                });
            }
        }
    }
    links
}

fn api_calls(content: &str) -> Vec<SourceReference> {
    let mut calls = Vec::new();
    let axios = Regex::new(
        r#"(?i)\b(?:axios|api|client|http)\s*\.\s*(get|post|put|patch|delete)\s*\(\s*[\"'`]([^\"'`]+)"#,
    )
    .expect("valid axios call regex");
    for captures in axios.captures_iter(content) {
        let (Some(method), Some(target), Some(whole)) =
            (captures.get(1), captures.get(2), captures.get(0))
        else {
            continue;
        };
        calls.push(SourceReference {
            target: target.as_str().to_string(),
            method: Some(method.as_str().to_ascii_uppercase()),
            line: line_number(content, whole.start()),
        });
    }

    let fetch =
        Regex::new(r#"(?i)\bfetch\s*\(\s*[\"'`]([^\"'`]+)"#).expect("valid fetch call regex");
    let method_regex = Regex::new(r#"(?i)\bmethod\s*:\s*[\"'](GET|POST|PUT|PATCH|DELETE)[\"']"#)
        .expect("valid fetch method regex");
    for captures in fetch.captures_iter(content) {
        let (Some(target), Some(whole)) = (captures.get(1), captures.get(0)) else {
            continue;
        };
        let tail_end = (whole.end() + 500).min(content.len());
        let method = method_regex
            .captures(&content[whole.end()..tail_end])
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().to_ascii_uppercase())
            .unwrap_or_else(|| "GET".to_string());
        calls.push(SourceReference {
            target: target.as_str().to_string(),
            method: Some(method),
            line: line_number(content, whole.start()),
        });
    }
    calls
}

fn find_page_target<'a>(pages: &'a [CanvasNode], target: &str) -> Option<&'a CanvasNode> {
    let target = normalize_route(target);
    pages.iter().find(|page| {
        page.route
            .as_deref()
            .is_some_and(|route| route_pattern_matches(route, &target))
    })
}

fn find_endpoint_target<'a>(
    endpoints: &'a [CanvasNode],
    target: &str,
    method: Option<&str>,
) -> Option<&'a CanvasNode> {
    let target = normalize_route(target);
    endpoints.iter().find(|endpoint| {
        let method_matches = method.is_none_or(|method| {
            endpoint
                .method
                .as_deref()
                .is_some_and(|candidate| candidate == method || candidate == "ALL")
        });
        method_matches
            && endpoint
                .route
                .as_deref()
                .is_some_and(|route| route_pattern_matches(route, &target))
    })
}

fn route_pattern_matches(pattern: &str, value: &str) -> bool {
    let pattern = normalize_route(pattern);
    let value = normalize_route(value);
    let left = pattern.trim_matches('/').split('/').collect::<Vec<_>>();
    let right = value.trim_matches('/').split('/').collect::<Vec<_>>();
    left.len() == right.len()
        && left.iter().zip(right).all(|(expected, actual)| {
            expected == &actual
                || expected.starts_with(':')
                || expected.starts_with('{')
                || expected.starts_with('[')
                || expected.starts_with('*')
        })
}

fn normalize_route(route: &str) -> String {
    let mut route = route.trim().to_string();
    if let Some(scheme) = route.find("://") {
        let after_host = &route[scheme + 3..];
        route = after_host
            .find('/')
            .map(|slash| after_host[slash..].to_string())
            .unwrap_or_else(|| "/".to_string());
    }
    if let Some(end) = route.find(['?', '#']) {
        route.truncate(end);
    }
    route = route.replace("${", ":").replace('}', "");
    if let Some((path, _)) = route.split_once("  (page)") {
        route = path.to_string();
    }
    if !route.starts_with('/') {
        route.insert(0, '/');
    }
    while route.len() > 1 && route.ends_with('/') {
        route.pop();
    }
    route
}

fn url_from_segments(path: &str) -> String {
    let segments = path
        .replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .filter(|segment| !(segment.starts_with('(') && segment.ends_with(')')))
        .map(|segment| {
            if segment.starts_with("[...") && segment.ends_with(']') {
                format!("*{}", &segment[4..segment.len() - 1])
            } else if segment.starts_with('[') && segment.ends_with(']') {
                format!(":{}", &segment[1..segment.len() - 1])
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

fn is_explicit_frontend_file(path: &str, content: &str) -> bool {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    matches!(extension, "jsx" | "tsx" | "vue" | "svelte" | "html" | "htm")
        || content.contains("react-router")
        || content.contains("vue-router")
        || content.contains("createBrowserRouter")
}

fn line_number(content: &str, byte_offset: usize) -> usize {
    content.as_bytes()[..byte_offset.min(content.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn push_edge(
    edges: &mut Vec<CanvasEdge>,
    seen: &mut HashSet<(String, String, CanvasEdgeKind, usize)>,
    edge: CanvasEdge,
) {
    let key = (edge.from.clone(), edge.to.clone(), edge.kind, edge.line);
    if seen.insert(key) {
        edges.push(edge);
    }
}

#[cfg(test)]
#[path = "project_canvas_tests.rs"]
mod tests;
