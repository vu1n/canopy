//! Shared mode orchestration for CLI and MCP
//!
//! Runtime fully owns both modes — no mode branching leaked to callers.

use crate::dirty;
use crate::merge;
use crate::predict::{extract_extensions_from_glob, predict_globs};
use crate::service_client::{is_error_code, ReindexResponse, ServiceClient, ServiceStatus};
use canopy_core::{
    HandleSource, IndexStats, QueryOptions, QueryParams, QueryResult, RepoIndex, RepoShard,
};
use std::collections::{HashMap, VecDeque};
use std::path::Path;

const PROVENANCE_CAP: usize = 10_000;
const ENSURE_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Indexing policy for standalone mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandalonePolicy {
    /// CLI index command: index everything
    FullIndex,
    /// CLI query/expand: query existing index only, no auto-indexing
    QueryOnly,
    /// MCP: caller handles predictive indexing before query
    Predictive,
}

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

/// Tracks where a handle came from for expand routing
#[derive(Debug, Clone)]
pub struct HandleProvenance {
    pub source: HandleSource,
    pub generation: Option<u64>,
    pub repo_id: Option<String>,
}

/// Outcome of an expand operation — supports partial success
pub struct ExpandOutcome {
    pub contents: Vec<(String, String)>,
    pub failed_ids: Vec<String>,
}

pub struct ClientRuntime {
    service: Option<ServiceClient>,
    /// (canonical_repo_path, handle_id) → provenance
    handle_provenance: HashMap<(String, String), HandleProvenance>,
    /// Insertion order for LRU eviction
    provenance_order: VecDeque<(String, String)>,
    /// Track last-known generation per repo to detect changes
    repo_generations: HashMap<String, u64>,
}

