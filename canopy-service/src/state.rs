use canopy_core::{CanopyError, QueryResult, RepoIndex, RepoShard};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

pub type SharedState = Arc<AppState>;
pub const QUERY_CACHE_MAX_ENTRIES: usize = 128;

pub struct CachedIndex {
    pub index: Mutex<RepoIndex>,
    pub generation: u64,
}

pub struct RepoQueryCache {
    entries: HashMap<String, QueryResult>,
    order: VecDeque<String>,
    generation: u64,
    max_entries: usize,
}

impl RepoQueryCache {
    pub fn new(generation: u64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            generation,
            max_entries,
        }
    }

    pub fn get(&self, key: &str, generation: u64) -> Option<&QueryResult> {
        if self.generation != generation {
            return None;
        }
        self.entries.get(key)
    }

    pub fn insert(&mut self, key: String, result: QueryResult, generation: u64) {
        if self.generation != generation {
            self.clear();
            self.generation = generation;
        }

        let key_exists = self.entries.contains_key(&key);
        self.entries.insert(key.clone(), result);

        // Keep FIFO stable by only tracking first insertion of a key.
        if !key_exists {
            self.order.push_back(key);
        }

        while self.entries.len() > self.max_entries {
            if let Some(oldest_key) = self.order.pop_front() {
                self.entries.remove(&oldest_key);
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

pub struct AppState {
    pub shards: RwLock<HashMap<String, RepoShard>>,
    indexes: RwLock<HashMap<String, Arc<CachedIndex>>>,
    query_caches: RwLock<HashMap<String, RepoQueryCache>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            shards: RwLock::new(HashMap::new()),
            indexes: RwLock::new(HashMap::new()),
            query_caches: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_or_open_index(
        &self,
        repo_id: &str,
        repo_root: &str,
        generation: u64,
    ) -> Result<Arc<CachedIndex>, CanopyError> {
        {
            let indexes = self.indexes.read().await;
            if let Some(cached) = indexes.get(repo_id) {
                if cached.generation == generation {
                    return Ok(Arc::clone(cached));
                }
            }
        }

        let repo_root = repo_root.to_string();
        let index = tokio::task::spawn_blocking(move || RepoIndex::open(Path::new(&repo_root)))
            .await
            .map_err(|err| {
                CanopyError::Io(io::Error::other(format!(
                    "RepoIndex open task failed: {err}"
                )))
            })??;

        let candidate = Arc::new(CachedIndex {
            index: Mutex::new(index),
            generation,
        });

        let mut indexes = self.indexes.write().await;
        if let Some(existing) = indexes.get(repo_id) {
            if existing.generation == generation {
                return Ok(Arc::clone(existing));
            }
        }

        indexes.insert(repo_id.to_string(), Arc::clone(&candidate));
        Ok(candidate)
    }

    pub async fn get_cached_query(
        &self,
        repo_id: &str,
        cache_key: &str,
        generation: u64,
    ) -> Option<QueryResult> {
        let caches = self.query_caches.read().await;
        caches
            .get(repo_id)
            .and_then(|repo_cache| repo_cache.get(cache_key, generation).cloned())
    }

    pub async fn insert_cached_query(
        &self,
        repo_id: &str,
        cache_key: String,
        result: QueryResult,
        generation: u64,
    ) {
        if result.auto_expanded {
            return;
        }

        let mut caches = self.query_caches.write().await;
        let repo_cache = caches
            .entry(repo_id.to_string())
            .or_insert_with(|| RepoQueryCache::new(generation, QUERY_CACHE_MAX_ENTRIES));
        repo_cache.insert(cache_key, result, generation);
    }

    pub async fn invalidate_repo(&self, repo_id: &str) {
        self.indexes.write().await.remove(repo_id);
        self.query_caches.write().await.remove(repo_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(total_matches: usize) -> QueryResult {
        QueryResult {
            handles: Vec::new(),
            ref_handles: None,
            total_tokens: 0,
            truncated: false,
            total_matches,
            auto_expanded: false,
            expand_note: None,
        }
    }

    #[test]
    fn query_cache_evicts_fifo_with_duplicate_keys() {
        let mut cache = RepoQueryCache::new(1, 2);

        cache.insert("a".to_string(), result(1), 1);
        cache.insert("a".to_string(), result(2), 1); // update existing key
        cache.insert("b".to_string(), result(3), 1);
        cache.insert("c".to_string(), result(4), 1); // should evict "a"

        assert!(cache.get("a", 1).is_none());
        assert!(cache.get("b", 1).is_some());
        assert!(cache.get("c", 1).is_some());
    }

    #[test]
    fn query_cache_flushes_on_generation_change() {
        let mut cache = RepoQueryCache::new(1, 2);

        cache.insert("a".to_string(), result(1), 1);
        assert!(cache.get("a", 1).is_some());

        cache.insert("b".to_string(), result(2), 2);

        assert!(cache.get("a", 2).is_none());
        assert!(cache.get("b", 2).is_some());
        assert!(cache.get("b", 1).is_none());
    }
}
