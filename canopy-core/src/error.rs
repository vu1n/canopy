//! Error types for canopy operations

use std::path::PathBuf;

/// Error types for canopy operations
#[derive(Debug, thiserror::Error)]
pub enum CanopyError {
    #[error("Invalid handle ID: {0}")]
    InvalidHandle(String),

    #[error("Stale index: file {path} changed since indexing (reindex required)")]
    StaleIndex { path: String },

    #[error("Query parse error at position {position}: {message}")]
    QueryParse { position: usize, message: String },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Not a canopy repo (no .canopy directory). Run 'canopy init' first.")]
    NotInitialized,

    #[error("Config already exists at {0}")]
    ConfigExists(PathBuf),

    #[error("Config parse error: {0}")]
    ConfigParse(String),

    #[error("Glob pattern error: {0}")]
    GlobPattern(String),

    #[error("Handle not found: {0}")]
    HandleNotFound(String),

    #[error("Tree-sitter parse error for {path}: {message}")]
    TreeSitterParse { path: String, message: String },

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
}
