use std::{fs, path::{Component, Path}};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct GitActionRequest {
    workspace: String,
}

#[derive(Debug, Deserialize)]
pub struct GitAddRequest {
    workspace: String,
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitCommitRequest {
    workspace: String,
    message: String,
}

#[derive(Debug, Deserialize)]
pub struct GitPushRequest {
    workspace: String,
    remote: String,
    branch: String,
}

#[derive(Debug, Serialize)]
pub struct GitMutationResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

pub async fn git_fetch(
    State(state): State<AppState>,
    Json(request): Json<GitActionRequest>,
) -> Result<Json<GitMutationResponse>, ApiError> {
    let workspace = require_workspace(&state, &request.workspace, false, true)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root).await?;
    run_git(&root, &["fetch", "--all", "--prune"]).await.map(Json)
}

pub async fn git_pull(
    State(state): State<AppState>,
    Json(request): Json<GitActionRequest>,
) -> Result<Json<GitMutationResponse>, ApiError> {
    let workspace = require_workspace(&state, &request.workspace, true, true)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root).await?;
    run_git(&root, &["pull", "--ff-only"]).await.map(Json)
}

pub async fn git_add(
    State(state): State<AppState>,
    Json(request): Json<GitAddRequest>,
) -> Result<Json<GitMutationResponse>, ApiError> {
    if request.paths.is_empty() {
        return Err(ApiError::BadRequest("paths must not be empty".into()));
    }
    for path in &request.paths {
        validate_repo_path(path)?;
    }

    let workspace = require_workspace(&state, &request.workspace, true, false)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root).await?;

    let mut args = vec!["add", "--"];
    args.extend(request.paths.iter().map(String::as_str));
    run_git(&root, &args).await.map(Json)
}

pub async fn git_commit(
    State(state): State<AppState>,
    Json(request): Json<GitCommitRequest>,
) -> Result<Json<GitMutationResponse>, ApiError> {
    if request.message.trim().is_empty() {
        return Err(ApiError::BadRequest("commit message must not be empty".into()));
    }
    if request.message.len() > 16_384 {
        return Err(ApiError::BadRequest("commit message is too large".into()));
    }

    let workspace = require_workspace(&state, &request.workspace, true, false)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root).await?;
    run_git(&root, &["commit", "-m", &request.message])
        .await
        .map(Json)
}

pub async fn git_push(
    State(state): State<AppState>,
    Json(request): Json<GitPushRequest>,
) -> Result<Json<GitMutationResponse>, ApiError> {
    validate_remote(&request.remote)?;
    let workspace = require_workspace(&state, &request.workspace, true, true)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root).await?;
    validate_branch(&root, &request.branch).await?;

    let refspec = format!("{}:refs/heads/{}", request.branch, request.branch);
    run_git(
        &root,
        &["push", "--porcelain", "--", &request.remote, &refspec],
    )
    .await
    .map(Json)
}

fn require_workspace<'a>(
    state: &'a AppState,
    name: &str,
    write: bool,
    network: bool,
) -> Result<&'a WorkspaceConfig, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    if write && !workspace.capabilities.git_write {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow Git writes"
        )));
    }
    if network && !workspace.capabilities.git_network {
        return Err(ApiError::Forbidden(format!(
            "workspace {name:?} does not allow Git network operations"
        )));
    }
    Ok(workspace)
}

fn canonical_root(workspace: &WorkspaceConfig) -> Result<std::path::PathBuf, ApiError> {
    fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))
}

async fn ensure_git_repo(root: &Path) -> Result<(), ApiError> {
    let result = run_git_raw(root, &["rev-parse", "--is-inside-work-tree"]).await?;
    if String::from_utf8_lossy(&result.stdout).trim() == "true" {
        Ok(())
    } else {
        Err(ApiError::BadRequest("workspace is not a Git work tree".into()))
    }
}

async fn validate_branch(root: &Path, branch: &str) -> Result<(), ApiError> {
    if branch.trim().is_empty() || branch.starts_with('-') {
        return Err(ApiError::BadRequest("invalid branch name".into()));
    }
    run_git_raw(root, &["check-ref-format", "--branch", branch])
        .await
        .map(|_| ())
        .map_err(|_| ApiError::BadRequest("invalid branch name".into()))
}

fn validate_remote(remote: &str) -> Result<(), ApiError> {
    if remote.is_empty()
        || remote.starts_with('-')
        || !remote
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(ApiError::BadRequest("invalid remote name".into()));
    }
    Ok(())
}

fn validate_repo_path(raw: &str) -> Result<(), ApiError> {
    if raw.is_empty() {
        return Err(ApiError::BadRequest("Git path must not be empty".into()));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ApiError::BadRequest("Git paths must be relative".into()));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ApiError::BadRequest("Git path traversal is not allowed".into()));
    }
    Ok(())
}

async fn run_git(root: &Path, args: &[&str]) -> Result<GitMutationResponse, ApiError> {
    let output = run_git_raw(root, args).await?;
    Ok(GitMutationResponse {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

async fn run_git_raw(root: &Path, args: &[&str]) -> Result<std::process::Output, ApiError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
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
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_option_like_remote() {
        assert!(validate_remote("--upload-pack=evil").is_err());
    }

    #[test]
    fn accepts_normal_remote() {
        assert!(validate_remote("origin").is_ok());
        assert!(validate_remote("company_upstream-2").is_ok());
    }

    #[test]
    fn rejects_git_path_traversal() {
        assert!(validate_repo_path("../outside").is_err());
    }
}
