//! HTTP route handlers for the canopy service.

use crate::error::AppError;
use crate::evidence::{normalize_query_params, reorder_expand_suggestions, run_evidence_plan};
use crate::feedback_recording::{try_record_feedback_expand, try_record_feedback_query};
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::{
    build_evidence_pack, query::execute_query_with_options, EvidencePack, Generation, HandleSource,
    NodeType, QueryParams, QueryResult, RepoIndex, RepoShard, ShardStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

fn timestamp() -> String {
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
        let index = cached_index.index.lock().map_err(|err| {
            canopy_core::CanopyError::Io(std::io::Error::other(format!(
                "Index mutex poisoned: {err}"
            )))
        })?;
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

// ---------------------------------------------------------------------------
// POST /query
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct QueryRequest {
    pub repo: String,
    #[serde(flatten)]
    pub params: QueryParams,
}

pub async fn query(
    State(state): State<SharedState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResult>, AppError> {
    let start = Instant::now();
    let repo_label = req.repo.clone();

    let shards = state.shards.read().await;
    let shard = shards.get(&req.repo).ok_or_else(AppError::repo_not_found)?;

    if shard.status != ShardStatus::Ready {
        return Err(AppError::internal(format!(
            "Repo {} is not ready (status: {:?})",
            req.repo, shard.status
        )));
    }

    let repo_id = shard.repo_id.clone();
    let repo_root = shard.repo_root.clone();
    let commit_sha = shard.commit_sha.clone();
    let generation = shard.generation.value();
    drop(shards);

    let params = normalize_query_params(req.params, false);
    let feedback_store = state.feedback_store_for_repo(&repo_id, &repo_root).await;
    let node_type_priors = state.load_node_type_priors(&repo_id, &repo_root).await;

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
            .entry(repo_id.clone())
            .or_insert(0) += 1;
    }

    state.metrics.query_count.fetch_add(1, Ordering::Relaxed);

    let (result, was_hit) = query_with_cache(
        &state,
        &repo_id,
        &repo_root,
        generation,
        &commit_sha,
        &params,
        node_type_priors,
    )
    .await?;

    if let Some(query_event_id) =
        try_record_feedback_query(feedback_store.as_ref(), &params, &result)
    {
        let handle_ids: Vec<String> = result.handles.iter().map(|h| h.id.to_string()).collect();
        state
            .remember_query_event_for_handles(&repo_id, &handle_ids, query_event_id)
            .await;
        state.invalidate_node_type_priors_cache(&repo_id).await;
    }

    let duration_ms = start.elapsed().as_millis();
    state
        .metrics
        .total_query_ms
        .fetch_add(duration_ms as u64, Ordering::Relaxed);
    let cache_state = if was_hit { "hit" } else { "miss" };
    eprintln!(
        "[{}] POST /query repo={} duration_ms={} cache={}",
        timestamp(),
        repo_label,
        duration_ms,
        cache_state
    );

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// POST /evidence_pack
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EvidencePackRequest {
    pub repo: String,
    #[serde(flatten)]
    pub params: QueryParams,
    #[serde(default)]
    pub max_handles: Option<usize>,
    #[serde(default)]
    pub max_per_file: Option<usize>,
    #[serde(default)]
    pub plan: Option<bool>,
}

pub async fn evidence_pack(
    State(state): State<SharedState>,
    Json(req): Json<EvidencePackRequest>,
) -> Result<Json<EvidencePack>, AppError> {
    let start = Instant::now();
    let repo_label = req.repo.clone();

    let shards = state.shards.read().await;
    let shard = shards.get(&req.repo).ok_or_else(AppError::repo_not_found)?;

    if shard.status != ShardStatus::Ready {
        return Err(AppError::internal(format!(
            "Repo {} is not ready (status: {:?})",
            req.repo, shard.status
        )));
    }

    let repo_id = shard.repo_id.clone();
    let repo_root = shard.repo_root.clone();
    let commit_sha = shard.commit_sha.clone();
    let generation = shard.generation.value();
    drop(shards);

    let seed_params = normalize_query_params(req.params, true);
    let max_handles = req.max_handles.unwrap_or(8).clamp(1, 64);
    let max_per_file = req.max_per_file.unwrap_or(2).clamp(1, 8);

    let feedback_store = state.feedback_store_for_repo(&repo_id, &repo_root).await;
    let node_type_priors = state.load_node_type_priors(&repo_id, &repo_root).await;

    state.metrics.query_count.fetch_add(1, Ordering::Relaxed);

    let plan_state = state.clone();
    let plan_repo_id = repo_id.clone();
    let plan_repo_root = repo_root.clone();
    let plan_commit_sha = commit_sha.clone();
    let plan_override = req.plan;
    let plan_result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        run_evidence_plan(
            seed_params,
            plan_override,
            max_handles,
            max_per_file,
            |params| {
                rt.block_on(query_with_cache(
                    &plan_state,
                    &plan_repo_id,
                    &plan_repo_root,
                    generation,
                    &plan_commit_sha,
                    params,
                    node_type_priors.clone(),
                ))
            },
        )
    })
    .await
    .map_err(AppError::internal)??;

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
            .remember_query_event_for_handles(&repo_id, &handle_ids, query_event_id)
            .await;
        state.invalidate_node_type_priors_cache(&repo_id).await;
    }

    let mut pack = build_evidence_pack(
        &plan_result.result,
        &plan_result.query_text,
        max_handles,
        max_per_file,
    );
    let suggested_ids = pack.expand_suggestion.clone();
    let recent_expanded = state
        .recent_expanded_handle_ids(&repo_id, &suggested_ids)
        .await;
    reorder_expand_suggestions(&mut pack, &recent_expanded);

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
    eprintln!(
        "[{}] POST /evidence_pack repo={} duration_ms={} cache={} plan={} steps={} selected={}",
        timestamp(),
        repo_label,
        duration_ms,
        cache_state,
        plan_result.planning_enabled,
        plan_result.plan_steps,
        pack.selected_count
    );

    Ok(Json(pack))
}

