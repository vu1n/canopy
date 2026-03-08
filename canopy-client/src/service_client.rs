//! HTTP client for canopy-service

use canopy_core::protocol::{
    AddRepoRequest, AddRepoResponse, EvidencePackConfig, ExpandHandle, ExpandRequest,
    ExpandResponse, EvidencePackRequest, QueryRequest, ReindexRequest,
};
use canopy_core::{
    CanopyError, ErrorEnvelope, EvidencePack, QueryParams, QueryResult, RepoShard, ShardStatus,
};
use std::collections::HashMap;
use std::path::Path;

// Re-export shared types for callers that depend on them via this crate.
pub use canopy_core::protocol::{ReindexResponse, ServiceStatus};

pub struct ServiceClient {
    base_url: String,
    client: reqwest::blocking::Client,
    /// API key for admin routes (sent as X-Api-Key header)
    api_key: Option<String>,
    /// Cache: canonical path → repo_id
    repo_id_cache: HashMap<String, String>,
}

impl ServiceClient {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::blocking::Client::new(),
            api_key,
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
        let mut builder = self.client.post(&url).json(&req);
        builder = self.apply_api_key(builder);
        let resp = builder.send().map_err(Self::connection_error)?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        let body: AddRepoResponse = resp.json().map_err(Self::parse_error)?;

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

