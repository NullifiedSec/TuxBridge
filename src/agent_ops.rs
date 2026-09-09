use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::{
    code_tools, command, error::ApiError, fs, git, git_mutation,
    operations::{OperationRecord, OperationStatus}, state::AppState, verification,
};

#[derive(Debug, Serialize)]
pub struct AgentToolResponse<T: Serialize> {
    session: String,
    result: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseFilesRequest {
    session: String,
    #[serde(default)]
    path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadCodeRequest {
    session: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    context_before: Option<usize>,
    context_after: Option<usize>,
    max_bytes: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchCodeRequest {
    session: String,
    #[serde(default)]
    path: String,
    query: String,
    max_results: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectCodeRequest {
    session: String,
    path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditCodeRequest {
    session: String,
    files: Vec<code_tools::FileEditPlan>,
    #[serde(default)]
    dry_run: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanVerificationRequest {
    session: String,
    #[serde(default)]
    changed_paths: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandRequest {
    session: String,
    argv: Vec<String>,
    timeout_seconds: Option<u64>,
    #[serde(default)]
    background: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOnlyRequest {
    session: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffRequest {
    session: String,
    #[serde(default)]
    staged: bool,
    path: Option<String>,
    max_bytes: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStageRequest {
    session: String,
    paths: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentGitCommitRequest {
    session: String,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GitSyncRequest {
    Fetch {
        session: String,
    },
    Pull {
        session: String,
    },
    Push {
        session: String,
        remote: String,
        branch: String,
    },
}

async fn context(
    state: &AppState,
    reference: &str,
    tool: &str,
) -> Result<(String, String), ApiError> {
    let (session, workspace) = state.sessions.active_context(reference).await?;
    state
        .events
        .emit(
            "agent.tool.started",
            Some(&workspace),
            format!("{tool} started in {session}"),
            serde_json::json!({"session":session.clone(),"tool":tool}),
        )
        .await;
    Ok((session, workspace))
}

async fn finish<T: Serialize>(
    state: &AppState,
    session: String,
    workspace: &str,
    tool: &str,
    result: Result<Json<T>, ApiError>,
) -> Result<Json<AgentToolResponse<T>>, ApiError> {
    match result {
        Ok(Json(value)) => {
            state
                .events
                .emit(
                    "agent.tool.completed",
                    Some(workspace),
                    format!("{tool} completed in {session}"),
                    serde_json::json!({"session":session.clone(),"tool":tool}),
                )
                .await;
            Ok(Json(AgentToolResponse {
                session,
                result: value,
            }))
        }
        Err(error) => {
            state
                .events
                .emit(
                    "agent.tool.failed",
                    Some(workspace),
                    format!("{tool} failed in {session}"),
                    serde_json::json!({"session":session,"tool":tool,"error":error.to_string()}),
                )
                .await;
            Err(error)
        }
    }
}

async fn submit_operation<T: Serialize>(
    state: &AppState,
    reference: &str,
    tool: &str,
    request: &T,
) -> Result<Json<OperationRecord>, ApiError> {
    let (session, workspace) = state.sessions.active_context(reference).await?;
    let args = serde_json::to_value(request)
        .map_err(|e| ApiError::Internal(format!("failed to freeze operation arguments: {e}")))?;
    let policy = state.config.approvals.agent_policy(tool);
    let record = state.operations.create(
        session.clone(), workspace.clone(), tool.into(), args, policy,
    ).await?;
    state.events.emit(
        "operation.requested", Some(&workspace), format!("{tool} requested in {session}"),
        serde_json::json!({"session":session,"operation_id":record.id.clone(),"tool":tool,"policy":policy,"status":record.status}),
    ).await;
    if record.status != OperationStatus::Executing {
        return Ok(Json(record));
    }
    let outcome = execute_frozen(state, &record).await;
    let final_record = state.operations.finish(&record.id, outcome).await?;
    state.events.emit(
        "operation.finished", Some(&workspace), format!("{tool} finished as {:?}", final_record.status),
        serde_json::json!({"session":final_record.session.clone(),"operation_id":final_record.id.clone(),"tool":tool,"status":final_record.status}),
    ).await;
    Ok(Json(final_record))
}
pub async fn execute_frozen(
    state: &AppState,
    operation: &OperationRecord,
) -> Result<serde_json::Value, ApiError> {
    match operation.tool.as_str() {
        "editCode" => {
            let req: EditCodeRequest = serde_json::from_value(operation.args.clone())
                .map_err(|e| ApiError::Internal(format!("invalid frozen editCode args: {e}")))?;
            let result = code_tools::code_edit_plan(
                State(state.clone()),
                Json(code_tools::CodeEditPlanRequest {
                    workspace: operation.workspace.clone(),
                    files: req.files,
                    session_id: Some(operation.session.clone()),
                    dry_run: req.dry_run,
                }),
            ).await?;
            serde_json::to_value(result.0).map_err(|e| ApiError::Internal(e.to_string()))
        }
        "runCommand" => execute_frozen_command(state, operation).await,
        "gitStage" => execute_frozen_stage(state, operation).await,
        "gitCommit" => execute_frozen_commit(state, operation).await,
        "gitSync.fetch" | "gitSync.pull" | "gitSync.push" => {
            execute_frozen_sync(state, operation).await
        }
        tool => Err(ApiError::Internal(format!("unsupported frozen operation tool {tool:?}"))),
    }
}
async fn execute_frozen_command(
    state: &AppState,
    operation: &OperationRecord,
) -> Result<serde_json::Value, ApiError> {
    let req: RunCommandRequest = serde_json::from_value(operation.args.clone())
        .map_err(|e| ApiError::Internal(format!("invalid frozen runCommand args: {e}")))?;
    let request = command::CommandRequest {
        workspace: operation.workspace.clone(),
        argv: req.argv,
        timeout_seconds: req.timeout_seconds,
    };
    if req.background {
        let Json(value) = command::start_command(State(state.clone()), Json(request)).await?;
        serde_json::to_value(value).map_err(|e| ApiError::Internal(e.to_string()))
    } else {
        let Json(value) = command::run_command(State(state.clone()), Json(request)).await?;
        serde_json::to_value(value).map_err(|e| ApiError::Internal(e.to_string()))
    }
}

async fn execute_frozen_stage(
    state: &AppState,
    operation: &OperationRecord,
) -> Result<serde_json::Value, ApiError> {
    let req: GitStageRequest = serde_json::from_value(operation.args.clone())
        .map_err(|e| ApiError::Internal(format!("invalid frozen gitStage args: {e}")))?;
    let Json(value) = git_mutation::git_add(
        State(state.clone()),
        Json(git_mutation::GitAddRequest { workspace: operation.workspace.clone(), paths: req.paths }),
    ).await?;
    serde_json::to_value(value).map_err(|e| ApiError::Internal(e.to_string()))
}
async fn execute_frozen_commit(
    state: &AppState,
    operation: &OperationRecord,
) -> Result<serde_json::Value, ApiError> {
    let req: AgentGitCommitRequest = serde_json::from_value(operation.args.clone())
        .map_err(|e| ApiError::Internal(format!("invalid frozen gitCommit args: {e}")))?;
    let Json(value) = git_mutation::git_commit(
        State(state.clone()),
        Json(git_mutation::GitCommitRequest {
            workspace: operation.workspace.clone(),
            message: req.message,
        }),
    ).await?;
    serde_json::to_value(value).map_err(|e| ApiError::Internal(e.to_string()))
}

async fn execute_frozen_sync(
    state: &AppState,
    operation: &OperationRecord,
) -> Result<serde_json::Value, ApiError> {
    let req: GitSyncRequest = serde_json::from_value(operation.args.clone())
        .map_err(|e| ApiError::Internal(format!("invalid frozen gitSync args: {e}")))?;
    let result = match req {
        GitSyncRequest::Fetch { .. } => git_mutation::git_fetch(
            State(state.clone()), Json(git_mutation::GitActionRequest { workspace: operation.workspace.clone() }),
        ).await?,
        GitSyncRequest::Pull { .. } => git_mutation::git_pull(
            State(state.clone()), Json(git_mutation::GitActionRequest { workspace: operation.workspace.clone() }),
        ).await?,
        GitSyncRequest::Push { remote, branch, .. } => git_mutation::git_push(
            State(state.clone()), Json(git_mutation::GitPushRequest { workspace: operation.workspace.clone(), remote, branch }),
        ).await?,
    };
    serde_json::to_value(result.0).map_err(|e| ApiError::Internal(e.to_string()))
}

pub async fn browse_files(
    State(state): State<AppState>,
    Json(req): Json<BrowseFilesRequest>,
) -> Result<Json<AgentToolResponse<Vec<fs::DirectoryEntry>>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "browseFiles").await?;
    let result = fs::list_directory(
        State(state.clone()),
        Json(fs::PathRequest {
            workspace: workspace.clone(),
            path: req.path,
        }),
    )
    .await;
    finish(&state, session, &workspace, "browseFiles", result).await
}

pub async fn read_code(
    State(state): State<AppState>,
    Json(req): Json<ReadCodeRequest>,
) -> Result<Json<AgentToolResponse<code_tools::CodeContextResponse>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "readCode").await?;
    let result = code_tools::code_context(
        State(state.clone()),
        Json(code_tools::CodeContextRequest {
            workspace: workspace.clone(),
            path: req.path,
            start_line: req.start_line,
            end_line: req.end_line,
            context_before: req.context_before,
            context_after: req.context_after,
            max_bytes: req.max_bytes,
        }),
    )
    .await;
    finish(&state, session, &workspace, "readCode", result).await
}

pub async fn search_code(
    State(state): State<AppState>,
    Json(req): Json<SearchCodeRequest>,
) -> Result<Json<AgentToolResponse<fs::SearchResponse>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "searchCode").await?;
    let result = fs::search_files(
        State(state.clone()),
        Json(fs::SearchRequest {
            workspace: workspace.clone(),
            path: req.path,
            query: req.query,
            max_results: req.max_results,
        }),
    )
    .await;
    finish(&state, session, &workspace, "searchCode", result).await
}

pub async fn inspect_code(
    State(state): State<AppState>,
    Json(req): Json<InspectCodeRequest>,
) -> Result<Json<AgentToolResponse<code_tools::CodeSymbolsResponse>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "inspectCode").await?;
    let result = code_tools::code_symbols(
        State(state.clone()),
        Json(code_tools::CodeSymbolsRequest {
            workspace: workspace.clone(),
            path: req.path,
        }),
    )
    .await;
    finish(&state, session, &workspace, "inspectCode", result).await
}

pub async fn edit_code(
    State(state): State<AppState>,
    Json(req): Json<EditCodeRequest>,
) -> Result<Json<OperationRecord>, ApiError> {
    let session = req.session.clone();
    submit_operation(&state, &session, "editCode", &req).await
}

pub async fn plan_verification(
    State(state): State<AppState>,
    Json(req): Json<PlanVerificationRequest>,
) -> Result<Json<AgentToolResponse<verification::VerificationPlan>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "planVerification").await?;
    let result = verification::verification_plan(
        State(state.clone()),
        Json(verification::VerificationRequest {
            workspace: workspace.clone(),
            changed_paths: req.changed_paths,
        }),
    )
    .await;
    finish(&state, session, &workspace, "planVerification", result).await
}

pub async fn run_command(
    State(state): State<AppState>,
    Json(req): Json<RunCommandRequest>,
) -> Result<Json<OperationRecord>, ApiError> {
    let session = req.session.clone();
    submit_operation(&state, &session, "runCommand", &req).await
}

pub async fn git_status(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> Result<Json<AgentToolResponse<git::GitStatusResponse>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "gitStatus").await?;
    let result = git::git_status(
        State(state.clone()),
        Json(git::GitRequest {
            workspace: workspace.clone(),
        }),
    )
    .await;
    finish(&state, session, &workspace, "gitStatus", result).await
}

pub async fn git_diff(
    State(state): State<AppState>,
    Json(req): Json<GitDiffRequest>,
) -> Result<Json<AgentToolResponse<git::GitDiffResponse>>, ApiError> {
    let (session, workspace) = context(&state, &req.session, "gitDiff").await?;
    let result = git::git_diff(
        State(state.clone()),
        Json(git::DiffRequest {
            workspace: workspace.clone(),
            staged: req.staged,
            path: req.path,
            max_bytes: req.max_bytes,
        }),
    )
    .await;
    finish(&state, session, &workspace, "gitDiff", result).await
}

pub async fn git_stage(
    State(state): State<AppState>,
    Json(req): Json<GitStageRequest>,
) -> Result<Json<OperationRecord>, ApiError> {
    let session = req.session.clone();
    submit_operation(&state, &session, "gitStage", &req).await
}

pub async fn git_commit(
    State(state): State<AppState>,
    Json(req): Json<AgentGitCommitRequest>,
) -> Result<Json<OperationRecord>, ApiError> {
    let session = req.session.clone();
    submit_operation(&state, &session, "gitCommit", &req).await
}

pub async fn git_sync(
    State(state): State<AppState>,
    Json(req): Json<GitSyncRequest>,
) -> Result<Json<OperationRecord>, ApiError> {
    let (session, tool) = match &req {
        GitSyncRequest::Fetch { session } => (session.clone(), "gitSync.fetch"),
        GitSyncRequest::Pull { session } => (session.clone(), "gitSync.pull"),
        GitSyncRequest::Push { session, .. } => (session.clone(), "gitSync.push"),
    };
    submit_operation(&state, &session, tool, &req).await
}
