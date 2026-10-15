use std::path::MAIN_SEPARATOR;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Error};
use openssh::{ControlPersist, KnownHosts, Session, SessionBuilder};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command as ProcessCommand;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::{self, ErrorCode, Result as JsonRpcResult};
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

    async fn do_rsync_cmd(&self, args: Option<&[&str]>) -> Result<(), Error> {
        let session = self.session.lock().await;
        let Some(session) = session.as_ref() else {
            return Ok(());
        };

        // TODO: check if session is ok.

        let workspace_path = self.workspace_path.read().await;
        let Some(workspace_path) = workspace_path.as_ref() else {
            return Ok(());
        };

        let initialization_options = self.initializiation_options.read().await;
        let Some(initialization_options) = initialization_options.as_ref() else {
            return Ok(());
        };

        let mut cmd = self.prepare_rsync_cmd(session, workspace_path, initialization_options, args);

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

    fn prepare_rsync_cmd(
        &self,
        session: &Session,
        workspace_path: &str,
        initialization_options: &BackendInitializationOptions,
        args: Option<&[&str]>,
    ) -> ProcessCommand {
        let mut cmd = ProcessCommand::new("rsync");

        if let Some(args) = args {
            cmd.args(args);
        }

        cmd.args(&[
            "-t",
            "--delete",
            "-e",
            &format!("ssh -S \'{}\'", session.control_socket().to_string_lossy()),
        ]);

        if let Some(exclude) = initialization_options.exclude.as_ref() {
            for excluded in exclude {
                cmd.args(&["--exclude", excluded]);
            }
        }

        cmd.args(&[
            &format!("{}{}", workspace_path, MAIN_SEPARATOR),
            &format!(
                "{}:{}",
                &initialization_options.remote, &initialization_options.remote_path
            ),
        ]);

        cmd
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
            id: Uuid::new_v4().to_string(),
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

    async fn progress_start(&self, title: String) -> String {
        let token = Uuid::new_v4().to_string();

        self.create_progress(token.clone()).await;

        let work_done_begin = WorkDoneProgressBegin {
            title,
            ..Default::default()
        };

        self.progress(token.clone(), WorkDoneProgress::Begin(work_done_begin))
            .await;

        token
    }

    async fn progress_end(&self, token: String) {
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

        if let Some(initialization_options) = params.initialization_options {
            let initialization_options =
                serde_json::from_value::<BackendInitializationOptions>(initialization_options);

            if let Err(err) = initialization_options {
                error!(
                    "Failed to parse initialization options: {}",
                    err.to_string()
                );

                return Err(jsonrpc::Error {
                    code: ErrorCode::ParseError,
                    message: format!(
                        "Failed to parse initialization options: {}",
                        err.to_string()
                    )
                    .into(),
                    data: None,
                });
            }

            *self.initializiation_options.write().await = initialization_options.ok();
        }

        if let Some(remote) = self
            .initializiation_options
            .read()
            .await
            .as_ref()
            .map(|initialization_options| &initialization_options.remote)
        {
            let mut builder = SessionBuilder::default();

            builder
                .known_hosts_check(KnownHosts::Strict)
                .control_persist(ControlPersist::Forever)
                .connect_timeout(Duration::from_secs(30))
                .server_alive_interval(Duration::from_secs(30));

            info!("Starting session");

            let session = builder.connect(remote).await.map_err(|err| jsonrpc::Error {
                code: ErrorCode::InternalError,
                message: format!("Failed to start session: {}", err.to_string()).into(),
                data: None,
            });

            if let Err(err) = session {
                error!("{}", err.message);
                return Err(err);
            }

            let session = session.unwrap();

            info!("Started session");
            info!(
                "Session control socket: '{}'",
                session.control_socket().to_string_lossy()
            );

            *self.session.lock().await = Some(session);
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

        let progress_token = self.progress_start("Syncing".to_string()).await;
        let result = self.do_rsync_cmd(Some(&["-zar"])).await;
        self.progress_end(progress_token).await;

        match result {
            Ok(_) => info!("Completed initial sync"),
            Err(err) => {
                info!("Initial sync failed");
                error!("{}", err.to_string());

                self.client
                    .show_message(
                        MessageType::ERROR,
                        format!("Sync failed: {}", err.to_string()),
                    )
                    .await;
            }
        }
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
        let workspace_path = self.workspace_path.read().await;
        let Some(workspace_path) = workspace_path.as_ref() else {
            error!("Workspace path is missing");
            return;
        };

        let include: Vec<&str> = params
            .changes
            .iter()
            .filter_map(|event| event.uri.path().strip_prefix(workspace_path))
            .collect();

        let mut args = vec!["-dr"];

        for included in &include {
            args.extend_from_slice(&["--include", included]);
        }

        let progress_token = self.progress_start("Syncing".to_string()).await;
        let result = self.do_rsync_cmd(Some(&args)).await;
        self.progress_end(progress_token).await;

        let all_included_files = include.join(", ");

        match result {
            Ok(_) => {
                info!("Synced file(s): {}", all_included_files)
            }
            Err(err) => {
                info!("Failed to sync file(s): {}", all_included_files);
                error!("{}", err);

                self.client
                    .show_message(MessageType::ERROR, err.to_string())
                    .await;
            }
        }
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> JsonRpcResult<Option<Value>> {
        if &params.command != Backend::RESYNC_COMMAND {
            return Err(jsonrpc::Error {
                code: ErrorCode::MethodNotFound,
                message: "".into(),
                data: None,
            });
        }

        let progress_token = self.progress_start("Syncing".to_string()).await;
        let result = self.do_rsync_cmd(Some(&["-zar"])).await;
        self.progress_end(progress_token).await;

        match result {
            Ok(_) => {
                info!("Completed sync");

                Ok(None)
            }
            Err(err) => {
                info!("Sync failed");
                error!("{}", err.to_string());

                Err(jsonrpc::Error {
                    code: ErrorCode::InternalError,
                    message: format!("Failed to sync: {}", err.to_string()).into(),
                    data: None,
                })
            }
        }
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
