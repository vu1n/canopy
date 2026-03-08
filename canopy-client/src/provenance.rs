//! Handle provenance tracking for expand routing.
//!
//! Tracks where handles came from (local vs service) so expand requests
//! can be routed to the correct backend.
//!
//! Methods return `Option` rather than `Result` by design: provenance is a
//! best-effort cache. Missing entries are expected (e.g., after eviction or
//! for handles predating tracking) and should never fail the calling operation.

use canopy_core::capped_map::{CappedMap, CappedSet};
use canopy_core::{HandleSource, NodeType};

/// Cap on provenance entries before FIFO eviction.
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
/// Keyed by (canonical_repo_path, handle_id) tuples with capped FIFO eviction.
pub struct ProvenanceTracker {
    provenance: CappedMap<(String, String), HandleProvenance>,
    recent_query_events: CappedMap<(String, String), i64>,
    recently_expanded: CappedSet<(String, String)>,
}

impl Default for ProvenanceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProvenanceTracker {
    pub fn new() -> Self {
        Self {
            provenance: CappedMap::new(PROVENANCE_CAP),
            recent_query_events: CappedMap::new(RECENT_QUERY_EVENT_CAP),
            recently_expanded: CappedSet::new(RECENT_EXPANDED_CAP),
        }
    }

    /// Record provenance for a handle.
    pub fn record(&mut self, repo_key: &str, handle_id: &str, provenance: HandleProvenance) {
        let key = (repo_key.to_string(), handle_id.to_string());
        self.provenance.insert(key, provenance);
    }

    /// Look up provenance for a handle.
    pub fn get(&self, repo_key: &str, handle_id: &str) -> Option<&HandleProvenance> {
        self.provenance
            .get(&(repo_key.to_string(), handle_id.to_string()))
    }

    /// Record the query event ID that produced a handle.
    pub fn record_query_event(&mut self, repo_key: &str, handle_id: &str, event_id: i64) {
        let key = (repo_key.to_string(), handle_id.to_string());
        self.recent_query_events.insert(key, event_id);
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
        self.recently_expanded.insert(key);
    }

    /// Check if a handle was recently expanded.
    pub fn was_recently_expanded(&self, repo_key: &str, handle_id: &str) -> bool {
        self.recently_expanded
            .contains(&(repo_key.to_string(), handle_id.to_string()))
    }

    /// Invalidate all provenance for a given repo_id.
    pub fn invalidate_repo(&mut self, repo_id: &str) {
        self.provenance
            .retain(|_, prov| prov.repo_id.as_deref() != Some(repo_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provenance(source: HandleSource, repo_id: Option<&str>) -> HandleProvenance {
        HandleProvenance {
            source,
            generation: Some(1),
            repo_id: repo_id.map(|s| s.to_string()),
            file_path: "src/main.rs".to_string(),
            node_type: NodeType::Function,
            token_count: 100,
        }
    }

    #[test]
    fn record_and_get_returns_provenance() {
        let mut tracker = ProvenanceTracker::new();
        let prov = make_provenance(HandleSource::Local, Some("repo1"));
        tracker.record("repo1", "handle_1", prov);

        let retrieved = tracker.get("repo1", "handle_1");
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.source, HandleSource::Local);
        assert_eq!(retrieved.file_path, "src/main.rs");
        assert_eq!(retrieved.token_count, 100);
    }

    #[test]
    fn get_returns_none_for_missing_handle() {
        let tracker = ProvenanceTracker::new();
        assert!(tracker.get("repo1", "nonexistent").is_none());
    }

    #[test]
    fn eviction_at_provenance_cap() {
        let mut tracker = ProvenanceTracker::new();

        // Fill to capacity + 1
        for i in 0..=PROVENANCE_CAP {
            let prov = make_provenance(HandleSource::Service, Some("repo1"));
            tracker.record("repo1", &format!("handle_{}", i), prov);
        }

        // The first entry should have been evicted
        assert!(tracker.get("repo1", "handle_0").is_none());
        // The last entry should still be present
        assert!(tracker
            .get("repo1", &format!("handle_{}", PROVENANCE_CAP))
            .is_some());
        // Total entries should be at the cap
        assert_eq!(tracker.provenance.len(), PROVENANCE_CAP);
    }

    #[test]
    fn mark_expanded_and_was_recently_expanded() {
        let mut tracker = ProvenanceTracker::new();

        assert!(!tracker.was_recently_expanded("repo1", "handle_1"));

        tracker.mark_expanded("repo1", "handle_1");
        assert!(tracker.was_recently_expanded("repo1", "handle_1"));

        // Different handle should not be marked
        assert!(!tracker.was_recently_expanded("repo1", "handle_2"));

        // Different repo should not be marked
        assert!(!tracker.was_recently_expanded("repo2", "handle_1"));
    }

    #[test]
    fn invalidate_repo_removes_matching_entries() {
        let mut tracker = ProvenanceTracker::new();

        tracker.record(
            "repo1",
            "h1",
            make_provenance(HandleSource::Service, Some("repo1")),
        );
        tracker.record(
            "repo2",
            "h2",
            make_provenance(HandleSource::Service, Some("repo2")),
        );
        tracker.record(
            "repo1",
            "h3",
            make_provenance(HandleSource::Local, None),
        );

        tracker.invalidate_repo("repo1");

        // Handle with repo_id="repo1" should be gone
        assert!(tracker.get("repo1", "h1").is_none());
        // Handle with repo_id="repo2" should remain
        assert!(tracker.get("repo2", "h2").is_some());
        // Handle with repo_id=None should remain (invalidate matches on repo_id, not repo_key)
        assert!(tracker.get("repo1", "h3").is_some());
    }

    #[test]
    fn query_event_record_and_lookup() {
        let mut tracker = ProvenanceTracker::new();

        assert!(tracker.query_event_id("repo1", "h1").is_none());

        tracker.record_query_event("repo1", "h1", 42);
        assert_eq!(tracker.query_event_id("repo1", "h1"), Some(42));

        // Different handle should return None
        assert!(tracker.query_event_id("repo1", "h2").is_none());
    }

    #[test]
    fn record_overwrites_provenance_without_adding_to_order() {
        let mut tracker = ProvenanceTracker::new();

        let prov1 = HandleProvenance {
            token_count: 100,
            ..make_provenance(HandleSource::Local, Some("repo1"))
        };
        let prov2 = HandleProvenance {
            token_count: 200,
            ..make_provenance(HandleSource::Service, Some("repo1"))
        };

        tracker.record("repo1", "h1", prov1);
        tracker.record("repo1", "h1", prov2);

        // Should have the updated value
        let retrieved = tracker.get("repo1", "h1").unwrap();
        assert_eq!(retrieved.token_count, 200);
        assert_eq!(retrieved.source, HandleSource::Service);

        // Map should only have one entry (no duplicate from overwrite)
        assert_eq!(tracker.provenance.len(), 1);
    }
}
