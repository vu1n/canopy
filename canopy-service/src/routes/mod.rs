//! HTTP route handlers for the canopy service.

mod expand;
mod query;
mod repos;

pub(crate) use expand::expand;
pub(crate) use query::{evidence_pack, query};
pub(crate) use repos::{add_repo, list_repos, reindex, status};

use crate::error::AppError;
use crate::state::SharedState;
use canopy_core::{
    query::execute_query_with_options, HandleSource, NodeType, QueryParams, QueryResult,
    ShardStatus,
};
use std::collections::HashMap;
use std::sync::atomic::Ordering;

/// Fields extracted from a ready shard, used by route handlers.
pub(crate) struct ReadyShard {
    pub(crate) repo_id: String,
    pub(crate) repo_root: String,
    pub(crate) commit_sha: Option<String>,
    pub(crate) generation: u64,
}

/// Look up a shard by repo name and validate it is ready.
pub(crate) async fn resolve_ready_shard(
    state: &SharedState,
    repo: &str,
) -> Result<ReadyShard, AppError> {
    let shards = state.shards.read().await;
    let shard = shards.get(repo).ok_or_else(AppError::repo_not_found)?;
    if shard.status != ShardStatus::Ready {
        return Err(AppError::repo_not_ready(repo, &format!("{:?}", shard.status)));
    }
    Ok(ReadyShard {
        repo_id: shard.repo_id.clone(),
        repo_root: shard.repo_root.clone(),
        commit_sha: shard.commit_sha.clone(),
        generation: shard.generation.value(),
    })
}

fn utc_log_timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    let format = time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
        .expect("static format");
    now.format(&format).unwrap_or_else(|_| "unknown".into())
}

async fn query_with_cache(
    state: &SharedState,
    repo_id: &str,
    repo_root: &str,
    generation: u64,
    commit_sha: &Option<String>,
    params: &QueryParams,
    node_type_priors: Option<HashMap<NodeType, f64>>,
) -> Result<(QueryResult, bool), AppError> {
    let cache_key = serde_json::to_string(params).map_err(AppError::internal)?;
    if let Some(result) = state
        .get_cached_query(repo_id, &cache_key, generation)
        .await
    {
        state
            .metrics
            .query_cache_hits
            .fetch_add(1, Ordering::Relaxed);
        return Ok((result, true));
    }

    state
        .metrics
        .query_cache_misses
        .fetch_add(1, Ordering::Relaxed);

    let cached_index = state
        .get_or_open_index(repo_id, repo_root, generation)
        .await
        .map_err(AppError::from)?;

    let params = params.clone();
    let commit_sha = commit_sha.clone();
    let result = tokio::task::spawn_blocking(move || {
        let index = cached_index.lock_index()?;
        let query = params.to_query()?;
        let mut options = params.to_options();
        if options.node_type_priors.is_none() {
            options.node_type_priors = node_type_priors;
        }
        let mut result = execute_query_with_options(&query, &index, options)?;
        for handle in &mut result.handles {
            handle.source = HandleSource::Service;
            handle.commit_sha = commit_sha.clone();
            handle.generation = Some(generation);
        }
        Ok::<_, canopy_core::CanopyError>(result)
    })
    .await
    .map_err(AppError::internal)??;

    if !result.auto_expanded {
        state
            .insert_cached_query(repo_id, cache_key, result.clone(), generation)
            .await;
    }

    Ok((result, false))
}

#[cfg(test)]
pub(super) fn test_state() -> SharedState {
    std::sync::Arc::new(crate::state::AppState::new())
}

#[cfg(test)]
pub(super) async fn insert_test_shard(
    state: &SharedState,
    repo_id: &str,
    name: &str,
    status: canopy_core::ShardStatus,
    generation: canopy_core::Generation,
) {
    let mut shards = state.shards.write().await;
    shards.insert(
        repo_id.to_string(),
        canopy_core::RepoShard {
            repo_id: repo_id.to_string(),
            repo_root: "/tmp/fake".to_string(),
            name: name.to_string(),
            commit_sha: None,
            generation,
            status,
            error_message: None,
        },
    );
}
