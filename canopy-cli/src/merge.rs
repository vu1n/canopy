//! Merge logic for combining local and service query results

use canopy_core::QueryResult;
use std::collections::HashSet;

/// Merge local and service query results
///
/// Rules:
/// - If local and service handle same file with overlapping line ranges, keep local
/// - Non-overlapping intervals in same file: keep both
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

    // Add service handles that don't overlap with local
    for service_handle in &service.handles {
        // If this file is not dirty, keep the service handle
        if !dirty_paths.contains(&service_handle.file_path) {
            merged_handles.push(service_handle.clone());
            continue;
        }

        // File is dirty — check if any local handle overlaps
        let overlaps = local.handles.iter().any(|local_handle| {
            local_handle.file_path == service_handle.file_path
                && ranges_overlap(local_handle.line_range, service_handle.line_range)
        });

        if !overlaps {
            merged_handles.push(service_handle.clone());
        }
    }

    let total_tokens: usize = merged_handles.iter().map(|h| h.token_count).sum();
    let total_matches = merged_handles.len();

    QueryResult {
        handles: merged_handles,
        ref_handles: merge_ref_handles(local.ref_handles, service.ref_handles),
        total_tokens,
        truncated: local.truncated || service.truncated,
        total_matches,
        auto_expanded: local.auto_expanded || service.auto_expanded,
        expand_note: local.expand_note.or(service.expand_note),
    }
}

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

fn merge_ref_handles(
    local: Option<Vec<canopy_core::RefHandle>>,
    service: Option<Vec<canopy_core::RefHandle>>,
) -> Option<Vec<canopy_core::RefHandle>> {
    match (local, service) {
        (Some(mut l), Some(s)) => {
            l.extend(s);
            Some(l)
        }
        (Some(l), None) => Some(l),
        (None, Some(s)) => Some(s),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ranges_overlap() {
        assert!(ranges_overlap((1, 10), (5, 15)));
        assert!(ranges_overlap((5, 15), (1, 10)));
        assert!(ranges_overlap((1, 10), (1, 10)));
        assert!(ranges_overlap((1, 10), (10, 20)));
        assert!(!ranges_overlap((1, 10), (11, 20)));
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
}
