//! Shared HTTP request/response types for the canopy service protocol.
//!
//! These types define the contract between canopy-service and canopy-client,
//! ensuring both sides stay in sync without manual duplication.

use crate::{QueryParams, RepoShard};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub repo: String,
    #[serde(flatten)]
    pub params: QueryParams,
}

/// Configuration for evidence pack assembly (limits and planning toggle).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidencePackConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_handles: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_file: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePackRequest {
    pub repo: String,
    #[serde(flatten)]
    pub params: QueryParams,
    #[serde(default)]
    pub config: EvidencePackConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandRequest {
    pub repo: String,
    pub handles: Vec<ExpandHandle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandHandle {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandResponse {
    pub contents: Vec<ExpandedContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandedContent {
    pub handle_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRepoRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRepoResponse {
    pub repo_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReindexRequest {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReindexResponse {
    pub generation: u64,
    pub status: String,
    pub commit_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub service: String,
    pub repos: Vec<RepoShard>,
}
