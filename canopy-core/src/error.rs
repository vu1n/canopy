//! Error types for canopy operations

use serde::Serialize;
use std::path::PathBuf;

/// Structured error payload shared between service and client.
///
/// Used as the HTTP error body in canopy-service and parsed from service
/// responses in canopy-client.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
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

#[derive(Debug, thiserror::Error)]
pub enum CanopyError {
    #[error("Invalid handle ID: {0}")]
    InvalidHandle(String),

    #[error("Stale index: file {} changed since indexing (reindex required)", .path.display())]
    StaleIndex { path: PathBuf },

    #[error("Query parse error at position {position}: {message}")]
    QueryParse { position: usize, message: String },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("File not found: {}", .0.display())]
    FileNotFound(PathBuf),

    #[error("Not a canopy repo (no .canopy directory). Run 'canopy init' first.")]
    NotInitialized,

    #[error("Config already exists at {}", .0.display())]
    ConfigExists(PathBuf),

    #[error("Config parse error: {0}")]
    ConfigParse(String),

    #[error("Glob pattern error: {0}")]
    GlobPattern(String),

    #[error("Handle not found: {0}")]
    HandleNotFound(String),

    #[error("Tree-sitter parse error for {}: {message}", .path.display())]
    TreeSitterParse { path: PathBuf, message: String },

    #[error("Schema version mismatch: database is v{found}, expected v{expected}. Run 'canopy invalidate' then 'canopy index' to reindex.")]
    SchemaVersionMismatch { found: i32, expected: i32 },

    #[error("Stale generation: expected {expected}, found {found}")]
    StaleGeneration { expected: u64, found: u64 },

    #[error("No service URL configured — pass --service-url or set CANOPY_SERVICE_URL")]
    NoServiceConfigured,

    #[error("Service error [{code}]: {message} — {hint}")]
    ServiceError {
        code: String,
        message: String,
        hint: String,
    },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_new_and_fields() {
        let env = ErrorEnvelope::new("test_code", "test message", "try again");
        assert_eq!(env.code, "test_code");
        assert_eq!(env.message, "test message");
        assert_eq!(env.hint, "try again");
    }

    #[test]
    fn error_envelope_stale_generation() {
        let env = ErrorEnvelope::stale_generation(5, 3);
        assert_eq!(env.code, "stale_generation");
        assert!(env.message.contains("5"));
        assert!(env.message.contains("3"));
        assert!(env.hint.contains("reindex"));
    }

    #[test]
    fn error_envelope_internal() {
        let env = ErrorEnvelope::internal("something broke");
        assert_eq!(env.code, "internal_error");
        assert_eq!(env.message, "something broke");
    }

    #[test]
    fn error_envelope_serialize_roundtrip() {
        let env = ErrorEnvelope::new("e1", "msg", "hint");
        let json = serde_json::to_string(&env).unwrap();
        let recovered: ErrorEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.code, "e1");
        assert_eq!(recovered.message, "msg");
        assert_eq!(recovered.hint, "hint");
    }

    #[test]
    fn canopy_error_display_invalid_handle() {
        let err = CanopyError::InvalidHandle("bad_id".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("bad_id"));
        assert!(msg.contains("Invalid handle"));
    }

    #[test]
    fn canopy_error_display_stale_index() {
        let err = CanopyError::StaleIndex {
            path: PathBuf::from("src/main.rs"),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("src/main.rs"));
        assert!(msg.contains("reindex"));
    }

    #[test]
    fn canopy_error_display_query_parse() {
        let err = CanopyError::QueryParse {
            position: 10,
            message: "unexpected token".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("10"));
        assert!(msg.contains("unexpected token"));
    }

    #[test]
    fn canopy_error_display_stale_generation() {
        let err = CanopyError::StaleGeneration {
            expected: 5,
            found: 3,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("5"));
        assert!(msg.contains("3"));
    }

    #[test]
    fn canopy_error_display_service_error() {
        let err = CanopyError::ServiceError {
            code: "not_found".to_string(),
            message: "repo not found".to_string(),
            hint: "register it first".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("not_found"));
        assert!(msg.contains("repo not found"));
        assert!(msg.contains("register it first"));
    }

    #[test]
    fn canopy_error_display_schema_version_mismatch() {
        let err = CanopyError::SchemaVersionMismatch {
            found: 1,
            expected: 3,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("v1"));
        assert!(msg.contains("v3"));
        assert!(msg.contains("reindex"));
    }

    #[test]
    fn canopy_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err: CanopyError = io_err.into();
        let msg = format!("{}", err);
        assert!(msg.contains("file gone"));
    }
}
