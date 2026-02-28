//! Feedback recording helpers for query and expand events.

use canopy_core::feedback::{ExpandEvent, FeedbackStore, QueryEvent, QueryHandle};
use canopy_core::index::ExpandedHandleDetail;
use canopy_core::{QueryParams, QueryResult};
use std::collections::HashMap;

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
        eprintln!("[canopy-service] feedback lock poisoned while recording query");
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
            eprintln!("[canopy-service] feedback: failed to record query event: {e}");
            return None;
        }
    };

    let handles: Vec<QueryHandle> = result
        .handles
        .iter()
        .map(|handle| QueryHandle {
            handle_id: handle.id.to_string(),
            file_path: handle.file_path.clone(),
            node_type: handle.node_type,
            token_count: handle.token_count,
            first_match_glob: None,
        })
        .collect();
    if let Err(e) = store.record_query_handles(query_event_id, &handles) {
        eprintln!("[canopy-service] feedback: failed to record query handles: {e}");
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
            eprintln!("[canopy-service] feedback: failed to record auto-expand event: {e}");
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
        eprintln!("[canopy-service] feedback lock poisoned while recording expand");
        return false;
    };

    let mut wrote_any = false;
    for (handle_id, file_path, node_type, token_count, _content) in rows {
        match store.record_expand_event(&ExpandEvent {
            query_event_id: recent_query_event_ids.get(handle_id).copied(),
            handle_id: handle_id.clone(),
            file_path: file_path.clone(),
            node_type: *node_type,
            token_count: *token_count,
            auto_expanded: false,
        }) {
            Ok(_) => wrote_any = true,
            Err(e) => eprintln!("[canopy-service] feedback: failed to record expand event: {e}"),
        }
    }

    wrote_any
}
