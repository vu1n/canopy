//! Query DSL, execution engine, and evidence pack builder.
//!
//! Submodules:
//! - `dsl` — Query AST and S-expression parser
//! - `params` — QueryParams builder API and match/kind types
//! - `executor` — Query execution against a RepoIndex
//! - `evidence` — Evidence pack types and ranked evidence builder

pub mod dsl;
pub mod evidence;
pub mod executor;
pub mod params;

pub use dsl::{parse_query, Query};
pub use evidence::{
    build_evidence_pack, EvidenceAction, EvidenceConfidence, EvidenceFileSummary, EvidenceGuidance,
    EvidenceHandle, EvidencePack,
};
pub use executor::{execute_query, execute_query_with_options, DEFAULT_EXPAND_BUDGET};
pub use params::{split_terms, MatchMode, QueryKind, QueryParams};

use crate::document::NodeType;
use crate::handle::{Handle, RefHandle};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Query result with handles
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryResult {
    pub handles: Vec<Handle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_handles: Option<Vec<RefHandle>>,
    pub total_tokens: usize,
    pub truncated: bool,
    pub total_matches: usize,
    /// True if handles have content populated (auto-expanded)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_expanded: bool,
    /// Message when expand_budget is exceeded
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expand_note: Option<String>,
    /// Number of handles with `content` populated
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expanded_count: usize,
    /// Total token count of expanded handles
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expanded_tokens: usize,
    /// Handle IDs that already include `content` in this response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanded_handle_ids: Vec<String>,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

/// Query options for executing queries
#[derive(Debug, Default)]
pub struct QueryOptions {
    /// Override default result limit
    pub limit: Option<usize>,
    /// Auto-expand results if total tokens fit within budget (default: disabled / 0)
    pub expand_budget: Option<usize>,
    /// Learned node type priors for scoring partial auto-expansion
    pub node_type_priors: Option<HashMap<NodeType, f64>>,
}

impl QueryOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn with_expand_budget(mut self, budget: usize) -> Self {
        self.expand_budget = Some(budget);
        self
    }

    pub fn with_node_type_priors(mut self, priors: HashMap<NodeType, f64>) -> Self {
        self.node_type_priors = Some(priors);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CanopyError;
    use crate::RepoIndex;
    use std::fs;

    fn temp_repo() -> std::path::PathBuf {
        let root = crate::temp_test_dir("query-test");
        fs::create_dir_all(root.join("src")).unwrap();
        root
    }

    #[test]
    fn test_parse_section() {
        let query = parse_query("(section \"auth\")").unwrap();
        assert!(matches!(query, Query::Section(s) if s == "auth"));
    }

    #[test]
    fn test_parse_grep() {
        let query = parse_query("(grep \"TODO\")").unwrap();
        assert!(matches!(query, Query::Grep(s) if s == "TODO"));
    }

    #[test]
    fn test_parse_file() {
        let query = parse_query("(file \"README.md\")").unwrap();
        assert!(matches!(query, Query::File(s) if s == "README.md"));
    }

    #[test]
    fn test_parse_code() {
        let query = parse_query("(code \"authenticate\")").unwrap();
        assert!(matches!(query, Query::Code(s) if s == "authenticate"));
    }

    #[test]
    fn test_parse_in_file() {
        let query = parse_query("(in-file \"src/**/*.rs\" (grep \"error\"))").unwrap();
        assert!(matches!(query, Query::InFile(glob, _) if glob == "src/**/*.rs"));
    }

    #[test]
    fn test_parse_union() {
        let query = parse_query("(union (grep \"TODO\") (grep \"FIXME\"))").unwrap();
        assert!(matches!(query, Query::Union(qs) if qs.len() == 2));
    }

    #[test]
    fn test_parse_intersect() {
        let query = parse_query("(intersect (grep \"test\") (code \"validate\"))").unwrap();
        assert!(matches!(query, Query::Intersect(qs) if qs.len() == 2));
    }

    #[test]
    fn test_parse_limit() {
        let query = parse_query("(limit 5 (grep \"TODO\"))").unwrap();
        assert!(matches!(query, Query::Limit(5, _)));
    }

    #[test]
    fn test_parse_nested() {
        let query = parse_query(
            "(limit 10 (in-file \"src/*.rs\" (union (grep \"error\") (grep \"panic\"))))",
        )
        .unwrap();
        assert!(matches!(query, Query::Limit(10, _)));
    }

    #[test]
    fn test_parse_escaped_string() {
        let query = parse_query("(grep \"line1\\nline2\")").unwrap();
        assert!(matches!(query, Query::Grep(s) if s == "line1\nline2"));
    }

    #[test]
    fn test_parse_error_missing_paren() {
        let result = parse_query("section \"auth\"");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_error_unknown_op() {
        let result = parse_query("(unknown \"arg\")");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_error_unterminated_string() {
        let result = parse_query("(grep \"unterminated)");
        assert!(result.is_err());
    }

    // ========== QueryParams API Tests ==========

    #[test]
    fn test_query_params_single_pattern() {
        let params = QueryParams::pattern("error");
        let query = params.to_query().unwrap();
        assert!(matches!(query, Query::Grep(s) if s == "error"));
    }

    #[test]
    fn test_query_params_multi_pattern_default_match_mode() {
        let params = QueryParams::patterns(vec!["TODO".to_string(), "FIXME".to_string()]);
        let query = params.to_query().unwrap();
        match query {
            Query::Union(queries) => {
                assert_eq!(queries.len(), 2);
                assert!(matches!(&queries[0], Query::Grep(s) if s == "TODO"));
                assert!(matches!(&queries[1], Query::Grep(s) if s == "FIXME"));
            }
            _ => panic!("Expected Query::Union"),
        }
    }

    #[test]
    fn test_query_params_multi_pattern_match_mode_all() {
        let params = QueryParams::patterns(vec!["auth".to_string(), "validate".to_string()])
            .with_match_mode(MatchMode::All);
        let query = params.to_query().unwrap();
        match query {
            Query::Intersect(queries) => {
                assert_eq!(queries.len(), 2);
                assert!(matches!(&queries[0], Query::Grep(s) if s == "auth"));
                assert!(matches!(&queries[1], Query::Grep(s) if s == "validate"));
            }
            _ => panic!("Expected Query::Intersect"),
        }
    }

    #[test]
    fn test_query_params_symbol_search() {
        let params = QueryParams::symbol("authenticate");
        let query = params.to_query().unwrap();
        assert!(matches!(query, Query::Code(s) if s == "authenticate"));
    }

    #[test]
    fn test_query_params_section_search() {
        let params = QueryParams::section("auth");
        let query = params.to_query().unwrap();
        assert!(matches!(query, Query::Section(s) if s == "auth"));
    }

    #[test]
    fn test_query_params_with_glob() {
        let params = QueryParams::pattern("error").with_glob("src/*.rs");
        let query = params.to_query().unwrap();
        match query {
            Query::InFile(glob, inner) => {
                assert_eq!(glob, "src/*.rs");
                assert!(matches!(*inner, Query::Grep(s) if s == "error"));
            }
            _ => panic!("Expected Query::InFile"),
        }
    }

    #[test]
    fn test_query_params_with_limit() {
        let params = QueryParams::pattern("error").with_limit(10);
        let query = params.to_query().unwrap();
        match query {
            Query::Limit(n, inner) => {
                assert_eq!(n, 10);
                assert!(matches!(*inner, Query::Grep(s) if s == "error"));
            }
            _ => panic!("Expected Query::Limit"),
        }
    }

    #[test]
    fn test_query_params_with_expand_budget() {
        let params = QueryParams::pattern("error").with_expand_budget(5000);
        // expand_budget doesn't affect the Query, only QueryOptions
        let query = params.to_query().unwrap();
        assert!(matches!(query, Query::Grep(s) if s == "error"));
        // Verify expand_budget is in options
        let options = params.to_options();
        assert_eq!(options.expand_budget, Some(5000));
    }

    #[test]
    fn test_query_params_empty_returns_error() {
        let params = QueryParams::new();
        let result = params.to_query();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CanopyError::QueryParse { message, .. } if message.contains("Must specify"))
        );
    }

    #[test]
    fn test_query_params_empty_patterns_returns_error() {
        let params = QueryParams::patterns(vec![]);
        let result = params.to_query();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CanopyError::QueryParse { message, .. } if message.contains("Empty patterns"))
        );
    }

    #[test]
    fn test_evidence_pack_guidance_requests_refine_when_empty() {
        let result = QueryResult::default();

        let pack = build_evidence_pack(&result, "auth middleware", 8, 2);
        assert_eq!(
            pack.guidance.recommended_action,
            EvidenceAction::RefineQuery
        );
        assert!(!pack.guidance.stop_querying);
        assert_eq!(pack.guidance.suggested_expand_count, 0);
    }

    #[test]
    fn test_evidence_pack_guidance_stops_querying_when_signal_is_strong() {
        let handles = vec![
            Handle::new(
                "src/auth/session.rs".to_string(),
                crate::NodeType::Function,
                crate::Span { start: 0, end: 12 },
                (10, 22),
                80,
                "authenticate_session user token".to_string(),
            ),
            Handle::new(
                "src/auth/middleware.rs".to_string(),
                crate::NodeType::Function,
                crate::Span { start: 30, end: 52 },
                (40, 68),
                90,
                "auth middleware validate request".to_string(),
            ),
            Handle::new(
                "src/http/router.rs".to_string(),
                crate::NodeType::Method,
                crate::Span {
                    start: 70,
                    end: 104,
                },
                (120, 152),
                95,
                "register auth middleware chain".to_string(),
            ),
        ];
        let result = QueryResult {
            handles,
            total_tokens: 265,
            total_matches: 9,
            ..QueryResult::default()
        };

        let pack = build_evidence_pack(&result, "auth middleware", 8, 2);
        assert_eq!(
            pack.guidance.recommended_action,
            EvidenceAction::ExpandThenAnswer
        );
        assert!(pack.guidance.stop_querying);
        assert_eq!(pack.guidance.max_additional_queries, 0);
        assert!(pack.guidance.confidence >= 0.55);
    }

    #[test]
    fn test_partial_auto_expand_reports_counts() {
        let repo = temp_repo();
        fs::write(
            repo.join("src/lib.rs"),
            r#"
fn target_small() {
    // hardentest
    println!("hardentest");
}

fn target_large() {
    // hardentest
    let mut acc = 0;
    for i in 0..400 {
        acc += i;
        if i % 3 == 0 {
            println!("hardentest {}", i);
        }
    }
    println!("{}", acc);
}
"#,
        )
        .unwrap();

        RepoIndex::init(&repo).unwrap();
        let mut index = RepoIndex::open(&repo).unwrap();
        index.index("**/*.rs").unwrap();

        let baseline = index
            .query_params(QueryParams::pattern("hardentest"))
            .unwrap();
        assert!(
            baseline.handles.len() >= 2,
            "expected at least two handles, got {}",
            baseline.handles.len()
        );

        let min_tokens = baseline
            .handles
            .iter()
            .map(|h| h.token_count)
            .min()
            .unwrap();
        let budget = min_tokens.max(1);

        let partial = index
            .query_params(QueryParams::pattern("hardentest").with_expand_budget(budget))
            .unwrap();

        assert!(!partial.auto_expanded);
        assert!(partial.expanded_count >= 1);
        assert!(partial.expanded_count < partial.handles.len());
        assert!(partial.expanded_tokens <= budget);
        assert!(partial
            .expand_note
            .as_deref()
            .unwrap_or_default()
            .contains("Expanded"));
    }

    // ========== Execute query integration tests ==========

    fn indexed_repo() -> (std::path::PathBuf, RepoIndex) {
        static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = crate::temp_test_dir(&format!("exec-test-{n}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/auth.rs"),
            "fn authenticate(user: &str) -> bool { true }\nfn validate_token(t: &str) -> bool { false }\n",
        ).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("README.md"),
            "# Project\nThis project has authentication.\n",
        )
        .unwrap();
        RepoIndex::init(&root).unwrap();
        let mut index = RepoIndex::open(&root).unwrap();
        index.index("**/*").unwrap();
        (root, index)
    }

    #[test]
    fn execute_grep_returns_matching_handles() {
        let (_root, index) = indexed_repo();
        let query = parse_query("(grep \"authenticate\")").unwrap();
        let result = execute_query(&query, &index, None).unwrap();
        assert!(!result.handles.is_empty());
        assert!(result.handles.iter().any(|h| h.file_path.contains("auth")));
    }

    #[test]
    fn execute_in_file_scopes_to_glob() {
        let (_root, index) = indexed_repo();
        let query = parse_query("(in-file \"src/auth*\" (grep \"validate\"))").unwrap();
        let result = execute_query(&query, &index, None).unwrap();
        for h in &result.handles {
            assert!(
                h.file_path.starts_with("src/auth"),
                "handle {} from wrong file: {}",
                h.id,
                h.file_path
            );
        }
    }

    #[test]
    fn execute_union_merges_results() {
        let (_root, index) = indexed_repo();
        let query =
            parse_query("(union (grep \"authenticate\") (grep \"validate_token\"))").unwrap();
        let result = execute_query(&query, &index, None).unwrap();
        assert!(
            result.handles.len() >= 1,
            "union should return at least one handle"
        );
    }

    #[test]
    fn execute_intersect_narrows_results() {
        let (_root, index) = indexed_repo();
        // Both terms appear in auth.rs but not in main.rs
        let query =
            parse_query("(intersect (grep \"authenticate\") (grep \"validate_token\"))").unwrap();
        let result = execute_query(&query, &index, None).unwrap();
        for h in &result.handles {
            assert!(
                h.file_path.contains("auth"),
                "intersect result should be from auth file: {}",
                h.file_path
            );
        }
    }

    #[test]
    fn execute_limit_caps_results() {
        let (_root, index) = indexed_repo();
        let query = parse_query("(limit 1 (grep \"authenticate\"))").unwrap();
        let result = execute_query(&query, &index, None).unwrap();
        assert!(result.handles.len() <= 1, "limit 1 should cap results");
    }

    #[test]
    fn execute_query_with_expand_budget() {
        let (_root, index) = indexed_repo();
        let query = parse_query("(grep \"authenticate\")").unwrap();
        let result = execute_query_with_options(
            &query,
            &index,
            QueryOptions {
                limit: None,
                expand_budget: Some(100_000),
                node_type_priors: None,
            },
        )
        .unwrap();
        assert!(
            result.auto_expanded || result.expanded_count > 0,
            "large budget should trigger expansion"
        );
    }
}