// ---------------------------------------------------------------------------
// POST /expand
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ExpandRequest {
    pub repo: String,
    pub handles: Vec<ExpandHandle>,
}

#[derive(Deserialize)]
pub struct ExpandHandle {
    pub id: String,
    #[serde(default)]
    pub generation: Option<u64>,
}

#[derive(Serialize)]
pub struct ExpandResponse {
    pub contents: Vec<ExpandedContent>,
}

#[derive(Serialize)]
pub struct ExpandedContent {
    pub handle_id: String,
    pub content: String,
}

pub async fn expand(
    State(state): State<SharedState>,
    Json(req): Json<ExpandRequest>,
) -> Result<Json<ExpandResponse>, AppError> {
    let start = Instant::now();
    let repo_label = req.repo.clone();
    let handle_count = req.handles.len();

    let shards = state.shards.read().await;
    let shard = shards.get(&req.repo).ok_or_else(AppError::repo_not_found)?;

    if shard.status != ShardStatus::Ready {
        return Err(AppError::internal(format!(
            "Repo {} is not ready (status: {:?})",
            req.repo, shard.status
        )));
    }

    let repo_id = shard.repo_id.clone();
    let repo_root = shard.repo_root.clone();
    let current_gen = shard.generation.value();
    // Validate generation if provided
    for h in &req.handles {
        if let Some(gen) = h.generation {
            if gen != current_gen {
                return Err(AppError::stale(current_gen, gen));
            }
        }
    }
    drop(shards);
    let feedback_store = state.feedback_store_for_repo(&repo_id, &repo_root).await;

    state.metrics.expand_count.fetch_add(1, Ordering::Relaxed);

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
        let index = cached_index.index.lock().map_err(|err| {
            canopy_core::CanopyError::Io(std::io::Error::other(format!(
                "Index mutex poisoned: {err}"
            )))
        })?;
        index.expand_with_details(&handle_ids)
    })
    .await
    .map_err(AppError::internal)??;

    // Track expanded file paths
    if let Ok(mut analytics) = state.metrics.analytics.lock() {
        for (_id, path, _node_type, _token_count, _content) in &expanded_details {
            *analytics
                .top_expanded_files
                .entry(path.clone())
                .or_insert(0) += 1;
        }
    }
    let expanded_ids: Vec<String> = expanded_details
        .iter()
        .map(|(id, _path, _node_type, _token_count, _content)| id.clone())
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
    eprintln!(
        "[{}] POST /expand repo={} duration_ms={} handles={}",
        timestamp(),
        repo_label,
        duration_ms,
        handle_count
    );

    Ok(Json(ExpandResponse {
        contents: expanded_details
            .into_iter()
            .map(
                |(id, _path, _node_type, _token_count, content)| ExpandedContent {
                    handle_id: id,
                    content,
                },
            )
            .collect(),
    }))
}

