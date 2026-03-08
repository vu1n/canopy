//! Feedback recording and provenance helpers.

use crate::provenance::{HandleProvenance, ProvenanceTracker};
use canopy_core::{
    feedback::{ExpandEvent, FeedbackStore, QueryEvent, QueryHandle, NODE_TYPE_PRIOR_CACHE_TTL},
    EvidencePack, HandleSource, NodeType, QueryResult,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use super::{canonical_path, ClientRuntime};

impl ClientRuntime {
    pub(super) fn feedback_store_for_repo(
        &mut self,
        repo_path: &Path,
    ) -> Option<&FeedbackStore> {
        let canonical = canonical_path(repo_path);
        if !self.feedback.stores.contains_key(&canonical) {
            match FeedbackStore::open(repo_path) {
                Ok(store) => {
                    self.feedback.stores.insert(canonical.clone(), store);
                }
                Err(err) => {
                    eprintln!("[canopy] feedback disabled: failed to open store: {}", err);
                    return None;
                }
            }
        }
        self.feedback.stores.get(&canonical)
    }

    pub(super) fn load_node_type_priors(
        &mut self,
        repo_path: &Path,
    ) -> Option<HashMap<NodeType, f64>> {
        let canonical = canonical_path(repo_path);
        if let Some((loaded_at, priors)) = self.cache.node_type_priors.get(&canonical) {
            if loaded_at.elapsed() < NODE_TYPE_PRIOR_CACHE_TTL {
                return Some(priors.clone());
            }
        }

        let store = self.feedback_store_for_repo(repo_path)?;
        match store.get_node_type_priors() {
            Ok(priors) if !priors.is_empty() => {
                self.cache
                    .node_type_priors
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

    pub(super) fn record_feedback_for_query(
        &mut self,
        repo_path: &Path,
        query_text: &str,
        result: &QueryResult,
    ) {
        let canonical = canonical_path(repo_path);
        let pending = self.feedback.pending_predictive.remove(&canonical);

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
            .map(|handle| {
                let glob = pending
                    .as_ref()
                    .and_then(|ctx| ctx.file_to_glob.get(&handle.file_path).cloned());
                QueryHandle::from_handle(handle, glob)
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

    pub(super) fn record_feedback_for_expand(
        &mut self,
        repo_path: &Path,
        contents: &[(String, String)],
    ) {
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
                    for d in details {
                        local_metadata
                            .insert(d.handle_id, (d.file_path, d.node_type, d.token_count));
                    }
                }
                Err(_) => {
                    for handle_id in &missing_handle_ids {
                        if let Ok(mut details) =
                            self.expand_local_details(repo_path, std::slice::from_ref(handle_id))
                        {
                            if let Some(d) = details.pop() {
                                local_metadata.insert(
                                    d.handle_id,
                                    (d.file_path, d.node_type, d.token_count),
                                );
                            }
                        }
                    }
                }
            }
        }

        let events: Vec<ExpandEvent> = contents
            .iter()
            .map(|(handle_id, content)| {
                let (file_path, node_type, token_count) = resolve_handle_metadata(
                    &self.tracker,
                    &canonical,
                    handle_id,
                    content,
                    &local_metadata,
                );
                let query_event_id = self.tracker.query_event_id(&canonical, handle_id);
                ExpandEvent {
                    query_event_id,
                    handle_id: handle_id.clone(),
                    file_path,
                    node_type,
                    token_count,
                    auto_expanded: false,
                }
            })
            .collect();

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

    pub(super) fn record_provenance_for_result(
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

    pub(super) fn record_provenance_for_evidence_pack(
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

    pub(super) fn rewrite_expand_suggestions(&self, repo_path: &Path, pack: &mut EvidencePack) {
        let canonical = canonical_path(repo_path);
        pack.reorder_expand_suggestions(|id| self.tracker.was_recently_expanded(&canonical, id));
    }

    pub(super) fn record_recently_expanded(
        &mut self,
        repo_path: &Path,
        contents: &[(String, String)],
    ) {
        if contents.is_empty() {
            return;
        }
        let canonical = canonical_path(repo_path);
        for (handle_id, _) in contents {
            self.tracker.mark_expanded(&canonical, handle_id);
        }
    }
}

/// Resolve handle metadata from provenance tracker, local index, or fallback estimation.
fn resolve_handle_metadata(
    tracker: &ProvenanceTracker,
    canonical: &str,
    handle_id: &str,
    content: &str,
    local_metadata: &HashMap<String, (String, NodeType, usize)>,
) -> (String, NodeType, usize) {
    if let Some(prov) = tracker.get(canonical, handle_id) {
        (prov.file_path.clone(), prov.node_type, prov.token_count)
    } else if let Some((file_path, node_type, token_count)) = local_metadata.get(handle_id) {
        (file_path.clone(), *node_type, *token_count)
    } else {
        (
            "<unknown>".to_string(),
            NodeType::Chunk,
            canopy_core::parse::estimate_tokens(content),
        )
    }
}
