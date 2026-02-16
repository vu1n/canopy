use crate::error::AppError;
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::feedback::{ExpandEvent, FeedbackStore, QueryEvent, QueryHandle};
use canopy_core::{
    build_evidence_pack, index::ExpandedHandleDetail, query::execute_query_with_options,
    EvidenceConfidence, EvidencePack, Generation, Handle, HandleSource, MatchMode, NodeType,
    QueryKind, QueryParams, QueryResult, RepoIndex, RepoShard, ShardStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

const EVIDENCE_PLAN_MAX_STEPS: usize = 3;
const EVIDENCE_PLAN_SYMBOLS_PER_STEP: usize = 1;
const EVIDENCE_PLAN_MIN_NEW_HANDLES: usize = 2;
const SERVICE_DEFAULT_QUERY_LIMIT: usize = 16;
const SERVICE_MAX_QUERY_LIMIT: usize = 50;

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

fn split_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for term in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
    {
        if seen.insert(term.to_string()) {
            out.push(term.to_string());
        }
    }
    out
}

fn pattern_fallback_params(params: &QueryParams) -> Option<QueryParams> {
    if params.patterns.is_some()
        || params.symbol.is_some()
        || params.section.is_some()
        || params.parent.is_some()
    {
        return None;
    }

    let pattern = params.pattern.as_ref()?;
    let terms = split_terms(pattern);
    if terms.len() <= 1 {
        return None;
    }

    let mut fallback = params.clone();
    fallback.pattern = None;
    fallback.patterns = Some(terms);
    fallback.match_mode = MatchMode::Any;
    Some(fallback)
}

fn extract_symbol_candidates_from_handles(
    handles: &[Handle],
    query_text: &str,
    limit: usize,
) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "the",
        "and",
        "for",
        "with",
        "from",
        "this",
        "that",
        "auth",
        "authentication",
        "token",
        "user",
        "users",
        "request",
        "response",
        "middleware",
        "handler",
        "function",
        "class",
        "method",
        "const",
        "let",
        "return",
        "true",
        "false",
        "null",
        "undefined",
    ];

    let query_terms: HashSet<String> = split_terms(query_text).into_iter().collect();
    let stop_words: HashSet<String> = STOP_WORDS.iter().map(|w| w.to_string()).collect();
    let mut scores: HashMap<String, usize> = HashMap::new();

    for handle in handles.iter().take(8) {
        for token in handle
            .preview
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 4)
        {
            if token
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            {
                continue;
            }
            let lower = token.to_lowercase();
            if query_terms.contains(&lower) || stop_words.contains(&lower) {
                continue;
            }

            // Prefer identifier-like tokens over plain words.
            let mut weight = 1usize;
            if token.chars().any(|c| c.is_uppercase()) {
                weight += 2;
            }
            if token.contains('_') {
                weight += 1;
            }
            *scores.entry(token.to_string()).or_insert(0) += weight;
        }
    }

    let mut ranked: Vec<(String, usize)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.len().cmp(&a.0.len())));
    ranked.into_iter().take(limit).map(|(sym, _)| sym).collect()
}

fn symbol_followup_params(base: &QueryParams, symbol: String) -> QueryParams {
    let mut params = QueryParams::symbol(symbol);
    params.kind = QueryKind::Definition;
    params.limit = Some(base.limit.unwrap_or(16).min(12));
    params.glob = base.glob.clone();
    params
}

fn normalize_query_params(mut params: QueryParams, force_preview_only: bool) -> QueryParams {
    let limit = params
        .limit
        .unwrap_or(SERVICE_DEFAULT_QUERY_LIMIT)
        .clamp(1, SERVICE_MAX_QUERY_LIMIT);
    params.limit = Some(limit);
    if force_preview_only {
        params.expand_budget = Some(0);
    }
    params
}