    /// Resolve a repo and ensure it is ready, retrying once on `repo_not_found`.
    ///
    /// Handles the common pattern: resolve_repo_id → ensure_ready → on not_found →
    /// invalidate_and_resolve → ensure_ready again.
    pub fn resolve_ready(
        &mut self,
        repo_path: &Path,
        timeout: std::time::Duration,
    ) -> Result<String, CanopyError> {
        let repo_id = self.resolve_repo_id(repo_path)?;
        match self.ensure_ready(&repo_id, timeout) {
            Ok(()) => Ok(repo_id),
            Err(e) if is_error_code(&e, "repo_not_found") => {
                let new_id = self.invalidate_and_resolve(repo_path)?;
                self.ensure_ready(&new_id, timeout)?;
                Ok(new_id)
            }
            Err(e) => Err(e),
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
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .map_err(Self::connection_error)?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json::<QueryResult>().map_err(Self::parse_error)
    }

    /// Build an evidence pack server-side.
    pub fn evidence_pack(
        &self,
        repo_id: &str,
        params: QueryParams,
        config: EvidencePackConfig,
    ) -> Result<EvidencePack, CanopyError> {
        let url = format!("{}/evidence_pack", self.base_url);
        let req = EvidencePackRequest {
            repo: repo_id.to_string(),
            params,
            config,
        };
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .map_err(Self::connection_error)?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json::<EvidencePack>().map_err(Self::parse_error)
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
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .map_err(Self::connection_error)?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        let body: ExpandResponse = resp.json().map_err(Self::parse_error)?;

        Ok(body
            .contents
            .into_iter()
            .map(|c| (c.handle_id, c.content))
            .collect())
    }

    pub fn list_repos(&self) -> Result<Vec<RepoShard>, CanopyError> {
        self.get_json("/repos")
    }

    pub fn status(&self) -> Result<ServiceStatus, CanopyError> {
        self.get_json("/status")
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
        let mut builder = self.client.post(&url).json(&req);
        builder = self.apply_api_key(builder);
        let resp = builder.send().map_err(Self::connection_error)?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json().map_err(Self::parse_error)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, CanopyError> {
        let url = format!("{}{}", self.base_url, path);
        let req = self.apply_api_key(self.client.get(&url));
        let resp = req.send().map_err(Self::connection_error)?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        resp.json().map_err(Self::parse_error)
    }

    fn connection_error(e: reqwest::Error) -> CanopyError {
        CanopyError::ServiceError {
            code: "connection_error".to_string(),
            message: e.to_string(),
            hint: "Is canopy-service running?".to_string(),
        }
    }

    fn parse_error(e: impl std::fmt::Display) -> CanopyError {
        CanopyError::ServiceError {
            code: "parse_error".to_string(),
            message: e.to_string(),
            hint: "Unexpected response from service".to_string(),
        }
    }

    /// Attach X-Api-Key header to a request if an API key is configured.
    fn apply_api_key(
        &self,
        builder: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(ref key) = self.api_key {
            builder.header("x-api-key", key)
        } else {
            builder
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_client_new_trims_trailing_slash() {
        let client = ServiceClient::new("http://localhost:3000/", None);
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn service_client_new_preserves_url_without_trailing_slash() {
        let client = ServiceClient::new("http://localhost:3000", None);
        assert_eq!(client.base_url, "http://localhost:3000");
    }

    #[test]
    fn service_client_stores_api_key() {
        let client = ServiceClient::new("http://localhost:3000", Some("secret-key".to_string()));
        assert_eq!(client.api_key, Some("secret-key".to_string()));
    }

    #[test]
    fn is_error_code_matches_service_error() {
        let err = CanopyError::ServiceError {
            code: "repo_not_found".to_string(),
            message: "not found".to_string(),
            hint: "register it".to_string(),
        };
        assert!(is_error_code(&err, "repo_not_found"));
        assert!(!is_error_code(&err, "stale_generation"));
    }

    #[test]
    fn is_error_code_returns_false_for_non_service_errors() {
        let err = CanopyError::HandleNotFound("h1".to_string());
        assert!(!is_error_code(&err, "handle_not_found"));

        let err = CanopyError::InvalidHandle("bad".to_string());
        assert!(!is_error_code(&err, "invalid_handle"));
    }

    #[test]
    fn query_request_serialization() {
        let req = QueryRequest {
            repo: "my-repo".to_string(),
            params: QueryParams {
                pattern: Some("auth".to_string()),
                ..Default::default()
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["repo"], "my-repo");
        assert_eq!(json["pattern"], "auth");
    }

    #[test]
    fn expand_request_serialization() {
        let req = ExpandRequest {
            repo: "my-repo".to_string(),
            handles: vec![
                ExpandHandle {
                    id: "h1".to_string(),
                    generation: Some(5),
                },
                ExpandHandle {
                    id: "h2".to_string(),
                    generation: None,
                },
            ],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["repo"], "my-repo");
        assert_eq!(json["handles"][0]["id"], "h1");
        assert_eq!(json["handles"][0]["generation"], 5);
        assert_eq!(json["handles"][1]["id"], "h2");
        assert!(json["handles"][1].get("generation").is_none());
    }

    #[test]
    fn expand_response_deserialization() {
        let json = r#"{
            "contents": [
                {"handle_id": "h1", "content": "fn main() {}"},
                {"handle_id": "h2", "content": "fn foo() {}"}
            ]
        }"#;
        let resp: ExpandResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.contents.len(), 2);
        assert_eq!(resp.contents[0].handle_id, "h1");
        assert_eq!(resp.contents[0].content, "fn main() {}");
    }

    #[test]
    fn reindex_response_deserialization() {
        let json = r#"{
            "generation": 42,
            "status": "ok",
            "commit_sha": "abc123"
        }"#;
        let resp: ReindexResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.generation, 42);
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.commit_sha, Some("abc123".to_string()));
    }

    #[test]
    fn reindex_response_without_commit_sha() {
        let json = r#"{
            "generation": 1,
            "status": "ok",
            "commit_sha": null
        }"#;
        let resp: ReindexResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.generation, 1);
        assert!(resp.commit_sha.is_none());
    }

    #[test]
    fn add_repo_request_skips_none_name() {
        let req = AddRepoRequest {
            path: "/home/user/repo".to_string(),
            name: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["path"], "/home/user/repo");
        assert!(json.get("name").is_none() || json["name"].is_null());
    }

    #[test]
    fn evidence_pack_request_nests_config() {
        let req = EvidencePackRequest {
            repo: "my-repo".to_string(),
            params: QueryParams::default(),
            config: EvidencePackConfig::default(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["repo"], "my-repo");
        // Config is now a nested object, not flattened
        let config = &json["config"];
        assert!(config.is_object());
        // skip_serializing_if = "Option::is_none" omits None fields inside config
        assert!(config.get("max_handles").is_none());
        assert!(config.get("max_per_file").is_none());
        assert!(config.get("plan").is_none());
    }
}
