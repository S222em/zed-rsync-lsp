use std::collections::HashSet;
use std::path::PathBuf;
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

#[derive(Deserialize, Debug, Default, Clone)]
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
    workspace_path: Arc<RwLock<Option<PathBuf>>>,
    initializiation_options: Arc<RwLock<Option<BackendInitializationOptions>>>,
    sync_lock: Arc<Mutex<()>>,
    path_queue: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Backend {
    const RESYNC_COMMAND: &str = "resync";

    pub fn new(client: Client) -> Self {
        Self {
            client: client.clone(),
            session: Arc::new(Mutex::new(None)),
            workspace_path: Arc::new(RwLock::new(None)),
            initializiation_options: Arc::new(RwLock::new(None)),
            sync_lock: Arc::new(Mutex::new(())),
            path_queue: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    async fn append_paths_to_queue(&self, paths: Vec<PathBuf>) {
        let mut queue = self.path_queue.lock().await;

        for path in paths {
            queue.insert(path);
        }
    }

    async fn drain_paths_from_queue(&self) -> Vec<PathBuf> {
        let mut queue = self.path_queue.lock().await;

        queue.drain().collect()
    }

    async fn sync_queued_files(&self) -> Result<(), Error> {
        let _guard = self.sync_lock.lock().await;

        let paths = self.drain_paths_from_queue().await;

        if paths.len() == 0 {
            return Ok(());
        }

        let result = self.sync(Some(&paths)).await;

        result
    }

    async fn sync_project(&self) -> Result<(), Error> {
        let _guard = self.sync_lock.lock().await;

        self.sync(None).await
    }

    async fn start_session(&self, remote: &str) -> Result<Session, Error> {
        let mut builder = SessionBuilder::default();

        builder
            .known_hosts_check(KnownHosts::Strict)
            .control_persist(ControlPersist::Forever)
            .connect_timeout(Duration::from_secs(30))
            .server_alive_interval(Duration::from_secs(30));

        let progress_token = self
            .progress_start(format!("Connecting to {}", remote))
            .await;

        let session = builder.connect(remote).await.map_err(|err| anyhow!(err));
        self.progress_end(progress_token).await;

        session
    }

    async fn get_control_socket(&self) -> Result<String, Error> {
        let session = self.session.lock().await;

        let Some(session) = session.as_ref() else {
            return Err(anyhow!("Session is closed"));
        };

        if let Err(err) = session.check().await {
            return Err(anyhow!(err));
        }

        Ok(session.control_socket().to_string_lossy().to_string())
    }

    async fn sync(&self, include_paths: Option<&Vec<PathBuf>>) -> Result<(), Error> {
        let workspace_path = self.workspace_path.read().await;
        let Some(workspace_path) = workspace_path.as_ref() else {
            return Err(anyhow!("Workspace path missing"));
        };

        let initialization_options = self.initializiation_options.read().await;
        let Some(initialization_options) = initialization_options.as_ref() else {
            return Err(anyhow!("Initialization options missing"));
        };

        let mut cmd = ProcessCommand::new("rsync");

        cmd.args(&[
            "-zadrt",
            "--delete",
            "-e",
            &format!("ssh -S \'{}\'", self.get_control_socket().await?),
        ]);

        if let Some(exclude_files) = initialization_options.exclude.as_ref() {
            for file in exclude_files {
                cmd.args(&["--exclude", file]);
            }
        }

        if let Some(include_paths) = include_paths {
            for path in include_paths {
                if let Ok(path) = path.strip_prefix(workspace_path) {
                    cmd.args(&["--include", &path.to_string_lossy()]);
                }
            }

            cmd.args(&["--exclude", "*"]);
        }

        cmd.args(&[
            &format!("{}/", workspace_path.display()),
            &format!(
                "{}:{}",
                &initialization_options.remote, &initialization_options.remote_path
            ),
        ]);

        let progress_title = match include_paths {
            Some(paths) if paths.len() == 1 => format!(
                "Syncing {}",
                paths[0]
                    .file_name()
                    .map(|file_name| file_name.to_string_lossy())
                    .unwrap_or("files".into())
            ),
            _ => "Syncing files".to_string(),
        };

        let progress_token = self.progress_start(progress_title).await;

        let result = cmd.output().await;
        self.progress_end(progress_token).await;

        let output = match result {
            Ok(output) => output,
            Err(err) => return Err(anyhow!(err)),
        };

        if !output.status.success() {
            match String::from_utf8(output.stderr) {
                Ok(msg) => return Err(anyhow!(msg)),
                Err(_) => return Err(anyhow!("")),
            }
        }

        Ok(())
    }

    async fn register_file_watchers(&self) -> Result<(), Error> {
        let watcher = FileSystemWatcher {
            glob_pattern: GlobPattern::String("*".to_string()),
            kind: None,
        };

        let registration_options = serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![watcher],
        })
        .map_err(|err| anyhow!(err))?;

        let registration = Registration {
            id: Uuid::new_v4().to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(registration_options),
        };

        self.client
            .register_capability(vec![registration])
            .await
            .map_err(|err| anyhow!(err))
    }

    async fn progress(&self, token: String, params: WorkDoneProgress) {
        let progress = ProgressParams {
            token: NumberOrString::String(token),
            value: ProgressParamsValue::WorkDone(params),
        };

        self.client.send_notification::<Progress>(progress).await;
    }

    async fn progress_start(&self, title: String) -> String {
        let token = Uuid::new_v4().to_string();

        self.client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: NumberOrString::String(token.clone()),
            })
            .await
            .ok();

        self.progress(
            token.clone(),
            WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title,
                ..Default::default()
            }),
        )
        .await;

        token
    }

    async fn progress_end(&self, token: String) {
        self.progress(
            token,
            WorkDoneProgress::End(WorkDoneProgressEnd {
                ..Default::default()
            }),
        )
        .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        *self.workspace_path.write().await =
            params.root_uri.and_then(|uri| uri.to_file_path().ok());

        if let Some(initialization_options) = params.initialization_options {
            let initialization_options = match serde_json::from_value::<BackendInitializationOptions>(
                initialization_options,
            ) {
                Ok(initialization_options) => initialization_options,
                Err(err) => {
                    error!("Failed to parse initialization options: {}", err);

                    return Err(jsonrpc::Error {
                        code: ErrorCode::ParseError,
                        message: "Failed to parse initialization options".into(),
                        data: None,
                    });
                }
            };

            match self.start_session(&initialization_options.remote).await {
                Ok(session) => {
                    info!("Started session");

                    *self.session.lock().await = Some(session);
                }
                Err(err) => {
                    error!("Failed to start session: {}", err);

                    return Err(jsonrpc::Error {
                        code: ErrorCode::InternalError,
                        message: "Failed to start session".into(),
                        data: None,
                    });
                }
            }

            *self.initializiation_options.write().await = Some(initialization_options);
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
        if self.session.lock().await.is_none() {
            return;
        }

        match self.register_file_watchers().await {
            Ok(_) => info!("Registered file watchers"),
            Err(err) => {
                error!("Failed to register file watchers: {}", err);
                self.client
                    .show_message(MessageType::ERROR, "Failed to register file watchers")
                    .await;
            }
        };

        match self.sync_project().await {
            Ok(_) => {
                info!("Synced project");
            }
            Err(err) => {
                error!("Failed to sync project: {}", err);

                self.client
                    .show_message(MessageType::ERROR, "Failed to sync project")
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
        let paths: Vec<PathBuf> = params
            .changes
            .iter()
            .filter_map(|event| event.uri.to_file_path().ok())
            .collect();

        self.append_paths_to_queue(paths).await;

        let Err(err) = self.sync_queued_files().await else {
            info!("Synced changed files");
            return;
        };

        error!("Failed to sync changed files: {}", err);

        self.client
            .show_message(MessageType::ERROR, "Failed to sync changed file(s)")
            .await;
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> JsonRpcResult<Option<Value>> {
        if &params.command != Backend::RESYNC_COMMAND {
            return Err(jsonrpc::Error {
                code: ErrorCode::MethodNotFound,
                message: "".into(),
                data: None,
            });
        }

        match self.sync_project().await {
            Ok(_) => {
                info!("Synced project");

                Ok(None)
            }
            Err(err) => {
                error!("Failed to sync project: {}", err);

                Err(jsonrpc::Error {
                    code: ErrorCode::InternalError,
                    message: "Failed to sync project".into(),
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
