//! Query dispatch: service vs standalone routing, dirty overlay merging.

use crate::dirty;
use crate::merge;
use crate::service_client::{is_error_code, ServiceClient};
use canopy_core::{HandleSource, QueryParams, QueryResult};
use std::path::Path;

use super::{ClientRuntime, ENSURE_READY_TIMEOUT};

impl ClientRuntime {
    pub(super) fn require_service(&self) -> canopy_core::Result<&ServiceClient> {
        self.service
            .as_ref()
            .ok_or(canopy_core::CanopyError::NoServiceConfigured)
    }

    pub(super) fn query_service(
        &mut self,
        repo_path: &Path,
        params: QueryParams,
    ) -> canopy_core::Result<QueryResult> {
        let service = self.service.as_mut().unwrap();
        let repo_id = service.resolve_ready(repo_path, ENSURE_READY_TIMEOUT)?;

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

    pub(super) fn query_service_with_id(
        &mut self,
        repo_path: &Path,
        repo_id: &str,
        params: QueryParams,
    ) -> canopy_core::Result<QueryResult> {
        let service = self.service.as_ref().unwrap();
        let service_result = service.query(repo_id, params.clone())?;
        self.merge_with_dirty(repo_path, repo_id, service_result, Some(params))
    }

    pub(super) fn merge_with_dirty(
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
            let old_gen = self.cache.repo_generations.insert(repo_id.to_string(), gen);
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

    pub(super) fn query_standalone(
        &mut self,
        repo_path: &Path,
        params: QueryParams,
    ) -> canopy_core::Result<QueryResult> {
        // No auto-indexing here. Callers are responsible:
        // - CLI `index` command uses runtime.index() explicitly
        // - MCP calls predictive_index_for_query() before runtime.query()
        // - QueryOnly and Predictive policies just query what's already indexed
        let index = self.open_local_index(repo_path)?;
        let query = params.to_query()?;
        let mut options = params.to_options();
        options.node_type_priors = self.load_node_type_priors(repo_path);
        let result =
            canopy_core::query::execute_query_with_options(&query, &index, options)?;

        self.record_provenance_for_result(repo_path, &result, HandleSource::Local, None, None);
        Ok(result)
    }
}
