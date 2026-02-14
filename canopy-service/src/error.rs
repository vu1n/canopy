use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
    pub hint: String,
}

impl ErrorEnvelope {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: hint.into(),
        }
    }

    pub fn stale_generation(expected: u64, found: u64) -> Self {
        Self::new(
            "stale_generation",
            format!("Expected generation {}, found {}", expected, found),
            "Call /reindex to get a fresh generation",
        )
    }

    pub fn internal(msg: &str) -> Self {
        Self::new("internal_error", msg, "Check service logs for details")
    }
}

pub struct AppError {
    pub status: StatusCode,
    pub body: ErrorEnvelope,
}

impl AppError {
    pub fn repo_not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorEnvelope::new(
                "repo_not_found",
                "Repository not found",
                "Register the repo via POST /repos/add first",
            ),
        }
    }

    pub fn handle_not_found(handle_id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorEnvelope::new(
                "handle_not_found",
                format!("Handle {} not found", handle_id),
                "The handle may have been invalidated by a reindex",
            ),
        }
    }

    pub fn stale(expected: u64, found: u64) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            body: ErrorEnvelope::stale_generation(expected, found),
        }
    }

    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorEnvelope::internal(&msg.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(self.body)).into_response()
    }
}

impl From<canopy_core::CanopyError> for AppError {
    fn from(err: canopy_core::CanopyError) -> Self {
        match &err {
            canopy_core::CanopyError::HandleNotFound(id) => AppError::handle_not_found(id),
            canopy_core::CanopyError::StaleGeneration { expected, found } => {
                AppError::stale(*expected, *found)
            }
            _ => AppError::internal(err),
        }
    }
}
