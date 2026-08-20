use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Unsupported(String),
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(message)
            | Self::Forbidden(message)
            | Self::NotFound(message)
            | Self::Unsupported(message)
            | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message),
            Self::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", message),
            Self::Unsupported(message) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                message,
            ),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                message,
            ),
        };

        (status, Json(ErrorBody { error: code, message })).into_response()
    }
}
