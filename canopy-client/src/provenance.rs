//! Handle provenance tracking for expand routing.
//!
//! Tracks where handles came from (local vs service) so expand requests
//! can be routed to the correct backend.

use canopy_core::{HandleSource, NodeType};
use std::collections::{HashMap, HashSet, VecDeque};

/// Cap on provenance entries before LRU eviction.
pub const PROVENANCE_CAP: usize = 10_000;
/// Cap on recent-expanded tracking entries.
const RECENT_EXPANDED_CAP: usize = 10_000;
/// Cap on recent query-event tracking entries.
const RECENT_QUERY_EVENT_CAP: usize = 10_000;

/// Where a handle came from — used to route expand requests.
#[derive(Debug, Clone)]
pub struct HandleProvenance {
    pub source: HandleSource,
    pub generation: Option<u64>,
    pub repo_id: Option<String>,
    pub file_path: String,
    pub node_type: NodeType,
    pub token_count: usize,
}

/// Tracks handle provenance, expand history, and query event associations.
///
/// Keyed by (canonical_repo_path, handle_id) tuples with capped LRU eviction.
pub struct ProvenanceTracker {
    /// (canonical_repo_path, handle_id) → provenance
    provenance: HashMap<(String, String), HandleProvenance>,
    /// Insertion order for LRU eviction
    provenance_order: VecDeque<(String, String)>,
    /// Best-effort mapping: (canonical_repo_path, handle_id) -> latest query_event_id
    recent_query_events: HashMap<(String, String), i64>,
    /// Insertion order for recent_query_events cap eviction
    query_event_order: VecDeque<(String, String)>,
    /// Session-local memory: handles already expanded in this runtime
    recently_expanded: HashSet<(String, String)>,
    /// Insertion order for recently_expanded cap eviction
    recently_expanded_order: VecDeque<(String, String)>,
}

impl Default for ProvenanceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProvenanceTracker {
    pub fn new() -> Self {
        Self {
            provenance: HashMap::new(),
            provenance_order: VecDeque::new(),
            recent_query_events: HashMap::new(),
            query_event_order: VecDeque::new(),
            recently_expanded: HashSet::new(),
            recently_expanded_order: VecDeque::new(),
        }
    }

    /// Record provenance for a handle.
    pub fn record(&mut self, repo_key: &str, handle_id: &str, provenance: HandleProvenance) {
        let key = (repo_key.to_string(), handle_id.to_string());
        if self.provenance.insert(key.clone(), provenance).is_none() {
            self.provenance_order.push_back(key);
            while self.provenance_order.len() > PROVENANCE_CAP {
                if let Some(old) = self.provenance_order.pop_front() {
                    self.provenance.remove(&old);
                }
            }
        }
    }

    /// Look up provenance for a handle.
    pub fn get(&self, repo_key: &str, handle_id: &str) -> Option<&HandleProvenance> {
        self.provenance
            .get(&(repo_key.to_string(), handle_id.to_string()))
    }

    /// Record the query event ID that produced a handle.
    pub fn record_query_event(&mut self, repo_key: &str, handle_id: &str, event_id: i64) {
        let key = (repo_key.to_string(), handle_id.to_string());
        if self
            .recent_query_events
            .insert(key.clone(), event_id)
            .is_none()
        {
            self.query_event_order.push_back(key);
            while self.query_event_order.len() > RECENT_QUERY_EVENT_CAP {
                if let Some(old) = self.query_event_order.pop_front() {
                    self.recent_query_events.remove(&old);
                }
            }
        }
    }

    /// Look up the query event ID for a handle.
    pub fn query_event_id(&self, repo_key: &str, handle_id: &str) -> Option<i64> {
        self.recent_query_events
            .get(&(repo_key.to_string(), handle_id.to_string()))
            .copied()
    }

    /// Mark a handle as recently expanded.
    pub fn mark_expanded(&mut self, repo_key: &str, handle_id: &str) {
        let key = (repo_key.to_string(), handle_id.to_string());
        if self.recently_expanded.insert(key.clone()) {
            self.recently_expanded_order.push_back(key);
            while self.recently_expanded_order.len() > RECENT_EXPANDED_CAP {
                if let Some(old) = self.recently_expanded_order.pop_front() {
                    self.recently_expanded.remove(&old);
                }
            }
        }
    }

    /// Check if a handle was recently expanded.
    pub fn was_recently_expanded(&self, repo_key: &str, handle_id: &str) -> bool {
        self.recently_expanded
            .contains(&(repo_key.to_string(), handle_id.to_string()))
    }

    /// Invalidate all provenance for a given repo_id.
    /// Stale VecDeque entries become lazy tombstones.
    pub fn invalidate_repo(&mut self, repo_id: &str) {
        self.provenance
            .retain(|_, prov| prov.repo_id.as_deref() != Some(repo_id));
    }
}