// ---------------------------------------------------------------------------
// POST /repos/add
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AddRepoRequest {
    pub path: String,
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct AddRepoResponse {
    pub repo_id: String,
    pub name: String,
}

pub async fn add_repo(
    State(state): State<SharedState>,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<AddRepoResponse>, AppError> {
    let path = std::path::Path::new(&req.path);

    // Validate it's a git repo
    if !path.join(".git").exists() {
        return Err(AppError {
            status: axum::http::StatusCode::BAD_REQUEST,
            body: crate::error::ErrorEnvelope::new(
                "invalid_repo",
                "Not a git repository",
                "Provide a path to a git repository root",
            ),
        });
    }

    // Canonicalize path ONCE before taking the lock
    let canonical = std::fs::canonicalize(&req.path)
        .map_err(AppError::internal)?
        .to_string_lossy()
        .to_string();

    // Init canopy if needed
    if !path.join(".canopy").exists() {
        tokio::task::spawn_blocking({
            let canonical = canonical.clone();
            move || RepoIndex::init(Path::new(&canonical))
        })
        .await
        .map_err(AppError::internal)??;
    }

    let name = req.name.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string())
    });

    // Idempotent: check if a shard with the same canonical root already exists
    let mut shards = state.shards.write().await;
    for (id, shard) in shards.iter() {
        if shard.repo_root == canonical {
            eprintln!(
                "[{}] POST /repos/add name={} repo_id={} (existing)",
                timestamp(),
                shard.name,
                id
            );
            return Ok(Json(AddRepoResponse {
                repo_id: id.clone(),
                name: shard.name.clone(),
            }));
        }
    }

    let repo_id = uuid::Uuid::new_v4().to_string();

    let shard = RepoShard {
        repo_id: repo_id.clone(),
        repo_root: canonical,
        name: name.clone(),
        commit_sha: None,
        generation: Generation::new(),
        status: ShardStatus::Pending,
    };

    shards.insert(repo_id.clone(), shard);
    drop(shards);

    eprintln!(
        "[{}] POST /repos/add name={} repo_id={}",
        timestamp(),
        name,
        repo_id
    );

    Ok(Json(AddRepoResponse { repo_id, name }))
}

// ---------------------------------------------------------------------------
// GET /repos
// ---------------------------------------------------------------------------

pub async fn list_repos(State(state): State<SharedState>) -> Json<Vec<RepoShard>> {
    let shards = state.shards.read().await;
    Json(shards.values().cloned().collect())
}

// ---------------------------------------------------------------------------
// GET /status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ServiceStatus {
    pub service: String,
    pub repos: Vec<RepoShard>,
}

pub async fn status(State(state): State<SharedState>) -> Json<ServiceStatus> {
    let shards = state.shards.read().await;
    Json(ServiceStatus {
        service: "canopy-service".to_string(),
        repos: shards.values().cloned().collect(),
    })
}

// ---------------------------------------------------------------------------
// POST /reindex
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReindexRequest {
    pub repo: String,
    pub glob: Option<String>,
}

#[derive(Serialize)]
pub struct ReindexResponse {
    pub generation: u64,
    pub status: String,
    pub commit_sha: Option<String>,
}

pub async fn reindex(
    State(state): State<SharedState>,
    Json(req): Json<ReindexRequest>,
) -> Result<Json<ReindexResponse>, AppError> {
    let repo_label = req.repo.clone();
    let mut shards = state.shards.write().await;
    let shard = shards
        .get_mut(&req.repo)
        .ok_or_else(AppError::repo_not_found)?;

    // Coalesce: if already indexing, return current generation
    if shard.status == ShardStatus::Indexing {
        eprintln!(
            "[{}] POST /reindex repo={} status=already_indexing",
            timestamp(),
            repo_label
        );
        return Ok(Json(ReindexResponse {
            generation: shard.generation.value(),
            status: "already_indexing".to_string(),
            commit_sha: shard.commit_sha.clone(),
        }));
    }

    shard.status = ShardStatus::Indexing;
    let repo_root = shard.repo_root.clone();
    let repo_id = shard.repo_id.clone();
    let glob = req.glob;
    drop(shards);

    state.metrics.reindex_count.fetch_add(1, Ordering::Relaxed);
    eprintln!(
        "[{}] POST /reindex repo={} status=started",
        timestamp(),
        repo_label
    );

    let state_clone = state.clone();

    tokio::task::spawn(async move {
        let result = tokio::task::spawn_blocking({
            let repo_root = repo_root.clone();
            let glob = glob.clone();
            move || {
                let commit_sha = canopy_core::git::head_commit_sha(Path::new(&repo_root));

                let mut index = RepoIndex::open(Path::new(&repo_root))?;
                let default_glob = index.config().default_glob().to_string();
                let glob_str = glob.as_deref().unwrap_or(&default_glob);
                let _stats = index.index(glob_str)?;

                Ok::<_, canopy_core::CanopyError>(commit_sha)
            }
        })
        .await;

        match result {
            Ok(Ok(commit_sha)) => {
                state_clone.invalidate_repo(&repo_id).await;
                let mut shards = state_clone.shards.write().await;
                if let Some(shard) = shards.get_mut(&repo_id) {
                    shard.generation = shard.generation.next();
                    shard.commit_sha = commit_sha;
                    shard.status = ShardStatus::Ready;
                }
            }
            _ => {
                let mut shards = state_clone.shards.write().await;
                if let Some(shard) = shards.get_mut(&repo_id) {
                    shard.status = ShardStatus::Error;
                }
            }
        }
    });

    // Return current state (indexing has started)
    let shards = state.shards.read().await;
    let shard = shards.get(&req.repo).ok_or_else(AppError::repo_not_found)?;
    Ok(Json(ReindexResponse {
        generation: shard.generation.value(),
        status: "indexing".to_string(),
        commit_sha: shard.commit_sha.clone(),
    }))
}
