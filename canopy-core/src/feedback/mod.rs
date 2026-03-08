//! Feedback storage for query/expand signals used by ranking heuristics.

mod store;
#[cfg(test)]
mod tests;

pub use store::FeedbackStore;

use crate::handle::Handle;
use crate::NodeType;
use std::time::{SystemTime, UNIX_EPOCH};

/// TTL for cached node-type prior distributions (shared by client and service).
pub const NODE_TYPE_PRIOR_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

pub(crate) const RETENTION_DAYS: i64 = 30;
pub(crate) const QUERY_EVENTS_CAP: i64 = 10_000;
pub(crate) const EXPAND_EVENTS_CAP: i64 = 50_000;
pub(crate) const TOP_K_GLOBS: usize = 5;

#[derive(Debug, Clone)]
pub struct QueryEvent {
    pub query_text: String,
    pub predicted_globs: Option<Vec<String>>,
    pub files_indexed: usize,
    pub handles_returned: usize,
    pub total_tokens: usize,
}

#[derive(Debug, Clone)]
pub struct QueryHandle {
    pub handle_id: String,
    pub file_path: String,
    pub node_type: NodeType,
    pub token_count: usize,
    pub first_match_glob: Option<String>,
}

impl QueryHandle {
    /// Build a `QueryHandle` from a `Handle`, optionally attaching a matched glob.
    pub fn from_handle(handle: &Handle, first_match_glob: Option<String>) -> Self {
        Self {
            handle_id: handle.id.to_string(),
            file_path: handle.file_path.clone(),
            node_type: handle.node_type,
            token_count: handle.token_count,
            first_match_glob,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExpandEvent {
    pub query_event_id: Option<i64>,
    pub handle_id: String,
    pub file_path: String,
    pub node_type: NodeType,
    pub token_count: usize,
    pub auto_expanded: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FeedbackMetrics {
    pub glob_hit_rate_at_k: f64,
    pub handle_expand_accept_rate: f64,
    pub avg_tokens_per_expand: f64,
    pub sample_count: usize,
}

pub(crate) fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
