use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct AuthError {
    error: &'static str,
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

    match provided {
        Some(value) if constant_time_eq(value.as_bytes(), state.api_key().as_bytes()) => {
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(AuthError {
                error: "unauthorized",
                message: "missing or invalid bearer token",
            }),
        )
            .into_response(),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    left.iter()
        .zip(right.iter())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
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
