//! Canopy Core - Token-efficient codebase queries
//!
//! This library provides the core functionality for parsing, indexing,
//! and querying codebases with a focus on token efficiency.

pub mod capped_map;
pub mod config;
pub mod document;
pub mod error;
pub mod feedback;
pub mod generation;
pub mod git;
pub mod handle;
pub mod index;
pub mod parse;
pub mod protocol;
pub mod query;
pub mod scoring;

pub use config::Config;
pub use document::{DocumentNode, NodeMetadata, NodeType, ParsedFile, RefType, Reference, Span};
pub use error::{CanopyError, ErrorEnvelope};
pub use generation::{Generation, RepoShard, ShardStatus};
pub use handle::{Handle, HandleId, HandleSource, RefHandle};
pub use index::{FileDiscovery, IndexStats, RepoIndex};
pub use query::{
    build_evidence_pack, split_terms, EvidenceAction, EvidenceConfidence, EvidenceFileSummary,
    EvidenceGuidance, EvidenceHandle, EvidencePack, MatchMode, Query, QueryKind, QueryOptions,
    QueryParams, QueryResult, DEFAULT_EXPAND_BUDGET,
};

/// Outcome of an expand operation — supports partial success.
///
/// Shared across canopy-client and canopy-mcp to ensure a consistent expand contract.
pub struct ExpandOutcome {
    /// Successfully expanded (handle_id, content) pairs.
    pub contents: Vec<(String, String)>,
    /// Handle IDs that could not be expanded.
    pub failed_ids: Vec<String>,
}

/// Result type alias for canopy operations
pub type Result<T> = std::result::Result<T, CanopyError>;

/// Create a uniquely-named temporary directory for test repos.
///
/// Returns an empty directory under `$TMPDIR/canopy-{prefix}-{nanos}`.
/// Callers are responsible for any further setup (e.g., writing files, calling `RepoIndex::init`).
pub fn temp_test_dir(prefix: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("canopy-{prefix}-{ts}"));
    std::fs::create_dir_all(&root).unwrap();
    root
}
