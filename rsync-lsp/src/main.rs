use std::path::MAIN_SEPARATOR;
use std::sync::Arc;

use anyhow::{anyhow, Error};
use openssh::{KnownHosts, Session};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command as ProcessCommand;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::Result as JsonRpcResult;
use tower_lsp::lsp_types::notification::Progress;
use tower_lsp::lsp_types::request::WorkDoneProgressCreate;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Deserialize, Debug, Default)]
pub struct BackendInitializationOptions {
    pub remote: String,
    pub remote_path: String,
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug)]
struct Backend {
    client: Client,
    session: Arc<Mutex<Option<Session>>>,
    workspace_path: Arc<RwLock<Option<String>>>,
    initializiation_options: Arc<RwLock<Option<BackendInitializationOptions>>>,
}

// TODO folders with [] cause error.
// TODO updating files in ignored folder cause error.

impl Backend {
    const RESYNC_COMMAND: &str = "resync";

    pub fn new(client: Client) -> Self {
        Self {
            client: client.clone(),
            session: Arc::new(Mutex::new(None)),
            workspace_path: Arc::new(RwLock::new(None)),
            initializiation_options: Arc::new(RwLock::new(None)),
        }
    }

    async fn handle_file_event(&self, event: FileEvent) {
        let session = self.session.lock().await;
        let Some(session) = session.as_ref() else {
            return;
        };

        let workspace_path_lock = self.workspace_path.read().await;
        let Some(workspace_path) = workspace_path_lock.as_ref() else {
            return;
        };

        let initialization_options_lock = self.initializiation_options.read().await;
        let Some(initialization_options) = initialization_options_lock.as_ref() else {
            return;
        };

        let local_file_path = event.uri.path();

        let cmd = self.prepare_rsync_update_cmd(
            session,
            local_file_path,
            workspace_path,
            initialization_options,
        );

        let progress_token = self.progress_sync_start().await;

        let result = self.execute_cmd(cmd).await;

        self.progress_sync_end(progress_token).await;

        let action = match event.typ {
            FileChangeType::CREATED => "created",
            FileChangeType::CHANGED => "changed",
            FileChangeType::DELETED => "deleted",
            _ => unreachable!(),
        };

        match result {
            Ok(_) => {
                info!("Synced {} '{}'", action, local_file_path)
            }
            Err(err) => {
                let msg = format!(
                    "Failed to sync {} '{}': {}",
                    action,
                    local_file_path,
                    err.to_string()
                );

                error!("{}", msg);

                self.client.show_message(MessageType::ERROR, msg).await;
            }
        }
    }

    async fn handle_resync(&self) {
        let session = self.session.lock().await;
        let Some(session) = session.as_ref() else {
            return;
        };

        let workspace_path_lock = self.workspace_path.read().await;
        let Some(workspace_path) = workspace_path_lock.as_ref() else {
            return;
        };

        let initialization_options_lock = self.initializiation_options.read().await;
        let Some(initialization_options) = initialization_options_lock.as_ref() else {
            return;
        };

        let cmd = self.prepare_rsync_full_cmd(session, workspace_path, initialization_options);

        let progress_token = self.progress_sync_start().await;

        let result = self.execute_cmd(cmd).await;

        self.progress_sync_end(progress_token).await;

        match result {
            Ok(_) => {
                info!("Resynced '{}'", workspace_path)
            }
            Err(err) => {
                let msg = format!("Failed to resync '{}': {}", workspace_path, err.to_string());

                error!("{}", msg);

                self.client.show_message(MessageType::ERROR, msg).await;
            }
        }
    }

    fn prepare_rsync_update_cmd(
        &self,
        session: &Session,
        local_file_path: &str,
        workspace_path: &str,
        initialization_options: &BackendInitializationOptions,
    ) -> ProcessCommand {
        let relative_file_path = local_file_path.replace(workspace_path, "");

        let mut cmd = self.prepare_rsync_cmd(session, &initialization_options.exclude);

        cmd.args(&[
            "-dr",
            "--delete",
            "--include",
            &relative_file_path,
            "--exclude",
            "*",
            &format!("{}{}", workspace_path, MAIN_SEPARATOR),
            &format!(
                "{}:{}",
                &initialization_options.remote, &initialization_options.remote_path
            ),
        ]);

        cmd
    }

