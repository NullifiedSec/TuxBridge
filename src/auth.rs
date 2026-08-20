use std::sync::Arc;

use axum::{
    Json,
    extract::{Request, State},
    http::{Method, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{config::AuthRole, security::SecurityProfile, state::AppState};

#[derive(Debug, Clone)]
pub struct AuthenticatedPrincipal {
    pub name: Arc<str>,
    pub roles: Arc<[AuthRole]>,
}

#[derive(Serialize)]
struct AuthError { error: &'static str, message: &'static str }
#[derive(Serialize)]
struct PolicyResponse { error: &'static str, message: &'static str }
#[derive(Serialize)]
struct ApprovalRequiredResponse {
    error: &'static str,
    approval_id: String,
    path: String,
    message: &'static str,
}

pub async fn require_api_key(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(provided) = provided else { return unauthorized(); };
    let Some(credential) = state.principals.iter().find(|principal| {
        constant_time_eq(provided.as_bytes(), principal.key.as_bytes())
    }) else { return unauthorized(); };

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if !credential.roles.iter().any(|role| role_allows(*role, &method, &path)) {
        return (
            StatusCode::FORBIDDEN,
            Json(AuthError {
                error: "role_forbidden",
                message: "authenticated principal is not authorized for this operation",
            }),
        ).into_response();
    }

    // Host security policy is intentionally evaluated only after authentication
    // and role authorization, so unauthenticated callers cannot probe or create
    // privileged control-plane state.
    if state.config.security.profile == SecurityProfile::Default
        && matches!(path.as_str(), "/v1/commands/run" | "/v1/commands/start")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(PolicyResponse {
                error: "security_profile_restriction",
                message: "default profile permits only constrained raw commands; switch to loose or i_want_to_nuke_my_server for unrestricted argv/background execution",
            }),
        ).into_response();
    }

    if state.config.approvals.required_paths.iter().any(|required| required == &path)
        && !path.starts_with("/v1/approvals/")
    {
        let supplied = request
            .headers()
            .get("x-tuxbridge-approval-id")
            .and_then(|value| value.to_str().ok());
        let approved = match supplied {
            Some(id) => state.approvals.consume(id, &path).await,
            None => false,
        };
        if !approved {
            let pending = state.approvals.create(&path).await;
            state.events.emit(
                "approval.required",
                None,
                format!("approval required for {path}"),
                serde_json::json!({
                    "approval_id": pending.id.clone(),
                    "path": path.clone(),
                    "principal": credential.name.as_ref(),
                }),
            ).await;
            return (
                StatusCode::PRECONDITION_REQUIRED,
                Json(ApprovalRequiredResponse {
                    error: "approval_required",
                    approval_id: pending.id,
                    path,
                    message: "approve this operation in Mission Control, then retry the request with X-TuxBridge-Approval-Id",
                }),
            ).into_response();
        }
    }

    request.extensions_mut().insert(AuthenticatedPrincipal {
        name: credential.name.clone(),
        roles: credential.roles.clone(),
    });
    next.run(request).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(AuthError { error: "unauthorized", message: "missing or invalid bearer token" }),
    ).into_response()
}

fn role_allows(role: AuthRole, method: &Method, path: &str) -> bool {
    match role {
        AuthRole::Admin => true,
        AuthRole::Developer => developer_allows(method, path),
        AuthRole::Reviewer => reviewer_allows(method, path),
        AuthRole::Operator => operator_allows(method, path),
    }
}

fn developer_allows(method: &Method, path: &str) -> bool {
    if method == Method::GET {
        return matches!(path, "/v1/lsp/servers" | "/v1/code/sessions")
            || session_path(path, "");
    }
    if method != Method::POST { return false; }
    matches!(
        path,
        "/v1/project/inspect"
            | "/v1/code/context"
            | "/v1/code/references"
            | "/v1/code/edit-plan"
            | "/v1/code/map"
            | "/v1/code/structure"
            | "/v1/code/node-at"
            | "/v1/code/replace-node"
            | "/v1/code/verification-plan"
            | "/v1/code/sessions"
            | "/v1/lsp/definition"
            | "/v1/lsp/references"
            | "/v1/lsp/hover"
            | "/v1/lsp/document-symbols"
            | "/v1/lsp/workspace-symbols"
            | "/v1/lsp/diagnostics"
            | "/v1/lsp/rename"
            | "/v1/lsp/format"
            | "/v1/commands/run"
            | "/v1/git/status"
            | "/v1/git/diff"
            | "/v1/git/log"
            | "/v1/git/add"
            | "/v1/git/commit"
            | "/v1/git/push"
    ) || session_path(path, "finalize") || session_path(path, "rollback")
}

