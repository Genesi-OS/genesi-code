use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
#[cfg(not(target_arch = "wasm32"))]
use command::r#async::Command;
use lsp_types::{
    ClientCapabilities, ClientInfo, CompletionClientCapabilities, CompletionItemCapability,
    CompletionItemCapabilityResolveSupport, DidChangeWatchedFilesClientCapabilities,
    GotoCapability, HoverClientCapabilities, InitializeParams, MarkupKind,
    PublishDiagnosticsClientCapabilities, TextDocumentClientCapabilities,
    TextDocumentSyncClientCapabilities, Uri, WindowClientCapabilities, WorkDoneProgressParams,
    WorkspaceClientCapabilities, WorkspaceFolder,
};

use crate::supported_servers::LSPServerType;

/// Result of resolving an LSP server command, including the command and init params.
#[cfg(not(target_arch = "wasm32"))]
pub struct ResolvedLspCommand {
    pub command: Command,
    pub params: InitializeParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageId {
    Rust,
    Go,
    Python,
    TypeScript,
    TypeScriptReact,
    JavaScript,
    JavaScriptReact,
    C,
    Cpp,
    Html,
    Css,
    Json,
}

/// Declarative description of a language Genesi Code understands.
///
/// `LANGUAGES` below is the single source of truth for language support: file
/// extensions, the `languageId` sent to the server, which LSP server backs the
/// language, and the characters that should auto-open the completion popup.
/// Adding a new language is ideally just a new [`LanguageSpec`] entry there (plus
/// an LSP server candidate in `supported_servers` when the server needs custom
/// install/detection logic).
#[derive(Debug, Clone, Copy)]
pub struct LanguageSpec {
    pub id: LanguageId,
    /// Lowercased file extensions (without the leading dot) that map here.
    pub extensions: &'static [&'static str],
    /// The `languageId` string sent to the server in `textDocument/didOpen`.
    pub lsp_id: &'static str,
    /// The LSP server that handles this language.
    pub server: LSPServerType,
    /// Characters that, when typed, auto-open the completion popup (member
    /// access `.`, tag open `<`, ...). Typing identifier characters opens the
    /// popup too, independently of this list.
    pub trigger_chars: &'static [char],
}

/// The language registry. First extension match wins.
///
/// NOTE on `.h`: ambiguous between C and C++. We map it to C++ because clangd
/// defaults to C++ for headers anyway; with a `compile_commands.json` present
/// clangd uses the correct language regardless of the `languageId` we send.
pub const LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        id: LanguageId::Rust,
        extensions: &["rs"],
        lsp_id: "rust",
        server: LSPServerType::RustAnalyzer,
        trigger_chars: &['.', ':'],
    },
    LanguageSpec {
        id: LanguageId::Go,
        extensions: &["go"],
        lsp_id: "go",
        server: LSPServerType::GoPls,
        trigger_chars: &['.'],
    },
    LanguageSpec {
        id: LanguageId::Python,
        extensions: &["py", "pyi", "pyw"],
        lsp_id: "python",
        server: LSPServerType::Pyright,
        trigger_chars: &['.'],
    },
    LanguageSpec {
        id: LanguageId::TypeScript,
        extensions: &["ts", "mts", "cts"],
        lsp_id: "typescript",
        server: LSPServerType::TypeScriptLanguageServer,
        trigger_chars: &['.'],
    },
    LanguageSpec {
        id: LanguageId::TypeScriptReact,
        extensions: &["tsx"],
        lsp_id: "typescriptreact",
        server: LSPServerType::TypeScriptLanguageServer,
        trigger_chars: &['.', '<'],
    },
    LanguageSpec {
        id: LanguageId::JavaScript,
        extensions: &["js", "mjs", "cjs", "jsm", "es6"],
        lsp_id: "javascript",
        server: LSPServerType::TypeScriptLanguageServer,
        trigger_chars: &['.'],
    },
    LanguageSpec {
        id: LanguageId::JavaScriptReact,
        extensions: &["jsx"],
        lsp_id: "javascriptreact",
        server: LSPServerType::TypeScriptLanguageServer,
        trigger_chars: &['.', '<'],
    },
    LanguageSpec {
        id: LanguageId::C,
        extensions: &["c"],
        lsp_id: "c",
        server: LSPServerType::Clangd,
        trigger_chars: &['.', '>', ':'],
    },
    LanguageSpec {
        id: LanguageId::Cpp,
        extensions: &["cc", "cpp", "cxx", "h", "hh", "hpp", "hxx"],
        lsp_id: "cpp",
        server: LSPServerType::Clangd,
        trigger_chars: &['.', '>', ':'],
    },
    LanguageSpec {
        id: LanguageId::Html,
        extensions: &["html", "htm", "xhtml"],
        lsp_id: "html",
        server: LSPServerType::VscodeHtmlLanguageServer,
        trigger_chars: &['<', '/', '"', '=', ':', '.', '&'],
    },
    LanguageSpec {
        id: LanguageId::Css,
        extensions: &["css", "scss", "sass", "less"],
        lsp_id: "css",
        server: LSPServerType::VscodeCssLanguageServer,
        trigger_chars: &[':', '-', '/', '@', '.', '#', '!'],
    },
    LanguageSpec {
        id: LanguageId::Json,
        extensions: &["json", "jsonc", "webmanifest", "code-workspace"],
        lsp_id: "json",
        server: LSPServerType::VscodeJsonLanguageServer,
        trigger_chars: &['"', ':', '/'],
    },
];

