use crate::error::AppError;
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::{Generation, HandleSource, QueryParams, RepoIndex, RepoShard, ShardStatus};
use serde::{Deserialize, Serialize};
use std::path::Path;

// POST /query
#[derive(Deserialize)]
pub struct QueryRequest {
    pub repo: String,
    #[serde(flatten)]
    pub params: QueryParams,
}

pub async fn query(
    State(state): State<SharedState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<canopy_core::QueryResult>, AppError> {
    let shards = state.shards.read().await;
    let shard = shards
        .get(&req.repo)
        .ok_or_else(|| AppError::not_found("repo"))?;

    if shard.status != ShardStatus::Ready {
        return Err(AppError::internal(format!(
            "Repo {} is not ready (status: {:?})",
            req.repo, shard.status
        )));
    }

    let repo_root = shard.repo_root.clone();
    let commit_sha = shard.commit_sha.clone();
    let generation = shard.generation.value();
    drop(shards);

    let params = req.params;
    // Run blocking index operations in spawn_blocking
    let result = tokio::task::spawn_blocking(move || {
        let index = RepoIndex::open(Path::new(&repo_root))?;
        let mut result = index.query_params(params)?;
        // Stamp handles with service metadata
        for handle in &mut result.handles {
            handle.source = HandleSource::Service;
            handle.commit_sha = commit_sha.clone();
            handle.generation = Some(generation);
        }
        Ok::<_, canopy_core::CanopyError>(result)
    })
    .await
    .map_err(|e| AppError::internal(e))??;

    Ok(Json(result))
}

// POST /expand
#[derive(Deserialize)]
pub struct ExpandRequest {
    pub repo: String,
    pub handles: Vec<ExpandHandle>,
}

#[derive(Deserialize)]
pub struct ExpandHandle {
    pub id: String,
    #[serde(default)]
    pub generation: Option<u64>,
}

#[derive(Serialize)]
pub struct ExpandResponse {
    pub contents: Vec<ExpandedContent>,
}

#[derive(Serialize)]
pub struct ExpandedContent {
    pub handle_id: String,
    pub content: String,
}

pub async fn expand(
    State(state): State<SharedState>,
    Json(req): Json<ExpandRequest>,
) -> Result<Json<ExpandResponse>, AppError> {
    let shards = state.shards.read().await;
    let shard = shards
        .get(&req.repo)
        .ok_or_else(|| AppError::not_found("repo"))?;

    let current_gen = shard.generation.value();
    // Validate generation if provided
    for h in &req.handles {
        if let Some(gen) = h.generation {
            if gen != current_gen {
                return Err(AppError::stale(current_gen, gen));
            }
        }
    }

    let repo_root = shard.repo_root.clone();
    drop(shards);

    let handle_ids: Vec<String> = req.handles.iter().map(|h| h.id.clone()).collect();

    let contents = tokio::task::spawn_blocking(move || {
        let index = RepoIndex::open(Path::new(&repo_root))?;
        index.expand(&handle_ids)
    })
    .await
    .map_err(|e| AppError::internal(e))??;

    Ok(Json(ExpandResponse {
        contents: contents
            .into_iter()
            .map(|(id, content)| ExpandedContent {
                handle_id: id,
                content,
            })
            .collect(),
    }))
}

// POST /repos/add
#[derive(Deserialize)]
pub struct AddRepoRequest {
    pub path: String,
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct AddRepoResponse {
    pub repo_id: String,
    pub name: String,
}

pub async fn add_repo(
    State(state): State<SharedState>,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<AddRepoResponse>, AppError> {
    let path = std::path::Path::new(&req.path);

    // Validate it's a git repo
    if !path.join(".git").exists() {
        return Err(AppError {
            status: axum::http::StatusCode::BAD_REQUEST,
            body: crate::error::ErrorEnvelope::new(
                "invalid_repo",
                "Not a git repository",
                "Provide a path to a git repository root",
            ),
        });
    }

    // Init canopy if needed
    if !path.join(".canopy").exists() {
        tokio::task::spawn_blocking({
            let path = req.path.clone();
            move || RepoIndex::init(Path::new(&path))
        })
        .await
        .map_err(|e| AppError::internal(e))??;
    }

    let name = req.name.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string())
    });

    let repo_id = uuid::Uuid::new_v4().to_string();

    let shard = RepoShard {
        repo_id: repo_id.clone(),
        repo_root: req.path.clone(),
        name: name.clone(),
        commit_sha: None,
        generation: Generation::new(),
        status: ShardStatus::Pending,
    };

    state.shards.write().await.insert(repo_id.clone(), shard);

    Ok(Json(AddRepoResponse { repo_id, name }))
}

