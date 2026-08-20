use std::{process::Stdio, time::Duration};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command, time};

use crate::{error::ApiError, security::SecurityProfile, state::AppState};

#[derive(Debug, Deserialize)]
pub struct RawCommandRequest {
    pub workspace: String,
    pub command: String,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RawCommandResult {
    pub command: String,
    pub profile: SecurityProfile,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

pub async fn run_raw_command(
    State(state): State<AppState>,
    Json(request): Json<RawCommandRequest>,
) -> Result<Json<RawCommandResult>, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(&request.workspace)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {:?} is not configured", request.workspace)))?;
    if !workspace.capabilities.commands {
        return Err(ApiError::Forbidden(format!(
            "workspace {:?} does not allow command execution",
            request.workspace
        )));
    }

    let root = std::fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))?;
    let limits = &state.config.limits;
    let timeout_seconds = request
        .timeout_seconds
        .unwrap_or(limits.command_timeout_seconds)
        .clamp(1, limits.max_command_timeout_seconds);
    let profile = state.config.security.profile;

    let mut command = if profile.allows_unrestricted_shell() {
        let mut cmd = Command::new("/bin/bash");
        cmd.args(["--noprofile", "--norc", "-lc", &request.command]);
        cmd
    } else {
        let argv = crate::security::validate_default_shell(
            &request.command,
            &state.config.security.default_command_allowlist,
        )?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd
    };

    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| ApiError::Internal(format!("failed to start command: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::Internal("failed to capture stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::Internal("failed to capture stderr".into()))?;
    let max_output = limits.command_output_bytes;
    let stdout_task = tokio::spawn(drain_limited(stdout, max_output));
    let stderr_task = tokio::spawn(drain_limited(stderr, max_output));

    let wait = time::timeout(Duration::from_secs(timeout_seconds), child.wait()).await;
    let (status, timed_out) = match wait {
        Ok(result) => (
            Some(result.map_err(|error| ApiError::Internal(format!("failed waiting for command: {error}")))?),
            false,
        ),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };

    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|error| ApiError::Internal(format!("stdout task failed: {error}")))?
        .map_err(|error| ApiError::Internal(format!("failed reading stdout: {error}")))?;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|error| ApiError::Internal(format!("stderr task failed: {error}")))?
        .map_err(|error| ApiError::Internal(format!("failed reading stderr: {error}")))?;

    Ok(Json(RawCommandResult {
        command: request.command,
        profile,
        exit_code: status.and_then(|value| value.code()),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        stdout_truncated,
        stderr_truncated,
        timed_out,
    }))
}

async fn drain_limited<R>(mut reader: R, max: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len() < max {
            let remaining = max - output.len();
            output.extend_from_slice(&buffer[..read.min(remaining)]);
            if read > remaining {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((output, truncated))
}
