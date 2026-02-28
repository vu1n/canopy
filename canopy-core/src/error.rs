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

    #[error("Service error [{code}]: {message} — {hint}")]
    ServiceError {
        code: String,
        message: String,
        hint: String,
    },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
