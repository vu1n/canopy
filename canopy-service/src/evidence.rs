//! Evidence pack planning helpers — symbol extraction, query fallbacks, suggestion reordering.

use canopy_core::{
    build_evidence_pack, split_terms, EvidenceConfidence, EvidencePack, Handle, QueryKind,
    QueryParams, QueryResult,
};
use std::collections::{HashMap, HashSet, VecDeque};

pub const EVIDENCE_PLAN_MAX_STEPS: usize = 3;
pub const EVIDENCE_PLAN_SYMBOLS_PER_STEP: usize = 1;
pub const EVIDENCE_PLAN_MIN_NEW_HANDLES: usize = 2;
pub const SERVICE_DEFAULT_QUERY_LIMIT: usize = 16;
pub const SERVICE_MAX_QUERY_LIMIT: usize = 50;

/// Extract identifier-like symbol candidates from handle previews.
///
/// Scores tokens by identifier heuristics (camelCase, snake_case) and returns
/// the top `limit` candidates, excluding query terms and stop words.
pub fn extract_symbol_candidates_from_handles(
    handles: &[Handle],
    query_text: &str,
    limit: usize,
) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "the",
        "and",
        "for",
        "with",
        "from",
        "this",
        "that",
        "auth",
        "authentication",
        "token",
        "user",
        "users",
        "request",
        "response",
        "middleware",
        "handler",
        "function",
        "class",
        "method",
        "const",
        "let",
        "return",
        "true",
        "false",
        "null",
        "undefined",
    ];

    let query_terms: HashSet<String> = split_terms(query_text).into_iter().collect();
    let stop_words: HashSet<String> = STOP_WORDS.iter().map(|w| w.to_string()).collect();
    let mut scores: HashMap<String, usize> = HashMap::new();

    for handle in handles.iter().take(8) {
        for token in handle
            .preview
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 4)
        {
            if token
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            {
                continue;
            }
            let lower = token.to_lowercase();
            if query_terms.contains(&lower) || stop_words.contains(&lower) {
                continue;
            }

            // Prefer identifier-like tokens over plain words.
            let mut weight = 1usize;
            if token.chars().any(|c| c.is_uppercase()) {
                weight += 2;
            }
            if token.contains('_') {
                weight += 1;
            }
            *scores.entry(token.to_string()).or_insert(0) += weight;
        }
    }

    let mut ranked: Vec<(String, usize)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.len().cmp(&a.0.len())));
    ranked.into_iter().take(limit).map(|(sym, _)| sym).collect()
}

/// Build a symbol-lookup follow-up query from a base query and symbol name.
pub fn symbol_followup_params(base: &QueryParams, symbol: String) -> QueryParams {
    let mut params = QueryParams::symbol(symbol);
    params.kind = QueryKind::Definition;
    params.limit = Some(base.limit.unwrap_or(16).min(12));
    params.glob = base.glob.clone();
    params
}

/// Clamp query limits and optionally force preview-only mode.
pub fn normalize_query_params(mut params: QueryParams, force_preview_only: bool) -> QueryParams {
    let limit = params
        .limit
        .unwrap_or(SERVICE_DEFAULT_QUERY_LIMIT)
        .clamp(1, SERVICE_MAX_QUERY_LIMIT);
    params.limit = Some(limit);
    if force_preview_only {
        params.expand_budget = Some(0);
    }
    params
}

/// Reorder expand suggestions so recently-expanded handles sort last.
pub fn reorder_expand_suggestions(pack: &mut EvidencePack, recent_expanded: &HashSet<String>) {
    pack.reorder_expand_suggestions(|id| recent_expanded.contains(id));
}

