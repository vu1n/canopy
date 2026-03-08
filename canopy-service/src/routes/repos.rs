//! Repo management route handlers: add_repo, list_repos, status, reindex.

use crate::error::AppError;
use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use canopy_core::protocol::{
    AddRepoRequest, AddRepoResponse, ReindexRequest, ReindexResponse, ServiceStatus,
};
use canopy_core::{Generation, RepoIndex, RepoShard, ShardStatus};
use std::path::Path;
use std::sync::atomic::Ordering;

use super::utc_log_timestamp;
use tracing::info;

pub(crate) async fn add_repo(
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

    // Canonicalize path ONCE before taking the lock
    let canonical = std::fs::canonicalize(&req.path)
        .map_err(AppError::internal)?
        .to_string_lossy()
        .to_string();

    // Init canopy if needed
    if !path.join(".canopy").exists() {
        tokio::task::spawn_blocking({
            let canonical = canonical.clone();
            move || RepoIndex::init(Path::new(&canonical))
        })
        .await
        .map_err(AppError::internal)??;
    }

    let name = req.name.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string())
    });

    // Idempotent: check if a shard with the same canonical root already exists
    let mut shards = state.shards.write().await;
    for (id, shard) in shards.iter() {
        if shard.repo_root == canonical {
            info!(
                "[{}] POST /repos/add name={} repo_id={} (existing)",
                utc_log_timestamp(),
                shard.name,
                id
            );
            return Ok(Json(AddRepoResponse {
                repo_id: id.clone(),
                name: shard.name.clone(),
            }));
        }
    }

    let repo_id = uuid::Uuid::new_v4().to_string();

    let shard = RepoShard {
        repo_id: repo_id.clone(),
        repo_root: canonical,
        name: name.clone(),
        commit_sha: None,
        generation: Generation::new(),
        status: ShardStatus::Pending,
        error_message: None,
    };

    shards.insert(repo_id.clone(), shard);
    drop(shards);

    info!(
        "[{}] POST /repos/add name={} repo_id={}",
        utc_log_timestamp(),
        name,
        repo_id
    );

    Ok(Json(AddRepoResponse { repo_id, name }))
}

pub(crate) async fn list_repos(State(state): State<SharedState>) -> Json<Vec<RepoShard>> {
    let shards = state.shards.read().await;
    Json(shards.values().cloned().collect())
}

pub(crate) async fn status(State(state): State<SharedState>) -> Json<ServiceStatus> {
    let shards = state.shards.read().await;
    Json(ServiceStatus {
        service: "canopy-service".to_string(),
        repos: shards.values().cloned().collect(),
    })
}

