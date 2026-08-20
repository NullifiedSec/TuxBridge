use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", message),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                message,
            ),
        };

        (status, Json(ErrorBody { error: code, message })).into_response()
    }
}
