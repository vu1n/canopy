//! Iterative evidence planning loop.

use super::symbol_extraction::extract_symbol_candidates_from_handles;
use canopy_core::{build_evidence_pack, EvidenceConfidence, Handle, QueryParams, QueryResult};
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;

pub const EVIDENCE_PLAN_MAX_STEPS: usize = 3;
pub const EVIDENCE_PLAN_SYMBOLS_PER_STEP: usize = 1;
pub const EVIDENCE_PLAN_MIN_NEW_HANDLES: usize = 2;

/// Build a symbol-lookup follow-up query from a base query and symbol name.
pub fn symbol_followup_params(base: &QueryParams, symbol: String) -> QueryParams {
    use canopy_core::QueryKind;
    let mut params = QueryParams::symbol(symbol);
    params.kind = QueryKind::Definition;
    params.limit = Some(base.limit.unwrap_or(16).min(12));
    params.glob = base.glob.clone();
    params
}

/// Result of the iterative evidence planning loop.
pub struct EvidencePlanResult {
    pub result: QueryResult,
    pub query_text: String,
    pub seed_params: QueryParams,
    pub planning_enabled: bool,
    pub plan_steps: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
}

/// Run the iterative evidence planning loop.
///
/// Executes one or more queries, deduplicates handles, and optionally generates
/// symbol-based follow-up queries. The `execute_query` callback is called for
/// each query step and should return `(QueryResult, was_cache_hit)`.
pub async fn run_evidence_plan<E: Send>(
    seed_params: QueryParams,
    plan_override: Option<bool>,
    max_handles: usize,
    max_per_file: usize,
    mut execute_query: impl FnMut(QueryParams) -> Pin<Box<dyn Future<Output = Result<(QueryResult, bool), E>> + Send>>
        + Send,
) -> Result<EvidencePlanResult, E> {
    let mut planning_enabled = matches!(plan_override, Some(true));
    let mut auto_plan_decided = plan_override.is_some();
    let query_text = seed_params.to_text();

    let mut pending: VecDeque<QueryParams> = VecDeque::from([seed_params.clone()]);
    let mut seen_param_keys: HashSet<String> = HashSet::new();
    let mut seen_handle_ids: HashSet<String> = HashSet::new();
    let mut aggregate_handles: Vec<Handle> = Vec::new();
    let mut aggregate_tokens = 0usize;
    let mut aggregate_truncated = false;
    let mut total_matches = 0usize;
    let mut expanded_ids: Vec<String> = Vec::new();
    let mut expanded_tokens = 0usize;
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;
    let mut plan_steps = 0usize;

    while let Some(current_params) = pending.pop_front() {
        let max_steps = if planning_enabled {
            EVIDENCE_PLAN_MAX_STEPS
        } else {
            2
        };
        if plan_steps >= max_steps {
            break;
        }
        let key = serde_json::to_string(&current_params).unwrap_or_default();
        if !seen_param_keys.insert(key) {
            continue;
        }
        plan_steps += 1;

        let (result, was_hit) = execute_query(current_params.clone()).await?;
        if was_hit {
            cache_hits += 1;
        } else {
            cache_misses += 1;
        }

        total_matches += result.total_matches;
        aggregate_truncated |= result.truncated;

        let mut new_handle_count = 0usize;
        for handle in result.handles {
            if seen_handle_ids.insert(handle.id.to_string()) {
                aggregate_tokens += handle.token_count;
                if handle.content.is_some() {
                    expanded_ids.push(handle.id.to_string());
                    expanded_tokens += handle.token_count;
                }
                aggregate_handles.push(handle);
                new_handle_count += 1;
            }
        }

        let provisional = QueryResult {
            handles: aggregate_handles.clone(),
            ref_handles: None,
            total_tokens: aggregate_tokens,
            truncated: aggregate_truncated,
            total_matches,
            auto_expanded: false,
            expand_note: None,
            expanded_count: expanded_ids.len(),
            expanded_tokens,
            expanded_handle_ids: expanded_ids.clone(),
        };
        let provisional_pack =
            build_evidence_pack(&provisional, &query_text, max_handles, max_per_file);

        if !auto_plan_decided {
            planning_enabled = provisional_pack.guidance.confidence_band == EvidenceConfidence::Low
                && !provisional_pack.guidance.stop_querying;
            auto_plan_decided = true;
        }

        if let Some(fallback) = current_params.pattern_fallback() {
            let fallback_key = serde_json::to_string(&fallback).unwrap_or_default();
            let allow_fallback = if planning_enabled {
                true
            } else {
                plan_steps == 1 && aggregate_handles.is_empty()
            };
            if allow_fallback && !seen_param_keys.contains(&fallback_key) {
                pending.push_back(fallback);
            }
        }

        if !planning_enabled {
            continue;
        }

        if new_handle_count < EVIDENCE_PLAN_MIN_NEW_HANDLES {
            continue;
        }
        if provisional_pack.guidance.stop_querying
            && provisional_pack.selected_count >= max_handles.min(4)
        {
            break;
        }

        let symbol_candidates = extract_symbol_candidates_from_handles(
            &provisional.handles,
            &query_text,
            EVIDENCE_PLAN_SYMBOLS_PER_STEP,
        );
        for symbol in symbol_candidates {
            let followup = symbol_followup_params(&seed_params, symbol);
            let followup_key = serde_json::to_string(&followup).unwrap_or_default();
            if !seen_param_keys.contains(&followup_key) {
                pending.push_back(followup);
            }
        }
    }

    let auto_expanded =
        !aggregate_handles.is_empty() && expanded_ids.len() == aggregate_handles.len();
    let result = QueryResult {
        handles: aggregate_handles,
        ref_handles: None,
        total_tokens: aggregate_tokens,
        truncated: aggregate_truncated,
        total_matches,
        auto_expanded,
        expand_note: None,
        expanded_count: expanded_ids.len(),
        expanded_tokens,
        expanded_handle_ids: expanded_ids,
    };

    Ok(EvidencePlanResult {
        result,
        query_text,
        seed_params,
        planning_enabled,
        plan_steps,
        cache_hits,
        cache_misses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use canopy_core::MatchMode;

    #[test]
    fn symbol_followup_params_builds_definition_query() {
        let base = QueryParams::pattern("auth".to_string());
        let followup = symbol_followup_params(&base, "AuthService".to_string());
        assert_eq!(followup.symbol, Some("AuthService".to_string()));
        assert_eq!(followup.kind, canopy_core::QueryKind::Definition);
        assert!(followup.limit.unwrap() <= 12);
    }

    #[test]
    fn query_params_to_text_pattern() {
        let params = QueryParams::pattern("auth".to_string());
        assert_eq!(params.to_text(), "auth");
    }

    #[test]
    fn query_params_to_text_symbol() {
        let params = QueryParams::symbol("Config".to_string());
        assert_eq!(params.to_text(), "Config");
    }

    #[test]
    fn query_params_to_text_empty() {
        let params = QueryParams::default();
        assert_eq!(params.to_text(), "");
    }

    #[test]
    fn pattern_fallback_single_term_returns_none() {
        let params = QueryParams::pattern("auth".to_string());
        assert!(params.pattern_fallback().is_none());
    }

    #[test]
    fn pattern_fallback_multi_term_splits() {
        let params = QueryParams::pattern("auth handler login".to_string());
        let fallback = params.pattern_fallback().unwrap();
        assert!(fallback.pattern.is_none());
        assert_eq!(
            fallback.patterns,
            Some(vec![
                "auth".to_string(),
                "handler".to_string(),
                "login".to_string()
            ])
        );
        assert_eq!(fallback.match_mode, MatchMode::Any);
    }

    #[test]
    fn pattern_fallback_with_symbol_returns_none() {
        let params = QueryParams::symbol("Config".to_string());
        assert!(params.pattern_fallback().is_none());
    }
}
