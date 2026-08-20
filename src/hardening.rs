use std::time::Instant;

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{
    audit::{AuditEvent, unix_millis},
    security::SecurityProfile,
    state::AppState,
};

#[derive(Serialize)]
struct BusyResponse { error: &'static str, message: &'static str }
#[derive(Serialize)]
struct PolicyResponse { error: &'static str, message: &'static str }
#[derive(Serialize)]
struct ApprovalRequiredResponse { error: &'static str, approval_id: String, path: String, message: &'static str }

pub async fn protect_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();

    if state.config.security.profile == SecurityProfile::Default
        && matches!(path.as_str(), "/v1/commands/run" | "/v1/commands/start")
    {
        return (StatusCode::FORBIDDEN, Json(PolicyResponse { error:"security_profile_restriction", message:"default profile permits only constrained raw commands; switch to loose or i_want_to_nuke_my_server for unrestricted argv/background execution" })).into_response();
    }

    if state.config.approvals.required_paths.iter().any(|required| required == &path)
        && !path.starts_with("/v1/approvals/")
    {
        let supplied = request.headers().get("x-tuxbridge-approval-id").and_then(|v| v.to_str().ok());
        let approved = match supplied { Some(id) => state.approvals.consume(id, &path).await, None => false };
        if !approved {
            let pending = state.approvals.create(&path).await;
            state.events.emit("approval.required", None, format!("approval required for {path}"), serde_json::json!({"approval_id":pending.id.clone(),"path":path.clone()})).await;
            return (StatusCode::PRECONDITION_REQUIRED, Json(ApprovalRequiredResponse { error:"approval_required", approval_id:pending.id, path, message:"approve this operation in Mission Control, then retry the request with X-TuxBridge-Approval-Id" })).into_response();
        }
    }

    let permit = match state.request_gate.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return (StatusCode::TOO_MANY_REQUESTS, Json(BusyResponse { error:"too_many_requests", message:"TuxBridge is at its configured concurrent-request limit" })).into_response(),
    };

    let request_id = state.next_request_id();
    let sequence = u64::from_str_radix(request_id.trim_start_matches("tb-"), 16).unwrap_or(0);
    let method = request.method().clone();
    let started = Instant::now();
    if path != "/v1/events/stream" {
        state.events.emit("request.started", None, format!("{} {}", method, path), serde_json::json!({"request_id":request_id.clone(),"method":method.to_string(),"path":path.clone()})).await;
    }

    let mut response = next.run(request).await;
    drop(permit);

    if let Ok(value) = HeaderValue::from_str(&request_id) { response.headers_mut().insert("x-tuxbridge-request-id", value); }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));

    let duration_ms = started.elapsed().as_millis();
    let status = response.status().as_u16();
    eprintln!("request_id={} method={} path={} status={} duration_ms={}", request_id, method, path, status, duration_ms);

    if path != "/v1/audit/events" && path != "/v1/events" && path != "/v1/events/stream" && path != "/ui" {
        state.audit.push(AuditEvent { sequence, timestamp_unix_ms:unix_millis(), request_id:request_id.clone(), method:method.to_string(), path:path.clone(), status, duration_ms }).await;
    }
    if path != "/v1/events/stream" {
        state.events.emit("request.finished", None, format!("{} {} -> {}", method, path, status), serde_json::json!({"request_id":request_id,"method":method.to_string(),"path":path,"status":status,"duration_ms":duration_ms})).await;
    }
    response
}
