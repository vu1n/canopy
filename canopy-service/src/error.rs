use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
pub use canopy_core::ErrorEnvelope;

#[derive(Debug)]
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

    pub fn repo_not_ready(repo: &str, status: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: ErrorEnvelope::new(
                "repo_not_ready",
                format!("Repo {} is not ready (status: {})", repo, status),
                "Wait for indexing to complete or check /status for details",
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_not_found_has_404_status() {
        let err = AppError::repo_not_found();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.body.code, "repo_not_found");
        assert_eq!(err.body.message, "Repository not found");
    }

    #[test]
    fn handle_not_found_includes_handle_id() {
        let err = AppError::handle_not_found("h_abc123");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.body.code, "handle_not_found");
        assert!(err.body.message.contains("h_abc123"));
    }

    #[test]
    fn stale_has_conflict_status() {
        let err = AppError::stale(5, 3);
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.body.code, "stale_generation");
        assert!(err.body.message.contains("5"));
        assert!(err.body.message.contains("3"));
    }

    #[test]
    fn internal_has_500_status() {
        let err = AppError::internal("something broke");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.body.code, "internal_error");
        assert!(err.body.message.contains("something broke"));
    }

    #[test]
    fn from_canopy_handle_not_found() {
        let canopy_err = canopy_core::CanopyError::HandleNotFound("h_xyz".to_string());
        let app_err = AppError::from(canopy_err);
        assert_eq!(app_err.status, StatusCode::NOT_FOUND);
        assert_eq!(app_err.body.code, "handle_not_found");
        assert!(app_err.body.message.contains("h_xyz"));
    }

    #[test]
    fn from_canopy_stale_generation() {
        let canopy_err = canopy_core::CanopyError::StaleGeneration {
            expected: 10,
            found: 8,
        };
        let app_err = AppError::from(canopy_err);
        assert_eq!(app_err.status, StatusCode::CONFLICT);
        assert_eq!(app_err.body.code, "stale_generation");
    }

    #[test]
    fn from_canopy_other_error_maps_to_internal() {
        let canopy_err = canopy_core::CanopyError::InvalidHandle("bad".to_string());
        let app_err = AppError::from(canopy_err);
        assert_eq!(app_err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(app_err.body.code, "internal_error");
    }

    #[test]
    fn repo_not_ready_has_503_status() {
        let err = AppError::repo_not_ready("my-repo", "Indexing");
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.body.code, "repo_not_ready");
        assert!(err.body.message.contains("my-repo"));
        assert!(err.body.message.contains("Indexing"));
    }

    #[test]
    fn error_envelope_serializes_to_json() {
        let err = AppError::repo_not_found();
        let json = serde_json::to_value(&err.body).unwrap();
        assert_eq!(json["code"], "repo_not_found");
        assert_eq!(json["message"], "Repository not found");
        assert!(json["hint"].as_str().unwrap().contains("/repos/add"));
    }
}
