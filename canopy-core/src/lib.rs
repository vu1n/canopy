//! Canopy Core - Token-efficient codebase queries
//!
//! This library provides the core functionality for parsing, indexing,
//! and querying codebases with a focus on token efficiency.

pub mod config;
pub mod document;
pub mod error;
pub mod handle;
pub mod index;
pub mod parse;
pub mod query;

pub use config::Config;
pub use document::{DocumentNode, NodeMetadata, NodeType, ParsedFile, Span};
pub use error::CanopyError;
pub use handle::{Handle, HandleId};
pub use index::{FileDiscovery, IndexStats, RepoIndex};
pub use query::{MatchMode, Query, QueryOptions, QueryParams, QueryResult, DEFAULT_EXPAND_BUDGET};

/// Result type alias for canopy operations
pub type Result<T> = std::result::Result<T, CanopyError>;
