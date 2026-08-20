use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path as AxumPath, State},
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex, oneshot},
    time,
};

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const MAX_TIMEOUT_SECONDS: u64 = 900;
const MAX_OUTPUT_BYTES: usize = 2_097_152;

#[derive(Clone, Default)]
pub struct JobStore {
    jobs: Arc<Mutex<HashMap<String, JobRecord>>>,
    next_id: Arc<AtomicU64>,
}

struct JobRecord {
    snapshot: JobSnapshot,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandRequest {
    workspace: String,
    argv: Vec<String>,
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    argv: Vec<String>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    timed_out: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobSnapshot {
    id: String,
    workspace: String,
    argv: Vec<String>,
    status: JobStatus,
    started_at_unix: u64,
    finished_at_unix: Option<u64>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

pub async fn run_command(
    State(state): State<AppState>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResult>, ApiError> {
    let workspace = command_workspace(&state, &request.workspace)?;
    validate_argv(&request.argv)?;
    let timeout = request
        .timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, MAX_TIMEOUT_SECONDS);
    let root = canonical_root(workspace)?;
    let result = execute(&root, request.argv, timeout).await?;
    Ok(Json(result))
}

pub async fn start_command(
    State(state): State<AppState>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<JobSnapshot>, ApiError> {
    let workspace = command_workspace(&state, &request.workspace)?;
    validate_argv(&request.argv)?;
    let timeout = request
        .timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, MAX_TIMEOUT_SECONDS);
    let root = canonical_root(workspace)?;

    let id = state.jobs.next_id();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let snapshot = JobSnapshot {
        id: id.clone(),
        workspace: request.workspace.clone(),
        argv: request.argv.clone(),
        status: JobStatus::Running,
        started_at_unix: unix_now(),
        finished_at_unix: None,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        error: None,
    };

    state
        .jobs
        .insert(
            id.clone(),
            JobRecord {
                snapshot: snapshot.clone(),
                cancel: Some(cancel_tx),
            },
        )
        .await;

    let jobs = state.jobs.clone();
    let argv = request.argv;
    tokio::spawn(async move {
        let completion = execute_inner(&root, argv, timeout, Some(cancel_rx)).await;
        jobs.finish(&id, completion).await;
    });

    Ok(Json(snapshot))
}

pub async fn get_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<JobSnapshot>, ApiError> {
    state
        .jobs
        .get(&id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("job {id:?} was not found")))
}

pub async fn cancel_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<JobSnapshot>, ApiError> {
    state.jobs.cancel(&id).await.map(Json)
}

impl JobStore {
    fn next_id(&self) -> String {
        let value = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("job-{value:016x}")
    }

    async fn insert(&self, id: String, record: JobRecord) {
        self.jobs.lock().await.insert(id, record);
    }

    async fn get(&self, id: &str) -> Option<JobSnapshot> {
        self.jobs
            .lock()
            .await
            .get(id)
            .map(|record| record.snapshot.clone())
    }

    async fn cancel(&self, id: &str) -> Result<JobSnapshot, ApiError> {
        let mut jobs = self.jobs.lock().await;
        let record = jobs
            .get_mut(id)
            .ok_or_else(|| ApiError::NotFound(format!("job {id:?} was not found")))?;
        if !matches!(record.snapshot.status, JobStatus::Running) {
            return Err(ApiError::Conflict("job is not running".into()));
        }
        let sender = record
            .cancel
            .take()
            .ok_or_else(|| ApiError::Conflict("job cannot be cancelled".into()))?;
        let _ = sender.send(());
        Ok(record.snapshot.clone())
    }

    async fn finish(&self, id: &str, completion: Result<CommandResult, ExecuteError>) {
        let mut jobs = self.jobs.lock().await;
        let Some(record) = jobs.get_mut(id) else {
            return;
        };
        record.cancel = None;
        record.snapshot.finished_at_unix = Some(unix_now());

        match completion {
            Ok(result) => {
                record.snapshot.exit_code = result.exit_code;
                record.snapshot.stdout = result.stdout;
                record.snapshot.stderr = result.stderr;
                record.snapshot.stdout_truncated = result.stdout_truncated;
                record.snapshot.stderr_truncated = result.stderr_truncated;
                record.snapshot.status = if result.timed_out {
                    JobStatus::TimedOut
                } else if result.exit_code == Some(0) {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
            }
            Err(ExecuteError::Cancelled {
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            }) => {
                record.snapshot.stdout = stdout;
                record.snapshot.stderr = stderr;
                record.snapshot.stdout_truncated = stdout_truncated;
                record.snapshot.stderr_truncated = stderr_truncated;
                record.snapshot.status = JobStatus::Cancelled;
            }
            Err(ExecuteError::Failed(message)) => {
                record.snapshot.error = Some(message);
                record.snapshot.status = JobStatus::Failed;
            }
        }
    }
}

#[derive(Debug)]
enum ExecuteError {
    Cancelled {
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    Failed(String),
}

async fn execute(
    root: &std::path::Path,
    argv: Vec<String>,
    timeout_seconds: u64,
) -> Result<CommandResult, ApiError> {
    execute_inner(root, argv, timeout_seconds, None)
        .await
        .map_err(|error| match error {
            ExecuteError::Cancelled { .. } => ApiError::Conflict("command was cancelled".into()),
            ExecuteError::Failed(message) => ApiError::Internal(message),
        })
}

async fn execute_inner(
    root: &std::path::Path,
    argv: Vec<String>,
    timeout_seconds: u64,
    cancel: Option<oneshot::Receiver<()>>,
) -> Result<CommandResult, ExecuteError> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| ExecuteError::Failed(format!("failed to start command: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecuteError::Failed("failed to capture stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecuteError::Failed("failed to capture stderr".into()))?;
    let stdout_task = tokio::spawn(drain_limited(stdout));
    let stderr_task = tokio::spawn(drain_limited(stderr));

    enum EndState {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }

    let end_state = if let Some(mut cancel) = cancel {
        tokio::select! {
            status = child.wait() => EndState::Exited(status.map_err(|error| ExecuteError::Failed(format!("failed waiting for command: {error}")))?),
            _ = time::sleep(Duration::from_secs(timeout_seconds)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                EndState::TimedOut
            }
            _ = &mut cancel => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                EndState::Cancelled
            }
        }
    } else {
        tokio::select! {
            status = child.wait() => EndState::Exited(status.map_err(|error| ExecuteError::Failed(format!("failed waiting for command: {error}")))?),
            _ = time::sleep(Duration::from_secs(timeout_seconds)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                EndState::TimedOut
            }
        }
    };

    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|error| ExecuteError::Failed(format!("stdout reader task failed: {error}")))?
        .map_err(|error| ExecuteError::Failed(format!("failed reading stdout: {error}")))?;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|error| ExecuteError::Failed(format!("stderr reader task failed: {error}")))?
        .map_err(|error| ExecuteError::Failed(format!("failed reading stderr: {error}")))?;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    match end_state {
        EndState::Exited(status) => Ok(CommandResult {
            argv,
            exit_code: status.code(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            timed_out: false,
        }),
        EndState::TimedOut => Ok(CommandResult {
            argv,
            exit_code: None,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            timed_out: true,
        }),
        EndState::Cancelled => Err(ExecuteError::Cancelled {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        }),
    }
}

async fn drain_limited<R>(mut reader: R) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len() < MAX_OUTPUT_BYTES {
            let remaining = MAX_OUTPUT_BYTES - output.len();
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

fn command_workspace<'a>(
    state: &'a AppState,
    name: &str,
) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    if !workspace.capabilities.commands {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow command execution"
        )));
    }
    Ok(workspace)
}

fn validate_argv(argv: &[String]) -> Result<(), ApiError> {
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Err(ApiError::BadRequest("argv must contain a program name".into()));
    }
    if argv.len() > 256 {
        return Err(ApiError::BadRequest("argv contains too many arguments".into()));
    }
    if argv.iter().any(|arg| arg.as_bytes().contains(&0)) {
        return Err(ApiError::BadRequest("argv must not contain NUL bytes".into()));
    }
    Ok(())
}

fn canonical_root(workspace: &WorkspaceConfig) -> Result<std::path::PathBuf, ApiError> {
    std::fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_argv() {
        assert!(validate_argv(&[]).is_err());
    }

    #[test]
    fn rejects_blank_program() {
        assert!(validate_argv(&["   ".into()]).is_err());
    }
}
