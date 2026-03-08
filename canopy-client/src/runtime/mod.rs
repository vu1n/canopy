//! Shared mode orchestration for CLI and MCP.
//!
//! In standalone mode, queries run against a local on-disk index.
//! In service mode, queries are dispatched to canopy-service.
//! DSL queries always run locally (the DSL engine is not exposed by the service).

mod expand;
mod feedback;
mod query_dispatch;

use crate::predict::{
    extract_extensions_from_glob, predict_globs, predict_globs_with_feedback, LARGE_REPO_THRESHOLD,
    MAX_PREDICTIVE_FILES,
};
use crate::provenance::ProvenanceTracker;
use crate::service_client::{is_error_code, ReindexResponse, ServiceClient, ServiceStatus};
use canopy_core::{
    build_evidence_pack, feedback::FeedbackStore, EvidencePack, ExpandOutcome, HandleSource,
    IndexStats, NodeType, QueryParams, QueryResult, RepoIndex, RepoShard,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

const ENSURE_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Result of an index/reindex operation
pub enum IndexResult {
    Local(IndexStats),
    Service(ReindexResponse),
}

struct PendingPredictiveContext {
    predicted_globs: Vec<String>,
    files_indexed: usize,
    file_to_glob: HashMap<String, String>,
}

/// Repo-local feedback stores and predictive indexing context.
struct FeedbackContext {
    /// Repo-local feedback DB handles (lazy-opened)
    stores: HashMap<String, FeedbackStore>,
    /// Predictive context staged between predictive_index_for_query() and query()
    pending_predictive: HashMap<String, PendingPredictiveContext>,
}

/// Cached per-repo metadata: generation tracking and node type priors.
struct CacheContext {
    /// Track last-known generation per repo to detect changes
    repo_generations: HashMap<String, u64>,
    /// Cached node type priors per repo
    node_type_priors: HashMap<String, (Instant, HashMap<NodeType, f64>)>,
}

pub struct ClientRuntime {
    service: Option<ServiceClient>,
    tracker: ProvenanceTracker,
    feedback: FeedbackContext,
    cache: CacheContext,
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
            feedback: FeedbackContext {
                stores: HashMap::new(),
                pending_predictive: HashMap::new(),
            },
            cache: CacheContext {
                repo_generations: HashMap::new(),
                node_type_priors: HashMap::new(),
            },
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
        params: QueryParams,
    ) -> canopy_core::Result<QueryResult> {
        let query_text = params.to_text();

        let is_dsl = params.dsl.is_some();

        let result = if self.service.is_some() && !is_dsl {
            self.query_service(repo_path, params)?
        } else {
            if self.service.is_some() && is_dsl {
                eprintln!("Warning: DSL query bypasses service mode, using local index");
            }
            self.query_standalone(repo_path, params)?
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
        params: QueryParams,
        max_handles: usize,
        max_per_file: usize,
        plan: Option<bool>,
    ) -> canopy_core::Result<EvidencePack> {
        let max_handles = max_handles.clamp(1, 64);
        let max_per_file = max_per_file.clamp(1, 8);
        let config = canopy_core::protocol::EvidencePackConfig {
            max_handles: Some(max_handles),
            max_per_file: Some(max_per_file),
            plan,
        };

        if let Some(service) = self.service.as_mut() {
            if params.dsl.is_none() {
                let mut params = params.clone();
                params.expand_budget = Some(0);

                let active_repo_id = service.resolve_ready(repo_path, ENSURE_READY_TIMEOUT)?;

                let (mut pack, used_repo_id) =
                    match service.evidence_pack(&active_repo_id, params.clone(), config.clone()) {
                        Ok(pack) => (pack, active_repo_id.clone()),
                        Err(e) if is_error_code(&e, "repo_not_found") => {
                            let new_id = service.invalidate_and_resolve(repo_path)?;
                            service.ensure_ready(&new_id, ENSURE_READY_TIMEOUT)?;
                            let pack = service.evidence_pack(&new_id, params, config)?;
                            (pack, new_id)
                        }
                        Err(e) => return Err(e),
                    };

                self.rewrite_expand_suggestions(repo_path, &mut pack);
                self.record_provenance_for_evidence_pack(repo_path, &pack, Some(used_repo_id));
                return Ok(pack);
            }
        }

        let fallback_params = params.pattern_fallback();
        let query_text = params.to_text();
        let result = self.query(repo_path, params)?;
        let mut pack = build_evidence_pack(&result, &query_text, max_handles, max_per_file);
        self.rewrite_expand_suggestions(repo_path, &mut pack);
        self.record_provenance_for_evidence_pack(repo_path, &pack, None);

        if pack.selected_count == 0 {
            if let Some(fallback) = fallback_params {
                let fallback_text = fallback.to_text();
                let fallback_result = self.query(repo_path, fallback)?;
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

    /// Service admin: list repos. Err(NoServiceConfigured) in standalone.
    pub fn list_repos(&self) -> canopy_core::Result<Vec<RepoShard>> {
        let service = self.require_service()?;
        service.list_repos()
    }

    /// Service admin: status. Err(NoServiceConfigured) in standalone.
    pub fn service_status(&self) -> canopy_core::Result<ServiceStatus> {
        let service = self.require_service()?;
        service.status()
    }

    /// Service admin: reindex by repo_id. Err(NoServiceConfigured) in standalone.
    pub fn reindex_by_id(
        &self,
        repo_id: &str,
        glob: Option<&str>,
    ) -> canopy_core::Result<ReindexResponse> {
        let service = self.require_service()?;
        service.reindex(repo_id, glob.map(String::from))
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
        self.feedback.pending_predictive.remove(&canonical);

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
                if let Ok(stats) = index.index(glob) {
                    total_indexed += stats.files_indexed;
                }
                if total_indexed >= MAX_PREDICTIVE_FILES {
                    break;
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

            self.feedback.pending_predictive.insert(
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
        RepoIndex::open_or_init(repo_path)
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
    use crate::provenance::HandleProvenance;

    fn temp_repo() -> std::path::PathBuf {
        let root = canopy_core::temp_test_dir("runtime-test");
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
        assert!(matches!(err, canopy_core::CanopyError::NoServiceConfigured));
    }

    #[test]
    fn test_service_status_without_service() {
        let rt = ClientRuntime::new(None, None);
        let err = rt.service_status().unwrap_err();
        assert!(matches!(err, canopy_core::CanopyError::NoServiceConfigured));
    }

    #[test]
    fn test_reindex_by_id_without_service() {
        let rt = ClientRuntime::new(None, None);
        let err = rt.reindex_by_id("some-id", None).unwrap_err();
        assert!(matches!(err, canopy_core::CanopyError::NoServiceConfigured));
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

    #[test]
    fn test_standalone_query_returns_results() {
        let repo = temp_repo();
        // Write a Rust file to index
        let src_dir = repo.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("auth.rs"),
            "pub fn authenticate(token: &str) -> bool {\n    !token.is_empty()\n}\n",
        )
        .unwrap();

        let mut rt = ClientRuntime::new(None, None);
        rt.index(&repo, Some("**/*.rs")).unwrap();

        let params = QueryParams::symbol("authenticate");
        let result = rt.query(&repo, params).unwrap();
        assert!(!result.handles.is_empty());
        assert_eq!(result.handles[0].file_path, "src/auth.rs");
    }

    #[test]
    fn test_standalone_query_empty_index_returns_empty() {
        let repo = temp_repo();
        let mut rt = ClientRuntime::new(None, None);

        let params = QueryParams::symbol("nonexistent");
        let result = rt.query(&repo, params).unwrap();
        assert!(result.handles.is_empty());
    }

    #[test]
    fn test_standalone_expand_after_query() {
        let repo = temp_repo();
        let src_dir = repo.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("lib.rs"),
            "pub struct Config {\n    pub name: String,\n    pub value: u32,\n}\n",
        )
        .unwrap();

        let mut rt = ClientRuntime::new(None, None);
        rt.index(&repo, Some("**/*.rs")).unwrap();

        let params = QueryParams::symbol("Config");
        let result = rt.query(&repo, params).unwrap();
        assert!(!result.handles.is_empty());

        // Expand the first handle
        let handle_ids: Vec<String> = result.handles.iter().map(|h| h.id.to_string()).collect();
        let outcome = rt.expand(&repo, &handle_ids).unwrap();
        assert!(!outcome.contents.is_empty());
        assert!(outcome.contents[0].1.contains("Config"));
    }
}
