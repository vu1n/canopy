//! HTTP client for canopy-service

use canopy_core::{CanopyError, EvidencePack, QueryParams, QueryResult, RepoShard, ShardStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub struct ServiceClient {
    base_url: String,
    client: reqwest::blocking::Client,
    /// Cache: canonical path → repo_id
    repo_id_cache: HashMap<String, String>,
}

#[derive(Serialize)]
struct QueryRequest {
    repo: String,
    #[serde(flatten)]
    params: QueryParams,
}

#[derive(Serialize)]
struct EvidencePackRequest {
    repo: String,
    #[serde(flatten)]
    params: QueryParams,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_handles: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_per_file: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<bool>,
}

#[derive(Serialize)]
struct ExpandRequest {
    repo: String,
    handles: Vec<ExpandHandle>,
}

#[derive(Serialize)]
struct ExpandHandle {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
}

#[derive(Deserialize)]
struct ExpandResponse {
    contents: Vec<ExpandedContent>,
}

#[derive(Deserialize)]
struct ExpandedContent {
    handle_id: String,
    content: String,
}

#[derive(Serialize)]
struct AddRepoRequest {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Deserialize)]
struct AddRepoResponse {
    repo_id: String,
    #[allow(dead_code)]
    name: String,
}

#[derive(Serialize)]
struct ReindexRequest {
    repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    glob: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReindexResponse {
    pub generation: u64,
    pub status: String,
    pub commit_sha: Option<String>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    code: String,
    message: String,
    hint: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceStatus {
    pub service: String,
    pub repos: Vec<RepoShard>,
}

impl ServiceClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::blocking::Client::new(),
            repo_id_cache: HashMap::new(),
        }
    }

    /// Resolve a local repo path to a service repo_id.
    ///
    /// Canonicalizes the path, then checks cache. On cache miss (or on
    /// `repo_not_found` error from a stale cache entry), registers the repo
    /// via POST /repos/add and caches the result.
    pub fn resolve_repo_id(&mut self, repo_path: &Path) -> Result<String, CanopyError> {
        let canonical = std::fs::canonicalize(repo_path)
            .map_err(CanopyError::Io)?
            .to_string_lossy()
            .to_string();

        if let Some(cached_id) = self.repo_id_cache.get(&canonical) {
            return Ok(cached_id.clone());
        }

        let repo_id = self.add_repo(&canonical)?;
        self.repo_id_cache.insert(canonical, repo_id.clone());
        Ok(repo_id)
    }

    /// Invalidate a cached repo_id (e.g., after `repo_not_found` error)
    /// and re-resolve.
    pub fn invalidate_and_resolve(&mut self, repo_path: &Path) -> Result<String, CanopyError> {
        let canonical = std::fs::canonicalize(repo_path)
            .map_err(CanopyError::Io)?
            .to_string_lossy()
            .to_string();

        self.repo_id_cache.remove(&canonical);
        let repo_id = self.add_repo(&canonical)?;
        self.repo_id_cache.insert(canonical, repo_id.clone());
        Ok(repo_id)
    }

    /// Register repo via POST /repos/add (idempotent — service returns existing id if path matches)
    fn add_repo(&self, canonical_path: &str) -> Result<String, CanopyError> {
        let url = format!("{}/repos/add", self.base_url);
        let req = AddRepoRequest {
            path: canonical_path.to_string(),
            name: None,
        };
        let resp =
            self.client
                .post(&url)
                .json(&req)
                .send()
                .map_err(|e| CanopyError::ServiceError {
                    code: "connection_error".to_string(),
                    message: e.to_string(),
                    hint: "Is canopy-service running?".to_string(),
                })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        let body: AddRepoResponse = resp.json().map_err(|e| CanopyError::ServiceError {
            code: "parse_error".to_string(),
            message: e.to_string(),
            hint: "Unexpected response from service".to_string(),
        })?;

        Ok(body.repo_id)
    }

    /// Poll until shard status is Ready, or timeout.
    pub fn ensure_ready(
        &self,
        repo_id: &str,
        timeout: std::time::Duration,
    ) -> Result<(), CanopyError> {
        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_millis(500);

        loop {
            let repos = self.list_repos()?;
            if let Some(shard) = repos.iter().find(|r| r.repo_id == repo_id) {
                match shard.status {
                    ShardStatus::Ready => return Ok(()),
                    ShardStatus::Error => {
                        return Err(CanopyError::ServiceError {
                            code: "index_error".to_string(),
                            message: format!("Repo {} indexing failed", repo_id),
                            hint: "Check service logs and retry with /reindex".to_string(),
                        });
                    }
                    ShardStatus::Pending | ShardStatus::Indexing => {
                        // Keep polling
                    }
                }
            } else {
                return Err(CanopyError::ServiceError {
                    code: "repo_not_found".to_string(),
                    message: format!("Repo {} not found in service", repo_id),
                    hint: "Register the repo first".to_string(),
                });
            }

            if start.elapsed() >= timeout {
                return Err(CanopyError::ServiceError {
                    code: "timeout".to_string(),
                    message: format!("Repo {} still indexing after {:?}", repo_id, timeout),
                    hint: "Wait for indexing to complete or increase timeout".to_string(),
                });
            }

            std::thread::sleep(poll_interval);
        }
    }

    /// Query a repo by repo_id.
    /// On `repo_not_found`, returns the error to the caller (runtime handles retry).
    pub fn query(&self, repo_id: &str, params: QueryParams) -> Result<QueryResult, CanopyError> {
        let url = format!("{}/query", self.base_url);
        let req = QueryRequest {
            repo: repo_id.to_string(),
            params,
        };
        let resp =
            self.client
                .post(&url)
                .json(&req)
                .send()
                .map_err(|e| CanopyError::ServiceError {
                    code: "connection_error".to_string(),
                    message: e.to_string(),
                    hint: "Is canopy-service running?".to_string(),
                })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json::<QueryResult>()
            .map_err(|e| CanopyError::ServiceError {
                code: "parse_error".to_string(),
                message: e.to_string(),
                hint: "Unexpected response from service".to_string(),
            })
    }

    /// Build an evidence pack server-side.
    pub fn evidence_pack(
        &self,
        repo_id: &str,
        params: QueryParams,
        max_handles: Option<usize>,
        max_per_file: Option<usize>,
        plan: Option<bool>,
    ) -> Result<EvidencePack, CanopyError> {
        let url = format!("{}/evidence_pack", self.base_url);
        let req = EvidencePackRequest {
            repo: repo_id.to_string(),
            params,
            max_handles,
            max_per_file,
            plan,
        };
        let resp =
            self.client
                .post(&url)
                .json(&req)
                .send()
                .map_err(|e| CanopyError::ServiceError {
                    code: "connection_error".to_string(),
                    message: e.to_string(),
                    hint: "Is canopy-service running?".to_string(),
                })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json::<EvidencePack>()
            .map_err(|e| CanopyError::ServiceError {
                code: "parse_error".to_string(),
                message: e.to_string(),
                hint: "Unexpected response from service".to_string(),
            })
    }

    /// Expand handles with optional per-handle generation.
    pub fn expand(
        &self,
        repo_id: &str,
        handle_ids: &[String],
        generation: Option<u64>,
    ) -> Result<Vec<(String, String)>, CanopyError> {
        let url = format!("{}/expand", self.base_url);
        let req = ExpandRequest {
            repo: repo_id.to_string(),
            handles: handle_ids
                .iter()
                .map(|id| ExpandHandle {
                    id: id.clone(),
                    generation,
                })
                .collect(),
        };
        let resp =
            self.client
                .post(&url)
                .json(&req)
                .send()
                .map_err(|e| CanopyError::ServiceError {
                    code: "connection_error".to_string(),
                    message: e.to_string(),
                    hint: "Is canopy-service running?".to_string(),
                })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        let body: ExpandResponse = resp.json().map_err(|e| CanopyError::ServiceError {
            code: "parse_error".to_string(),
            message: e.to_string(),
            hint: "Unexpected response from service".to_string(),
        })?;

        Ok(body
            .contents
            .into_iter()
            .map(|c| (c.handle_id, c.content))
            .collect())
    }

    pub fn list_repos(&self) -> Result<Vec<RepoShard>, CanopyError> {
        let url = format!("{}/repos", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .map_err(|e| CanopyError::ServiceError {
                code: "connection_error".to_string(),
                message: e.to_string(),
                hint: "Is canopy-service running?".to_string(),
            })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json().map_err(|e| CanopyError::ServiceError {
            code: "parse_error".to_string(),
            message: e.to_string(),
            hint: "Unexpected response from service".to_string(),
        })
    }

    pub fn status(&self) -> Result<ServiceStatus, CanopyError> {
        let url = format!("{}/status", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .map_err(|e| CanopyError::ServiceError {
                code: "connection_error".to_string(),
                message: e.to_string(),
                hint: "Is canopy-service running?".to_string(),
            })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json().map_err(|e| CanopyError::ServiceError {
            code: "parse_error".to_string(),
            message: e.to_string(),
            hint: "Unexpected response from service".to_string(),
        })
    }

    pub fn reindex(
        &self,
        repo_id: &str,
        glob: Option<String>,
    ) -> Result<ReindexResponse, CanopyError> {
        let url = format!("{}/reindex", self.base_url);
        let req = ReindexRequest {
            repo: repo_id.to_string(),
            glob,
        };
        let resp =
            self.client
                .post(&url)
                .json(&req)
                .send()
                .map_err(|e| CanopyError::ServiceError {
                    code: "connection_error".to_string(),
                    message: e.to_string(),
                    hint: "Is canopy-service running?".to_string(),
                })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json().map_err(|e| CanopyError::ServiceError {
            code: "parse_error".to_string(),
            message: e.to_string(),
            hint: "Unexpected response from service".to_string(),
        })
    }

    fn handle_error<T>(&self, resp: reqwest::blocking::Response) -> Result<T, CanopyError> {
        let status = resp.status();
        match resp.json::<ErrorEnvelope>() {
            Ok(envelope) => Err(CanopyError::ServiceError {
                code: envelope.code,
                message: envelope.message,
                hint: envelope.hint,
            }),
            Err(_) => Err(CanopyError::ServiceError {
                code: format!("http_{}", status.as_u16()),
                message: format!("HTTP {} from service", status),
                hint: "Check service logs".to_string(),
            }),
        }
    }
}

/// Check if a service error has a specific error code
pub fn is_error_code(err: &CanopyError, code: &str) -> bool {
    matches!(err, CanopyError::ServiceError { code: c, .. } if c == code)
}
