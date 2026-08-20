use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{security::SecurityProfile, state::AppState};

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
    request: Request,
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
    if !credential.roles.iter().any(|role| state.role_policy.allows(*role, &method, &path)) {
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

    next.run(request).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(AuthError { error: "unauthorized", message: "missing or invalid bearer token" }),
    ).into_response()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() { return false; }
    left.iter().zip(right.iter()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn equality_requires_identical_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
    }
}