    fn prepare_rsync_full_cmd(
        &self,
        session: &Session,
        workspace_path: &str,
        initialization_options: &BackendInitializationOptions,
    ) -> ProcessCommand {
        let mut cmd = self.prepare_rsync_cmd(session, &initialization_options.exclude);

        cmd.args(&[
            "-zr",
            &format!("{}{}", workspace_path, MAIN_SEPARATOR),
            &format!(
                "{}:{}",
                initialization_options.remote, initialization_options.remote_path
            ),
        ]);

        cmd
    }

    fn prepare_rsync_cmd(
        &self,
        session: &Session,
        exclude: &Option<Vec<String>>,
    ) -> ProcessCommand {
        let mut cmd = ProcessCommand::new("rsync");

        cmd.args(&[
            "-e",
            &format!("ssh -S \'{}\'", session.control_socket().to_string_lossy()),
        ]);

        if let Some(exclude) = exclude {
            for excluded in exclude {
                cmd.args(&["--exclude", excluded]);
            }
        }

        cmd
    }

    async fn execute_cmd(&self, mut cmd: ProcessCommand) -> Result<(), Error> {
        let output = match cmd.output().await {
            Ok(output) => output,
            Err(err) => return Err(anyhow!(err.to_string())),
        };

        if !output.status.success() {
            match String::from_utf8(output.stderr) {
                Ok(msg) => return Err(anyhow!(msg)),
                Err(_) => return Err(anyhow!("")),
            }
        }

        Ok(())
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

    async fn progress(&self, token: String, params: WorkDoneProgress) {
        let progress = ProgressParams {
            token: NumberOrString::String(token),
            value: ProgressParamsValue::WorkDone(params),
        };

        self.client.send_notification::<Progress>(progress).await;
    }

    async fn create_progress(&self, token: String) -> Option<()> {
        let work_done_create = WorkDoneProgressCreateParams {
            token: NumberOrString::String(token),
        };

        self.client
            .send_request::<WorkDoneProgressCreate>(work_done_create)
            .await
            .ok()
    }

    async fn progress_sync_start(&self) -> String {
        let token = Uuid::new_v4().to_string();

        self.create_progress(token.clone()).await;

        let work_done_begin = WorkDoneProgressBegin {
            title: "Syncing".to_string(),
            ..Default::default()
        };

        self.progress(token.clone(), WorkDoneProgress::Begin(work_done_begin))
            .await;

        token
    }

    async fn progress_sync_end(&self, token: String) {
        let work_done_end = WorkDoneProgressEnd {
            ..Default::default()
        };

        self.progress(token, WorkDoneProgress::End(work_done_end))
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        *self.workspace_path.write().await = params.root_uri.map(|uri| uri.path().to_string());

        *self.initializiation_options.write().await =
            params.initialization_options.and_then(|options| {
                serde_json::from_value::<BackendInitializationOptions>(options).ok()
            });

        if let Some(remote) = self
            .initializiation_options
            .read()
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
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![Backend::RESYNC_COMMAND.to_string()],
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.register_file_watchers().await;

        self.handle_resync().await;
    }

    async fn did_open(&self, _params: DidOpenTextDocumentParams) {
        // Do nothing
    }

    async fn did_close(&self, _params: DidCloseTextDocumentParams) {
        // Do nothing
    }

    async fn did_change_configuration(&self, _params: DidChangeConfigurationParams) {
        // Do nothing
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for event in params.changes {
            self.handle_file_event(event).await;
        }
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> JsonRpcResult<Option<Value>> {
        if &params.command != Backend::RESYNC_COMMAND {
            return Ok(None);
        }

        self.handle_resync().await;

        Ok(None)
    }

    async fn shutdown(&self) -> JsonRpcResult<()> {
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
