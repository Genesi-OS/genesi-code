use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "local_fs")]
use anyhow::Context;
use async_trait::async_trait;

use crate::language_server_candidate::{LanguageServerCandidate, LanguageServerMetadata};
#[cfg(feature = "local_fs")]
use crate::supported_servers::CustomBinaryConfig;
use crate::CommandBuilder;

/// Everything that distinguishes one npm-published language server from another.
///
/// Four servers already shipped as near-identical copies of the same ~150 lines
/// of npm plumbing, which is a poor trade for the two facts that actually
/// differ: what to install and which file to hand to node. A new npm-backed
/// language is now a `NpmServerSpec` const and a match arm.
#[derive(Debug, Clone, Copy)]
pub struct NpmServerSpec {
    /// The npm package to install, and the directory it gets its own install in.
    pub package: &'static str,
    /// The launcher to run under node, relative to the install directory.
    ///
    /// Taken from the package's own `bin` entry. Running that file directly
    /// rather than the wrapper matters: the wrapper's shebang wants a `node` on
    /// PATH, which is exactly what a managed install exists to avoid needing.
    pub launcher: &'static str,
    /// Human name for logs.
    pub display_name: &'static str,
}

/// `bash-language-server`. Shell scripts are where a missing language server is
/// most obvious, because the shell will happily run a typo.
pub const BASH: NpmServerSpec = NpmServerSpec {
    package: "bash-language-server",
    launcher: "node_modules/bash-language-server/out/cli.js",
    display_name: "bash-language-server",
};

/// `yaml-language-server`. Also the one that knows the schemas for CI configs,
/// compose files and k8s manifests.
pub const YAML: NpmServerSpec = NpmServerSpec {
    package: "yaml-language-server",
    launcher: "node_modules/yaml-language-server/bin/yaml-language-server",
    display_name: "yaml-language-server",
};

/// `intelephense`, the PHP server. The free tier covers completion, hover and
/// go-to-definition, which is the whole of what this editor asks of it.
pub const PHP: NpmServerSpec = NpmServerSpec {
    package: "intelephense",
    launcher: "node_modules/intelephense/lib/intelephense.js",
    display_name: "intelephense",
};

/// A language server distributed as an npm package, installed privately under
/// the data dir and run through node.
#[cfg_attr(not(feature = "local_fs"), allow(dead_code))]
pub struct NpmLanguageServerCandidate {
    client: Arc<http_client::Client>,
    spec: NpmServerSpec,
}

impl NpmLanguageServerCandidate {
    pub fn new(spec: NpmServerSpec, client: Arc<http_client::Client>) -> Self {
        Self { client, spec }
    }

    /// Where this server's private npm install lives. One directory per package
    /// so upgrading one server cannot disturb another's dependency tree.
    #[cfg(feature = "local_fs")]
    fn install_dir(spec: &NpmServerSpec) -> std::path::PathBuf {
        warp_core::paths::data_dir().join(spec.package)
    }

    /// The config for running this server from the managed install, or `None`
    /// when it is not installed (or node cannot be found to run it).
    #[cfg(feature = "local_fs")]
    pub async fn find_installed_binary_config(
        spec: &NpmServerSpec,
        path_env_var: Option<&str>,
    ) -> Option<CustomBinaryConfig> {
        let launcher = Self::install_dir(spec).join(spec.launcher);
        if !launcher.is_file() {
            log::info!(
                "{} launcher not found at {}",
                spec.display_name,
                launcher.display()
            );
            return None;
        }

        let node_binary = node_runtime::find_working_node_binary(path_env_var).await?;

        Some(CustomBinaryConfig {
            binary_path: node_binary,
            prepend_args: vec![launcher.to_string_lossy().to_string()],
        })
    }
}

#[async_trait]
#[cfg(feature = "local_fs")]
impl LanguageServerCandidate for NpmLanguageServerCandidate {
    async fn should_suggest_for_repo(&self, _path: &Path, _executor: &CommandBuilder) -> bool {
        false
    }

    async fn is_installed_in_data_dir(&self, executor: &CommandBuilder) -> bool {
        Self::find_installed_binary_config(&self.spec, executor.path_env_var())
            .await
            .is_some()
    }

    async fn is_installed_on_path(&self, _executor: &CommandBuilder) -> bool {
        // These bins have no reliable `--version` and several block on stdin, so
        // probing PATH risks hanging server startup. The managed install is the
        // only source we trust — the same call this makes for the vscode servers.
        false
    }

    async fn install(
        &self,
        metadata: LanguageServerMetadata,
        executor: &CommandBuilder,
    ) -> anyhow::Result<()> {
        let spec = &self.spec;
        log::info!(
            "Installing {} version {}",
            spec.display_name,
            metadata.version
        );

        let install_dir = Self::install_dir(spec);
        async_fs::create_dir_all(&install_dir)
            .await
            .with_context(|| format!("Failed to create {} install directory", spec.display_name))?;

        let use_system_node = match executor.path_env_var() {
            Some(path) => node_runtime::detect_system_node(path).await.is_ok(),
            None => false,
        };

        let custom_node_paths = if use_system_node {
            log::info!("Using system Node.js to install {}", spec.display_name);
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
            .arg(format!("{}@{}", spec.package, metadata.version))
            .current_dir(&install_dir);

        let output = cmd
            .output()
            .await
            .with_context(|| format!("Failed to run npm install for {}", spec.package))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to install {} via npm: {}", spec.package, stderr);
        }

        log::info!("{} installed successfully", spec.display_name);
        Ok(())
    }

    async fn fetch_latest_server_metadata(&self) -> anyhow::Result<LanguageServerMetadata> {
        let version = node_runtime::fetch_npm_package_version(&self.client, self.spec.package)
            .await
            .with_context(|| {
                format!(
                    "Failed to fetch {} version from npm registry",
                    self.spec.package
                )
            })?;

        Ok(LanguageServerMetadata {
            version,
            url: None,
            digest: None,
        })
    }
}

#[async_trait]
#[cfg(not(feature = "local_fs"))]
impl LanguageServerCandidate for NpmLanguageServerCandidate {
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
