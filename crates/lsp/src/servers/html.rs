use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::language_server_candidate::{LanguageServerCandidate, LanguageServerMetadata};
use crate::CommandBuilder;

/// Candidate for `vscode-html-language-server`, the HTML language server shipped
/// as part of the npm package `vscode-langservers-extracted` (which also bundles
/// the CSS and JSON servers). For now we only detect it on `PATH`; there is no
/// managed/custom install, so it "just works" when the user has the package
/// installed globally (`npm i -g vscode-langservers-extracted`).
pub struct HtmlLanguageServerCandidate;

impl HtmlLanguageServerCandidate {
    pub fn new(_client: Arc<http_client::Client>) -> Self {
        Self
    }
}

#[async_trait]
#[cfg(feature = "local_fs")]
impl LanguageServerCandidate for HtmlLanguageServerCandidate {
    async fn should_suggest_for_repo(&self, _path: &Path, _executor: &CommandBuilder) -> bool {
        // We don't manage an install for the HTML server yet, so don't nudge the
        // user toward an install flow we can't fulfill. Detection on PATH still
        // lets the server start when it's available.
        false
    }

    async fn is_installed_in_data_dir(&self, _executor: &CommandBuilder) -> bool {
        false
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
        _metadata: LanguageServerMetadata,
        _executor: &CommandBuilder,
    ) -> anyhow::Result<()> {
        anyhow::bail!(
            "Managed install for the HTML language server is not supported yet. \
             Install it with `npm i -g vscode-langservers-extracted` so \
             `vscode-html-language-server` is on PATH."
        )
    }

    async fn fetch_latest_server_metadata(&self) -> anyhow::Result<LanguageServerMetadata> {
        anyhow::bail!("HTML language server has no managed install metadata")
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
