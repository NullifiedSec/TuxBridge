use std::{fs, path::Path, process::Stdio, time::Duration};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time};

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

const MAX_PROCESSES: usize = 512;
const DEFAULT_TREE_DEPTH: usize = 4;
const HARD_TREE_DEPTH: usize = 12;
const DEFAULT_TREE_ENTRIES: usize = 20_000;
const HARD_TREE_ENTRIES: usize = 100_000;

#[derive(Debug, Serialize)]
pub struct ToolVersion {
    name: &'static str,
    available: bool,
    version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProcessInfo {
    pid: u32,
    name: String,
    state: Option<String>,
    uid: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ListenerInfo {
    protocol: String,
    local: String,
    peer: String,
    state: String,
}

#[derive(Debug, Serialize)]
pub struct DiskUsage {
    filesystem: String,
    bytes_total: u64,
    bytes_used: u64,
    bytes_available: u64,
    capacity: String,
    mountpoint: String,
}

#[derive(Debug, Deserialize)]
pub struct TreeSummaryRequest {
    workspace: String,
    max_depth: Option<usize>,
    max_entries: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct TreeSummary {
    workspace: String,
    files: u64,
    directories: u64,
    symlinks: u64,
    other: u64,
    bytes: u64,
    entries_scanned: usize,
    max_depth: usize,
    truncated: bool,
}

pub async fn tool_versions() -> Json<Vec<ToolVersion>> {
    let specs: [(&str, &[&str]); 10] = [
        ("git", &["--version"]),
        ("rustc", &["--version"]),
        ("cargo", &["--version"]),
        ("node", &["--version"]),
        ("npm", &["--version"]),
        ("bun", &["--version"]),
        ("go", &["version"]),
        ("python3", &["--version"]),
        ("php", &["--version"]),
        ("docker", &["--version"]),
    ];

    let mut results = Vec::with_capacity(specs.len());
    for (name, args) in specs {
        results.push(probe_version(name, args).await);
    }
    Json(results)
}

pub async fn processes() -> Result<Json<Vec<ProcessInfo>>, ApiError> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(pid) = name.parse::<u32>() {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.truncate(MAX_PROCESSES);

    let mut results = Vec::with_capacity(pids.len());
    for pid in pids {
        let base = format!("/proc/{pid}");
        let name = fs::read_to_string(format!("{base}/comm"))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".into());
        let status = fs::read_to_string(format!("{base}/status")).ok();
        let state = status.as_deref().and_then(|raw| status_field(raw, "State:"));
        let uid = status
            .as_deref()
            .and_then(|raw| status_field(raw, "Uid:"))
            .and_then(|value| value.split_whitespace().next()?.parse().ok());
        results.push(ProcessInfo {
            pid,
            name,
            state,
            uid,
        });
    }

    Ok(Json(results))
}

pub async fn listeners() -> Result<Json<Vec<ListenerInfo>>, ApiError> {
    let output = run_bounded("ss", &["-H", "-lnt"], 2).await?;
    let mut listeners = Vec::new();
    for line in output.lines().take(2048) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 5 {
            continue;
        }
        listeners.push(ListenerInfo {
            protocol: "tcp".into(),
            state: fields[0].to_owned(),
            local: fields[3].to_owned(),
            peer: fields[4].to_owned(),
        });
    }
    Ok(Json(listeners))
}

pub async fn disks() -> Result<Json<Vec<DiskUsage>>, ApiError> {
    let output = run_bounded("df", &["-P", "-B1"], 3).await?;
    let mut disks = Vec::new();
    for line in output.lines().skip(1).take(512) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 {
            continue;
        }
        let (Ok(total), Ok(used), Ok(available)) = (
            fields[1].parse::<u64>(),
            fields[2].parse::<u64>(),
            fields[3].parse::<u64>(),
        ) else {
            continue;
        };
        disks.push(DiskUsage {
            filesystem: fields[0].to_owned(),
            bytes_total: total,
            bytes_used: used,
            bytes_available: available,
            capacity: fields[4].to_owned(),
            mountpoint: fields[5..].join(" "),
        });
    }
    Ok(Json(disks))
}

pub async fn workspace_tree_summary(
    State(state): State<AppState>,
    Json(request): Json<TreeSummaryRequest>,
) -> Result<Json<TreeSummary>, ApiError> {
    let workspace = readable_workspace(&state, &request.workspace)?;
    let root = fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))?;
    let max_depth = request
        .max_depth
        .unwrap_or(DEFAULT_TREE_DEPTH)
        .clamp(0, HARD_TREE_DEPTH);
    let max_entries = request
        .max_entries
        .unwrap_or(DEFAULT_TREE_ENTRIES)
        .clamp(1, HARD_TREE_ENTRIES);

    let mut summary = TreeSummary {
        workspace: request.workspace,
        files: 0,
        directories: 0,
        symlinks: 0,
        other: 0,
        bytes: 0,
        entries_scanned: 0,
        max_depth,
        truncated: false,
    };
    walk_tree(&root, 0, max_depth, max_entries, &mut summary)?;
    Ok(Json(summary))
}

async fn probe_version(name: &'static str, args: &[&str]) -> ToolVersion {
    match run_bounded(name, args, 2).await {
        Ok(output) => ToolVersion {
            name,
            available: true,
            version: output.lines().next().map(str::to_owned),
        },
        Err(_) => ToolVersion {
            name,
            available: false,
            version: None,
        },
    }
}

async fn run_bounded(program: &str, args: &[&str], seconds: u64) -> Result<String, ApiError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = time::timeout(Duration::from_secs(seconds), command.output())
        .await
        .map_err(|_| ApiError::BadRequest(format!("{program} timed out")))?
        .map_err(|error| ApiError::NotFound(format!("failed to execute {program}: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ApiError::BadRequest(if stderr.is_empty() {
            format!("{program} failed")
        } else {
            stderr
        }));
    }
    let mut bytes = output.stdout;
    bytes.truncate(1_048_576);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn status_field(raw: &str, key: &str) -> Option<String> {
    raw.lines()
        .find_map(|line| line.strip_prefix(key).map(str::trim))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn readable_workspace<'a>(
    state: &'a AppState,
    name: &str,
) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    if !workspace.capabilities.fs_read {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow filesystem reads"
        )));
    }
    Ok(workspace)
}

fn walk_tree(
    path: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    summary: &mut TreeSummary,
) -> Result<(), ApiError> {
    if depth > max_depth || summary.entries_scanned >= max_entries {
        summary.truncated = true;
        return Ok(());
    }

    for entry in fs::read_dir(path).map_err(map_io)? {
        if summary.entries_scanned >= max_entries {
            summary.truncated = true;
            break;
        }
        let entry = entry.map_err(map_io)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(map_io)?;
        summary.entries_scanned += 1;
        if metadata.file_type().is_symlink() {
            summary.symlinks += 1;
        } else if metadata.is_file() {
            summary.files += 1;
            summary.bytes = summary.bytes.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            summary.directories += 1;
            if depth < max_depth {
                walk_tree(&entry.path(), depth + 1, max_depth, max_entries, summary)?;
            }
        } else {
            summary.other += 1;
        }
    }
    Ok(())
}

fn map_io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound(error.to_string()),
        std::io::ErrorKind::PermissionDenied => ApiError::Forbidden(error.to_string()),
        _ => ApiError::Internal(error.to_string()),
    }
}
