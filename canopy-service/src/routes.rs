use crate::error::AppError;
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::feedback::{ExpandEvent, FeedbackStore, QueryEvent, QueryHandle};
use canopy_core::{
    index::ExpandedHandleDetail, query::execute_query_with_options, Generation, HandleSource,
    QueryParams, QueryResult, RepoIndex, RepoShard, ShardStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    // UTC timestamp, good enough without chrono
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        1970 + secs / 31557600,
        ((secs % 31557600) / 2629800) + 1,
        ((secs % 2629800) / 86400) + 1,
        hours,
        mins,
        s
    )
}

fn query_params_text(params: &QueryParams) -> String {
    let mut parts = Vec::new();
    if let Some(s) = &params.pattern {
        parts.push(s.clone());
    }
    if let Some(ss) = &params.patterns {
        parts.extend(ss.clone());
    }
    if let Some(s) = &params.symbol {
        parts.push(s.clone());
    }
    if let Some(s) = &params.section {
        parts.push(s.clone());
    }
    if let Some(s) = &params.parent {
        parts.push(s.clone());
    }
    if let Some(s) = &params.glob {
        parts.push(s.clone());
    }
    parts.join(" ")
}

fn try_record_feedback_query(
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
        query_text: query_params_text(params),
        predicted_globs: None,
        files_indexed: 0,
        handles_returned: result.handles.len(),
        total_tokens: result.total_tokens,
    };

    let Ok(query_event_id) = store.record_query_event(&event) else {
        return None;
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
    let _ = store.record_query_handles(query_event_id, &handles);

    for handle in result.handles.iter().filter(|h| h.content.is_some()) {
        let _ = store.record_expand_event(&ExpandEvent {
            query_event_id: Some(query_event_id),
            handle_id: handle.id.to_string(),
            file_path: handle.file_path.clone(),
            node_type: handle.node_type,
            token_count: handle.token_count,
            auto_expanded: true,
        });
    }

    Some(query_event_id)
}

fn try_record_feedback_expand(
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
        let _ = store.record_expand_event(&ExpandEvent {
            query_event_id: recent_query_event_ids.get(handle_id).copied(),
            handle_id: handle_id.clone(),
            file_path: file_path.clone(),
            node_type: *node_type,
            token_count: *token_count,
            auto_expanded: false,
        });
        wrote_any = true;
    }

    wrote_any
}

// POST /query
#[derive(Deserialize)]
pub struct QueryRequest {
    pub repo: String,
    #[serde(flatten)]
    pub params: QueryParams,
}

pub async fn query(
    State(state): State<SharedState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<canopy_core::QueryResult>, AppError> {
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

    let params = req.params;
    let params_for_feedback = params.clone();
    let cache_key = serde_json::to_string(&params).map_err(AppError::internal)?;
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
            .queries_by_repo
            .entry(repo_id.clone())
            .or_insert(0) += 1;
    }

    state.metrics.query_count.fetch_add(1, Ordering::Relaxed);

    if let Some(result) = state
        .get_cached_query(&repo_id, &cache_key, generation)
        .await
    {
        if let Some(query_event_id) =
            try_record_feedback_query(feedback_store.as_ref(), &params, &result)
        {
            let handle_ids: Vec<String> = result.handles.iter().map(|h| h.id.to_string()).collect();
            state
                .remember_query_event_for_handles(&repo_id, &handle_ids, query_event_id)
                .await;
            state.invalidate_node_type_priors_cache(&repo_id).await;
        }
        state
            .metrics
            .query_cache_hits
            .fetch_add(1, Ordering::Relaxed);
        let duration_ms = start.elapsed().as_millis();
        state
            .metrics
            .total_query_ms
            .fetch_add(duration_ms as u64, Ordering::Relaxed);
        eprintln!(
            "[{}] POST /query repo={} duration_ms={} cache=hit",
            timestamp(),
            repo_label,
            duration_ms
        );
        return Ok(Json(result));
    }

    state
        .metrics
        .query_cache_misses
        .fetch_add(1, Ordering::Relaxed);

    let cached_index = state
        .get_or_open_index(&repo_id, &repo_root, generation)
        .await
        .map_err(AppError::from)?;

    // Run blocking index operations in spawn_blocking
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
        // Stamp handles with service metadata
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
            .insert_cached_query(&repo_id, cache_key, result.clone(), generation)
            .await;
    }
    if let Some(query_event_id) =
        try_record_feedback_query(feedback_store.as_ref(), &params_for_feedback, &result)
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
    eprintln!(
        "[{}] POST /query repo={} duration_ms={} cache=miss",
        timestamp(),
        repo_label,
        duration_ms
    );

    Ok(Json(result))
}

// POST /expand
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
            .queries_by_repo
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

// POST /repos/add
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

// GET /repos
pub async fn list_repos(State(state): State<SharedState>) -> Json<Vec<RepoShard>> {
    let shards = state.shards.read().await;
    Json(shards.values().cloned().collect())
}

// GET /status
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

