//! Feedback recording helpers for query and expand events.

use canopy_core::feedback::{ExpandEvent, FeedbackStore, QueryEvent, QueryHandle};
use canopy_core::index::ExpandedHandleDetail;
use canopy_core::{QueryParams, QueryResult};
use std::collections::HashMap;
use tracing::warn;

/// Record a query event to the feedback store, returning the event ID on success.
///
/// Returns `None` if no feedback store is available or if the lock is poisoned.
pub fn try_record_feedback_query(
    feedback_store: Option<&std::sync::Arc<std::sync::Mutex<FeedbackStore>>>,
    params: &QueryParams,
    result: &QueryResult,
) -> Option<i64> {
    let feedback_store = feedback_store?;
    let Ok(store) = feedback_store.lock() else {
        warn!("[canopy-service] feedback lock poisoned while recording query");
        return None;
    };

    let event = QueryEvent {
        query_text: params.to_text(),
        predicted_globs: None,
        files_indexed: 0,
        handles_returned: result.handles.len(),
        total_tokens: result.total_tokens,
    };

    let query_event_id = match store.record_query_event(&event) {
        Ok(id) => id,
        Err(e) => {
            warn!("[canopy-service] feedback: failed to record query event: {e}");
            return None;
        }
    };

    let handles: Vec<QueryHandle> = result
        .handles
        .iter()
        .map(|handle| QueryHandle::from_handle(handle, None))
        .collect();
    if let Err(e) = store.record_query_handles(query_event_id, &handles) {
        warn!("[canopy-service] feedback: failed to record query handles: {e}");
    }

    for handle in result.handles.iter().filter(|h| h.content.is_some()) {
        if let Err(e) = store.record_expand_event(&ExpandEvent {
            query_event_id: Some(query_event_id),
            handle_id: handle.id.to_string(),
            file_path: handle.file_path.clone(),
            node_type: handle.node_type,
            token_count: handle.token_count,
            auto_expanded: true,
        }) {
            warn!("[canopy-service] feedback: failed to record auto-expand event: {e}");
        }
    }

    Some(query_event_id)
}

/// Record expand events to the feedback store.
///
/// Returns `true` if at least one event was recorded.
pub fn try_record_feedback_expand(
    feedback_store: Option<&std::sync::Arc<std::sync::Mutex<FeedbackStore>>>,
    rows: &[ExpandedHandleDetail],
    recent_query_event_ids: &HashMap<String, i64>,
) -> bool {
    let Some(feedback_store) = feedback_store else {
        return false;
    };
    let Ok(store) = feedback_store.lock() else {
        warn!("[canopy-service] feedback lock poisoned while recording expand");
        return false;
    };

    let mut wrote_any = false;
    for row in rows {
        match store.record_expand_event(&ExpandEvent {
            query_event_id: recent_query_event_ids.get(&row.handle_id).copied(),
            handle_id: row.handle_id.clone(),
            file_path: row.file_path.clone(),
            node_type: row.node_type,
            token_count: row.token_count,
            auto_expanded: false,
        }) {
            Ok(_) => wrote_any = true,
            Err(e) => warn!("[canopy-service] feedback: failed to record expand event: {e}"),
        }
    }

    wrote_any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_record_feedback_query_returns_none_when_no_store() {
        let params = QueryParams::default();
        let result = QueryResult::default();
        assert!(try_record_feedback_query(None, &params, &result).is_none());
    }

    #[test]
    fn try_record_feedback_expand_returns_false_when_no_store() {
        let rows: Vec<ExpandedHandleDetail> = vec![];
        let ids = HashMap::new();
        assert!(!try_record_feedback_expand(None, &rows, &ids));
    }
}
