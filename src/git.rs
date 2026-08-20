use std::{fs, process::Command};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::WorkspaceConfig, error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct GitRequest {
    workspace: String,
}

#[derive(Debug, Deserialize)]
pub struct LogRequest {
    workspace: String,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct DiffRequest {
    workspace: String,
    #[serde(default)]
    staged: bool,
    path: Option<String>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct GitStatusResponse {
    branch: String,
    head: Option<String>,
    upstream: Option<String>,
    ahead: usize,
    behind: usize,
    clean: bool,
    changes: Vec<GitChange>,
}

#[derive(Debug, Serialize)]
pub struct GitChange {
    index: char,
    worktree: char,
    path: String,
}

#[derive(Debug, Serialize)]
pub struct GitBranch {
    name: String,
    sha: String,
    upstream: Option<String>,
    current: bool,
}

#[derive(Debug, Serialize)]
pub struct GitLogEntry {
    sha: String,
    author: String,
    authored_at: String,
    subject: String,
}

#[derive(Debug, Serialize)]
pub struct GitDiffResponse {
    content: String,
    truncated: bool,
}

pub async fn git_status(
    State(state): State<AppState>,
    Json(request): Json<GitRequest>,
) -> Result<Json<GitStatusResponse>, ApiError> {
    let workspace = git_read_workspace(&state, &request.workspace)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root)?;

    let status = run_git(&root, &["status", "--porcelain=v1", "--branch"])?;
    let mut lines = status.lines();
    let header = lines.next().unwrap_or("## HEAD (no branch)");
    let (branch, upstream, ahead, behind) = parse_branch_header(header);
    let changes = lines
        .filter_map(parse_status_line)
        .collect::<Vec<_>>();
    let head = run_git_optional(&root, &["rev-parse", "HEAD"]);

    Ok(Json(GitStatusResponse {
        branch,
        head,
        upstream,
        ahead,
        behind,
        clean: changes.is_empty(),
        changes,
    }))
}

pub async fn git_branches(
    State(state): State<AppState>,
    Json(request): Json<GitRequest>,
) -> Result<Json<Vec<GitBranch>>, ApiError> {
    let workspace = git_read_workspace(&state, &request.workspace)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root)?;

    let output = run_git(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname:short)%09%(objectname)%09%(upstream:short)%09%(HEAD)",
            "refs/heads",
        ],
    )?;

    let branches = output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let name = parts.next()?.to_owned();
            let sha = parts.next()?.to_owned();
            let upstream = parts.next().filter(|value| !value.is_empty()).map(str::to_owned);
            let current = parts.next().is_some_and(|value| value.trim() == "*");
            Some(GitBranch {
                name,
                sha,
                upstream,
                current,
            })
        })
        .collect();

    Ok(Json(branches))
}

pub async fn git_log(
    State(state): State<AppState>,
    Json(request): Json<LogRequest>,
) -> Result<Json<Vec<GitLogEntry>>, ApiError> {
    let workspace = git_read_workspace(&state, &request.workspace)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root)?;

    let limit = request.limit.unwrap_or(20).clamp(1, 200);
    let output = run_git(
        &root,
        &[
            "log",
            &format!("-{limit}"),
            "--date=iso-strict",
            "--format=%H%x09%an%x09%aI%x09%s",
        ],
    )?;

    let entries = output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            Some(GitLogEntry {
                sha: parts.next()?.to_owned(),
                author: parts.next()?.to_owned(),
                authored_at: parts.next()?.to_owned(),
                subject: parts.next()?.to_owned(),
            })
        })
        .collect();

    Ok(Json(entries))
}

pub async fn git_diff(
    State(state): State<AppState>,
    Json(request): Json<DiffRequest>,
) -> Result<Json<GitDiffResponse>, ApiError> {
    let workspace = git_read_workspace(&state, &request.workspace)?;
    let root = canonical_root(workspace)?;
    ensure_git_repo(&root)?;

    let max_bytes = request.max_bytes.unwrap_or(1_048_576).clamp(1, 8_388_608);
    let mut args = vec!["diff", "--no-ext-diff", "--no-color"];
    if request.staged {
        args.push("--cached");
    }
    if let Some(path) = request.path.as_deref() {
        if path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(ApiError::BadRequest("diff path must stay within the workspace".into()));
        }
        args.push("--");
        args.push(path);
    }

    let output = run_git_bytes(&root, &args)?;
    let truncated = output.len() > max_bytes;
    let bytes = if truncated {
        &output[..max_bytes]
    } else {
        &output
    };
    let content = String::from_utf8_lossy(bytes).into_owned();

    Ok(Json(GitDiffResponse { content, truncated }))
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

fn canonical_root(workspace: &WorkspaceConfig) -> Result<std::path::PathBuf, ApiError> {
    fs::canonicalize(&workspace.root)
        .map_err(|error| ApiError::Internal(format!("failed to resolve workspace root: {error}")))
}

fn ensure_git_repo(root: &std::path::Path) -> Result<(), ApiError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|error| ApiError::Internal(format!("failed to execute git: {error}")))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        return Err(ApiError::BadRequest("workspace is not a Git work tree".into()));
    }
    Ok(())
}

fn run_git(root: &std::path::Path, args: &[&str]) -> Result<String, ApiError> {
    let output = run_git_bytes(root, args)?;
    String::from_utf8(output)
        .map_err(|_| ApiError::Internal("git returned non-UTF-8 output".into()))
}

fn run_git_bytes(root: &std::path::Path, args: &[&str]) -> Result<Vec<u8>, ApiError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| ApiError::Internal(format!("failed to execute git: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(ApiError::BadRequest(if stderr.is_empty() {
            "git command failed".into()
        } else {
            stderr
        }));
    }
    Ok(output.stdout)
}

fn run_git_optional(root: &std::path::Path, args: &[&str]) -> Option<String> {
    run_git(root, args)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_branch_header(header: &str) -> (String, Option<String>, usize, usize) {
    let raw = header.strip_prefix("## ").unwrap_or(header);
    let (main, tracking) = raw
        .split_once(" [")
        .map_or((raw, None), |(main, tracking)| (main, Some(tracking.trim_end_matches(']'))));
    let (branch, upstream) = main
        .split_once("...")
        .map_or((main.to_owned(), None), |(branch, upstream)| {
            (branch.to_owned(), Some(upstream.to_owned()))
        });

    let mut ahead = 0;
    let mut behind = 0;
    if let Some(tracking) = tracking {
        for part in tracking.split(',').map(str::trim) {
            if let Some(value) = part.strip_prefix("ahead ") {
                ahead = value.parse().unwrap_or(0);
            } else if let Some(value) = part.strip_prefix("behind ") {
                behind = value.parse().unwrap_or(0);
            }
        }
    }

    (branch, upstream, ahead, behind)
}

fn parse_status_line(line: &str) -> Option<GitChange> {
    let bytes = line.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    Some(GitChange {
        index: bytes[0] as char,
        worktree: bytes[1] as char,
        path: line[3..].to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tracking_counts() {
        let (branch, upstream, ahead, behind) =
            parse_branch_header("## main...origin/main [ahead 2, behind 3]");
        assert_eq!(branch, "main");
        assert_eq!(upstream.as_deref(), Some("origin/main"));
        assert_eq!(ahead, 2);
        assert_eq!(behind, 3);
    }

    #[test]
    fn parses_status_change() {
        let change = parse_status_line(" M src/main.rs").unwrap();
        assert_eq!(change.index, ' ');
        assert_eq!(change.worktree, 'M');
        assert_eq!(change.path, "src/main.rs");
    }
}