// GET /repos
pub async fn list_repos(State(state): State<SharedState>) -> Json<Vec<RepoShard>> {
    let shards = state.shards.read().await;
    Json(shards.values().cloned().collect())
}

// GET /status
#[derive(Serialize)]
pub struct ServiceStatus {
    pub service: String,
    pub repos: Vec<RepoShard>,
}

pub async fn status(State(state): State<SharedState>) -> Json<ServiceStatus> {
    let shards = state.shards.read().await;
    Json(ServiceStatus {
        service: "canopy-service".to_string(),
        repos: shards.values().cloned().collect(),
    })
}

// POST /reindex
#[derive(Deserialize)]
pub struct ReindexRequest {
    pub repo: String,
    pub glob: Option<String>,
}

#[derive(Serialize)]
pub struct ReindexResponse {
    pub generation: u64,
    pub status: String,
    pub commit_sha: Option<String>,
}

pub async fn reindex(
    State(state): State<SharedState>,
    Json(req): Json<ReindexRequest>,
) -> Result<Json<ReindexResponse>, AppError> {
    let mut shards = state.shards.write().await;
    let shard = shards
        .get_mut(&req.repo)
        .ok_or_else(|| AppError::not_found("repo"))?;

    // Coalesce: if already indexing, return current generation
    if shard.status == ShardStatus::Indexing {
        return Ok(Json(ReindexResponse {
            generation: shard.generation.value(),
            status: "already_indexing".to_string(),
            commit_sha: shard.commit_sha.clone(),
        }));
    }

    shard.status = ShardStatus::Indexing;
    let repo_root = shard.repo_root.clone();
    let repo_id = shard.repo_id.clone();
    let glob = req.glob;
    drop(shards);

    let state_clone = state.clone();

    tokio::task::spawn(async move {
        let result = tokio::task::spawn_blocking({
            let repo_root = repo_root.clone();
            let glob = glob.clone();
            move || {
                // Get commit SHA
                let commit_sha = std::process::Command::new("git")
                    .arg("rev-parse")
                    .arg("HEAD")
                    .current_dir(&repo_root)
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

                let mut index = RepoIndex::open(Path::new(&repo_root))?;
                let default_glob = index.config().default_glob().to_string();
                let glob_str = glob.as_deref().unwrap_or(&default_glob);
                let _stats = index.index(glob_str)?;

                Ok::<_, canopy_core::CanopyError>(commit_sha)
            }
        })
        .await;

        let mut shards = state_clone.shards.write().await;
        if let Some(shard) = shards.get_mut(&repo_id) {
            match result {
                Ok(Ok(commit_sha)) => {
                    shard.generation = shard.generation.next();
                    shard.commit_sha = commit_sha;
                    shard.status = ShardStatus::Ready;
                }
                _ => {
                    shard.status = ShardStatus::Error;
                }
            }
        }
    });

    // Return current state (indexing has started)
    let shards = state.shards.read().await;
    let shard = shards
        .get(&req.repo)
        .ok_or_else(|| AppError::not_found("repo"))?;
    Ok(Json(ReindexResponse {
        generation: shard.generation.value(),
        status: "indexing".to_string(),
        commit_sha: shard.commit_sha.clone(),
    }))
}
