//! Evidence pack planning helpers — symbol extraction, query fallbacks, suggestion reordering.

mod planning;
pub mod symbol_extraction;

pub use planning::run_evidence_plan;

use canopy_core::QueryParams;

pub const SERVICE_DEFAULT_QUERY_LIMIT: usize = 16;
pub const SERVICE_MAX_QUERY_LIMIT: usize = 50;

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

#[cfg(test)]
mod tests {
    use super::*;
    use canopy_core::{split_terms, EvidencePack};
    use std::collections::HashSet;

    #[test]
    fn normalize_defaults() {
        let params = QueryParams::pattern("test".to_string());
        let normalized = normalize_query_params(params, false);
        assert_eq!(normalized.limit, Some(SERVICE_DEFAULT_QUERY_LIMIT));
        assert!(normalized.expand_budget.is_none());
    }

    #[test]
    fn normalize_force_preview_only() {
        let params = QueryParams::pattern("test".to_string());
        let normalized = normalize_query_params(params, true);
        assert_eq!(normalized.expand_budget, Some(0));
    }

    #[test]
    fn normalize_clamps_limit() {
        let mut params = QueryParams::pattern("test".to_string());
        params.limit = Some(999);
        let normalized = normalize_query_params(params, false);
        assert_eq!(normalized.limit, Some(SERVICE_MAX_QUERY_LIMIT));
    }

    #[test]
    fn split_terms_basic() {
        let terms = split_terms("auth login handler");
        assert_eq!(terms, vec!["auth", "login", "handler"]);
    }

    #[test]
    fn split_terms_deduplicates() {
        let terms = split_terms("auth auth login");
        assert_eq!(terms, vec!["auth", "login"]);
    }

    #[test]
    fn split_terms_special_chars() {
        let terms = split_terms("user_auth.handler");
        assert_eq!(terms, vec!["user_auth", "handler"]);
    }

    #[test]
    fn split_terms_empty() {
        let terms = split_terms("");
        assert!(terms.is_empty());
    }

    #[test]
    fn reorder_expand_suggestions_no_recent() {
        let mut pack = make_test_pack(vec!["a", "b"]);
        let recent: HashSet<String> = HashSet::new();
        pack.reorder_expand_suggestions(|id| recent.contains(id));
        assert_eq!(pack.expand_suggestion, vec!["a", "b"]);
    }

    #[test]
    fn reorder_expand_suggestions_moves_recent_to_end() {
        let mut pack = make_test_pack(vec!["a", "b", "c"]);
        let mut recent = HashSet::new();
        recent.insert("a".to_string());
        pack.reorder_expand_suggestions(|id| recent.contains(id));
        assert_eq!(pack.expand_suggestion[0], "b");
        assert_eq!(pack.expand_suggestion[1], "c");
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
}
