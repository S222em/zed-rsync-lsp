use std::fs;

use zed_extension_api::{
    register_extension,
    serde_json::Value,
    settings::{CommandSettings, LspSettings},
    Command, Extension, LanguageServerId, Result, Worktree,
};

struct RsyncLspExtension {
    cached_binary_path: Option<String>,
}

impl RsyncLspExtension {
    const LANGUAGE_SERVER_ID: &'static str = "rsync";

    fn language_server_binary_path(
        &mut self,
        _worktree: &Worktree,
        binary_settings: &Option<CommandSettings>,
    ) -> Result<String> {
        // Check for cached binary path.
        if let Some(cached_path) = &self.cached_binary_path {
            if fs::metadata(cached_path).is_ok() {
                return Ok(cached_path.clone());
            }
        }

        // Get binary path from zed settings.
        let binary_path = binary_settings
            .as_ref()
            .and_then(|binary_settings| binary_settings.path.clone());

        if let Some(binary_path) = binary_path {
            self.cached_binary_path = Some(binary_path.clone());
            return Ok(binary_path);
        }

        // TODO: get binary from GitHub.

        Err("Failed to locate Rsync LSP executable".to_string())
    }
}

impl Extension for RsyncLspExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        if language_server_id.as_ref() != RsyncLspExtension::LANGUAGE_SERVER_ID {
            return Err(format!("Unknown language server '{language_server_id}'"));
        }

        let binary_settings =
            LspSettings::for_worktree(RsyncLspExtension::LANGUAGE_SERVER_ID, worktree)
                .ok()
                .and_then(|lsp_settings| lsp_settings.binary);

        let binary_path = self.language_server_binary_path(worktree, &binary_settings)?;

        let binary_args = binary_settings
            .as_ref()
            .and_then(|binary_settings| binary_settings.arguments.clone())
            .unwrap_or(vec![]);

        Ok(Command {
            command: binary_path,
            args: binary_args,
            env: Default::default(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<zed_extension_api::serde_json::Value>> {
        if language_server_id.as_ref() != RsyncLspExtension::LANGUAGE_SERVER_ID {
            return Err(format!("Unknown language server '{language_server_id}'"));
        }

        let initialization_options =
            LspSettings::for_worktree(RsyncLspExtension::LANGUAGE_SERVER_ID, worktree)
                .ok()
                .and_then(|lsp_settings| lsp_settings.initialization_options);

        return Ok(initialization_options);
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<Value>> {
        if language_server_id.as_ref() != RsyncLspExtension::LANGUAGE_SERVER_ID {
            return Err(format!("Unknown language server '{language_server_id}'"));
        }

        let settings = LspSettings::for_worktree(RsyncLspExtension::LANGUAGE_SERVER_ID, worktree)
            .ok()
            .and_then(|lsp_settings| lsp_settings.settings);

        return Ok(settings);
    }
}

register_extension!(RsyncLspExtension);
