//! Query parameter types and builder API.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::dsl::Query;
use crate::error::CanopyError;

/// Split text into unique lowercase terms, splitting on non-alphanumeric/underscore.
pub fn split_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for term in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
    {
        if seen.insert(term.to_string()) {
            out.push(term.to_string());
        }
    }
    out
}

/// Match mode for multi-pattern queries
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Union: match any of the patterns (OR)
    #[default]
    Any,
    /// Intersect: match all of the patterns (AND)
    All,
}

impl MatchMode {
    /// Parse from a string value. "all" → All, anything else → Any.
    pub fn parse(s: &str) -> Self {
        match s {
            "all" => Self::All,
            _ => Self::Any,
        }
    }
}

/// Query kind for filtering results
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryKind {
    /// Match any (default behavior)
    #[default]
    Any,
    /// Only match definitions
    Definition,
    /// Only match references (calls, imports, type usages)
    Reference,
}

/// Simplified query parameters (params-only API)
///
/// This provides a cleaner interface than the s-expression DSL.
/// Internally converts to the Query AST.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryParams {
    /// Text pattern to search for (single pattern)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,

    /// Multiple text patterns to search for
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patterns: Option<Vec<String>>,

    /// Code symbol to search for (function, class, struct, method)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,

    /// Section heading to search for (markdown sections)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,

    /// Parent symbol to scope results (e.g., class name for methods)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,

    /// Query kind: definition, reference, or any (default)
    #[serde(default)]
    pub kind: QueryKind,

    /// File glob filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,

    /// Match mode for multi-pattern: any (OR) or all (AND)
    #[serde(default)]
    pub match_mode: MatchMode,

    /// Maximum number of results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// Auto-expand results if total tokens fit within budget
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expand_budget: Option<usize>,

    /// Raw s-expression DSL query (takes precedence over structured fields when set)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsl: Option<String>,
}

impl QueryParams {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a text pattern search
    pub fn pattern(pattern: impl Into<String>) -> Self {
        Self {
            pattern: Some(pattern.into()),
            ..Default::default()
        }
    }

    /// Create a multi-pattern search
    pub fn patterns(patterns: Vec<String>) -> Self {
        Self {
            patterns: Some(patterns),
            ..Default::default()
        }
    }

    /// Create a symbol search
    pub fn symbol(symbol: impl Into<String>) -> Self {
        Self {
            symbol: Some(symbol.into()),
            ..Default::default()
        }
    }

    /// Create a section search
    pub fn section(heading: impl Into<String>) -> Self {
        Self {
            section: Some(heading.into()),
            ..Default::default()
        }
    }

    /// Create a parent search (get all children)
    pub fn parent(parent: impl Into<String>) -> Self {
        Self {
            parent: Some(parent.into()),
            ..Default::default()
        }
    }

    /// Set the parent filter
    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Set the query kind (definition, reference, any)
    pub fn with_kind(mut self, kind: QueryKind) -> Self {
        self.kind = kind;
        self
    }

    /// Filter by file glob
    pub fn with_glob(mut self, glob: impl Into<String>) -> Self {
        self.glob = Some(glob.into());
        self
    }

    /// Set match mode for multi-pattern queries
    pub fn with_match_mode(mut self, mode: MatchMode) -> Self {
        self.match_mode = mode;
        self
    }

    /// Set result limit
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set expand budget for auto-expansion
    pub fn with_expand_budget(mut self, budget: usize) -> Self {
        self.expand_budget = Some(budget);
        self
    }

    /// Parse a kind string ("definition", "reference", "any") into a QueryKind.
    pub fn parse_kind(s: &str) -> QueryKind {
        match s {
            "definition" => QueryKind::Definition,
            "reference" => QueryKind::Reference,
            _ => QueryKind::Any,
        }
    }

    /// Returns true if at least one search target field is set.
    pub fn has_search_target(&self) -> bool {
        self.pattern.is_some()
            || self.patterns.is_some()
            || self.symbol.is_some()
            || self.section.is_some()
            || self.parent.is_some()
            || self.dsl.is_some()
    }

