use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "local_fs")]
use anyhow::Context;
use async_trait::async_trait;

use crate::language_server_candidate::{LanguageServerCandidate, LanguageServerMetadata};
#[cfg(feature = "local_fs")]
use crate::supported_servers::CustomBinaryConfig;
use crate::CommandBuilder;

/// Candidate for `vscode-json-language-server`, shipped in the same npm package
/// as the HTML and CSS servers (`vscode-langservers-extracted`). We manage a
/// private install of that package under the data dir and run it via `node`,
/// mirroring the HTML server / typescript-language-server.
#[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
pub struct JsonLanguageServerCandidate {
    client: Arc<http_client::Client>,
}

impl JsonLanguageServerCandidate {
    /// Path to the JSON server launcher JS within the npm package, relative to
    /// the install directory.
    #[cfg(feature = "local_fs")]
    const SERVER_JS_PATH: &str =
        "node_modules/vscode-langservers-extracted/bin/vscode-json-language-server";

    pub fn new(client: Arc<http_client::Client>) -> Self {
        Self { client }
    }

    /// Finds the configuration for running vscode-json-language-server from our
    /// managed installation. Runs node directly with the launcher JS rather than
    /// the bin wrapper (which relies on a node-in-PATH shebang).
    #[cfg(feature = "local_fs")]
    pub async fn find_installed_binary_config(
        path_env_var: Option<&str>,
    ) -> Option<CustomBinaryConfig> {
        let install_dir = warp_core::paths::data_dir().join("vscode-langservers-extracted");
        let server_js = install_dir.join(Self::SERVER_JS_PATH);

        if !server_js.is_file() {
            log::info!(
                "vscode-json-language-server launcher not found at {}",
                server_js.display()
            );
            return None;
        }

        let node_binary = node_runtime::find_working_node_binary(path_env_var).await?;

        Some(CustomBinaryConfig {
            binary_path: node_binary,
            prepend_args: vec![server_js.to_string_lossy().to_string()],
        })
    }
}

#[async_trait]
#[cfg(feature = "local_fs")]
impl LanguageServerCandidate for JsonLanguageServerCandidate {
    async fn should_suggest_for_repo(&self, _path: &Path, _executor: &CommandBuilder) -> bool {
        false
    }

    async fn is_installed_in_data_dir(&self, executor: &CommandBuilder) -> bool {
        Self::find_installed_binary_config(executor.path_env_var())
            .await
            .is_some()
    }

    async fn is_installed_on_path(&self, _executor: &CommandBuilder) -> bool {
        // The vscode-langservers-extracted bins have no reliable `--version` and
        // may block on stdin, so probing PATH risks hanging server startup. Rely
        // solely on the managed data-dir install instead.
        false
    }

    async fn install(
        &self,
        metadata: LanguageServerMetadata,
        executor: &CommandBuilder,
    ) -> anyhow::Result<()> {
        log::info!(
            "Installing vscode-langservers-extracted (JSON) version {}",
            metadata.version
        );

        let install_dir = warp_core::paths::data_dir().join("vscode-langservers-extracted");

        async_fs::create_dir_all(&install_dir)
            .await
            .context("Failed to create vscode-langservers-extracted installation directory")?;

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

        log::info!("vscode-json-language-server installed successfully");
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
            url: None,
            digest: None,
        })
    }
}

#[async_trait]
#[cfg(not(feature = "local_fs"))]
impl LanguageServerCandidate for JsonLanguageServerCandidate {
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