impl LanguageId {
    /// The registry entry for this language. Every `LanguageId` has exactly one.
    fn spec(&self) -> &'static LanguageSpec {
        LANGUAGES
            .iter()
            .find(|spec| spec.id == *self)
            .expect("every LanguageId must have a LanguageSpec entry in LANGUAGES")
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        let extn = path.extension()?.to_str()?.to_ascii_lowercase();
        LANGUAGES
            .iter()
            .find(|spec| spec.extensions.contains(&extn.as_str()))
            .map(|spec| spec.id)
    }

    /// Returns the language identifier as used by LSP.
    /// See: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocumentItem
    pub(crate) fn lsp_language_identifier(&self) -> &'static str {
        self.spec().lsp_id
    }

    /// The LSP server type that handles this language.
    pub fn server_type(&self) -> LSPServerType {
        self.spec().server
    }

    /// Characters that should auto-open the completion popup for this language.
    pub fn trigger_chars(&self) -> &'static [char] {
        self.spec().trigger_chars
    }
}

/// Configuration for spawning an LSP server process.
#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct LspServerConfig {
    server_type: LSPServerType,
    initial_workspace: PathBuf,
    /// The local PATH variable set when starting the server. This is needed when the app is started
    /// without a shell based parent process.
    /// TODO(kevin): This might not be sufficient for all cases (e.g. user might remove LSP from PATH).
    path_env_var: Option<String>,
    client_name: String,
    /// Shared HTTP client used for LSP installation checks and downloads.
    client: Arc<http_client::Client>,
    /// Optional path relative to the LSP log namespace for server stderr output.
    log_relative_path: Option<PathBuf>,
}

impl fmt::Debug for LspServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LspServerConfig")
            .field("server_type", &self.server_type)
            .field("initial_workspace", &self.initial_workspace)
            .field("path_env_var", &self.path_env_var)
            .field("client_name", &self.client_name)
            .field("log_relative_path", &self.log_relative_path)
            .finish()
    }
}

impl LspServerConfig {
    pub fn new(
        server_type: LSPServerType,
        initial_workspace: PathBuf,
        path_env_var: Option<String>,
        client_name: String,
        client: Arc<http_client::Client>,
    ) -> Self {
        Self {
            server_type,
            initial_workspace,
            path_env_var,
            client_name,
            client,
            log_relative_path: None,
        }
    }

    /// Sets the relative log path for this server's stderr output.
    pub fn with_log_relative_path(mut self, log_relative_path: PathBuf) -> Self {
        self.log_relative_path = Some(log_relative_path);
        self
    }
    /// Returns the relative log path if configured.
    pub fn log_relative_path(&self) -> Option<&PathBuf> {
        self.log_relative_path.as_ref()
    }

    /// Returns the initial workspace path.
    pub fn initial_workspace(&self) -> &Path {
        &self.initial_workspace
    }

    pub(crate) fn server_name(&self) -> String {
        self.server_type.binary_name().to_string()
    }

    pub(crate) fn languages(&self) -> Vec<LanguageId> {
        self.server_type.languages()
    }

    /// Creates the command and init params for the LSP server.
    ///
    /// PATH takes precedence over custom installations. If the binary is available
    /// and working on PATH, we use that. Otherwise, we fall back to our custom installation.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) async fn command_and_params(self) -> Result<ResolvedLspCommand> {
        // PATH takes precedence - only use custom installation if not working on PATH
        let executor = crate::CommandBuilder::new(self.path_env_var.clone());
        let is_working_on_path = self
            .server_type
            .is_working_on_path(&executor, self.client.clone())
            .await;
        let custom_binary_config = if is_working_on_path {
            // Binary works on PATH, don't use custom installation
            None
        } else {
            // Not working on PATH, check for custom installation
            self.server_type
                .find_installed_binary_config(executor.path_env_var())
                .await
        };

        // Bail early with a clear error instead of attempting to spawn a
        // binary that doesn't exist (which would fail with a confusing
        // "No such file or directory" OS error).
        if !is_working_on_path && custom_binary_config.is_none() {
            anyhow::bail!(
                "{} is not installed. Binary was not found on PATH and no custom installation exists",
                self.server_type.binary_name()
            );
        }

