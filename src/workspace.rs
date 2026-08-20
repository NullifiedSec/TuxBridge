use std::{
    fs,
    path::{Component, Path},
};

use axum::{
    Json,
    extract::{Path as AxumPath, State},
};
use serde::{Deserialize, Serialize};

use crate::{
    config::{Capabilities, WorkspaceConfig},
    error::ApiError,
    state::AppState,
};

#[derive(Serialize)]
pub struct WorkspaceSummary {
    name: String,
    root: String,
    exists: bool,
    capabilities: Capabilities,
}

#[derive(Deserialize)]
pub struct ResolveRequest {
    workspace: String,
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
pub struct ResolveResponse {
    workspace: String,
    requested: String,
    relative: String,
    absolute: String,
    kind: &'static str,
}

pub async fn list_workspaces(State(state): State<AppState>) -> Json<Vec<WorkspaceSummary>> {
    Json(
        state
            .config
            .workspaces
            .iter()
            .map(|(name, workspace)| summary(name, workspace))
            .collect(),
    )
}

pub async fn get_workspace(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<WorkspaceSummary>, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(&name)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {name:?} is not configured")))?;
    Ok(Json(summary(&name, workspace)))
}

pub async fn resolve_path(
    State(state): State<AppState>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(&request.workspace)
        .ok_or_else(|| ApiError::NotFound(format!("workspace {:?} is not configured", request.workspace)))?;
    if !workspace.capabilities.fs_read {
        return Err(ApiError::Forbidden(format!(
            "workspace {:?} does not allow filesystem reads",
            request.workspace
        )));
    }

    let relative = Path::new(&request.path);
    validate_relative(relative)?;
    let root = fs::canonicalize(&workspace.root).map_err(map_io)?;
    let target = fs::canonicalize(root.join(relative)).map_err(map_io)?;
    if !target.starts_with(&root) {
        return Err(ApiError::Forbidden("resolved path escapes workspace root".into()));
    }
    let metadata = fs::symlink_metadata(&target).map_err(map_io)?;
    let kind = if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    };

    Ok(Json(ResolveResponse {
        workspace: request.workspace,
        requested: request.path,
        relative: target
            .strip_prefix(&root)
            .unwrap_or(&target)
            .to_string_lossy()
            .into_owned(),
        absolute: target.display().to_string(),
        kind,
    }))
}

fn summary(name: &str, workspace: &WorkspaceConfig) -> WorkspaceSummary {
    WorkspaceSummary {
        name: name.to_owned(),
        root: workspace.root.display().to_string(),
        exists: workspace.root.is_dir(),
        capabilities: workspace.capabilities.clone(),
    }
}

fn validate_relative(path: &Path) -> Result<(), ApiError> {
    if path.is_absolute() {
        return Err(ApiError::BadRequest("path must be relative to the workspace".into()));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ApiError::BadRequest("path traversal is not allowed".into()));
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
