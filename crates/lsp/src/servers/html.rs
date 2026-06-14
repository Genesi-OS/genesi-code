use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "local_fs")]
use anyhow::Context;
use async_trait::async_trait;

use crate::language_server_candidate::{LanguageServerCandidate, LanguageServerMetadata};
#[cfg(feature = "local_fs")]
use crate::supported_servers::CustomBinaryConfig;
use crate::CommandBuilder;

/// Candidate for `vscode-html-language-server`, shipped as part of the npm
/// package `vscode-langservers-extracted` (which also bundles the CSS and JSON
/// servers). We manage a private install of that package under the data dir and
/// run it via `node`, mirroring how Pyright / typescript-language-server work.
#[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
pub struct HtmlLanguageServerCandidate {
    client: Arc<http_client::Client>,
}

impl HtmlLanguageServerCandidate {
    /// Path to the HTML server launcher JS within the npm package, relative to
    /// the install directory.
    #[cfg(feature = "local_fs")]
    const SERVER_JS_PATH: &str =
        "node_modules/vscode-langservers-extracted/bin/vscode-html-language-server";

    pub fn new(client: Arc<http_client::Client>) -> Self {
        Self { client }
    }

    /// Finds the configuration for running vscode-html-language-server from our
    /// managed installation.
    ///
    /// Instead of running the bin wrapper (which relies on a shebang requiring
    /// node in PATH), we run node directly with the launcher JS file. This is the
    /// same pattern used for Pyright and typescript-language-server.
    ///
    /// # Arguments
    /// * `path_env_var` - The PATH environment variable to use when checking for system node.
    #[cfg(feature = "local_fs")]
    pub async fn find_installed_binary_config(
        path_env_var: Option<&str>,
    ) -> Option<CustomBinaryConfig> {
        let install_dir = warp_core::paths::data_dir().join("vscode-langservers-extracted");
        let server_js = install_dir.join(Self::SERVER_JS_PATH);

        if !server_js.is_file() {
            log::info!(
                "vscode-html-language-server launcher not found at {}",
                server_js.display()
            );
            return None;
        }

        // Try to find a working node binary - first custom, then system.
        let node_binary = node_runtime::find_working_node_binary(path_env_var).await?;

        Some(CustomBinaryConfig {
            binary_path: node_binary,
            prepend_args: vec![server_js.to_string_lossy().to_string()],
        })
    }
}

#[async_trait]
#[cfg(feature = "local_fs")]
impl LanguageServerCandidate for HtmlLanguageServerCandidate {
    async fn should_suggest_for_repo(&self, _path: &Path, _executor: &CommandBuilder) -> bool {
        // HTML has no manifest file to key off of; we don't proactively nudge.
        // The "Install" affordance on an open .html file still drives install().
        false
    }

    async fn is_installed_in_data_dir(&self, executor: &CommandBuilder) -> bool {
        Self::find_installed_binary_config(executor.path_env_var())
            .await
            .is_some()
    }

    async fn is_installed_on_path(&self, executor: &CommandBuilder) -> bool {
        executor
            .command("vscode-html-language-server")
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    async fn install(
        &self,
        metadata: LanguageServerMetadata,
        executor: &CommandBuilder,
    ) -> anyhow::Result<()> {
        log::info!(
            "Installing vscode-langservers-extracted version {}",
            metadata.version
        );

        let install_dir = warp_core::paths::data_dir().join("vscode-langservers-extracted");

        // Create the installation directory
        async_fs::create_dir_all(&install_dir)
            .await
            .context("Failed to create vscode-langservers-extracted installation directory")?;

        // First, check if system node is available and meets requirements
        let use_system_node = match executor.path_env_var() {
            Some(path) => node_runtime::detect_system_node(path).await.is_ok(),
            None => false,
        };

        let custom_node_paths = if use_system_node {
            log::info!("Using system Node.js for vscode-langservers-extracted installation");
            None
        } else {
            log::info!("System Node.js not found or too old, installing custom Node.js");
            node_runtime::install_npm(&self.client).await?;
            Some((
                node_runtime::node_binary_path()?,
                node_runtime::npm_binary_path()?,
            ))
        };

        log::info!(
            "Installing vscode-langservers-extracted@{} using npm",
            metadata.version
        );

        // Build the npm install command:
        // - System node: run `npm` directly (it's on PATH)
        // - Custom node: run `node <npm_path>` to avoid relying on shebang resolution
        let mut cmd = if let Some((node_path, npm_path)) = &custom_node_paths {
            let mut c = executor.command(node_path);
            c.arg(npm_path);
            c
        } else {
            executor.command("npm")
        };

        cmd.arg("install")
            .arg("--ignore-scripts")
            .arg(format!("vscode-langservers-extracted@{}", metadata.version))
            .current_dir(&install_dir);

        let output = cmd.output().await.context("Failed to run npm install")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to install vscode-langservers-extracted via npm: {}",
                stderr
            );
        }

        log::info!("vscode-html-language-server installed successfully");
        Ok(())
    }

    async fn fetch_latest_server_metadata(&self) -> anyhow::Result<LanguageServerMetadata> {
        let version =
            node_runtime::fetch_npm_package_version(&self.client, "vscode-langservers-extracted")
                .await
                .context(
                    "Failed to fetch vscode-langservers-extracted version from npm registry",
                )?;

        Ok(LanguageServerMetadata {
            version,
            url: None, // npm packages don't have direct download URLs
            digest: None,
        })
    }
}

#[async_trait]
#[cfg(not(feature = "local_fs"))]
impl LanguageServerCandidate for HtmlLanguageServerCandidate {
    async fn should_suggest_for_repo(&self, _path: &Path, _executor: &CommandBuilder) -> bool {
        false
    }

    async fn is_installed_in_data_dir(&self, _executor: &CommandBuilder) -> bool {
        false
    }

    async fn is_installed_on_path(&self, _executor: &CommandBuilder) -> bool {
        false
    }

    async fn install(
        &self,
        _metadata: LanguageServerMetadata,
        _executor: &CommandBuilder,
    ) -> anyhow::Result<()> {
        todo!()
    }

    async fn fetch_latest_server_metadata(&self) -> anyhow::Result<LanguageServerMetadata> {
        todo!()
    }
}
