//! Query and evidence_pack route handlers.

use crate::error::AppError;
use crate::evidence::{normalize_query_params, run_evidence_plan};
use crate::feedback_recording::try_record_feedback_query;
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::protocol::{EvidencePackRequest, QueryRequest};
use canopy_core::{build_evidence_pack, EvidencePack, QueryParams, QueryResult};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{query_with_cache, resolve_ready_shard, utc_log_timestamp};
use tracing::info;

pub(crate) async fn query(
    State(state): State<SharedState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResult>, AppError> {
    let start = Instant::now();
    let repo_label = req.repo.clone();

    let shard = resolve_ready_shard(&state, &req.repo).await?;

    let params = normalize_query_params(req.params, false);
    let feedback_store = state
        .feedback_store_for_repo(&shard.repo_id, &shard.repo_root)
        .await;
    let node_type_priors = state
        .load_node_type_priors(&shard.repo_id, &shard.repo_root)
        .await;

    // Track analytics
    if let Ok(mut analytics) = state.metrics.analytics.lock() {
        if let Some(ref sym) = params.symbol {
            *analytics.top_symbols.entry(sym.clone()).or_insert(0) += 1;
        }
        if let Some(ref pat) = params.pattern {
            *analytics.top_patterns.entry(pat.clone()).or_insert(0) += 1;
        }
        *analytics
            .requests_by_repo
            .entry(shard.repo_id.clone())
            .or_insert(0) += 1;
    }

    state.metrics.query_count.fetch_add(1, Ordering::Relaxed);

    let (result, was_hit) = query_with_cache(
        &state,
        &shard.repo_id,
        &shard.repo_root,
        shard.generation,
        &shard.commit_sha,
        &params,
        node_type_priors,
    )
    .await?;

    if let Some(query_event_id) =
        try_record_feedback_query(feedback_store.as_ref(), &params, &result)
    {
        let handle_ids: Vec<String> = result.handles.iter().map(|h| h.id.to_string()).collect();
        state
            .remember_query_event_for_handles(&shard.repo_id, &handle_ids, query_event_id)
            .await;
        state.invalidate_node_type_priors_cache(&shard.repo_id).await;
    }

    let duration_ms = start.elapsed().as_millis();
    state
        .metrics
        .total_query_ms
        .fetch_add(duration_ms as u64, Ordering::Relaxed);
    let cache_state = if was_hit { "hit" } else { "miss" };
    info!(
        "[{}] POST /query repo={} duration_ms={} cache={}",
        utc_log_timestamp(),
        repo_label,
        duration_ms,
        cache_state
    );

    Ok(Json(result))
}

pub(crate) async fn evidence_pack(
    State(state): State<SharedState>,
    Json(req): Json<EvidencePackRequest>,
) -> Result<Json<EvidencePack>, AppError> {
    let start = Instant::now();
    let repo_label = req.repo.clone();

    let shard = resolve_ready_shard(&state, &req.repo).await?;

    let seed_params = normalize_query_params(req.params, true);
    let max_handles = req.config.max_handles.unwrap_or(8).clamp(1, 64);
    let max_per_file = req.config.max_per_file.unwrap_or(2).clamp(1, 8);

    let feedback_store = state
        .feedback_store_for_repo(&shard.repo_id, &shard.repo_root)
        .await;
    let node_type_priors = state
        .load_node_type_priors(&shard.repo_id, &shard.repo_root)
        .await;

    state.metrics.query_count.fetch_add(1, Ordering::Relaxed);

    let plan_result = run_evidence_plan(
        seed_params,
        req.config.plan,
        max_handles,
        max_per_file,
        |params: QueryParams| {
            let s = state.clone();
            let rid = shard.repo_id.clone();
            let rroot = shard.repo_root.clone();
            let csha = shard.commit_sha.clone();
            let gen = shard.generation;
            let priors = node_type_priors.clone();
            Box::pin(async move {
                query_with_cache(&s, &rid, &rroot, gen, &csha, &params, priors).await
            })
        },
    )
    .await?;

    if let Some(query_event_id) = try_record_feedback_query(
        feedback_store.as_ref(),
        &plan_result.seed_params,
        &plan_result.result,
    ) {
        let handle_ids: Vec<String> = plan_result
            .result
            .handles
            .iter()
            .map(|h| h.id.to_string())
            .collect();
        state
            .remember_query_event_for_handles(&shard.repo_id, &handle_ids, query_event_id)
            .await;
        state.invalidate_node_type_priors_cache(&shard.repo_id).await;
    }

    let mut pack = build_evidence_pack(
        &plan_result.result,
        &plan_result.query_text,
        max_handles,
        max_per_file,
    );
    let suggested_ids = pack.expand_suggestion.clone();
    let recent_expanded = state
        .recent_expanded_handle_ids(&shard.repo_id, &suggested_ids)
        .await;
    pack.reorder_expand_suggestions(|id| recent_expanded.contains(id));

    let duration_ms = start.elapsed().as_millis();
    state
        .metrics
        .total_query_ms
        .fetch_add(duration_ms as u64, Ordering::Relaxed);
    let cache_state = if plan_result.cache_hits > 0 && plan_result.cache_misses > 0 {
        "mixed"
    } else if plan_result.cache_hits > 0 {
        "hit"
    } else {
        "miss"
    };
    info!(
        "[{}] POST /evidence_pack repo={} duration_ms={} cache={} plan={} steps={} selected={}",
        utc_log_timestamp(),
        repo_label,
        duration_ms,
        cache_state,
        plan_result.planning_enabled,
        plan_result.plan_steps,
        pack.selected_count
    );

    Ok(Json(pack))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{insert_test_shard, test_state};
    use canopy_core::{Generation, QueryParams, ShardStatus};

    #[tokio::test]
    async fn query_unknown_repo_returns_error() {
        let state = test_state();
        let result = query(
            State(state),
            Json(QueryRequest {
                repo: "nonexistent".to_string(),
                params: QueryParams::new(),
            }),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn query_pending_repo_returns_error() {
        let state = test_state();
        let repo_id = "pending-repo";
        insert_test_shard(
            &state,
            repo_id,
            "pending",
            ShardStatus::Pending,
            Generation::new(),
        )
        .await;

        let result = query(
            State(state),
            Json(QueryRequest {
                repo: repo_id.to_string(),
                params: QueryParams::new(),
            }),
        )
        .await;
        assert!(result.is_err());
    }
}
