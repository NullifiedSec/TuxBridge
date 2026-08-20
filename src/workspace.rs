use std::path::Path;

use axum::{
    Json,
    extract::{Path as AxumPath, State},
};
use serde::Serialize;

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

pub async fn list_workspaces(State(state): State<AppState>) -> Json<Vec<WorkspaceSummary>> {
    let workspaces = state
        .config
        .workspaces
        .iter()
        .map(|(name, workspace)| summary(name, workspace))
        .collect();

    Json(workspaces)
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

fn summary(name: &str, workspace: &WorkspaceConfig) -> WorkspaceSummary {
    WorkspaceSummary {
        name: name.to_owned(),
        root: workspace.root.display().to_string(),
        exists: Path::new(&workspace.root).is_dir(),
        capabilities: workspace.capabilities.clone(),
    }
}
