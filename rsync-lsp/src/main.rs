use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{error, info};

use crate::rsync::{close_connection, open_connection, sync};

mod rsync;

#[derive(Deserialize, Debug, Default)]
struct RsyncSettings {
    pub remote: Option<String>,
    pub remote_workspace_root: Option<String>,
}

#[derive(Debug)]
struct RsyncLsp {
    client: Client,
    pub workspace_root: Arc<Mutex<Option<String>>>,
    pub settings: Arc<Mutex<RsyncSettings>>,
}

impl RsyncLsp {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace_root: Arc::new(Mutex::new(None)),
            settings: Arc::new(Mutex::new(Default::default())),
        }
    }

    pub async fn open_connection(&self) {
        _ = open_connection(&self.settings);
    }

    pub async fn close_connection(&self) {
        _ = close_connection(&self.settings);
    }

    pub async fn sync(&self) {
        _ = sync(&self.workspace_root, &self.settings).await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for RsyncLsp {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        info!("Initializing");

        self.open_connection().await;
        self.sync().await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: None,
                    file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                        did_create: Some(FileOperationRegistrationOptions { filters: vec![] }),
                        did_rename: Some(FileOperationRegistrationOptions { filters: vec![] }),
                        did_delete: Some(FileOperationRegistrationOptions { filters: vec![] }),
                        ..Default::default()
                    }),
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("Initialized");
    }

    async fn did_open(&self, _params: DidOpenTextDocumentParams) {
        // Do nothing
    }

    async fn did_create_files(&self, _params: CreateFilesParams) {
        self.sync().await;
    }

    async fn did_rename_files(&self, _params: RenameFilesParams) {
        self.sync().await;
    }

    async fn did_delete_files(&self, _params: DeleteFilesParams) {
        self.sync().await;
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        self.sync().await;
    }

    async fn shutdown(&self) -> Result<()> {
        info!("Shutting down");

        self.close_connection().await;

        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let settings = serde_json::from_value::<RsyncSettings>(params.settings);

        let Ok(settings) = settings else {
            error!(
                "Failed to parse settings: {}",
                settings.unwrap_err().to_string()
            );
            return;
        };

        info!("Updating settings");

        *self.settings.lock().await = settings;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    info!("Starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| RsyncLsp::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;
}
