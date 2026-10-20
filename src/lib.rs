use zed_extension_api::{self as zed, settings::LspSettings, LanguageServerId, Result};

const BINARY: &str = "openapi-lsp";

struct OpenApiExtension;

impl zed::Extension for OpenApiExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.binary);
        let path = binary
            .as_ref()
            .and_then(|b| b.path.clone())
            .or_else(|| worktree.which(BINARY))
            .ok_or_else(|| {
                format!(
                    "`{BINARY}` not found in PATH. Install it with `cargo install --path server` \
                     from the extension directory or set `lsp.{BINARY}.binary.path`"
                )
            })?;
        let mut env = worktree.shell_env();
        if let Some(extra) = binary.as_ref().and_then(|b| b.env.clone()) {
            env.extend(extra);
        }
        Ok(zed::Command {
            command: path,
            args: binary.and_then(|b| b.arguments).unwrap_or_default(),
            env,
        })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.settings))
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        self.language_server_initialization_options(language_server_id, worktree)
    }
}

zed::register_extension!(OpenApiExtension);
