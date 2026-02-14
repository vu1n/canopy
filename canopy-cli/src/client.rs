//! HTTP client for canopy-service

use canopy_core::{CanopyError, QueryParams, QueryResult, RepoShard};
use serde::{Deserialize, Serialize};

pub struct ServiceClient {
    base_url: String,
    client: reqwest::blocking::Client,
}

#[derive(Serialize)]
struct QueryRequest {
    repo: String,
    #[serde(flatten)]
    params: QueryParams,
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
struct ReindexRequest {
    repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    glob: Option<String>,
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
pub struct ServiceStatus {
    pub service: String,
    pub repos: Vec<RepoShard>,
}

impl ServiceClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::blocking::Client::new(),
        }
    }

    pub fn query(&self, repo: &str, params: QueryParams) -> Result<QueryResult, CanopyError> {
        let url = format!("{}/query", self.base_url);
        let req = QueryRequest {
            repo: repo.to_string(),
            params,
        };
        let resp = self
            .client
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

    pub fn expand(
        &self,
        repo: &str,
        handle_ids: &[String],
        generation: Option<u64>,
    ) -> Result<Vec<(String, String)>, CanopyError> {
        let url = format!("{}/expand", self.base_url);
        let req = ExpandRequest {
            repo: repo.to_string(),
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
            .map_err(|e| CanopyError::ServiceError {
                code: "connection_error".to_string(),
                message: e.to_string(),
                hint: "Is canopy-service running?".to_string(),
            })?;

        if !resp.status().is_success() {
            return self.handle_error(resp);
        }

        let body: ExpandResponse =
            resp.json()
                .map_err(|e| CanopyError::ServiceError {
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
        repo: &str,
        glob: Option<String>,
    ) -> Result<ReindexResponse, CanopyError> {
        let url = format!("{}/reindex", self.base_url);
        let req = ReindexRequest {
            repo: repo.to_string(),
            glob,
        };
        let resp = self
            .client
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