/// Concatenate all query parameter text fields into a single string for feedback.
///
/// Delegates to `QueryParams::to_text()` in canopy-core.
pub(crate) fn query_params_text(params: &QueryParams) -> String {
    params.to_text()
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
pub fn run_evidence_plan<F, E>(
    seed_params: QueryParams,
    plan_override: Option<bool>,
    max_handles: usize,
    max_per_file: usize,
    mut execute_query: F,
) -> Result<EvidencePlanResult, E>
where
    F: FnMut(&QueryParams) -> Result<(QueryResult, bool), E>,
{
    let mut planning_enabled = matches!(plan_override, Some(true));
    let mut auto_plan_decided = plan_override.is_some();
    let query_text = query_params_text(&seed_params);

    let mut pending: VecDeque<QueryParams> = VecDeque::from([seed_params.clone()]);
    let mut seen_param_keys: HashSet<String> = HashSet::new();
    let mut seen_handle_ids: HashSet<String> = HashSet::new();
    let mut aggregate_handles: Vec<Handle> = Vec::new();
    let mut aggregate_tokens = 0usize;
    let mut aggregate_truncated = false;
    let mut total_matches = 0usize;
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

        let (result, was_hit) = execute_query(&current_params)?;
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
                aggregate_handles.push(handle);
                new_handle_count += 1;
            }
        }

        let expanded_handle_ids: Vec<String> = aggregate_handles
            .iter()
            .filter(|h| h.content.is_some())
            .map(|h| h.id.to_string())
            .collect();
        let provisional = QueryResult {
            handles: aggregate_handles.clone(),
            ref_handles: None,
            total_tokens: aggregate_tokens,
            truncated: aggregate_truncated,
            total_matches,
            auto_expanded: false,
            expand_note: None,
            expanded_count: expanded_handle_ids.len(),
            expanded_tokens: aggregate_handles
                .iter()
                .filter(|h| h.content.is_some())
                .map(|h| h.token_count)
                .sum(),
            expanded_handle_ids,
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

    let expanded_handle_ids: Vec<String> = aggregate_handles
        .iter()
        .filter(|h| h.content.is_some())
        .map(|h| h.id.to_string())
        .collect();
    let expanded_count = expanded_handle_ids.len();
    let expanded_tokens: usize = aggregate_handles
        .iter()
        .filter(|h| h.content.is_some())
        .map(|h| h.token_count)
        .sum();
    let auto_expanded = !aggregate_handles.is_empty() && expanded_count == aggregate_handles.len();
    let result = QueryResult {
        handles: aggregate_handles,
        ref_handles: None,
        total_tokens: aggregate_tokens,
        truncated: aggregate_truncated,
        total_matches,
        auto_expanded,
        expand_note: None,
        expanded_count,
        expanded_tokens,
        expanded_handle_ids,
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
    use canopy_core::document::NodeType;
    use canopy_core::handle::HandleSource;
    use canopy_core::MatchMode;

    fn make_handle(id: &str, preview: &str) -> Handle {
        Handle {
            id: id.parse().unwrap(),
            file_path: "test.rs".to_string(),
            node_type: NodeType::Function,
            span: 0..10,
            line_range: (1, 1),
            token_count: 5,
            preview: preview.to_string(),
            content: None,
            source: HandleSource::Local,
            commit_sha: None,
            generation: None,
        }
    }

    #[test]
    fn test_split_terms_basic() {
        let terms = split_terms("auth login handler");
        assert_eq!(terms, vec!["auth", "login", "handler"]);
    }

    #[test]
    fn test_split_terms_deduplicates() {
        let terms = split_terms("auth auth login");
        assert_eq!(terms, vec!["auth", "login"]);
    }

    #[test]
    fn test_split_terms_special_chars() {
        let terms = split_terms("user_auth.handler");
        assert_eq!(terms, vec!["user_auth", "handler"]);
    }

    #[test]
    fn test_split_terms_empty() {
        let terms = split_terms("");
        assert!(terms.is_empty());
    }

    #[test]
    fn test_pattern_fallback_single_term_returns_none() {
        let params = QueryParams::pattern("auth".to_string());
        assert!(params.pattern_fallback().is_none());
    }

    #[test]
    fn test_pattern_fallback_multi_term_splits() {
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
    fn test_pattern_fallback_with_symbol_returns_none() {
        let params = QueryParams::symbol("Config".to_string());
        assert!(params.pattern_fallback().is_none());
    }

    #[test]
    fn test_normalize_query_params_defaults() {
        let params = QueryParams::pattern("test".to_string());
        let normalized = normalize_query_params(params, false);
        assert_eq!(normalized.limit, Some(SERVICE_DEFAULT_QUERY_LIMIT));
        assert!(normalized.expand_budget.is_none());
    }

    #[test]
    fn test_normalize_query_params_force_preview_only() {
        let params = QueryParams::pattern("test".to_string());
        let normalized = normalize_query_params(params, true);
        assert_eq!(normalized.expand_budget, Some(0));
    }

    #[test]
    fn test_normalize_query_params_clamps_limit() {
        let mut params = QueryParams::pattern("test".to_string());
        params.limit = Some(999);
        let normalized = normalize_query_params(params, false);
        assert_eq!(normalized.limit, Some(SERVICE_MAX_QUERY_LIMIT));
    }

    #[test]
    fn test_symbol_followup_params() {
        let base = QueryParams::pattern("auth".to_string());
        let followup = symbol_followup_params(&base, "AuthService".to_string());
        assert_eq!(followup.symbol, Some("AuthService".to_string()));
        assert_eq!(followup.kind, QueryKind::Definition);
        assert!(followup.limit.unwrap() <= 12);
    }

    #[test]
    fn test_query_params_text_pattern() {
        let params = QueryParams::pattern("auth".to_string());
        assert_eq!(query_params_text(&params), "auth");
    }

    #[test]
    fn test_query_params_text_symbol() {
        let params = QueryParams::symbol("Config".to_string());
        assert_eq!(query_params_text(&params), "Config");
    }

    #[test]
    fn test_query_params_text_empty() {
        let params = QueryParams::default();
        assert_eq!(query_params_text(&params), "");
    }

    #[test]
    fn test_extract_symbol_candidates_skips_short_tokens() {
        let handle = make_handle("a1b2c3", "fn ab() {}"); // "ab" and "fn" are < 4 chars
        let results = extract_symbol_candidates_from_handles(&[handle], "query", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_extract_symbol_candidates_prefers_identifiers() {
        let handle = make_handle("d4e5f6", "fn handleAuth() { verify_token(ctx); }");
        let results = extract_symbol_candidates_from_handles(&[handle], "test", 5);
        // handleAuth has uppercase (weight +2) and verify_token has underscore (weight +1)
        assert!(!results.is_empty());
        // CamelCase should score higher than plain words
        assert!(results.contains(&"handleAuth".to_string()));
    }

    fn make_test_pack(suggestions: Vec<&str>) -> EvidencePack {
        EvidencePack {
            query_text: String::new(),
            total_matches: 0,
            truncated: false,
            selected_count: 0,
            selected_tokens: 0,
            handles: vec![],
            files: vec![],
            expand_suggestion: suggestions.into_iter().map(String::from).collect(),
            guidance: canopy_core::EvidenceGuidance {
                confidence: 0.0,
                confidence_band: canopy_core::EvidenceConfidence::Low,
                stop_querying: false,
                recommended_action: canopy_core::EvidenceAction::RefineQuery,
                suggested_expand_count: 0,
                max_additional_queries: 0,
                rationale: String::new(),
                next_step: String::new(),
            },
        }
    }

    #[test]
    fn test_reorder_expand_suggestions_no_recent() {
        let mut pack = make_test_pack(vec!["a", "b"]);
        let recent = HashSet::new();
        reorder_expand_suggestions(&mut pack, &recent);
        assert_eq!(pack.expand_suggestion, vec!["a", "b"]);
    }

    #[test]
    fn test_reorder_expand_suggestions_moves_recent_to_end() {
        let mut pack = make_test_pack(vec!["a", "b", "c"]);
        let mut recent = HashSet::new();
        recent.insert("a".to_string());
        reorder_expand_suggestions(&mut pack, &recent);
        // "a" was recently expanded, so "b" and "c" should come first
        assert_eq!(pack.expand_suggestion[0], "b");
        assert_eq!(pack.expand_suggestion[1], "c");
    }
}
