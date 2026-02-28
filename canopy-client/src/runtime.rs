//! Shared mode orchestration for CLI and MCP
//!
//! Runtime fully owns both modes — no mode branching leaked to callers.

use crate::dirty;
use crate::merge;
use crate::predict::{
    extract_extensions_from_glob, predict_globs, predict_globs_with_feedback, LARGE_REPO_THRESHOLD,
    MAX_PREDICTIVE_FILES,
};
use crate::provenance::{HandleProvenance, ProvenanceTracker};
use crate::service_client::{is_error_code, ReindexResponse, ServiceClient, ServiceStatus};
use canopy_core::index::ExpandedHandleDetail;
use canopy_core::{
    build_evidence_pack,
    feedback::{ExpandEvent, FeedbackStore, QueryEvent, QueryHandle},
    EvidencePack, HandleSource, IndexStats, NodeType, QueryOptions, QueryParams, QueryResult,
    RepoIndex, RepoShard,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

const ENSURE_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const NODE_TYPE_PRIOR_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Input for a query — either structured params or DSL string
pub enum QueryInput {
    /// Structured query params (works with both local and service)
    Params(QueryParams),
    /// DSL s-expression (local-only; service mode falls back to local index)
    Dsl(String, QueryOptions),
}

/// Result of an index/reindex operation
pub enum IndexResult {
    Local(IndexStats),
    Service(ReindexResponse),
}

/// Outcome of an expand operation — supports partial success
pub struct ExpandOutcome {
    pub contents: Vec<(String, String)>,
    pub failed_ids: Vec<String>,
}

struct PendingPredictiveContext {
    predicted_globs: Vec<String>,
    files_indexed: usize,
    file_to_glob: HashMap<String, String>,
}

pub struct ClientRuntime {
    service: Option<ServiceClient>,
    /// Handle provenance + expand tracking (extracted module)
    tracker: ProvenanceTracker,
    /// Track last-known generation per repo to detect changes
    repo_generations: HashMap<String, u64>,
    /// Repo-local feedback DB handles (lazy-opened)
    feedback_stores: HashMap<String, FeedbackStore>,
    /// Predictive context staged between predictive_index_for_query() and query()
    pending_predictive: HashMap<String, PendingPredictiveContext>,
    /// Cached node type priors per repo
    node_type_priors_cache: HashMap<String, (Instant, HashMap<NodeType, f64>)>,
}

impl ClientRuntime {
    /// Create a new runtime.
    ///
    /// Callers choose their own indexing strategy:
    /// - CLI `index` command: calls `runtime.index()` explicitly
    /// - MCP: calls `runtime.predictive_index_for_query()` before query
    /// - CLI query/expand: queries whatever is already indexed
    pub fn new(service_url: Option<&str>, api_key: Option<String>) -> Self {
        Self {
            service: service_url.map(|url| ServiceClient::new(url, api_key)),
            tracker: ProvenanceTracker::new(),
            repo_generations: HashMap::new(),
            feedback_stores: HashMap::new(),
            pending_predictive: HashMap::new(),
            node_type_priors_cache: HashMap::new(),
        }
    }

    pub fn is_service_mode(&self) -> bool {
        self.service.is_some()
    }

    /// Query — handles both modes internally.
    ///
    /// Service: resolve repo → ensure_ready → query → dirty detect → merge
    /// Standalone: open index → index per policy → query
    pub fn query(
        &mut self,
        repo_path: &Path,
        input: QueryInput,
    ) -> canopy_core::Result<QueryResult> {
        let query_text = Self::query_input_text(&input);

        let result = if self.service.is_some() {
            // DSL queries bypass service mode
            if let QueryInput::Dsl(ref dsl, ref opts) = input {
                eprintln!("Warning: DSL query bypasses service mode, using local index");
                let index = self.open_local_index(repo_path)?;
                let mut options = QueryOptions {
                    limit: opts.limit,
                    expand_budget: opts.expand_budget,
                    node_type_priors: opts.node_type_priors.clone(),
                };
                if options.node_type_priors.is_none() {
                    options.node_type_priors = self.load_node_type_priors(repo_path);
                }
                let result = index.query_with_options(dsl, options)?;
                self.record_provenance_for_result(
                    repo_path,
                    &result,
                    HandleSource::Local,
                    None,
                    None,
                );
                result
            } else {
                self.query_service(repo_path, input)?
            }
        } else {
            self.query_standalone(repo_path, input)?
        };

        self.record_feedback_for_query(repo_path, &query_text, &result);
        Ok(result)
    }

    /// Build a compact evidence pack for a task.
    ///
    /// Service mode with params uses server-side pack construction to reduce payload size.
    pub fn evidence_pack(
        &mut self,
        repo_path: &Path,
        input: QueryInput,
        max_handles: usize,
        max_per_file: usize,
        plan: Option<bool>,
    ) -> canopy_core::Result<EvidencePack> {
        let max_handles = max_handles.clamp(1, 64);
        let max_per_file = max_per_file.clamp(1, 8);

        if let Some(service) = self.service.as_mut() {
            if let QueryInput::Params(params_in) = &input {
                let mut params = params_in.clone();
                params.expand_budget = Some(0);

                let repo_id = service.resolve_repo_id(repo_path)?;
                let active_repo_id = match service.ensure_ready(&repo_id, ENSURE_READY_TIMEOUT) {
                    Ok(()) => repo_id,
                    Err(e) if is_error_code(&e, "repo_not_found") => {
                        let new_id = service.invalidate_and_resolve(repo_path)?;
                        service.ensure_ready(&new_id, ENSURE_READY_TIMEOUT)?;
                        new_id
                    }
                    Err(e) => return Err(e),
                };

                let (mut pack, used_repo_id) = match service.evidence_pack(
                    &active_repo_id,
                    params.clone(),
                    Some(max_handles),
                    Some(max_per_file),
                    plan,
                ) {
                    Ok(pack) => (pack, active_repo_id.clone()),
                    Err(e) if is_error_code(&e, "repo_not_found") => {
                        let new_id = service.invalidate_and_resolve(repo_path)?;
                        service.ensure_ready(&new_id, ENSURE_READY_TIMEOUT)?;
                        let pack = service.evidence_pack(
                            &new_id,
                            params,
                            Some(max_handles),
                            Some(max_per_file),
                            plan,
                        )?;
                        (pack, new_id)
                    }
                    Err(e) => return Err(e),
                };

                self.rewrite_expand_suggestions(repo_path, &mut pack);
                self.record_provenance_for_evidence_pack(repo_path, &pack, Some(used_repo_id));
                return Ok(pack);
            }
        }

        let fallback_params = match &input {
            QueryInput::Params(params) => params.pattern_fallback(),
            QueryInput::Dsl(..) => None,
        };
        let query_text = Self::query_input_text(&input);
        let result = self.query(repo_path, input)?;
        let mut pack = build_evidence_pack(&result, &query_text, max_handles, max_per_file);
        self.rewrite_expand_suggestions(repo_path, &mut pack);
        self.record_provenance_for_evidence_pack(repo_path, &pack, None);

        if pack.selected_count == 0 {
            if let Some(fallback) = fallback_params {
                let fallback_text = fallback.to_text();
                let fallback_result = self.query(repo_path, QueryInput::Params(fallback))?;
                let fallback_pack = build_evidence_pack(
                    &fallback_result,
                    &fallback_text,
                    max_handles,
                    max_per_file,
                );
                if fallback_pack.selected_count > 0 {
                    let mut fallback_pack = fallback_pack;
                    self.rewrite_expand_suggestions(repo_path, &mut fallback_pack);
                    self.record_provenance_for_evidence_pack(repo_path, &fallback_pack, None);
                    pack = fallback_pack;
                }
            }
        }
        Ok(pack)
    }

    /// Expand — pre-split by provenance, per-handle error tolerance.
    ///
    /// Service handles → batch service.expand (with generation)
    /// Local handles → batch index.expand
    /// Unknown handles → try one-by-one: local first, then service
    /// Returns ExpandOutcome with partial results; fails only if ALL handles fail
    pub fn expand(
        &mut self,
        repo_path: &Path,
        handle_ids: &[String],
    ) -> canopy_core::Result<ExpandOutcome> {
        let canonical = canonical_path(repo_path);

        // Deduplicate handle IDs
        let mut seen_ids = HashSet::new();
        let unique_handle_ids: Vec<String> = handle_ids
            .iter()
            .filter(|id| seen_ids.insert((*id).clone()))
            .cloned()
            .collect();

        // Partition by provenance
        let mut local_ids: Vec<String> = Vec::new();
        let mut service_ids: Vec<(String, Option<u64>, Option<String>)> = Vec::new();
        let mut unknown_ids: Vec<String> = Vec::new();

        for id in &unique_handle_ids {
            if let Some(prov) = self.tracker.get(&canonical, id) {
                match prov.source {
                    HandleSource::Local => local_ids.push(id.clone()),
                    HandleSource::Service => {
                        service_ids.push((id.clone(), prov.generation, prov.repo_id.clone()));
                    }
                }
            } else {
                unknown_ids.push(id.clone());
            }
        }

        // Expand each partition
        let mut contents: Vec<(String, String)> = Vec::new();
        let mut failed_ids: Vec<String> = Vec::new();

        self.expand_local_batch(repo_path, local_ids, &mut contents, &mut failed_ids);
        self.expand_service_batch(repo_path, service_ids, &mut contents, &mut failed_ids);
        self.expand_unknown(repo_path, unknown_ids, &mut contents, &mut failed_ids);

        // Record feedback
        self.record_recently_expanded(repo_path, &contents);
        if self.service.is_some() {
            // Service mode records expand feedback server-side;
            // only record local-handle expansions here to avoid double counting.
            let local_contents: Vec<(String, String)> = contents
                .iter()
                .filter_map(|(id, content)| {
                    self.tracker
                        .get(&canonical, id)
                        .filter(|prov| matches!(prov.source, HandleSource::Local))
                        .map(|_| (id.clone(), content.clone()))
                })
                .collect();
            self.record_feedback_for_expand(repo_path, &local_contents);
        } else {
            self.record_feedback_for_expand(repo_path, &contents);
        }

        if contents.is_empty() && !failed_ids.is_empty() {
            return Err(canopy_core::CanopyError::HandleNotFound(
                failed_ids.join(", "),
            ));
        }

        Ok(ExpandOutcome {
            contents,
            failed_ids,
        })
    }

    /// Expand local handles: try batch first, fall back to per-handle on failure.
    fn expand_local_batch(
        &self,
        repo_path: &Path,
        ids: Vec<String>,
        contents: &mut Vec<(String, String)>,
        failed_ids: &mut Vec<String>,
    ) {
        if ids.is_empty() {
            return;
        }
        match self.expand_local(repo_path, &ids) {
            Ok(mut c) => contents.append(&mut c),
            Err(_) => {
                for id in ids {
                    match self.expand_local(repo_path, std::slice::from_ref(&id)) {
                        Ok(c) => contents.extend(c),
                        Err(_) => failed_ids.push(id),
                    }
                }
            }
        }
    }

    /// Expand service handles: resolve repo, ensure ready, batch expand with fallbacks.
    fn expand_service_batch(
        &mut self,
        repo_path: &Path,
        service_ids: Vec<(String, Option<u64>, Option<String>)>,
        contents: &mut Vec<(String, String)>,
        failed_ids: &mut Vec<String>,
    ) {
        if service_ids.is_empty() {
            return;
        }

        let first_repo_id = service_ids.first().and_then(|(_, _, r)| r.clone());
        let Some(repo_id) = first_repo_id else {
            failed_ids.extend(service_ids.into_iter().map(|(id, _, _)| id));
            return;
        };

        let Some(service) = &self.service else {
            failed_ids.extend(service_ids.into_iter().map(|(id, _, _)| id));
            return;
        };

        // Resolve an active repo_id (re-resolving on repo_not_found)
        let active_repo_id = match service.ensure_ready(&repo_id, ENSURE_READY_TIMEOUT) {
            Ok(()) => Some(repo_id),
            Err(e) if is_error_code(&e, "repo_not_found") => {
                let service = self.service.as_mut().unwrap();
                service
                    .invalidate_and_resolve(repo_path)
                    .ok()
                    .filter(|new_id| service.ensure_ready(new_id, ENSURE_READY_TIMEOUT).is_ok())
            }
            Err(_) => None,
        };

        let Some(repo_id) = active_repo_id else {
            failed_ids.extend(service_ids.into_iter().map(|(id, _, _)| id));
            return;
        };

        let all_ids: Vec<String> = service_ids.iter().map(|(id, _, _)| id.clone()).collect();
        let batch_gen = service_ids.first().and_then(|(_, g, _)| *g);
        let service = self.service.as_ref().unwrap();

        match service.expand(&repo_id, &all_ids, batch_gen) {
            Ok(mut c) => contents.append(&mut c),
            Err(e) if is_error_code(&e, "repo_not_found") => {
                let service = self.service.as_mut().unwrap();
                let resolved = service.invalidate_and_resolve(repo_path).ok();
                for (id, gen, _) in &service_ids {
                    if let Some(ref rid) = resolved {
                        if let Ok(c) = service.expand(rid, std::slice::from_ref(id), *gen) {
                            contents.extend(c);
                            continue;
                        }
                    }
                    failed_ids.push(id.clone());
                }
            }
            Err(_) => {
                // Batch failed (e.g., mixed generations) — per-handle fallback
                for (id, gen, rid) in &service_ids {
                    let target_id = rid.as_deref().unwrap_or(&repo_id);
                    match service.expand(target_id, std::slice::from_ref(id), *gen) {
                        Ok(c) => contents.extend(c),
                        Err(_) => failed_ids.push(id.clone()),
                    }
                }
            }
        }
    }

    /// Expand handles with unknown provenance: try local first, then service.
    fn expand_unknown(
        &mut self,
        repo_path: &Path,
        ids: Vec<String>,
        contents: &mut Vec<(String, String)>,
        failed_ids: &mut Vec<String>,
    ) {
        for id in ids {
            if let Ok(c) = self.expand_local(repo_path, std::slice::from_ref(&id)) {
                contents.extend(c);
                continue;
            }
            if let Some(service) = &mut self.service {
                if let Ok(repo_id) = service.resolve_repo_id(repo_path) {
                    if service.ensure_ready(&repo_id, ENSURE_READY_TIMEOUT).is_ok() {
                        if let Ok(c) = service.expand(&repo_id, std::slice::from_ref(&id), None) {
                            contents.extend(c);
                            continue;
                        }
                    }
                }
            }
            failed_ids.push(id);
        }
    }

    /// Index/reindex — ensure_ready NOT called here (would deadlock on first index)
    pub fn index(
        &mut self,
        repo_path: &Path,
        glob: Option<&str>,
    ) -> canopy_core::Result<IndexResult> {
        if let Some(service) = &mut self.service {
            let repo_id = service.resolve_repo_id(repo_path)?;
            let response = service.reindex(&repo_id, glob.map(String::from))?;
            Ok(IndexResult::Service(response))
        } else {
            let mut index = self.open_local_index(repo_path)?;
            let default_glob = index.config().default_glob().to_string();
            let glob = glob.unwrap_or(&default_glob);
            let stats = index.index(glob)?;
            Ok(IndexResult::Local(stats))
        }
    }

    /// Service admin: list repos. Err(no_service_url) in standalone.
    pub fn list_repos(&self) -> canopy_core::Result<Vec<RepoShard>> {
        let service = self.require_service()?;
        service.list_repos()
    }

    /// Service admin: status. Err(no_service_url) in standalone.
    pub fn service_status(&self) -> canopy_core::Result<ServiceStatus> {
        let service = self.require_service()?;
        service.status()
    }

    /// Service admin: reindex by repo_id. Err(no_service_url) in standalone.
    pub fn reindex_by_id(
        &self,
        repo_id: &str,
        glob: Option<&str>,
    ) -> canopy_core::Result<ReindexResponse> {
        let service = self.require_service()?;
        service.reindex(repo_id, glob.map(String::from))
    }

    // ── Private helpers ──

    fn require_service(&self) -> canopy_core::Result<&ServiceClient> {
        self.service
            .as_ref()
            .ok_or_else(|| canopy_core::CanopyError::ServiceError {
                code: "no_service_url".to_string(),
                message: "No service URL configured".to_string(),
                hint: "Pass --service-url or set CANOPY_SERVICE_URL".to_string(),
            })
    }

    fn query_input_text(input: &QueryInput) -> String {
        match input {
            QueryInput::Dsl(dsl, _) => dsl.clone(),
            QueryInput::Params(params) => params.to_text(),
        }
    }

    fn feedback_store_for_repo(&mut self, repo_path: &Path) -> Option<&FeedbackStore> {
        let canonical = canonical_path(repo_path);
        if !self.feedback_stores.contains_key(&canonical) {
            match FeedbackStore::open(repo_path) {
                Ok(store) => {
                    self.feedback_stores.insert(canonical.clone(), store);
                }
                Err(err) => {
                    eprintln!("[canopy] feedback disabled: failed to open store: {}", err);
                    return None;
                }
            }
        }
        self.feedback_stores.get(&canonical)
    }

    fn load_node_type_priors(&mut self, repo_path: &Path) -> Option<HashMap<NodeType, f64>> {
        let canonical = canonical_path(repo_path);
        if let Some((loaded_at, priors)) = self.node_type_priors_cache.get(&canonical) {
            if loaded_at.elapsed() < NODE_TYPE_PRIOR_CACHE_TTL {
                return Some(priors.clone());
            }
        }

        let store = self.feedback_store_for_repo(repo_path)?;
        match store.get_node_type_priors() {
            Ok(priors) if !priors.is_empty() => {
                self.node_type_priors_cache
                    .insert(canonical, (Instant::now(), priors.clone()));
                Some(priors)
            }
            Ok(_) => None,
            Err(err) => {
                eprintln!(
                    "[canopy] feedback: failed to load node type priors: {}",
                    err
                );
                None
            }
        }
    }

    fn remember_recent_query_event(&mut self, repo: &str, handle_id: &str, query_event_id: i64) {
        self.tracker
            .record_query_event(repo, handle_id, query_event_id);
    }

    fn record_feedback_for_query(
        &mut self,
        repo_path: &Path,
        query_text: &str,
        result: &QueryResult,
    ) {
        let canonical = canonical_path(repo_path);
        let pending = self.pending_predictive.remove(&canonical);

        let predicted_globs = pending
            .as_ref()
            .and_then(|ctx| (!ctx.predicted_globs.is_empty()).then(|| ctx.predicted_globs.clone()));
        let files_indexed = pending.as_ref().map(|ctx| ctx.files_indexed).unwrap_or(0);

        let query_event = QueryEvent {
            query_text: query_text.to_string(),
            predicted_globs,
            files_indexed,
            handles_returned: result.handles.len(),
            total_tokens: result.total_tokens,
        };

        let query_handles: Vec<QueryHandle> = result
            .handles
            .iter()
            .map(|handle| QueryHandle {
                handle_id: handle.id.to_string(),
                file_path: handle.file_path.clone(),
                node_type: handle.node_type,
                token_count: handle.token_count,
                first_match_glob: pending
                    .as_ref()
                    .and_then(|ctx| ctx.file_to_glob.get(&handle.file_path).cloned()),
            })
            .collect();

        let query_event_id: i64;
        {
            let Some(store) = self.feedback_store_for_repo(repo_path) else {
                return;
            };

            match store.record_query_event(&query_event) {
                Ok(id) => query_event_id = id,
                Err(err) => {
                    eprintln!("[canopy] feedback: failed to record query event: {}", err);
                    return;
                }
            }

            if let Err(err) = store.record_query_handles(query_event_id, &query_handles) {
                eprintln!("[canopy] feedback: failed to record query handles: {}", err);
            }

            for handle in result.handles.iter().filter(|h| h.content.is_some()) {
                let event = ExpandEvent {
                    query_event_id: Some(query_event_id),
                    handle_id: handle.id.to_string(),
                    file_path: handle.file_path.clone(),
                    node_type: handle.node_type,
                    token_count: handle.token_count,
                    auto_expanded: true,
                };
                if let Err(err) = store.record_expand_event(&event) {
                    eprintln!(
                        "[canopy] feedback: failed to record auto-expand event: {}",
                        err
                    );
                }
            }
        }

        for handle in &result.handles {
            self.remember_recent_query_event(&canonical, &handle.id.to_string(), query_event_id);
        }
    }

    fn record_feedback_for_expand(&mut self, repo_path: &Path, contents: &[(String, String)]) {
        if contents.is_empty() {
            return;
        }

        let canonical = canonical_path(repo_path);
        let mut local_metadata: HashMap<String, (String, NodeType, usize)> = HashMap::new();
        let missing_handle_ids: Vec<String> = contents
            .iter()
            .map(|(handle_id, _)| handle_id.clone())
            .filter(|handle_id| self.tracker.get(&canonical, handle_id).is_none())
            .collect();

        if !missing_handle_ids.is_empty() {
            match self.expand_local_details(repo_path, &missing_handle_ids) {
                Ok(details) => {
                    for (handle_id, file_path, node_type, token_count, _content) in details {
                        local_metadata.insert(handle_id, (file_path, node_type, token_count));
                    }
                }
                Err(_) => {
                    for handle_id in &missing_handle_ids {
                        if let Ok(mut details) =
                            self.expand_local_details(repo_path, std::slice::from_ref(handle_id))
                        {
                            if let Some((id, file_path, node_type, token_count, _content)) =
                                details.pop()
                            {
                                local_metadata.insert(id, (file_path, node_type, token_count));
                            }
                        }
                    }
                }
            }
        }

        let mut events = Vec::new();
        for (handle_id, content) in contents {
            let (file_path, node_type, token_count) = if let Some(prov) =
                self.tracker.get(&canonical, handle_id)
            {
                (prov.file_path.clone(), prov.node_type, prov.token_count)
            } else if let Some((file_path, node_type, token_count)) = local_metadata.get(handle_id)
            {
                (file_path.clone(), *node_type, *token_count)
            } else {
                (
                    "<unknown>".to_string(),
                    NodeType::Chunk,
                    canopy_core::parse::estimate_tokens(content),
                )
            };

            let query_event_id = self.tracker.query_event_id(&canonical, handle_id);
            events.push(ExpandEvent {
                query_event_id,
                handle_id: handle_id.clone(),
                file_path,
                node_type,
                token_count,
                auto_expanded: false,
            });
        }

        if events.is_empty() {
            return;
        }

        let Some(store) = self.feedback_store_for_repo(repo_path) else {
            return;
        };
        for event in events {
            if let Err(err) = store.record_expand_event(&event) {
                eprintln!("[canopy] feedback: failed to record expand event: {}", err);
            }
        }
    }

    fn query_service(
        &mut self,
        repo_path: &Path,
        input: QueryInput,
    ) -> canopy_core::Result<QueryResult> {
        let params = match input {
            QueryInput::Params(p) => p,
            QueryInput::Dsl(..) => unreachable!("DSL handled before calling query_service"),
        };

        let service = self.service.as_mut().unwrap();
        let repo_id = service.resolve_repo_id(repo_path)?;

        // ensure_ready before querying
        if let Err(e) = service.ensure_ready(&repo_id, ENSURE_READY_TIMEOUT) {
            // If repo not found, try re-resolve
            if is_error_code(&e, "repo_not_found") {
                let new_id = service.invalidate_and_resolve(repo_path)?;
                service.ensure_ready(&new_id, ENSURE_READY_TIMEOUT)?;
                return self.query_service_with_id(repo_path, &new_id, params);
            }
            return Err(e);
        }

        // Try query, with retry on repo_not_found
        match service.query(&repo_id, params.clone()) {
            Ok(service_result) => {
                self.merge_with_dirty(repo_path, &repo_id, service_result, Some(params))
            }
            Err(e) if is_error_code(&e, "repo_not_found") => {
                let service = self.service.as_mut().unwrap();
                let new_id = service.invalidate_and_resolve(repo_path)?;
                service.ensure_ready(&new_id, ENSURE_READY_TIMEOUT)?;
                self.query_service_with_id(repo_path, &new_id, params)
            }
            Err(e) => Err(e),
        }
    }

    fn query_service_with_id(
        &mut self,
        repo_path: &Path,
        repo_id: &str,
        params: QueryParams,
    ) -> canopy_core::Result<QueryResult> {
        let service = self.service.as_ref().unwrap();
        let service_result = service.query(repo_id, params.clone())?;
        self.merge_with_dirty(repo_path, repo_id, service_result, Some(params))
    }

    fn merge_with_dirty(
        &mut self,
        repo_path: &Path,
        repo_id: &str,
        service_result: QueryResult,
        local_params: Option<QueryParams>,
    ) -> canopy_core::Result<QueryResult> {
        // Detect dirty files
        let dirty_state = dirty::detect_dirty(repo_path)?;
        let dirty_paths = dirty_state.dirty_paths();

        // Rebuild local index for dirty files if needed
        if !dirty_state.is_clean() && dirty::needs_rebuild(&dirty_state, repo_path) {
            let mut index = self.open_local_index(repo_path)?;
            dirty::rebuild_local_index(&mut index, &dirty_state, repo_path)?;
            dirty::save_fingerprint(&dirty_state, repo_path)?;
        }

        // Query local index if there are dirty files
        let local_result = if !dirty_state.is_clean() {
            if let Some(params) = local_params {
                let index = self.open_local_index(repo_path)?;
                let query = params.to_query()?;
                let mut options = params.to_options();
                options.node_type_priors = self.load_node_type_priors(repo_path);
                Some(canopy_core::query::execute_query_with_options(
                    &query, &index, options,
                )?)
            } else {
                None
            }
        } else {
            None
        };

        // Record service handle provenance
        let gen = service_result.handles.first().and_then(|h| h.generation);
        self.record_provenance_for_result(
            repo_path,
            &service_result,
            HandleSource::Service,
            gen,
            Some(repo_id.to_string()),
        );

        // Update generation tracking
        if let Some(gen) = gen {
            let old_gen = self.repo_generations.insert(repo_id.to_string(), gen);
            if old_gen.is_some() && old_gen != Some(gen) {
                self.tracker.invalidate_repo(repo_id);
            }
        }

        let result = if let Some(local_result) = local_result {
            // Record local handle provenance
            self.record_provenance_for_result(
                repo_path,
                &local_result,
                HandleSource::Local,
                None,
                None,
            );
            merge::merge_results(local_result, service_result, &dirty_paths)
        } else {
            service_result
        };

        Ok(result)
    }

    fn query_standalone(
        &mut self,
        repo_path: &Path,
        input: QueryInput,
    ) -> canopy_core::Result<QueryResult> {
        // No auto-indexing here. Callers are responsible:
        // - CLI `index` command uses runtime.index() explicitly
        // - MCP calls predictive_index_for_query() before runtime.query()
        // - QueryOnly and Predictive policies just query what's already indexed
        let index = self.open_local_index(repo_path)?;
        let priors = self.load_node_type_priors(repo_path);

        let result = match input {
            QueryInput::Params(params) => {
                let query = params.to_query()?;
                let mut options = params.to_options();
                options.node_type_priors = priors.clone();
                canopy_core::query::execute_query_with_options(&query, &index, options)?
            }
            QueryInput::Dsl(dsl, mut opts) => {
                if opts.node_type_priors.is_none() {
                    opts.node_type_priors = priors;
                }
                index.query_with_options(&dsl, opts)?
            }
        };

        self.record_provenance_for_result(repo_path, &result, HandleSource::Local, None, None);
        Ok(result)
    }

    /// Predictive index with specific query text (used by MCP tool_query)
    pub fn predictive_index_for_query(
        &mut self,
        repo_path: &Path,
        index: &mut RepoIndex,
        query_text: &str,
    ) -> canopy_core::Result<()> {
        let default_glob = index.config().default_glob().to_string();
        let status = index.status()?;
        let canonical = canonical_path(repo_path);
        self.pending_predictive.remove(&canonical);

        let is_large_repo = if status.files_indexed == 0 {
            let all_files = index.walk_files(&default_glob).unwrap_or_default();
            all_files.len() > LARGE_REPO_THRESHOLD
        } else {
            status.files_indexed > LARGE_REPO_THRESHOLD
        };

        if status.files_indexed == 0 && !is_large_repo {
            index.index(&default_glob)?;
        } else if is_large_repo {
            let extensions = extract_extensions_from_glob(&default_glob);
            let predicted_globs = if let Some(feedback) = self.feedback_store_for_repo(repo_path) {
                predict_globs_with_feedback(query_text, &extensions, feedback)
            } else {
                predict_globs(query_text, &extensions)
            };

            eprintln!(
                "[canopy] Large repo, predictive indexing for: {:?}",
                predicted_globs.iter().take(5).collect::<Vec<_>>()
            );

            let mut total_indexed = 0;
            let mut file_to_glob: HashMap<String, String> = HashMap::new();
            for glob in &predicted_globs {
                if let Ok(files) = index.walk_files(glob) {
                    for file in files {
                        let path = file.to_string_lossy().to_string();
                        file_to_glob.entry(path).or_insert_with(|| glob.clone());
                    }
                }
                if total_indexed >= MAX_PREDICTIVE_FILES {
                    break;
                }
                if let Ok(stats) = index.index(glob) {
                    total_indexed += stats.files_indexed;
                }
            }

            if total_indexed > 0 {
                eprintln!("[canopy] Predictively indexed {} new files", total_indexed);
            }

            let current_status = index.status()?;
            if current_status.files_indexed == 0 {
                eprintln!("[canopy] No files indexed, adding entry points");
                let _ = index.index("**/main.*");
                let _ = index.index("**/index.*");
                let _ = index.index("**/app.*");
            }

            self.pending_predictive.insert(
                canonical,
                PendingPredictiveContext {
                    predicted_globs,
                    files_indexed: total_indexed,
                    file_to_glob,
                },
            );
        }

        Ok(())
    }

    fn open_local_index(&self, repo_path: &Path) -> canopy_core::Result<RepoIndex> {
        // Auto-init if .canopy doesn't exist
        if !repo_path.join(".canopy").exists() {
            RepoIndex::init(repo_path)?;
        }
        RepoIndex::open(repo_path)
    }

    fn expand_local(
        &self,
        repo_path: &Path,
        handle_ids: &[String],
    ) -> canopy_core::Result<Vec<(String, String)>> {
        let index = self.open_local_index(repo_path)?;
        index.expand(handle_ids)
    }

    fn expand_local_details(
        &self,
        repo_path: &Path,
        handle_ids: &[String],
    ) -> canopy_core::Result<Vec<ExpandedHandleDetail>> {
        let index = self.open_local_index(repo_path)?;
        index.expand_with_details(handle_ids)
    }

    fn record_provenance_for_result(
        &mut self,
        repo_path: &Path,
        result: &QueryResult,
        source: HandleSource,
        generation: Option<u64>,
        repo_id: Option<String>,
    ) {
        let canonical = canonical_path(repo_path);
        for handle in &result.handles {
            self.tracker.record(
                &canonical,
                &handle.id.to_string(),
                HandleProvenance {
                    source: source.clone(),
                    generation: generation.or(handle.generation),
                    repo_id: repo_id.clone(),
                    file_path: handle.file_path.clone(),
                    node_type: handle.node_type,
                    token_count: handle.token_count,
                },
            );
        }
    }

    fn record_provenance_for_evidence_pack(
        &mut self,
        repo_path: &Path,
        pack: &EvidencePack,
        service_repo_id: Option<String>,
    ) {
        let canonical = canonical_path(repo_path);
        for handle in &pack.handles {
            let repo_id = match handle.source {
                HandleSource::Service => service_repo_id.clone(),
                HandleSource::Local => None,
            };
            self.tracker.record(
                &canonical,
                &handle.id,
                HandleProvenance {
                    source: handle.source.clone(),
                    generation: handle.generation,
                    repo_id,
                    file_path: handle.file_path.clone(),
                    node_type: handle.node_type,
                    token_count: handle.token_count,
                },
            );
        }
    }

    fn rewrite_expand_suggestions(&self, repo_path: &Path, pack: &mut EvidencePack) {
        let canonical = canonical_path(repo_path);
        pack.reorder_expand_suggestions(|id| self.tracker.was_recently_expanded(&canonical, id));
    }

    fn record_recently_expanded(&mut self, repo_path: &Path, contents: &[(String, String)]) {
        if contents.is_empty() {
            return;
        }
        let canonical = canonical_path(repo_path);
        for (handle_id, _) in contents {
            self.tracker.mark_expanded(&canonical, handle_id);
        }
    }
}

fn canonical_path(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo() -> std::path::PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("canopy-runtime-test-{ts}"));
        fs::create_dir_all(&root).unwrap();
        RepoIndex::init(&root).unwrap();
        root
    }

    #[test]
    fn test_standalone_no_service() {
        let rt = ClientRuntime::new(None, None);
        assert!(!rt.is_service_mode());
    }

    #[test]
    fn test_service_mode() {
        let rt = ClientRuntime::new(Some("http://localhost:3000"), None);
        assert!(rt.is_service_mode());
    }

    #[test]
    fn test_list_repos_without_service() {
        let rt = ClientRuntime::new(None, None);
        let err = rt.list_repos().unwrap_err();
        assert!(matches!(
            err,
            canopy_core::CanopyError::ServiceError { ref code, .. } if code == "no_service_url"
        ));
    }

    #[test]
    fn test_service_status_without_service() {
        let rt = ClientRuntime::new(None, None);
        let err = rt.service_status().unwrap_err();
        assert!(matches!(
            err,
            canopy_core::CanopyError::ServiceError { ref code, .. } if code == "no_service_url"
        ));
    }

    #[test]
    fn test_reindex_by_id_without_service() {
        let rt = ClientRuntime::new(None, None);
        let err = rt.reindex_by_id("some-id", None).unwrap_err();
        assert!(matches!(
            err,
            canopy_core::CanopyError::ServiceError { ref code, .. } if code == "no_service_url"
        ));
    }

    #[test]
    fn test_provenance_eviction() {
        use crate::provenance::PROVENANCE_CAP;

        let mut rt = ClientRuntime::new(None, None);

        // Insert more than PROVENANCE_CAP entries
        for i in 0..PROVENANCE_CAP + 10 {
            let handle_id = format!("h{:024x}", i);
            rt.tracker.record(
                "/tmp/test-repo",
                &handle_id,
                HandleProvenance {
                    source: HandleSource::Local,
                    generation: None,
                    repo_id: None,
                    file_path: "src/test.rs".to_string(),
                    node_type: NodeType::Function,
                    token_count: 10,
                },
            );
        }

        // The tracker should have capped at PROVENANCE_CAP
        // Verify that the earliest entry was evicted
        assert!(rt
            .tracker
            .get("/tmp/test-repo", &format!("h{:024x}", 0))
            .is_none());
        // And a later entry still exists
        assert!(rt
            .tracker
            .get("/tmp/test-repo", &format!("h{:024x}", PROVENANCE_CAP + 5))
            .is_some());
    }

    #[test]
    fn test_expand_feedback_records_without_provenance() {
        let repo = temp_repo();
        let mut rt = ClientRuntime::new(None, None);
        let handle_id = "h000000000000000000000000".to_string();
        let contents = vec![(handle_id, "fn hello_world() {}".to_string())];

        rt.record_feedback_for_expand(&repo, &contents);

        let store = FeedbackStore::open(&repo).unwrap();
        let metrics = store.compute_metrics(1.0).unwrap();
        assert!(metrics.avg_tokens_per_expand > 0.0);
    }
}
