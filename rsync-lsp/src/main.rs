use std::sync::Arc;

use openssh::{KnownHosts, Session};
use serde::Deserialize;
use tokio::process::Command as ProcessCommand;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{error, info};

#[derive(Deserialize, Debug, Default)]
pub struct BackendInitializationOptions {
    pub remote: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct BackendConfiguration {
    remote_path: Option<String>,
    exclude: Option<Vec<String>>,
}

#[derive(Debug)]
struct Backend {
    client: Client,
    session: Arc<Mutex<Option<Session>>>,
    workspace_path: Arc<Mutex<Option<String>>>,
    initializiation_options: Arc<Mutex<Option<BackendInitializationOptions>>>,
    configuration: Arc<Mutex<BackendConfiguration>>,
}

// TODO full sync on start + sync action.

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client: client.clone(),
            session: Arc::new(Mutex::new(None)),
            workspace_path: Arc::new(Mutex::new(None)),
            initializiation_options: Arc::new(Mutex::new(None)),
            configuration: Arc::new(Mutex::new(Default::default())),
        }
    }

    async fn handle_file_event(&self, event: FileEvent) {
        let session = self.session.lock().await;

        let Some(session) = session.as_ref() else {
            return;
        };

        let path = event.uri.path();

        match event.typ {
            FileChangeType::CREATED | FileChangeType::CHANGED => {
                self.sync_file_updated(session, path).await
            }
            FileChangeType::DELETED => self.sync_file_deleted(session, path).await,
            _ => {}
        }
    }

    async fn sync_file_updated(&self, session: &Session, local_file_path: &str) {
        let mut cmd = ProcessCommand::new("rsync");

        let workspace_path = self.workspace_path.lock().await;
        let Some(workspace_path) = workspace_path.as_ref() else {
            return;
        };

        let initialization_options = self.initializiation_options.lock().await;
        let Some(initialization_options) = initialization_options.as_ref() else {
            return;
        };

        let configuration = self.configuration.lock().await;
        let Some(remote_path) = configuration.remote_path.as_ref() else {
            return;
        };

        let remote_file_path = local_file_path.replace(workspace_path, remote_path);
        let remote = format!("{}:{}", &initialization_options.remote, &remote_file_path);

        if let Some(exclude) = configuration.exclude.as_ref() {
            for excluded in exclude {
                cmd.args(&["--exclude", excluded]);
            }
        }

        cmd.args(&[
            "-cd",
            "-e",
            &format!("ssh -S \'{}\'", session.control_socket().to_string_lossy()),
            local_file_path,
            &remote,
        ]);

        let output = cmd.output().await;

        let err_msg = match output {
            Ok(output) => match output.status.success() {
                true => None,
                false => Some(String::from_utf8(output.stderr).unwrap_or("unknown".to_string())),
            },
            Err(err) => Some(err.to_string()),
        };

        match err_msg {
            Some(err_msg) => {
                let msg = format!("Failed to sync file '{}': {}", local_file_path, err_msg);

                error!("{}", msg);

                self.client.show_message(MessageType::ERROR, msg).await;
            }
            None => {
                info!("Synced file '{}'", remote_file_path);
            }
        }
    }

    async fn sync_file_deleted(&self, session: &Session, local_file_path: &str) {
        // TODO: ignore paths

        let mut cmd = session.command("rm");

        let workspace_path = self.workspace_path.lock().await;
        let Some(workspace_path) = workspace_path.as_ref() else {
            return;
        };

        let configuration = self.configuration.lock().await;
        let Some(remote_path) = configuration.remote_path.as_ref() else {
            return;
        };

        let remote_file_path = local_file_path.replace(workspace_path, remote_path);

        cmd.args(&["-f", &remote_file_path]);

        let output = cmd.output().await;

        let err_msg = match output {
            Ok(output) => match output.status.success() {
                true => None,
                false => Some(String::from_utf8(output.stderr).unwrap_or("unknown".to_string())),
            },
            Err(err) => Some(err.to_string()),
        };

        match err_msg {
            Some(err_msg) => {
                let msg = format!("Failed to delete file '{}': {}", local_file_path, err_msg);

                error!("{}", msg);

                self.client.show_message(MessageType::ERROR, msg).await;
            }
            None => {
                info!("Deleted file '{}'", remote_file_path);
            }
        }
    }

    async fn register_file_watchers(&self) {
        let watcher = FileSystemWatcher {
            glob_pattern: GlobPattern::String("*".to_string()),
            kind: None,
        };

        let registration_options = serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![watcher],
        });

        if let Err(err) = registration_options {
            error!("Failed to convert registration options to value: {}", err);
            return;
        }

        let registration = Registration {
            id: "file-watchers".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(registration_options.unwrap()),
        };

        let result = self.client.register_capability(vec![registration]).await;

        match result {
            Ok(_) => info!("Registered file watchers"),
            Err(err) => error!("Failed to register file watchers: {}", err),
        };
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        *self.workspace_path.lock().await = params.root_uri.map(|uri| uri.path().to_string());

        *self.initializiation_options.lock().await =
            params.initialization_options.and_then(|options| {
                serde_json::from_value::<BackendInitializationOptions>(options).ok()
            });

        if let Some(remote) = self
            .initializiation_options
            .lock()
            .await
            .as_ref()
            .map(|initialization_options| &initialization_options.remote)
        {
            let session = Session::connect(remote, KnownHosts::Strict).await;

            match session {
                Ok(session) => {
                    *self.session.lock().await = Some(session);
                    info!("Started session");
                }
                Err(err) => {
                    error!("Failed to start session: {}", err);
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.register_file_watchers().await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for event in params.changes {
            self.handle_file_event(event).await;
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let configuration = serde_json::from_value::<BackendConfiguration>(params.settings);

        let Ok(configuration) = configuration else {
            return;
        };

        *self.configuration.lock().await = configuration;
    }

    async fn shutdown(&self) -> Result<()> {
        info!("Shutting down");

        if let Some(session) = self.session.lock().await.take() {
            _ = session.close().await;
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
