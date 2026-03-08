//! Metrics response types and the GET /metrics handler.

use crate::state::SharedState;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

const TOP_N: usize = 20;

#[derive(Serialize)]
pub struct MetricsResponse {
    pub performance: PerformanceMetrics,
    pub analytics: AnalyticsMetrics,
}

#[derive(Serialize)]
pub struct PerformanceMetrics {
    pub queries: u64,
    pub query_cache_hit_rate: f64,
    pub query_cache_hits: u64,
    pub query_cache_misses: u64,
    pub expands: u64,
    pub index_cache_hits: u64,
    pub index_cache_misses: u64,
    pub reindexes: u64,
    pub avg_query_ms: u64,
    pub avg_expand_ms: u64,
}

#[derive(Serialize)]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct PatternCount {
    pub pattern: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct PathCount {
    pub path: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct AnalyticsMetrics {
    pub top_symbols: Vec<NamedCount>,
    pub top_patterns: Vec<PatternCount>,
    pub top_expanded_files: Vec<PathCount>,
    pub requests_by_repo: HashMap<String, u64>,
    pub feedback_by_repo: HashMap<String, FeedbackSummary>,
}

#[derive(Serialize, Clone)]
pub struct FeedbackSummary {
    pub glob_hit_rate_at_k: f64,
    pub handle_expand_accept_rate: f64,
    pub avg_tokens_per_expand: f64,
    pub sample_count: usize,
}

pub(crate) fn top_n_sorted(map: &HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut entries: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(n);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_n_sorted_returns_sorted_by_count_descending() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), 10);
        map.insert("b".to_string(), 30);
        map.insert("c".to_string(), 20);
        let result = top_n_sorted(&map, 3);
        assert_eq!(result[0], ("b".to_string(), 30));
        assert_eq!(result[1], ("c".to_string(), 20));
        assert_eq!(result[2], ("a".to_string(), 10));
    }

    #[test]
    fn top_n_sorted_truncates_to_n() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), 10);
        map.insert("b".to_string(), 30);
        map.insert("c".to_string(), 20);
        let result = top_n_sorted(&map, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].1, 30);
        assert_eq!(result[1].1, 20);
    }

    #[test]
    fn top_n_sorted_handles_empty_map() {
        let map: HashMap<String, u64> = HashMap::new();
        let result = top_n_sorted(&map, 5);
        assert!(result.is_empty());
    }

    #[test]
    fn metrics_response_serializes_to_json() {
        let resp = MetricsResponse {
            performance: PerformanceMetrics {
                queries: 100,
                query_cache_hit_rate: 0.75,
                query_cache_hits: 75,
                query_cache_misses: 25,
                expands: 50,
                index_cache_hits: 40,
                index_cache_misses: 10,
                reindexes: 3,
                avg_query_ms: 15,
                avg_expand_ms: 5,
            },
            analytics: AnalyticsMetrics {
                top_symbols: vec![NamedCount {
                    name: "Config".to_string(),
                    count: 42,
                }],
                top_patterns: vec![],
                top_expanded_files: vec![],
                requests_by_repo: HashMap::new(),
                feedback_by_repo: HashMap::new(),
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["performance"]["queries"], 100);
        assert_eq!(json["performance"]["query_cache_hit_rate"], 0.75);
        assert_eq!(json["analytics"]["top_symbols"][0]["name"], "Config");
    }
}

pub async fn metrics(State(state): State<SharedState>) -> Json<MetricsResponse> {
    let queries = state.metrics.query_count.load(Ordering::Relaxed);
    let query_cache_hits = state.metrics.query_cache_hits.load(Ordering::Relaxed);
    let query_cache_misses = state.metrics.query_cache_misses.load(Ordering::Relaxed);
    let expands = state.metrics.expand_count.load(Ordering::Relaxed);
    let index_cache_hits = state.metrics.index_cache_hits.load(Ordering::Relaxed);
    let index_cache_misses = state.metrics.index_cache_misses.load(Ordering::Relaxed);
    let reindexes = state.metrics.reindex_count.load(Ordering::Relaxed);
    let total_query_ms = state.metrics.total_query_ms.load(Ordering::Relaxed);
    let total_expand_ms = state.metrics.total_expand_ms.load(Ordering::Relaxed);

    let query_cache_total = query_cache_hits + query_cache_misses;
    let query_cache_hit_rate = if query_cache_total > 0 {
        query_cache_hits as f64 / query_cache_total as f64
    } else {
        0.0
    };

    let avg_query_ms = if queries > 0 {
        total_query_ms / queries
    } else {
        0
    };
    let avg_expand_ms = if expands > 0 {
        total_expand_ms / expands
    } else {
        0
    };

    let feedback_by_repo = {
        // Clone shard metadata under the lock, then drop it before async work.
        let shard_info: Vec<(String, String)> = {
            let shards = state.shards.read().await;
            shards
                .values()
                .map(|s| (s.repo_id.clone(), s.repo_root.clone()))
                .collect()
        };

        let mut out = HashMap::new();
        for (repo_id, repo_root) in &shard_info {
            if let Some(store) = state.feedback_store_for_repo(repo_id, repo_root).await {
                let Ok(store_guard) = store.lock() else {
                    continue;
                };
                if let Ok(m) = store_guard.compute_metrics(7.0) {
                    out.insert(
                        repo_id.clone(),
                        FeedbackSummary {
                            glob_hit_rate_at_k: m.glob_hit_rate_at_k,
                            handle_expand_accept_rate: m.handle_expand_accept_rate,
                            avg_tokens_per_expand: m.avg_tokens_per_expand,
                            sample_count: m.sample_count,
                        },
                    );
                }
            }
        }
        out
    };

    let analytics = if let Ok(a) = state.metrics.analytics.lock() {
        AnalyticsMetrics {
            top_symbols: top_n_sorted(&a.top_symbols, TOP_N)
                .into_iter()
                .map(|(name, count)| NamedCount { name, count })
                .collect(),
            top_patterns: top_n_sorted(&a.top_patterns, TOP_N)
                .into_iter()
                .map(|(pattern, count)| PatternCount { pattern, count })
                .collect(),
            top_expanded_files: top_n_sorted(&a.top_expanded_files, TOP_N)
                .into_iter()
                .map(|(path, count)| PathCount { path, count })
                .collect(),
            requests_by_repo: a.requests_by_repo.clone(),
            feedback_by_repo: feedback_by_repo.clone(),
        }
    } else {
        AnalyticsMetrics {
            top_symbols: Vec::new(),
            top_patterns: Vec::new(),
            top_expanded_files: Vec::new(),
            requests_by_repo: HashMap::new(),
            feedback_by_repo,
        }
    };

    Json(MetricsResponse {
        performance: PerformanceMetrics {
            queries,
            query_cache_hit_rate,
            query_cache_hits,
            query_cache_misses,
            expands,
            index_cache_hits,
            index_cache_misses,
            reindexes,
            avg_query_ms,
            avg_expand_ms,
        },
        analytics,
    })
}