impl ClientRuntime {
    /// Create a new runtime.
    ///
    /// `_policy` is accepted for caller documentation but the runtime does not
    /// auto-index.  Callers choose their own indexing strategy:
    /// - CLI `index` command: calls `runtime.index()` explicitly
    /// - MCP: calls `runtime.predictive_index_for_query()` before query
    /// - CLI query/expand: queries whatever is already indexed
    pub fn new(service_url: Option<&str>, _policy: StandalonePolicy) -> Self {
        Self {
            service: service_url.map(ServiceClient::new),
            handle_provenance: HashMap::new(),
            provenance_order: VecDeque::new(),
            repo_generations: HashMap::new(),
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
        if self.service.is_some() {
            // DSL queries bypass service mode
            if let QueryInput::Dsl(ref dsl, ref opts) = input {
                eprintln!("Warning: DSL query bypasses service mode, using local index");
                let index = self.open_local_index(repo_path)?;
                let result = index.query_with_options(
                    dsl,
                    QueryOptions {
                        limit: opts.limit,
                        expand_budget: opts.expand_budget,
                    },
                )?;
                self.record_provenance_for_result(
                    repo_path,
                    &result,
                    HandleSource::Local,
                    None,
                    None,
                );
                return Ok(result);
            }

            self.query_service(repo_path, input)
        } else {
            self.query_standalone(repo_path, input)
        }
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
        let mut contents: Vec<(String, String)> = Vec::new();
        let mut failed_ids: Vec<String> = Vec::new();

        // Partition handles by provenance
        let mut local_ids: Vec<String> = Vec::new();
        let mut service_ids: Vec<(String, Option<u64>, Option<String>)> = Vec::new(); // (id, gen, repo_id)
        let mut unknown_ids: Vec<String> = Vec::new();

        for id in handle_ids {
            let key = (canonical.clone(), id.clone());
            if let Some(prov) = self.handle_provenance.get(&key) {
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

        // Expand local handles — try batch first, fall back to per-handle
        if !local_ids.is_empty() {
            match self.expand_local(repo_path, &local_ids) {
                Ok(mut c) => contents.append(&mut c),
                Err(_) => {
                    // Batch failed — try each handle individually
                    for id in local_ids {
                        match self.expand_local(repo_path, std::slice::from_ref(&id)) {
                            Ok(c) => contents.extend(c),
                            Err(_) => failed_ids.push(id),
                        }
                    }
                }
            }
        }

        // Expand service handles — try batch first, fall back to per-handle
        if !service_ids.is_empty() {
            if let Some(service) = &self.service {
                let first_repo_id = service_ids.first().and_then(|(_, _, r)| r.clone());

                if let Some(repo_id) = first_repo_id {
                    // ensure_ready; on repo_not_found, re-resolve and retry
                    let active_repo_id = match service.ensure_ready(&repo_id, ENSURE_READY_TIMEOUT)
                    {
                        Ok(()) => Some(repo_id),
                        Err(e) if is_error_code(&e, "repo_not_found") => {
                            let service = self.service.as_mut().unwrap();
                            match service.invalidate_and_resolve(repo_path) {
                                Ok(new_id) => {
                                    match service.ensure_ready(&new_id, ENSURE_READY_TIMEOUT) {
                                        Ok(()) => Some(new_id),
                                        Err(_) => None,
                                    }
                                }
                                Err(_) => None,
                            }
                        }
                        Err(_) => None,
                    };

                    if let Some(repo_id) = active_repo_id {
                        let service = self.service.as_ref().unwrap();
                        // Try batch expand; on failure, fall back to per-handle
                        let all_ids: Vec<String> =
                            service_ids.iter().map(|(id, _, _)| id.clone()).collect();
                        let batch_gen = service_ids.first().and_then(|(_, g, _)| *g);

                        match service.expand(&repo_id, &all_ids, batch_gen) {
                            Ok(mut c) => contents.append(&mut c),
                            Err(e) if is_error_code(&e, "repo_not_found") => {
                                // Re-resolve, then per-handle
                                let service = self.service.as_mut().unwrap();
                                let resolved = service.invalidate_and_resolve(repo_path).ok();
                                for (id, gen, _) in &service_ids {
                                    if let Some(ref rid) = resolved {
                                        if let Ok(c) =
                                            service.expand(rid, std::slice::from_ref(id), *gen)
                                        {
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
                                    match service.expand(target_id, std::slice::from_ref(id), *gen)
                                    {
                                        Ok(c) => contents.extend(c),
                                        Err(_) => failed_ids.push(id.clone()),
                                    }
                                }
                            }
                        }
                    } else {
                        let ids: Vec<String> =
                            service_ids.into_iter().map(|(id, _, _)| id).collect();
                        failed_ids.extend(ids);
                    }
                } else {
                    let ids: Vec<String> = service_ids.into_iter().map(|(id, _, _)| id).collect();
                    failed_ids.extend(ids);
                }
            } else {
                let ids: Vec<String> = service_ids.into_iter().map(|(id, _, _)| id).collect();
                failed_ids.extend(ids);
            }
        }

        // Expand unknown handles: try local first, then service
        for id in unknown_ids {
            // Try local
            if let Ok(c) = self.expand_local(repo_path, std::slice::from_ref(&id)) {
                contents.extend(c);
                continue;
            }
            // Try service
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
                Some(index.query_params(params)?)
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
                self.invalidate_provenance_for_repo(repo_id);
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

        let result = match input {
            QueryInput::Params(params) => index.query_params(params)?,
            QueryInput::Dsl(dsl, opts) => index.query_with_options(&dsl, opts)?,
        };

        self.record_provenance_for_result(repo_path, &result, HandleSource::Local, None, None);
        Ok(result)
    }

    /// Predictive index with specific query text (used by MCP tool_query)
    pub fn predictive_index_for_query(
        &self,
        index: &mut RepoIndex,
        query_text: &str,
    ) -> canopy_core::Result<()> {
        let default_glob = index.config().default_glob().to_string();
        let status = index.status()?;

        const LARGE_REPO_THRESHOLD: usize = 1000;
        const MAX_PREDICTIVE_FILES: usize = 500;

        let is_large_repo = if status.files_indexed == 0 {
            let all_files = index.walk_files(&default_glob).unwrap_or_default();
            all_files.len() > LARGE_REPO_THRESHOLD
        } else {
            status.files_indexed < LARGE_REPO_THRESHOLD
        };

        if status.files_indexed == 0 && !is_large_repo {
            index.index(&default_glob)?;
        } else if is_large_repo {
            let extensions = extract_extensions_from_glob(&default_glob);
            let predicted_globs = predict_globs(query_text, &extensions);

            eprintln!(
                "[canopy] Large repo, predictive indexing for: {:?}",
                predicted_globs.iter().take(5).collect::<Vec<_>>()
            );

            let mut total_indexed = 0;
            for glob in &predicted_globs {
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
            let key = (canonical.clone(), handle.id.to_string());
            let prov = HandleProvenance {
                source: source.clone(),
                generation: generation.or(handle.generation),
                repo_id: repo_id.clone(),
            };

            // On update, skip re-enqueue — just update HashMap in-place
            if self.handle_provenance.contains_key(&key) {
                self.handle_provenance.insert(key, prov);
            } else {
                self.handle_provenance.insert(key.clone(), prov);
                self.provenance_order.push_back(key);
            }
        }

        // Evict if over cap
        while self.handle_provenance.len() > PROVENANCE_CAP {
            if let Some(oldest) = self.provenance_order.pop_front() {
                // Lazy tombstone: skip if already gone (was updated in-place and
                // has a newer entry in the deque)
                self.handle_provenance.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn invalidate_provenance_for_repo(&mut self, repo_id: &str) {
        // Remove all HashMap entries for this repo; stale VecDeque entries become tombstones
        self.handle_provenance
            .retain(|_, prov| prov.repo_id.as_deref() != Some(repo_id));
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

    #[test]
    fn test_standalone_no_service() {
        let rt = ClientRuntime::new(None, StandalonePolicy::FullIndex);
        assert!(!rt.is_service_mode());
    }

    #[test]
    fn test_service_mode() {
        let rt = ClientRuntime::new(Some("http://localhost:3000"), StandalonePolicy::FullIndex);
        assert!(rt.is_service_mode());
    }

    #[test]
    fn test_list_repos_without_service() {
        let rt = ClientRuntime::new(None, StandalonePolicy::FullIndex);
        let err = rt.list_repos().unwrap_err();
        assert!(matches!(
            err,
            canopy_core::CanopyError::ServiceError { ref code, .. } if code == "no_service_url"
        ));
    }

    #[test]
    fn test_service_status_without_service() {
        let rt = ClientRuntime::new(None, StandalonePolicy::FullIndex);
        let err = rt.service_status().unwrap_err();
        assert!(matches!(
            err,
            canopy_core::CanopyError::ServiceError { ref code, .. } if code == "no_service_url"
        ));
    }

    #[test]
    fn test_reindex_by_id_without_service() {
        let rt = ClientRuntime::new(None, StandalonePolicy::FullIndex);
        let err = rt.reindex_by_id("some-id", None).unwrap_err();
        assert!(matches!(
            err,
            canopy_core::CanopyError::ServiceError { ref code, .. } if code == "no_service_url"
        ));
    }

    #[test]
    fn test_provenance_eviction() {
        let mut rt = ClientRuntime::new(None, StandalonePolicy::FullIndex);
        let path = Path::new("/tmp/test-repo");

        // Insert more than PROVENANCE_CAP entries
        for i in 0..PROVENANCE_CAP + 10 {
            let key = ("/tmp/test-repo".to_string(), format!("h{:024x}", i));
            rt.handle_provenance.insert(
                key.clone(),
                HandleProvenance {
                    source: HandleSource::Local,
                    generation: None,
                    repo_id: None,
                },
            );
            rt.provenance_order.push_back(key);
        }

        // Trigger eviction
        let result = QueryResult {
            handles: vec![],
            ref_handles: None,
            total_tokens: 0,
            truncated: false,
            total_matches: 0,
            auto_expanded: false,
            expand_note: None,
        };
        rt.record_provenance_for_result(path, &result, HandleSource::Local, None, None);

        assert!(rt.handle_provenance.len() <= PROVENANCE_CAP);
    }
}