    /// Concatenate all query parameter text fields into a single string.
    ///
    /// Useful for feedback recording and keyword extraction.
    pub fn to_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(s) = &self.pattern {
            parts.push(s.clone());
        }
        if let Some(ss) = &self.patterns {
            parts.extend(ss.clone());
        }
        if let Some(s) = &self.symbol {
            parts.push(s.clone());
        }
        if let Some(s) = &self.section {
            parts.push(s.clone());
        }
        if let Some(s) = &self.parent {
            parts.push(s.clone());
        }
        if let Some(s) = &self.glob {
            parts.push(s.clone());
        }
        parts.join(" ")
    }

    /// Build a multi-term fallback from a single-pattern query.
    ///
    /// Returns `None` if the query already uses multi-field search or the
    /// pattern has only one term.
    pub fn pattern_fallback(&self) -> Option<QueryParams> {
        if self.patterns.is_some()
            || self.symbol.is_some()
            || self.section.is_some()
            || self.parent.is_some()
        {
            return None;
        }

        let pattern = self.pattern.as_ref()?;
        let terms = split_terms(pattern);
        if terms.len() <= 1 {
            return None;
        }

        let mut fallback = self.clone();
        fallback.pattern = None;
        fallback.patterns = Some(terms);
        fallback.match_mode = MatchMode::Any;
        Some(fallback)
    }

    /// Convert params to Query AST
    pub fn to_query(&self) -> crate::Result<Query> {
        // DSL takes precedence over structured fields
        if let Some(ref dsl) = self.dsl {
            return super::dsl::parse_query(dsl);
        }

        // Validate: pattern and patterns are mutually exclusive
        if self.pattern.is_some() && self.patterns.is_some() {
            return Err(CanopyError::QueryParse {
                position: 0,
                message: "Cannot specify both 'pattern' and 'patterns'; use one or the other"
                    .to_string(),
            });
        }

        // Validate: kind requires symbol
        if !matches!(self.kind, QueryKind::Any) && self.symbol.is_none() {
            return Err(CanopyError::QueryParse {
                position: 0,
                message: "kind parameter requires symbol to be specified".to_string(),
            });
        }

        // Build the base query based on kind
        let base_query = match &self.kind {
            QueryKind::Definition => {
                let symbol = self.symbol.as_ref().unwrap(); // validated above
                                                            // If parent is specified, use ChildrenNamed, otherwise Definition
                if let Some(parent) = &self.parent {
                    Query::ChildrenNamed(parent.clone(), symbol.clone())
                } else {
                    Query::Definition(symbol.clone())
                }
            }
            QueryKind::Reference => {
                let symbol = self.symbol.as_ref().unwrap(); // validated above
                Query::References(symbol.clone())
            }
            QueryKind::Any => {
                // Check for parent + symbol combination
                if let (Some(parent), Some(symbol)) = (&self.parent, &self.symbol) {
                    Query::ChildrenNamed(parent.clone(), symbol.clone())
                } else if let Some(parent) = &self.parent {
                    // Just parent - get all children
                    Query::Children(parent.clone())
                } else if let Some(symbol) = &self.symbol {
                    Query::Code(symbol.clone())
                } else if let Some(section) = &self.section {
                    Query::Section(section.clone())
                } else if let Some(pattern) = &self.pattern {
                    Query::Grep(pattern.clone())
                } else if let Some(patterns) = &self.patterns {
                    if patterns.is_empty() {
                        return Err(CanopyError::QueryParse {
                            position: 0,
                            message: "Empty patterns array".to_string(),
                        });
                    }
                    let queries: Vec<Query> =
                        patterns.iter().map(|p| Query::Grep(p.clone())).collect();
                    match self.match_mode {
                        MatchMode::Any => Query::Union(queries),
                        MatchMode::All => Query::Intersect(queries),
                    }
                } else {
                    return Err(CanopyError::QueryParse {
                        position: 0,
                        message: "Must specify pattern, patterns, symbol, section, or parent"
                            .to_string(),
                    });
                }
            }
        };

        // Apply glob filter if specified
        let query = if let Some(glob) = &self.glob {
            Query::InFile(glob.clone(), Box::new(base_query))
        } else {
            base_query
        };

        // Apply limit if specified
        let query = if let Some(limit) = self.limit {
            Query::Limit(limit, Box::new(query))
        } else {
            query
        };

        Ok(query)
    }

    /// Convert to QueryOptions for execution
    pub fn to_options(&self) -> super::QueryOptions {
        super::QueryOptions {
            limit: self.limit,
            expand_budget: self.expand_budget,
            node_type_priors: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_terms_deduplicates_and_lowercases() {
        let terms = split_terms("Hello World hello");
        assert_eq!(terms, vec!["hello", "world"]);
    }

    #[test]
    fn split_terms_splits_on_punctuation_keeps_underscore() {
        let terms = split_terms("foo-bar_baz.qux/abc");
        assert_eq!(terms, vec!["foo", "bar_baz", "qux", "abc"]);
    }

    #[test]
    fn split_terms_empty_input() {
        let terms = split_terms("");
        assert!(terms.is_empty());
        let terms = split_terms("   ");
        assert!(terms.is_empty());
    }

    #[test]
    fn to_query_rejects_both_pattern_and_patterns() {
        let params = QueryParams {
            pattern: Some("foo".to_string()),
            patterns: Some(vec!["bar".to_string()]),
            ..Default::default()
        };
        let err = params.to_query().unwrap_err();
        assert!(
            matches!(err, CanopyError::QueryParse { ref message, .. } if message.contains("both"))
        );
    }

    #[test]
    fn to_query_definition_kind_requires_symbol() {
        let params = QueryParams {
            pattern: Some("test".to_string()),
            kind: QueryKind::Definition,
            ..Default::default()
        };
        let err = params.to_query().unwrap_err();
        assert!(
            matches!(err, CanopyError::QueryParse { ref message, .. } if message.contains("kind parameter requires symbol"))
        );
    }

    #[test]
    fn to_query_definition_kind_with_parent_produces_children_named() {
        let params = QueryParams::symbol("do_work")
            .with_parent("MyClass")
            .with_kind(QueryKind::Definition);
        let q = params.to_query().unwrap();
        match q {
            Query::ChildrenNamed(parent, symbol) => {
                assert_eq!(parent, "MyClass");
                assert_eq!(symbol, "do_work");
            }
            _ => panic!("expected ChildrenNamed, got {:?}", q),
        }
    }

    #[test]
    fn to_query_reference_kind_produces_references() {
        let params = QueryParams::symbol("authenticate").with_kind(QueryKind::Reference);
        let q = params.to_query().unwrap();
        assert!(matches!(q, Query::References(s) if s == "authenticate"));
    }

    #[test]
    fn to_query_glob_and_limit_wrap_correctly() {
        let params = QueryParams::pattern("error")
            .with_glob("src/**/*.rs")
            .with_limit(5);
        let q = params.to_query().unwrap();
        // Outermost should be Limit, then InFile, then Grep
        match q {
            Query::Limit(5, inner) => match *inner {
                Query::InFile(ref glob, ref sub) => {
                    assert_eq!(glob, "src/**/*.rs");
                    assert!(matches!(sub.as_ref(), Query::Grep(s) if s == "error"));
                }
                _ => panic!("expected InFile"),
            },
            _ => panic!("expected Limit"),
        }
    }

    #[test]
    fn to_options_mirrors_params() {
        let params = QueryParams::pattern("x")
            .with_limit(42)
            .with_expand_budget(8000);
        let opts = params.to_options();
        assert_eq!(opts.limit, Some(42));
        assert_eq!(opts.expand_budget, Some(8000));
        assert!(opts.node_type_priors.is_none());
    }

    #[test]
    fn pattern_fallback_splits_multi_term_pattern() {
        let params = QueryParams::pattern("auth middleware handler");
        let fb = params.pattern_fallback().unwrap();
        assert!(fb.pattern.is_none());
        assert_eq!(
            fb.patterns,
            Some(vec![
                "auth".to_string(),
                "middleware".to_string(),
                "handler".to_string()
            ])
        );
        assert_eq!(fb.match_mode, MatchMode::Any);
    }

    #[test]
    fn pattern_fallback_returns_none_for_single_term() {
        let params = QueryParams::pattern("auth");
        assert!(params.pattern_fallback().is_none());
    }

    #[test]
    fn pattern_fallback_returns_none_when_symbol_set() {
        let params = QueryParams {
            pattern: Some("auth middleware".to_string()),
            symbol: Some("validate".to_string()),
            ..Default::default()
        };
        assert!(params.pattern_fallback().is_none());
    }

    #[test]
    fn has_search_target_detects_all_fields() {
        assert!(!QueryParams::new().has_search_target());
        assert!(QueryParams::pattern("x").has_search_target());
        assert!(QueryParams::symbol("x").has_search_target());
        assert!(QueryParams::section("x").has_search_target());
        assert!(QueryParams::parent("x").has_search_target());
        assert!(QueryParams::patterns(vec!["x".to_string()]).has_search_target());

        let dsl = QueryParams {
            dsl: Some("(grep \"x\")".to_string()),
            ..Default::default()
        };
        assert!(dsl.has_search_target());
    }

    #[test]
    fn to_text_concatenates_all_fields() {
        let params = QueryParams {
            pattern: Some("alpha".to_string()),
            patterns: Some(vec!["beta".to_string(), "gamma".to_string()]),
            symbol: Some("delta".to_string()),
            section: Some("epsilon".to_string()),
            parent: Some("zeta".to_string()),
            glob: Some("*.rs".to_string()),
            ..Default::default()
        };
        let text = params.to_text();
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
        assert!(text.contains("gamma"));
        assert!(text.contains("delta"));
        assert!(text.contains("epsilon"));
        assert!(text.contains("zeta"));
        assert!(text.contains("*.rs"));
    }

    #[test]
    fn parse_kind_handles_variants() {
        assert_eq!(QueryParams::parse_kind("definition"), QueryKind::Definition);
        assert_eq!(QueryParams::parse_kind("reference"), QueryKind::Reference);
        assert_eq!(QueryParams::parse_kind("any"), QueryKind::Any);
        assert_eq!(QueryParams::parse_kind("unknown"), QueryKind::Any);
    }
}
