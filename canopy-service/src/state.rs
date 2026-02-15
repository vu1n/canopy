use canopy_core::{
    feedback::FeedbackStore, CanopyError, NodeType, QueryResult, RepoIndex, RepoShard,
};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::RwLock;

pub type SharedState = Arc<AppState>;
pub const QUERY_CACHE_MAX_ENTRIES: usize = 128;
pub const RECENT_QUERY_EVENT_CAP: usize = 20_000;
pub const NODE_TYPE_PRIOR_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
type NodeTypePriors = HashMap<NodeType, f64>;
type NodeTypePriorsCacheEntry = (Instant, NodeTypePriors);

pub struct ServiceMetrics {
    pub query_count: AtomicU64,
    pub query_cache_hits: AtomicU64,
    pub query_cache_misses: AtomicU64,
    pub expand_count: AtomicU64,
    pub index_cache_hits: AtomicU64,
    pub index_cache_misses: AtomicU64,
    pub reindex_count: AtomicU64,
    pub total_query_ms: AtomicU64,
    pub total_expand_ms: AtomicU64,
    pub analytics: Mutex<QueryAnalytics>,
}

impl ServiceMetrics {
    pub fn new() -> Self {
        Self {
            query_count: AtomicU64::new(0),
            query_cache_hits: AtomicU64::new(0),
            query_cache_misses: AtomicU64::new(0),
            expand_count: AtomicU64::new(0),
            index_cache_hits: AtomicU64::new(0),
            index_cache_misses: AtomicU64::new(0),
            reindex_count: AtomicU64::new(0),
            total_query_ms: AtomicU64::new(0),
            total_expand_ms: AtomicU64::new(0),
            analytics: Mutex::new(QueryAnalytics::new()),
        }
    }
}

pub struct QueryAnalytics {
    pub top_symbols: HashMap<String, u64>,
    pub top_patterns: HashMap<String, u64>,
    pub top_expanded_files: HashMap<String, u64>,
    pub queries_by_repo: HashMap<String, u64>,
}

impl QueryAnalytics {
    pub fn new() -> Self {
        Self {
            top_symbols: HashMap::new(),
            top_patterns: HashMap::new(),
            top_expanded_files: HashMap::new(),
            queries_by_repo: HashMap::new(),
        }
    }
}

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
    pub metrics: ServiceMetrics,
    indexes: RwLock<HashMap<String, Arc<CachedIndex>>>,
    query_caches: RwLock<HashMap<String, RepoQueryCache>>,
    feedback_stores: RwLock<HashMap<String, Arc<Mutex<FeedbackStore>>>>,
    node_type_priors_cache: RwLock<HashMap<String, NodeTypePriorsCacheEntry>>,
    recent_handle_query_events: RwLock<HashMap<(String, String), i64>>,
    recent_handle_query_order: RwLock<VecDeque<(String, String)>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            shards: RwLock::new(HashMap::new()),
            metrics: ServiceMetrics::new(),
            indexes: RwLock::new(HashMap::new()),
            query_caches: RwLock::new(HashMap::new()),
            feedback_stores: RwLock::new(HashMap::new()),
            node_type_priors_cache: RwLock::new(HashMap::new()),
            recent_handle_query_events: RwLock::new(HashMap::new()),
            recent_handle_query_order: RwLock::new(VecDeque::new()),
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
                    self.metrics
                        .index_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(Arc::clone(cached));
                }
            }
        }
        self.metrics
            .index_cache_misses
            .fetch_add(1, Ordering::Relaxed);

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
        self.feedback_stores.write().await.remove(repo_id);
        self.node_type_priors_cache.write().await.remove(repo_id);
        self.recent_handle_query_events
            .write()
            .await
            .retain(|(r, _), _| r != repo_id);
        self.recent_handle_query_order
            .write()
            .await
            .retain(|(r, _)| r != repo_id);
    }

    pub async fn feedback_store_for_repo(
        &self,
        repo_id: &str,
        repo_root: &str,
    ) -> Option<Arc<Mutex<FeedbackStore>>> {
        {
            let stores = self.feedback_stores.read().await;
            if let Some(store) = stores.get(repo_id) {
                return Some(Arc::clone(store));
            }
        }

        let opened = match FeedbackStore::open(Path::new(repo_root)) {
            Ok(store) => Arc::new(Mutex::new(store)),
            Err(err) => {
                eprintln!(
                    "[canopy-service] feedback disabled for repo {}: {}",
                    repo_id, err
                );
                return None;
            }
        };

        let mut stores = self.feedback_stores.write().await;
        if let Some(existing) = stores.get(repo_id) {
            return Some(Arc::clone(existing));
        }
        stores.insert(repo_id.to_string(), Arc::clone(&opened));
        Some(opened)
    }

    pub async fn load_node_type_priors(
        &self,
        repo_id: &str,
        repo_root: &str,
    ) -> Option<NodeTypePriors> {
        {
            let cache = self.node_type_priors_cache.read().await;
            if let Some((loaded_at, priors)) = cache.get(repo_id) {
                if loaded_at.elapsed() < NODE_TYPE_PRIOR_CACHE_TTL {
                    return Some(priors.clone());
                }
            }
        }

        let store = self.feedback_store_for_repo(repo_id, repo_root).await?;
        let priors = {
            let Ok(store_guard) = store.lock() else {
                eprintln!(
                    "[canopy-service] feedback lock poisoned while loading priors for {}",
                    repo_id
                );
                return None;
            };
            match store_guard.get_node_type_priors() {
                Ok(priors) => priors,
                Err(err) => {
                    eprintln!(
                        "[canopy-service] failed to load node type priors for {}: {}",
                        repo_id, err
                    );
                    return None;
                }
            }
        };

        if priors.is_empty() {
            return None;
        }

        self.node_type_priors_cache
            .write()
            .await
            .insert(repo_id.to_string(), (Instant::now(), priors.clone()));
        Some(priors)
    }

    pub async fn invalidate_node_type_priors_cache(&self, repo_id: &str) {
        self.node_type_priors_cache.write().await.remove(repo_id);
    }

    pub async fn remember_query_event_for_handles(
        &self,
        repo_id: &str,
        handle_ids: &[String],
        query_event_id: i64,
    ) {
        let mut map = self.recent_handle_query_events.write().await;
        let mut order = self.recent_handle_query_order.write().await;

        for handle_id in handle_ids {
            let key = (repo_id.to_string(), handle_id.clone());
            if map.contains_key(&key) {
                map.insert(key, query_event_id);
            } else {
                map.insert(key.clone(), query_event_id);
                order.push_back(key);
            }
        }

        while map.len() > RECENT_QUERY_EVENT_CAP {
            if let Some(oldest) = order.pop_front() {
                map.remove(&oldest);
            } else {
                break;
            }
        }
    }

    pub async fn recent_query_events_for_handles(
        &self,
        repo_id: &str,
        handle_ids: &[String],
    ) -> HashMap<String, i64> {
        let map = self.recent_handle_query_events.read().await;
        let mut out = HashMap::new();
        for handle_id in handle_ids {
            let key = (repo_id.to_string(), handle_id.clone());
            if let Some(query_event_id) = map.get(&key).copied() {
                out.insert(handle_id.clone(), query_event_id);
            }
        }
        out
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
            expanded_count: 0,
            expanded_tokens: 0,
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