        let mut command = self
            .server_type
            .create_command(custom_binary_config.clone(), &executor);

        // Set the working directory to the workspace root. This is required for
        // LSP servers like rust-analyzer to properly discover the project structure.
        command.current_dir(&self.initial_workspace);

        log::info!(
            "LSP {} starting with custom_binary_config: {:?}",
            self.server_type.binary_name(),
            custom_binary_config
        );

        let params = default_init_params(&self.initial_workspace, self.client_name)?;

        Ok(ResolvedLspCommand { command, params })
    }

    pub(crate) fn server_type(&self) -> LSPServerType {
        self.server_type
    }
}

pub(crate) fn path_to_lsp_uri(path: &Path) -> Result<Uri> {
    if !path.is_absolute() {
        return Err(anyhow::anyhow!("Path must be absolute: {}", path.display()));
    }

    // url::Url::from_file_path handles percent-encoding internally but is not
    // available on WASM. LSP is not supported on WASM either, so the fallback
    // is a simple string concatenation.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let url = url::Url::from_file_path(path).map_err(|()| {
            anyhow::anyhow!("Failed to convert path to file URI: {}", path.display())
        })?;

        // The url crate doesn't encode brackets, but LSP requires them to be
        // percent-encoded (e.g. Next.js [slug].tsx routes).
        let uri_str = url.as_str().replace('[', "%5B").replace(']', "%5D");

        uri_str.parse::<Uri>().map_err(anyhow::Error::from)
    }

    #[cfg(target_arch = "wasm32")]
    {
        let path_str = path.to_string_lossy();
        let uri_string = format!("file://{path_str}");
        uri_string.parse::<Uri>().map_err(anyhow::Error::from)
    }
}

pub(crate) fn lsp_uri_to_path(uri: &Uri) -> Result<PathBuf> {
    // Validate this is a file URI
    let scheme = uri.scheme().map(|s| s.as_str());
    if scheme != Some("file") {
        return Err(anyhow::anyhow!("Invalid file URI: {}", uri.as_str()));
    }

    // Decode percent-encoded characters (e.g., %40 -> @)
    // This is necessary because LSP servers return URL-encoded paths
    let decoded_path = uri
        .path()
        .as_estr()
        .decode()
        .into_string()
        .map_err(|e| anyhow::anyhow!("Invalid UTF-8 in URI path: {e}"))?;

    let mut path_str: &str = decoded_path.as_ref();

    // Windows URIs are formatted like: file:///C:/path/to/file
    // The path component is `/C:/path/to/file`, strip the leading slash.
    if cfg!(windows) {
        path_str = path_str.strip_prefix('/').unwrap_or(path_str);
        return Ok(PathBuf::from(path_str.replace('/', "\\")));
    }

    Ok(PathBuf::from(path_str))
}

fn path_to_workspace_folder(path: &Path) -> Result<WorkspaceFolder> {
    path_to_lsp_uri(path).map(|url| WorkspaceFolder {
        uri: url,
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
    })
}

fn default_client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            did_change_watched_files: Option::from(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(true),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                did_save: Some(true),
            }),
            definition: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                // Request Markdown content from the LSP for hover responses.
                // This enables proper syntax highlighting in hover tooltips.
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            // Genesi: advertise textDocument/completion so language servers offer
            // classic (non-AI) autocomplete. We also enable snippet / insert-
            // replace support so servers can return richer edits like HTML tag
            // expansions with cursor placeholders.
            completion: Some(CompletionClientCapabilities {
                dynamic_registration: Some(false),
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(true),
                    insert_replace_support: Some(true),
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    // Servers withhold the expensive fields unless the client
                    // says it will ask for them later, so both of these have to
                    // be on for the popup to ever show documentation.
                    label_details_support: Some(true),
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec![
                            "documentation".to_string(),
                            "detail".to_string(),
                            "labelDetails".to_string(),
                            "additionalTextEdits".to_string(),
                        ],
                    }),
                    ..Default::default()
                }),
                context_support: Some(true),
                ..Default::default()
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(true),
                related_information: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn default_init_params(workspace_uri: &Path, client_name: String) -> Result<InitializeParams> {
    let workspace_folder = path_to_workspace_folder(workspace_uri)?;

    Ok(InitializeParams {
        process_id: Some(std::process::id()),
        capabilities: default_client_capabilities(),
        workspace_folders: Some(vec![workspace_folder]),
        client_info: Some(ClientInfo {
            name: client_name,
            version: option_env!("GIT_RELEASE_TAG").map(|s| s.to_string()),
        }),
        locale: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        ..Default::default()
    })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
