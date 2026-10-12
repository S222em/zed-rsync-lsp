use std::fs;

use zed_extension_api::{self as zed, settings::LspSettings, Result};

struct RsyncLspExtension {
    cached_binary_path: Option<String>,
}

impl RsyncLspExtension {
    const LANGUAGE_SERVER_ID: &'static str = "rsync";

    fn language_server_binary_path(&mut self, worktree: &zed::Worktree) -> Result<String> {
        let binary_path =
            LspSettings::for_worktree(RsyncLspExtension::LANGUAGE_SERVER_ID, worktree)
                .ok()
                .and_then(|lsp_settings| lsp_settings.binary)
                .and_then(|lsp_binary| lsp_binary.path);

        if let Some(binary_path) = binary_path {
            self.cached_binary_path = Some(binary_path.clone());
        }

        // Check if we have a cached binary path
        if let Some(cached_path) = &self.cached_binary_path {
            if fs::metadata(cached_path).is_ok() {
                return Ok(cached_path.clone());
            }
        }

        // let (os, arch) = zed::current_platform();
        // let binary_name = format!(
        //     "{os}-{arch}",
        //     os = match os {
        //         zed::Os::Linux => "linux",
        //         zed::Os::Mac => "mac",
        //         zed::Os::Windows => "windows",
        //     },
        //     arch = match arch {
        //         zed::Architecture::Aarch64 => "aarch64",
        //         zed::Architecture::X86 => "x86",
        //         zed::Architecture::X8664 => "x8664",
        //     },
        // );

        return Err("Failed to locate Rsync LSP executable".to_string());
    }
}

impl zed::Extension for RsyncLspExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        if language_server_id.as_ref() != RsyncLspExtension::LANGUAGE_SERVER_ID {
            return Err(format!("Unknown language server '{language_server_id}'"));
        }

        let binary_path = self.language_server_binary_path(worktree)?;

        Ok(zed::Command {
            command: binary_path,
            args: vec![],
            env: Default::default(),
        })
    }
}

zed::register_extension!(RsyncLspExtension);
