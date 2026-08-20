use std::time::Instant;

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{audit::{AuditEvent, unix_millis}, state::AppState};

#[derive(Serialize)]
struct BusyResponse { error: &'static str, message: &'static str }

pub async fn protect_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let permit = match state.request_gate.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(BusyResponse {
                error: "too_many_requests",
                message: "TuxBridge is at its configured concurrent-request limit",
            }),
        ).into_response(),
    };

    let request_id = state.next_request_id();
    let sequence = u64::from_str_radix(request_id.trim_start_matches("tb-"), 16).unwrap_or(0);
    let method = request.method().clone();
    let started = Instant::now();
    if path != "/v1/events/stream" {
        state.events.emit(
            "request.started",
            None,
            format!("{} {}", method, path),
            serde_json::json!({
                "request_id": request_id.clone(),
                "method": method.to_string(),
                "path": path.clone()
            }),
        ).await;
    }

    let mut response = next.run(request).await;
    drop(permit);

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-tuxbridge-request-id", value);
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));

    let duration_ms = started.elapsed().as_millis();
    let status = response.status().as_u16();
    eprintln!("request_id={} method={} path={} status={} duration_ms={}", request_id, method, path, status, duration_ms);

    if path != "/v1/audit/events" && path != "/v1/events" && path != "/v1/events/stream" && path != "/ui" {
        state.audit.push(AuditEvent {
            sequence,
            timestamp_unix_ms: unix_millis(),
            request_id: request_id.clone(),
            method: method.to_string(),
            path: path.clone(),
            status,
            duration_ms,
        }).await;
    }
    if path != "/v1/events/stream" {
        state.events.emit(
            "request.finished",
            None,
            format!("{} {} -> {}", method, path, status),
            serde_json::json!({
                "request_id": request_id,
                "method": method.to_string(),
                "path": path,
                "status": status,
                "duration_ms": duration_ms
            }),
        ).await;
    }
    response
}