fn reviewer_allows(method: &Method, path: &str) -> bool {
    if method == Method::GET {
        return matches!(path, "/v1/lsp/servers" | "/v1/workspaces")
            || path.strip_prefix("/v1/workspaces/")
                .is_some_and(|name| !name.is_empty() && !name.contains('/'));
    }
    method == Method::POST && matches!(
        path,
        "/v1/project/inspect"
            | "/v1/code/context"
            | "/v1/code/references"
            | "/v1/code/map"
            | "/v1/code/structure"
            | "/v1/code/node-at"
            | "/v1/code/verification-plan"
            | "/v1/lsp/definition"
            | "/v1/lsp/references"
            | "/v1/lsp/hover"
            | "/v1/lsp/document-symbols"
            | "/v1/lsp/workspace-symbols"
            | "/v1/lsp/diagnostics"
            | "/v1/git/status"
            | "/v1/git/diff"
            | "/v1/git/log"
            | "/v1/git/branches"
            | "/v1/git/head"
            | "/v1/git/remotes"
    )
}

fn operator_allows(method: &Method, path: &str) -> bool {
    if method == Method::GET {
        return matches!(
            path,
            "/v1/dashboard"
                | "/v1/audit/events"
                | "/v1/events"
                | "/v1/events/stream"
                | "/v1/approvals"
                | "/v1/security/profile"
                | "/v1/system"
                | "/v1/system/tool-versions"
                | "/v1/system/processes"
                | "/v1/system/listeners"
                | "/v1/system/disks"
                | "/v1/doctor"
                | "/v1/workspaces"
                | "/v1/jobs"
        ) || job_path(path);
    }
    if method == Method::DELETE { return job_path(path); }
    if method != Method::POST { return false; }
    matches!(
        path,
        "/v1/fs/list"
            | "/v1/fs/stat"
            | "/v1/fs/read"
            | "/v1/fs/search"
            | "/v1/fs/hash"
            | "/v1/fs/write"
            | "/v1/fs/patch"
            | "/v1/commands/run"
            | "/v1/commands/raw"
            | "/v1/commands/start"
            | "/v1/git/status"
            | "/v1/git/branches"
            | "/v1/git/fetch"
            | "/v1/git/pull"
    ) || approval_path(path, "approve") || approval_path(path, "deny")
}

fn session_path(path: &str, action: &str) -> bool {
    dynamic_action_path(path, "/v1/code/sessions/", action)
}
fn approval_path(path: &str, action: &str) -> bool {
    dynamic_action_path(path, "/v1/approvals/", action)
}
fn dynamic_action_path(path: &str, prefix: &str, action: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else { return false; };
    let mut parts = rest.split('/');
    let Some(id) = parts.next() else { return false; };
    if id.is_empty() { return false; }
    match (parts.next(), parts.next()) {
        (None, None) => action.is_empty(),
        (Some(found), None) => found == action && !action.is_empty(),
        _ => false,
    }
}
fn job_path(path: &str) -> bool {
    path.strip_prefix("/v1/jobs/")
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() { return false; }
    left.iter().zip(right.iter()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn equality_requires_identical_bytes(){assert!(constant_time_eq(b"secret",b"secret"));assert!(!constant_time_eq(b"secret",b"Secret"));assert!(!constant_time_eq(b"secret",b"secret2"));}
    #[test] fn reviewer_cannot_mutate(){assert!(reviewer_allows(&Method::POST,"/v1/git/diff"));assert!(!reviewer_allows(&Method::POST,"/v1/git/commit"));assert!(!reviewer_allows(&Method::POST,"/v1/code/edit-plan"));}
    #[test] fn developer_can_code_but_not_operate_host(){assert!(developer_allows(&Method::POST,"/v1/code/edit-plan"));assert!(developer_allows(&Method::POST,"/v1/git/commit"));assert!(!developer_allows(&Method::GET,"/v1/system/processes"));}
    #[test] fn operator_can_manage_jobs_and_approvals(){assert!(operator_allows(&Method::DELETE,"/v1/jobs/job-1"));assert!(operator_allows(&Method::POST,"/v1/approvals/a-1/approve"));assert!(!operator_allows(&Method::POST,"/v1/lsp/rename"));}
}
