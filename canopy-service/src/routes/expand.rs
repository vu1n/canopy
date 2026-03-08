//! Expand route handler.

use crate::error::AppError;
use crate::feedback_recording::try_record_feedback_expand;
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::protocol::{ExpandRequest, ExpandResponse, ExpandedContent};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{resolve_ready_shard, utc_log_timestamp};
use tracing::info;

pub(crate) async fn expand(
    State(state): State<SharedState>,
    Json(req): Json<ExpandRequest>,
) -> Result<Json<ExpandResponse>, AppError> {
    let start = Instant::now();
    let repo_label = req.repo.clone();
    let handle_count = req.handles.len();

    let shard = resolve_ready_shard(&state, &req.repo).await?;
    let current_gen = shard.generation;

    // Validate generation if provided
    for h in &req.handles {
        if let Some(gen) = h.generation {
            if gen != current_gen {
                return Err(AppError::stale(current_gen, gen));
            }
        }
    }
    let feedback_store = state
        .feedback_store_for_repo(&shard.repo_id, &shard.repo_root)
        .await;

    state.metrics.expand_count.fetch_add(1, Ordering::Relaxed);

    let repo_id = shard.repo_id;
    let repo_root = shard.repo_root;

    // Track per-repo analytics
    if let Ok(mut analytics) = state.metrics.analytics.lock() {
        *analytics
            .requests_by_repo
            .entry(repo_id.clone())
            .or_insert(0) += 1;
    }

    let handle_ids: Vec<String> = req.handles.iter().map(|h| h.id.clone()).collect();
    let cached_index = state
        .get_or_open_index(&repo_id, &repo_root, current_gen)
        .await
        .map_err(AppError::from)?;

    let expanded_details = tokio::task::spawn_blocking(move || {
        let index = cached_index.lock_index()?;
        index.expand_with_details(&handle_ids)
    })
    .await
    .map_err(AppError::internal)??;

    // Track expanded file paths
    if let Ok(mut analytics) = state.metrics.analytics.lock() {
        for d in &expanded_details {
            *analytics
                .top_expanded_files
                .entry(d.file_path.clone())
                .or_insert(0) += 1;
        }
    }
    let expanded_ids: Vec<String> = expanded_details
        .iter()
        .map(|d| d.handle_id.clone())
        .collect();
    state
        .remember_expanded_handles(&repo_id, &expanded_ids)
        .await;
    let recent_query_event_ids = state
        .recent_query_events_for_handles(&repo_id, &expanded_ids)
        .await;
    if try_record_feedback_expand(
        feedback_store.as_ref(),
        &expanded_details,
        &recent_query_event_ids,
    ) {
        state.invalidate_node_type_priors_cache(&repo_id).await;
    }

    let duration_ms = start.elapsed().as_millis();
    state
        .metrics
        .total_expand_ms
        .fetch_add(duration_ms as u64, Ordering::Relaxed);
    info!(
        "[{}] POST /expand repo={} duration_ms={} handles={}",
        utc_log_timestamp(),
        repo_label,
        duration_ms,
        handle_count
    );

    Ok(Json(ExpandResponse {
        contents: expanded_details
            .into_iter()
            .map(|d| ExpandedContent {
                handle_id: d.handle_id,
                content: d.content,
            })
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{insert_test_shard, test_state};
    use canopy_core::protocol::ExpandHandle;
    use canopy_core::{Generation, ShardStatus};

    #[tokio::test]
    async fn expand_unknown_repo_returns_error() {
        let state = test_state();
        let result = expand(
            State(state),
            Json(ExpandRequest {
                repo: "nonexistent".to_string(),
                handles: vec![],
            }),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn expand_stale_generation_returns_error() {
        let state = test_state();
        let repo_id = "test-repo";
        insert_test_shard(
            &state,
            repo_id,
            "test",
            ShardStatus::Ready,
            Generation::from_value(5),
        )
        .await;

        let result = expand(
            State(state),
            Json(ExpandRequest {
                repo: repo_id.to_string(),
                handles: vec![ExpandHandle {
                    id: "h_abc".to_string(),
                    generation: Some(3),
                }],
            }),
        )
        .await;
        assert!(result.is_err());
    }
}