// POST /reindex
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
                // Get commit SHA
                let commit_sha = std::process::Command::new("git")
                    .arg("rev-parse")
                    .arg("HEAD")
                    .current_dir(&repo_root)
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

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

// GET /metrics
const TOP_N: usize = 20;

#[derive(Serialize)]
pub struct MetricsResponse {
    pub performance: PerformanceMetrics,
    pub analytics: AnalyticsMetrics,
}

#[derive(Serialize)]
pub struct PerformanceMetrics {
    pub queries: u64,
    pub query_cache_hit_rate: f64,
    pub query_cache_hits: u64,
    pub query_cache_misses: u64,
    pub expands: u64,
    pub index_cache_hits: u64,
    pub index_cache_misses: u64,
    pub reindexes: u64,
    pub avg_query_ms: u64,
    pub avg_expand_ms: u64,
}

#[derive(Serialize)]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct PatternCount {
    pub pattern: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct PathCount {
    pub path: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct AnalyticsMetrics {
    pub top_symbols: Vec<NamedCount>,
    pub top_patterns: Vec<PatternCount>,
    pub top_expanded_files: Vec<PathCount>,
    pub queries_by_repo: std::collections::HashMap<String, u64>,
    pub feedback_by_repo: std::collections::HashMap<String, FeedbackSummary>,
}

#[derive(Serialize, Clone)]
pub struct FeedbackSummary {
    pub glob_hit_rate_at_k: f64,
    pub handle_expand_accept_rate: f64,
    pub avg_tokens_per_expand: f64,
    pub sample_count: usize,
}

fn top_n_sorted(map: &std::collections::HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut entries: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(n);
    entries
}

pub async fn metrics(State(state): State<SharedState>) -> Json<MetricsResponse> {
    let queries = state.metrics.query_count.load(Ordering::Relaxed);
    let query_cache_hits = state.metrics.query_cache_hits.load(Ordering::Relaxed);
    let query_cache_misses = state.metrics.query_cache_misses.load(Ordering::Relaxed);
    let expands = state.metrics.expand_count.load(Ordering::Relaxed);
    let index_cache_hits = state.metrics.index_cache_hits.load(Ordering::Relaxed);
    let index_cache_misses = state.metrics.index_cache_misses.load(Ordering::Relaxed);
    let reindexes = state.metrics.reindex_count.load(Ordering::Relaxed);
    let total_query_ms = state.metrics.total_query_ms.load(Ordering::Relaxed);
    let total_expand_ms = state.metrics.total_expand_ms.load(Ordering::Relaxed);

    let query_cache_total = query_cache_hits + query_cache_misses;
    let query_cache_hit_rate = if query_cache_total > 0 {
        query_cache_hits as f64 / query_cache_total as f64
    } else {
        0.0
    };

    let avg_query_ms = if queries > 0 {
        total_query_ms / queries
    } else {
        0
    };
    let avg_expand_ms = if expands > 0 {
        total_expand_ms / expands
    } else {
        0
    };

    let feedback_by_repo = {
        let shards = state.shards.read().await;
        let mut out = std::collections::HashMap::new();
        for shard in shards.values() {
            if let Some(store) = state
                .feedback_store_for_repo(&shard.repo_id, &shard.repo_root)
                .await
            {
                let Ok(store_guard) = store.lock() else {
                    continue;
                };
                if let Ok(m) = store_guard.compute_metrics(7.0) {
                    out.insert(
                        shard.repo_id.clone(),
                        FeedbackSummary {
                            glob_hit_rate_at_k: m.glob_hit_rate_at_k,
                            handle_expand_accept_rate: m.handle_expand_accept_rate,
                            avg_tokens_per_expand: m.avg_tokens_per_expand,
                            sample_count: m.sample_count,
                        },
                    );
                }
            }
        }
        out
    };

    let analytics = if let Ok(a) = state.metrics.analytics.lock() {
        AnalyticsMetrics {
            top_symbols: top_n_sorted(&a.top_symbols, TOP_N)
                .into_iter()
                .map(|(name, count)| NamedCount { name, count })
                .collect(),
            top_patterns: top_n_sorted(&a.top_patterns, TOP_N)
                .into_iter()
                .map(|(pattern, count)| PatternCount { pattern, count })
                .collect(),
            top_expanded_files: top_n_sorted(&a.top_expanded_files, TOP_N)
                .into_iter()
                .map(|(path, count)| PathCount { path, count })
                .collect(),
            queries_by_repo: a.queries_by_repo.clone(),
            feedback_by_repo: feedback_by_repo.clone(),
        }
    } else {
        AnalyticsMetrics {
            top_symbols: Vec::new(),
            top_patterns: Vec::new(),
            top_expanded_files: Vec::new(),
            queries_by_repo: std::collections::HashMap::new(),
            feedback_by_repo,
        }
    };

    Json(MetricsResponse {
        performance: PerformanceMetrics {
            queries,
            query_cache_hit_rate,
            query_cache_hits,
            query_cache_misses,
            expands,
            index_cache_hits,
            index_cache_misses,
            reindexes,
            avg_query_ms,
            avg_expand_ms,
        },
        analytics,
    })
}