fn reorder_expand_suggestions(pack: &mut EvidencePack, recent_expanded: &HashSet<String>) {
    if pack.expand_suggestion.is_empty() || recent_expanded.is_empty() {
        return;
    }

    let mut fresh = Vec::new();
    let mut repeated = Vec::new();
    for id in &pack.expand_suggestion {
        if recent_expanded.contains(id) {
            repeated.push(id.clone());
        } else {
            fresh.push(id.clone());
        }
    }

    if fresh.is_empty() {
        for handle in &pack.handles {
            if fresh.len() >= pack.expand_suggestion.len() {
                break;
            }
            if recent_expanded.contains(&handle.id) {
                continue;
            }
            if !fresh.iter().any(|id| id == &handle.id) {
                fresh.push(handle.id.clone());
            }
        }
    }

    if !fresh.is_empty() {
        fresh.extend(repeated);
        fresh.truncate(pack.expand_suggestion.len());
        pack.expand_suggestion = fresh;
    }
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

    let params = normalize_query_params(req.params, false);
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

// POST /evidence_pack
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
    let plan_override = req.plan;
    let mut planning_enabled = matches!(plan_override, Some(true));
    let mut auto_plan_decided = plan_override.is_some();
    let query_text = query_params_text(&seed_params);
    let max_handles = req.max_handles.unwrap_or(8).clamp(1, 64);
    let max_per_file = req.max_per_file.unwrap_or(2).clamp(1, 8);

    let feedback_store = state.feedback_store_for_repo(&repo_id, &repo_root).await;
    let node_type_priors = state.load_node_type_priors(&repo_id, &repo_root).await;

    state.metrics.query_count.fetch_add(1, Ordering::Relaxed);
    let mut pending: VecDeque<QueryParams> = VecDeque::from([seed_params.clone()]);
    let mut seen_param_keys: HashSet<String> = HashSet::new();
    let mut seen_handle_ids: HashSet<String> = HashSet::new();
    let mut aggregate_handles: Vec<Handle> = Vec::new();
    let mut aggregate_tokens = 0usize;
    let mut aggregate_truncated = false;
    let mut total_matches = 0usize;
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;
    let mut plan_steps = 0usize;

    while let Some(current_params) = pending.pop_front() {
        let max_steps = if planning_enabled {
            EVIDENCE_PLAN_MAX_STEPS
        } else {
            2
        };
        if plan_steps >= max_steps {
            break;
        }
        let key = serde_json::to_string(&current_params).map_err(AppError::internal)?;
        if !seen_param_keys.insert(key) {
            continue;
        }
        plan_steps += 1;

        let (result, was_hit) = query_with_cache(
            &state,
            &repo_id,
            &repo_root,
            generation,
            &commit_sha,
            &current_params,
            node_type_priors.clone(),
        )
        .await?;
        if was_hit {
            cache_hits += 1;
        } else {
            cache_misses += 1;
        }

        total_matches += result.total_matches;
        aggregate_truncated |= result.truncated;

        let mut new_handle_count = 0usize;
        for handle in result.handles {
            if seen_handle_ids.insert(handle.id.to_string()) {
                aggregate_tokens += handle.token_count;
                aggregate_handles.push(handle);
                new_handle_count += 1;
            }
        }

        let expanded_handle_ids: Vec<String> = aggregate_handles
            .iter()
            .filter(|h| h.content.is_some())
            .map(|h| h.id.to_string())
            .collect();
        let provisional = QueryResult {
            handles: aggregate_handles.clone(),
            ref_handles: None,
            total_tokens: aggregate_tokens,
            truncated: aggregate_truncated,
            total_matches,
            auto_expanded: false,
            expand_note: None,
            expanded_count: expanded_handle_ids.len(),
            expanded_tokens: aggregate_handles
                .iter()
                .filter(|h| h.content.is_some())
                .map(|h| h.token_count)
                .sum(),
            expanded_handle_ids,
        };
        let provisional_pack =
            build_evidence_pack(&provisional, &query_text, max_handles, max_per_file);

        if !auto_plan_decided {
            planning_enabled = provisional_pack.guidance.confidence_band == EvidenceConfidence::Low
                && !provisional_pack.guidance.stop_querying;
            auto_plan_decided = true;
        }

        if let Some(fallback) = pattern_fallback_params(&current_params) {
            let fallback_key = serde_json::to_string(&fallback).map_err(AppError::internal)?;
            let allow_fallback = if planning_enabled {
                true
            } else {
                plan_steps == 1 && aggregate_handles.is_empty()
            };
            if allow_fallback && !seen_param_keys.contains(&fallback_key) {
                pending.push_back(fallback);
            }
        }

        if !planning_enabled {
            continue;
        }

        if new_handle_count < EVIDENCE_PLAN_MIN_NEW_HANDLES {
            continue;
        }
        if provisional_pack.guidance.stop_querying
            && provisional_pack.selected_count >= max_handles.min(4)
        {
            break;
        }

        let symbol_candidates = extract_symbol_candidates_from_handles(
            &provisional.handles,
            &query_text,
            EVIDENCE_PLAN_SYMBOLS_PER_STEP,
        );
        for symbol in symbol_candidates {
            let followup = symbol_followup_params(&seed_params, symbol);
            let followup_key = serde_json::to_string(&followup).map_err(AppError::internal)?;
            if !seen_param_keys.contains(&followup_key) {
                pending.push_back(followup);
            }
        }
    }

    let expanded_handle_ids: Vec<String> = aggregate_handles
        .iter()
        .filter(|h| h.content.is_some())
        .map(|h| h.id.to_string())
        .collect();
    let expanded_count = expanded_handle_ids.len();
    let expanded_tokens: usize = aggregate_handles
        .iter()
        .filter(|h| h.content.is_some())
        .map(|h| h.token_count)
        .sum();
    let auto_expanded = !aggregate_handles.is_empty() && expanded_count == aggregate_handles.len();
    let result = QueryResult {
        handles: aggregate_handles,
        ref_handles: None,
        total_tokens: aggregate_tokens,
        truncated: aggregate_truncated,
        total_matches,
        auto_expanded,
        expand_note: None,
        expanded_count,
        expanded_tokens,
        expanded_handle_ids,
    };

    if let Some(query_event_id) =
        try_record_feedback_query(feedback_store.as_ref(), &seed_params, &result)
    {
        let handle_ids: Vec<String> = result.handles.iter().map(|h| h.id.to_string()).collect();
        state
            .remember_query_event_for_handles(&repo_id, &handle_ids, query_event_id)
            .await;
        state.invalidate_node_type_priors_cache(&repo_id).await;
    }

    let mut pack = build_evidence_pack(&result, &query_text, max_handles, max_per_file);
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
    let cache_state = if cache_hits > 0 && cache_misses > 0 {
        "mixed"
    } else if cache_hits > 0 {
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
        planning_enabled,
        plan_steps,
        pack.selected_count
    );

    Ok(Json(pack))
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
