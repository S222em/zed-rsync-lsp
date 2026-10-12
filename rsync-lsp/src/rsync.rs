use std::{io::Stdin, process::Stdio, sync::Arc};

use tokio::{process::Command, sync::Mutex};
use tracing::{error, info};

use crate::RsyncSettings;

pub async fn open_connection(
    settings: &Arc<Mutex<RsyncSettings>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO: implement

    Ok(())
}

pub async fn close_connection(
    settings: &Arc<Mutex<RsyncSettings>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // TODO implement

    Ok(())
}

pub async fn sync(
    workspace_root: &Arc<Mutex<Option<String>>>,
    settings: &Arc<Mutex<RsyncSettings>>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting sync");

    let workspace_root = workspace_root.lock().await;

    let Some(workspace_root) = workspace_root.as_ref() else {
        error!("No workspace root present");
        return Ok(());
    };

    let settings = settings.lock().await;

    let Some(remote) = settings.remote.as_ref() else {
        info!("No remote configured");
        return Ok(());
    };

    let Some(remote_workspace_root) = settings.remote_workspace_root.as_ref() else {
        info!("No remote workspace root configured");
        return Ok(());
    };

    let remote_with_path = format!("{remote}:{remote_workspace_root}");

    let mut command = Command::new("rsync");

    command
        .arg("-crzE")
        .arg("--delete")
        .arg(workspace_root)
        .arg(remote_with_path);

    command.stdout(Stdio::piped());

    Ok(())
}
