//! Expand orchestration — batch local, batch service, and unknown-provenance expansion.

use crate::service_client::is_error_code;
use canopy_core::index::ExpandedHandleDetail;
use std::path::Path;

use super::{ClientRuntime, ENSURE_READY_TIMEOUT};

impl ClientRuntime {
    /// Expand local handles: try batch first, fall back to per-handle on failure.
    pub(super) fn expand_local_batch(
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
    pub(super) fn expand_service_batch(
        &mut self,
        repo_path: &Path,
        service_ids: Vec<(String, Option<u64>, Option<String>)>,
        contents: &mut Vec<(String, String)>,
        failed_ids: &mut Vec<String>,
    ) {
        if service_ids.is_empty() {
            return;
        }

        let Some(service) = self.service.as_mut() else {
            failed_ids.extend(service_ids.into_iter().map(|(id, _, _)| id));
            return;
        };

        let Some(repo_id) = service.resolve_ready(repo_path, ENSURE_READY_TIMEOUT).ok() else {
            failed_ids.extend(service_ids.into_iter().map(|(id, _, _)| id));
            return;
        };

        let all_ids: Vec<String> = service_ids.iter().map(|(id, _, _)| id.clone()).collect();
        let batch_gen = service_ids.first().and_then(|(_, g, _)| *g);

        match service.expand(&repo_id, &all_ids, batch_gen) {
            Ok(mut c) => contents.append(&mut c),
            Err(e) if is_error_code(&e, "repo_not_found") => {
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
    pub(super) fn expand_unknown(
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

    pub(super) fn expand_local(
        &self,
        repo_path: &Path,
        handle_ids: &[String],
    ) -> canopy_core::Result<Vec<(String, String)>> {
        let index = self.open_local_index(repo_path)?;
        index.expand(handle_ids)
    }

    pub(super) fn expand_local_details(
        &self,
        repo_path: &Path,
        handle_ids: &[String],
    ) -> canopy_core::Result<Vec<ExpandedHandleDetail>> {
        let index = self.open_local_index(repo_path)?;
        index.expand_with_details(handle_ids)
    }
}
