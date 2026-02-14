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

    pub fn not_found(what: &str) -> Self {
        Self::new(
            "not_found",
            format!("{} not found", what),
            "Check the repo ID and try again",
        )
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
    pub fn not_found(what: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorEnvelope::not_found(what),
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
            canopy_core::CanopyError::HandleNotFound(_) => AppError::not_found("handle"),
            canopy_core::CanopyError::StaleGeneration { expected, found } => {
                AppError::stale(*expected, *found)
            }
            _ => AppError::internal(err),
        }
    }
}
