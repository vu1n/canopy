//! Merge logic for combining local and service query results

use canopy_core::QueryResult;
use std::collections::HashSet;

/// Merge local and service query results
///
/// Rules:
/// - Dirty paths: drop ALL service handles for that file (not just overlapping).
///   When lines shift due to edits, both overlapping and non-overlapping service
///   handles can be stale.
/// - Files not in dirty set: keep service handles as-is
pub fn merge_results(
    local: QueryResult,
    service: QueryResult,
    dirty_paths: &HashSet<String>,
) -> QueryResult {
    let mut merged_handles = Vec::new();

    // Add all local handles first (they take precedence for dirty files)
    for handle in &local.handles {
        merged_handles.push(handle.clone());
    }

    // Add service handles only for non-dirty files
    for service_handle in &service.handles {
        if !dirty_paths.contains(&service_handle.file_path) {
            merged_handles.push(service_handle.clone());
        }
    }

    let total_tokens: usize = merged_handles.iter().map(|h| h.token_count).sum();
    let total_matches = merged_handles.len();

    QueryResult {
        handles: merged_handles,
        ref_handles: merge_ref_handles(local.ref_handles, service.ref_handles, dirty_paths),
        total_tokens,
        truncated: local.truncated || service.truncated,
        total_matches,
        auto_expanded: local.auto_expanded || service.auto_expanded,
        expand_note: local.expand_note.or(service.expand_note),
    }
}

fn merge_ref_handles(
    local: Option<Vec<canopy_core::RefHandle>>,
    service: Option<Vec<canopy_core::RefHandle>>,
    dirty_paths: &HashSet<String>,
) -> Option<Vec<canopy_core::RefHandle>> {
    match (local, service) {
        (Some(mut l), Some(s)) => {
            // Drop service ref_handles for dirty paths
            let filtered: Vec<_> = s
                .into_iter()
                .filter(|r| !dirty_paths.contains(&r.file_path))
                .collect();
            l.extend(filtered);
            Some(l)
        }
        (Some(l), None) => Some(l),
        (None, Some(s)) => {
            let filtered: Vec<_> = s
                .into_iter()
                .filter(|r| !dirty_paths.contains(&r.file_path))
                .collect();
            if filtered.is_empty() {
                None
            } else {
                Some(filtered)
            }
        }
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canopy_core::{Handle, NodeType, Span};

    fn make_handle(file: &str, start: usize, end: usize) -> Handle {
        Handle::new(
            file.to_string(),
            NodeType::Function,
            Span {
                start: start * 10,
                end: end * 10,
            },
            (start, end),
            100,
            "preview".to_string(),
        )
    }

    #[test]
    fn test_ranges_overlap() {
        // Old test preserved for reference — merge now drops all dirty file handles
    }

    #[test]
    fn test_merge_empty() {
        let local = QueryResult {
            handles: vec![],
            ref_handles: None,
            total_tokens: 0,
            truncated: false,
            total_matches: 0,
            auto_expanded: false,
            expand_note: None,
        };
        let service = QueryResult {
            handles: vec![],
            ref_handles: None,
            total_tokens: 0,
            truncated: false,
            total_matches: 0,
            auto_expanded: false,
            expand_note: None,
        };
        let dirty = HashSet::new();
        let result = merge_results(local, service, &dirty);
        assert!(result.handles.is_empty());
    }

    #[test]
    fn test_merge_drops_all_service_handles_for_dirty_files() {
        let local = QueryResult {
            handles: vec![make_handle("src/dirty.rs", 1, 10)],
            ref_handles: None,
            total_tokens: 100,
            truncated: false,
            total_matches: 1,
            auto_expanded: false,
            expand_note: None,
        };
        let service = QueryResult {
            handles: vec![
                make_handle("src/dirty.rs", 20, 30), // non-overlapping but dirty
                make_handle("src/clean.rs", 1, 10),  // clean file, kept
            ],
            ref_handles: None,
            total_tokens: 200,
            truncated: false,
            total_matches: 2,
            auto_expanded: false,
            expand_note: None,
        };
        let mut dirty = HashSet::new();
        dirty.insert("src/dirty.rs".to_string());

        let result = merge_results(local, service, &dirty);
        assert_eq!(result.handles.len(), 2); // 1 local + 1 clean service
        assert_eq!(result.handles[0].file_path, "src/dirty.rs"); // local
        assert_eq!(result.handles[1].file_path, "src/clean.rs"); // service
    }

    #[test]
    fn test_merge_keeps_all_handles_for_clean_files() {
        let local = QueryResult {
            handles: vec![],
            ref_handles: None,
            total_tokens: 0,
            truncated: false,
            total_matches: 0,
            auto_expanded: false,
            expand_note: None,
        };
        let service = QueryResult {
            handles: vec![
                make_handle("src/a.rs", 1, 10),
                make_handle("src/b.rs", 1, 10),
            ],
            ref_handles: None,
            total_tokens: 200,
            truncated: false,
            total_matches: 2,
            auto_expanded: false,
            expand_note: None,
        };
        let dirty = HashSet::new();
        let result = merge_results(local, service, &dirty);
        assert_eq!(result.handles.len(), 2);
    }
}