pub(crate) async fn reindex(
    State(state): State<SharedState>,
    Json(req): Json<ReindexRequest>,
) -> Result<Json<ReindexResponse>, AppError> {
    let repo_label = req.repo.clone();
    let mut shards = state.shards.write().await;
    let shard = shards
        .get_mut(&req.repo)
        .ok_or_else(AppError::repo_not_found)?;

    // Coalesce: if already indexing, return current generation
    if shard.status == ShardStatus::Indexing {
        info!(
            "[{}] POST /reindex repo={} status=already_indexing",
            utc_log_timestamp(),
            repo_label
        );
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

    state.metrics.reindex_count.fetch_add(1, Ordering::Relaxed);
    info!(
        "[{}] POST /reindex repo={} status=started",
        utc_log_timestamp(),
        repo_label
    );

    let state_clone = state.clone();

    tokio::task::spawn(async move {
        let result = tokio::task::spawn_blocking({
            let repo_root = repo_root.clone();
            let glob = glob.clone();
            move || {
                let commit_sha = canopy_core::git::head_commit_sha(Path::new(&repo_root));

                let mut index = RepoIndex::open(Path::new(&repo_root))?;
                let default_glob = index.config().default_glob().to_string();
                let glob_str = glob.as_deref().unwrap_or(&default_glob);
                let _stats = index.index(glob_str)?;

                Ok::<_, canopy_core::CanopyError>(commit_sha)
            }
        })
        .await;

        match result {
            Ok(Ok(commit_sha)) => {
                state_clone.invalidate_repo(&repo_id).await;
                let mut shards = state_clone.shards.write().await;
                if let Some(shard) = shards.get_mut(&repo_id) {
                    shard.generation = shard.generation.next();
                    shard.commit_sha = commit_sha;
                    shard.status = ShardStatus::Ready;
                    shard.error_message = None;
                }
            }
            Ok(Err(e)) => {
                let mut shards = state_clone.shards.write().await;
                if let Some(shard) = shards.get_mut(&repo_id) {
                    shard.status = ShardStatus::Error;
                    shard.error_message = Some(e.to_string());
                }
            }
            Err(e) => {
                let mut shards = state_clone.shards.write().await;
                if let Some(shard) = shards.get_mut(&repo_id) {
                    shard.status = ShardStatus::Error;
                    shard.error_message = Some(format!("task panicked: {}", e));
                }
            }
        }
    });

    // Return current state (indexing has started)
    let shards = state.shards.read().await;
    let shard = shards.get(&req.repo).ok_or_else(AppError::repo_not_found)?;
    Ok(Json(ReindexResponse {
        generation: shard.generation.value(),
        status: "indexing".to_string(),
        commit_sha: shard.commit_sha.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{insert_test_shard, test_state};
    use tempfile::TempDir;

    fn make_git_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        dir
    }

    #[tokio::test]
    async fn add_repo_rejects_non_git_dir() {
        let state = test_state();
        let dir = TempDir::new().unwrap();
        let result = add_repo(
            State(state),
            Json(AddRepoRequest {
                path: dir.path().to_string_lossy().to_string(),
                name: None,
            }),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn add_repo_succeeds_for_git_repo() {
        let state = test_state();
        let dir = make_git_repo();
        let result = add_repo(
            State(state),
            Json(AddRepoRequest {
                path: dir.path().to_string_lossy().to_string(),
                name: Some("test-repo".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(result.name, "test-repo");
        assert!(!result.repo_id.is_empty());
    }

    #[tokio::test]
    async fn add_repo_is_idempotent() {
        let state = test_state();
        let dir = make_git_repo();
        let path = dir.path().to_string_lossy().to_string();

        let first = add_repo(
            State(state.clone()),
            Json(AddRepoRequest {
                path: path.clone(),
                name: None,
            }),
        )
        .await
        .unwrap();

        let second = add_repo(
            State(state),
            Json(AddRepoRequest {
                path,
                name: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(first.repo_id, second.repo_id);
    }

    #[tokio::test]
    async fn list_repos_empty_initially() {
        let state = test_state();
        let result = list_repos(State(state)).await;
        assert!(result.0.is_empty());
    }

    #[tokio::test]
    async fn list_repos_after_add() {
        let state = test_state();
        let dir = make_git_repo();
        let _ = add_repo(
            State(state.clone()),
            Json(AddRepoRequest {
                path: dir.path().to_string_lossy().to_string(),
                name: Some("my-repo".to_string()),
            }),
        )
        .await
        .unwrap();

        let repos = list_repos(State(state)).await;
        assert_eq!(repos.0.len(), 1);
        assert_eq!(repos.0[0].name, "my-repo");
    }

    #[tokio::test]
    async fn reindex_unknown_repo_returns_error() {
        let state = test_state();
        let result = reindex(
            State(state),
            Json(ReindexRequest {
                repo: "nonexistent".to_string(),
                glob: None,
            }),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reindex_coalesces_concurrent_requests() {
        let state = test_state();
        let repo_id = "indexing-repo";
        insert_test_shard(
            &state,
            repo_id,
            "indexing",
            ShardStatus::Indexing,
            Generation::from_value(2),
        )
        .await;

        let result = reindex(
            State(state),
            Json(ReindexRequest {
                repo: repo_id.to_string(),
                glob: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(result.status, "already_indexing");
        assert_eq!(result.generation, 2);
    }

    #[tokio::test]
    async fn status_includes_service_name() {
        let state = test_state();
        let result = status(State(state)).await;
        assert_eq!(result.service, "canopy-service");
        assert!(result.repos.is_empty());
    }
}
