use std::fs;

use axum::{
    extract::{Path as AxumPath, State},
    Json,
};

use crate::{error::ApiError, state::AppState};

use super::*;
use super::support::{
    atomic_write, digest, git_snapshot, map_io, resolve_key, safe_existing, validate_text,
};

pub async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<Json<SessionView>, ApiError> {
    let workspace = state
        .config
        .workspaces
        .get(&request.workspace)
        .ok_or_else(|| ApiError::NotFound("workspace not configured".into()))?;
    if !workspace.capabilities.fs_read {
        return Err(ApiError::Forbidden(
            "workspace does not allow filesystem reads".into(),
        ));
    }
    let title = request.title.unwrap_or_else(|| "Agent coding session".into());
    validate_text("session title", &title, MAX_SESSION_TITLE_BYTES)?;
    let baseline = if workspace.capabilities.git_read {
        git_snapshot(&workspace.root)
    } else {
        None
    };
    let id = state
        .sessions
        .create(&request.workspace, title, baseline)
        .await?;
    state
        .events
        .emit(
            "session.created",
            Some(&request.workspace),
            format!("created coding session {id}"),
            serde_json::json!({"session_id":id.clone(),"session":id.clone()}),
        )
        .await;
    get_session(State(state), AxumPath(id)).await
}

pub async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionView>> {
    Json(state.sessions.views(&state).await)
}

pub async fn get_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError> {
    Ok(Json(state.sessions.view(&state, &id).await?))
}

pub async fn checkpoint(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<TrackFilesRequest>,
) -> Result<Json<SessionView>, ApiError> {
    if request.paths.is_empty() || request.paths.len() > MAX_FILES_PER_SESSION {
        return Err(ApiError::BadRequest(
            "paths must contain 1..128 files".into(),
        ));
    }
    let workspace = state.sessions.workspace_for(&id).await?;
    let workspace_config = state
        .config
        .workspaces
        .get(&workspace)
        .ok_or_else(|| ApiError::NotFound("workspace no longer configured".into()))?;
    if !workspace_config.capabilities.fs_read {
        return Err(ApiError::Forbidden(
            "workspace does not allow reads".into(),
        ));
    }
    let root = fs::canonicalize(&workspace_config.root).map_err(map_io)?;
    state
        .sessions
        .checkpoint(&id, &workspace, &root, &request.paths)
        .await?;
    state
        .events
        .emit(
            "session.checkpointed",
            Some(&workspace),
            format!("checkpointed {} files in {id}", request.paths.len()),
            serde_json::json!({"session_id":id.clone(),"session":id.clone(),"paths":request.paths}),
        )
        .await;
    get_session(State(state), AxumPath(id)).await
}

pub async fn refresh(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<TrackFilesRequest>,
) -> Result<Json<SessionView>, ApiError> {
    let workspace = state.sessions.workspace_for(&id).await?;
    let workspace_config = state
        .config
        .workspaces
        .get(&workspace)
        .ok_or_else(|| ApiError::NotFound("workspace no longer configured".into()))?;
    let root = fs::canonicalize(&workspace_config.root).map_err(map_io)?;
    state
        .sessions
        .refresh(&id, &workspace, &root, &request.paths)
        .await?;
    state
        .events
        .emit(
            "session.updated",
            Some(&workspace),
            format!("updated session hashes for {} files", request.paths.len()),
            serde_json::json!({"session_id":id.clone(),"session":id.clone(),"paths":request.paths}),
        )
        .await;
    get_session(State(state), AxumPath(id)).await
}

pub async fn finalize(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<SessionView>, ApiError> {
    state.sessions.complete(&id, None).await?;
    let view = state.sessions.view(&state, &id).await?;
    state
        .events
        .emit(
            "session.finalized",
            Some(&view.workspace),
            format!("finalized coding session {}", view.session),
            serde_json::json!({"session_id":view.session.clone(),"session":view.session.clone()}),
        )
        .await;
    Ok(Json(view))
}

pub async fn rollback(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RollbackResponse>, ApiError> {
    let (key, workspace, snapshots) = {
        let sessions = state.sessions.inner.lock().await;
        let key = resolve_key(&sessions, &id)?;
        let session = sessions
            .get(&key)
            .ok_or_else(|| ApiError::NotFound(format!("coding session {id:?} not found")))?;
        if !matches!(session.status, SessionStatus::Active) {
            return Err(ApiError::Conflict(
                "only active sessions can roll back".into(),
            ));
        }
        (
            key,
            session.workspace.clone(),
            session
                .files
                .iter()
                .map(|(path, file)| {
                    (
                        path.clone(),
                        file.before_sha256.clone(),
                        file.after_sha256.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    let workspace_config = state
        .config
        .workspaces
        .get(&workspace)
        .ok_or_else(|| ApiError::NotFound("workspace no longer configured".into()))?;
    if !workspace_config.capabilities.fs_write {
        return Err(ApiError::Forbidden(
            "workspace does not allow filesystem writes".into(),
        ));
    }
    let root = fs::canonicalize(&workspace_config.root).map_err(map_io)?;
    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    for (relative, before_sha, after_sha) in snapshots {
        let path = safe_existing(&root, &relative)?;
        let current = fs::read(&path).map_err(map_io)?;
        if digest(&current) != after_sha {
            skipped.push(relative);
            continue;
        }
        let before = state.sessions.read_snapshot(&key, &before_sha)?;
        let permissions = fs::metadata(&path).map_err(map_io)?.permissions();
        prepared.push((relative, path, before, permissions));
    }
    if !skipped.is_empty() {
        return Err(ApiError::Conflict(format!(
            "rollback refused because {} session files changed after the agent wrote them: {}",
            skipped.len(),
            skipped.join(", ")
        )));
    }
    let mut restored = Vec::new();
    for (relative, path, before, permissions) in prepared {
        atomic_write(&path, &before, permissions)?;
        restored.push(relative);
    }
    state.sessions.mark_rolled_back(&key).await?;
    state
        .events
        .emit(
            "session.rolled_back",
            Some(&workspace),
            format!("rolled back coding session {key}"),
            serde_json::json!({"session_id":key.clone(),"session":key.clone(),"restored":restored.clone()}),
        )
        .await;
    Ok(Json(RollbackResponse {
        id: key.clone(),
        session: key,
        restored,
        skipped: Vec::new(),
    }))
}
