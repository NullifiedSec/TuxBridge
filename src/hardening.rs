use std::time::Instant;

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{security::SecurityProfile, state::AppState};

#[derive(Serialize)]
struct BusyResponse {
    error: &'static str,
    message: &'static str,
}

#[derive(Serialize)]
struct PolicyResponse {
    error: &'static str,
    message: &'static str,
}

pub async fn protect_host(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();

    // In the default profile the only command endpoint is /v1/commands/raw,
    // whose parser rejects shell grammar and enforces the executable allowlist.
    // Blocking the argv/background APIs here prevents them from bypassing that policy.
    if state.config.security.profile == SecurityProfile::Default
        && matches!(path.as_str(), "/v1/commands/run" | "/v1/commands/start")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(PolicyResponse {
                error: "security_profile_restriction",
                message: "default profile permits only constrained raw commands; switch to loose or i_want_to_nuke_my_server for unrestricted argv/background execution",
            }),
        )
            .into_response();
    }

    let permit = match state.request_gate.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(BusyResponse {
                    error: "too_many_requests",
                    message: "TuxBridge is at its configured concurrent-request limit",
                }),
            )
                .into_response();
        }
    };

    let request_id = state.next_request_id();
    let method = request.method().clone();
    let started = Instant::now();

    let mut response = next.run(request).await;
    drop(permit);

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-tuxbridge-request-id", value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );

    eprintln!(
        "request_id={} method={} path={} status={} duration_ms={}",
        request_id,
        method,
        path,
        response.status().as_u16(),
        started.elapsed().as_millis()
    );

    response
}
