use canopy_core::RepoShard;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub shards: RwLock<HashMap<String, RepoShard>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            shards: RwLock::new(HashMap::new()),
        }
    }
}
