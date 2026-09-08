use axum::{extract::State, Json};

use crate::{error::ApiError, state::AppState};

use super::*;
use super::support::{git_snapshot, validate_text};

pub async fn find_workspaces(
    State(state): State<AppState>,
    Json(request): Json<FindWorkspacesRequest>,
) -> Json<FindWorkspacesResponse> {
    let query = request
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_ascii_lowercase);
    let workspaces = state
        .config
        .workspaces
        .iter()
        .filter(|(name, workspace)| {
            query.as_ref().map_or(true, |query| {
                name.to_ascii_lowercase().contains(query)
                    || workspace
                        .root
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(query)
            })
        })
        .map(|(name, workspace)| AgentWorkspaceSummary {
            id: name.clone(),
            name: name.clone(),
            root: workspace.root.display().to_string(),
            exists: workspace.root.is_dir(),
            capabilities: workspace.capabilities.clone(),
            git: if workspace.capabilities.git_read {
                git_snapshot(&workspace.root)
            } else {
                None
            },
        })
        .collect();
    Json(FindWorkspacesResponse { workspaces })
}

pub async fn select_workspace(
    State(state): State<AppState>,
    Json(request): Json<SelectWorkspaceRequest>,
) -> Result<Json<SessionView>, ApiError> {
    validate_text(
        "session title",
        &request.session_title,
        MAX_SESSION_TITLE_BYTES,
    )?;
    let workspace = state
        .config
        .workspaces
        .get(&request.workspace)
        .ok_or_else(|| {
            ApiError::NotFound(format!("workspace {:?} is not configured", request.workspace))
        })?;
    if !workspace.root.is_dir() {
        return Err(ApiError::NotFound(format!(
            "workspace root {} does not exist",
            workspace.root.display()
        )));
    }
    let baseline = if workspace.capabilities.git_read {
        git_snapshot(&workspace.root)
    } else {
        None
    };
    let session = state
        .sessions
        .create(&request.workspace, request.session_title, baseline)
        .await?;
    state
        .events
        .emit(
            "session.created",
            Some(&request.workspace),
            format!("created agent session {session}"),
            serde_json::json!({"session":session.clone()}),
        )
        .await;
    Ok(Json(state.sessions.view(&state, &session).await?))
}

pub async fn choose_session(
    State(state): State<AppState>,
    Json(request): Json<ChooseSessionRequest>,
) -> Result<Json<SessionView>, ApiError> {
    let view = state.sessions.view(&state, &request.session).await?;
    state
        .events
        .emit(
            "session.resumed",
            Some(&view.workspace),
            format!("resumed agent session {}", view.session),
            serde_json::json!({"session":view.session.clone()}),
        )
        .await;
    Ok(Json(view))
}

pub async fn set_summary(
    State(state): State<AppState>,
    Json(request): Json<SetSummaryRequest>,
) -> Result<Json<SessionView>, ApiError> {
    state
        .sessions
        .set_summary(&request.session, request.summary)
        .await?;
    let view = state.sessions.view(&state, &request.session).await?;
    state
        .events
        .emit(
            "session.summary.updated",
            Some(&view.workspace),
            format!("updated summary for {}", view.session),
            serde_json::json!({"session":view.session.clone()}),
        )
        .await;
    Ok(Json(view))
}

pub async fn set_plan(
    State(state): State<AppState>,
    Json(request): Json<SetPlanRequest>,
) -> Result<Json<SessionView>, ApiError> {
    state.sessions.set_plan(&request.session, request.plan).await?;
    let view = state.sessions.view(&state, &request.session).await?;
    state
        .events
        .emit(
            "session.plan.updated",
            Some(&view.workspace),
            format!("updated plan for {}", view.session),
            serde_json::json!({"session":view.session.clone()}),
        )
        .await;
    Ok(Json(view))
}

pub async fn todo(
    State(state): State<AppState>,
    Json(request): Json<TodoRequest>,
) -> Result<Json<TodoResponse>, ApiError> {
    let changed = !matches!(&request, TodoRequest::List { .. });
    let response = state.sessions.todos(request).await?;
    if changed {
        state
            .events
            .emit(
                "session.todos.updated",
                None,
                format!("updated todos for {}", response.session),
                serde_json::json!({"session":response.session.clone(),"count":response.todos.len()}),
            )
            .await;
    }
    Ok(Json(response))
}

pub async fn complete_session(
    State(state): State<AppState>,
    Json(request): Json<CompleteSessionRequest>,
) -> Result<Json<SessionView>, ApiError> {
    state
        .sessions
        .complete(&request.session, request.result_summary)
        .await?;
    let view = state.sessions.view(&state, &request.session).await?;
    state
        .events
        .emit(
            "session.finalized",
            Some(&view.workspace),
            format!("finalized coding session {}", view.session),
            serde_json::json!({"session":view.session.clone()}),
        )
        .await;
    Ok(Json(view))
}
