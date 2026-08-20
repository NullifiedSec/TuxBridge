use std::{fs, path::Path};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct GitExtraRequest {
    workspace: String,
}

#[derive(Debug, Serialize)]
pub struct GitHeadInfo {
    branch: Option<String>,
    head: Option<String>,
    upstream: Option<String>,
    repository_root: String,
}

#[derive(Debug, Serialize)]
pub struct GitRemoteInfo {
    name: String,
    fetch_url: Option<String>,
    push_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GitStashEntry {
    index: usize,
    reference: String,
    subject: String,
}

pub async fn git_head(
    State(state): State<AppState>,
    Json(request): Json<GitExtraRequest>,
) -> Result<Json<GitHeadInfo>, ApiError> {
    let root = git_root(&state, &request.workspace)?;
    ensure_git_repo(&root).await?;
    let branch = optional_git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
    let head = optional_git(&root, &["rev-parse", "HEAD"]).await;
    let upstream = optional_git(&root, &["rev-parse", "--abbrev-ref", "@{upstream}"]).await;
    Ok(Json(GitHeadInfo {
        branch,
        head,
        upstream,
        repository_root: root.display().to_string(),
    }))
}

pub async fn git_remotes(
    State(state): State<AppState>,
    Json(request): Json<GitExtraRequest>,
) -> Result<Json<Vec<GitRemoteInfo>>, ApiError> {
    let root = git_root(&state, &request.workspace)?;
    ensure_git_repo(&root).await?;
    let names = run_git(&root, &["remote"]).await?;
    let mut remotes = Vec::new();
    for name in names.lines().filter(|value| !value.trim().is_empty()).take(128) {
        remotes.push(GitRemoteInfo {
            name: name.to_owned(),
            fetch_url: optional_git(&root, &["remote", "get-url", name]).await,
            push_url: optional_git(&root, &["remote", "get-url", "--push", name]).await,
        });
    }
    Ok(Json(remotes))
}

pub async fn git_stashes(
    State(state): State<AppState>,
    Json(request): Json<GitExtraRequest>,
) -> Result<Json<Vec<GitStashEntry>>, ApiError> {
    let root = git_root(&state, &request.workspace)?;
    ensure_git_repo(&root).await?;
    let output = run_git(
        &root,
        &["stash", "list", "--format=%gd%x09%gs"],
    )
    .await?;
    let entries = output
        .lines()
        .take(200)
        .enumerate()
        .filter_map(|(index, line)| {
            let (reference, subject) = line.split_once('\t')?;
            Some(GitStashEntry {
                index,
                reference: reference.to_owned(),
                subject: subject.to_owned(),
            })
        })
        .collect();
    Ok(Json(entries))
}

fn git_root(state: &AppState, name: &str) -> Result<std::path::PathBuf, ApiError> {
    let workspace = git_read_workspace(state, name)?;
    fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))
}

fn git_read_workspace<'a>(
    state: &'a AppState,
    name: &str,
) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    if !workspace.capabilities.git_read {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow Git reads"
        )));
    }
    Ok(workspace)
}

async fn ensure_git_repo(root: &Path) -> Result<(), ApiError> {
    let result = run_git(root, &["rev-parse", "--is-inside-work-tree"]).await?;
    if result.trim() == "true" {
        Ok(())
    } else {
        Err(ApiError::BadRequest("workspace is not a Git work tree".into()))
    }
}

async fn optional_git(root: &Path, args: &[&str]) -> Option<String> {
    run_git(root, args)
        .await
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

async fn run_git(root: &Path, args: &[&str]) -> Result<String, ApiError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|error| ApiError::Internal(format!("failed to execute git: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ApiError::BadRequest(if stderr.is_empty() {
            "git command failed".into()
        } else {
            stderr
        }));
    }
    let mut stdout = output.stdout;
    stdout.truncate(2_097_152);
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}
