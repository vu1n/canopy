//! Query DSL parser and executor

use crate::error::CanopyError;
use crate::handle::{Handle, RefHandle};
use crate::index::RepoIndex;
use crate::parse::estimate_tokens;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Query AST
#[derive(Debug, Clone)]
pub enum Query {
    /// (section "heading") - fuzzy match on section headings
    Section(String),
    /// (grep "pattern") - FTS5 search
    Grep(String),
    /// (file "path") - entire file as handle
    File(String),
    /// (code "symbol") - AST symbol search
    Code(String),
    /// (in-file "glob" query) - search within specific files
    InFile(String, Box<Query>),
    /// (union q1 q2 ...) - combine results
    Union(Vec<Query>),
    /// (intersect q1 q2 ...) - intersection of results
    Intersect(Vec<Query>),
    /// (limit N query) - limit results
    Limit(usize, Box<Query>),
    /// (children "parent") - get all children of a parent symbol
    Children(String),
    /// (children-named "parent" "symbol") - get named children of a parent
    ChildrenNamed(String, String),
    /// (definition "symbol") - exact match symbol definition
    Definition(String),
    /// (references "symbol") - find references to a symbol
    References(String),
}

/// Match mode for multi-pattern queries
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Union: match any of the patterns (OR)
    #[default]
    Any,
    /// Intersect: match all of the patterns (AND)
    All,
}

/// Query kind for filtering results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Parent symbol to filter by (e.g., class name for methods)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,

    /// Query kind: definition, reference, or any (default)
    #[serde(default)]
    pub kind: QueryKind,

    /// File path glob pattern to filter results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,

    /// Match mode for multi-pattern queries
    #[serde(default)]
    pub match_mode: MatchMode,

    /// Maximum number of results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// Auto-expand results if total tokens fit within budget
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expand_budget: Option<usize>,
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

    /// Convert params to Query AST
    pub fn to_query(&self) -> crate::Result<Query> {
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
    pub fn to_options(&self) -> QueryOptions {
        QueryOptions {
            limit: self.limit,
            expand_budget: self.expand_budget,
        }
    }
}

/// Query result with handles
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Parse a query string into a Query AST
pub fn parse_query(input: &str) -> crate::Result<Query> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CanopyError::QueryParse {
            position: 0,
            message: "Empty query".to_string(),
        });
    }

    let mut parser = QueryParser::new(input);
    parser.parse()
}

/// Query options for executing queries
#[derive(Debug, Default)]
pub struct QueryOptions {
    /// Override default result limit
    pub limit: Option<usize>,
    /// Auto-expand results if total tokens fit within budget (default: 5000)
    pub expand_budget: Option<usize>,
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
}

/// Default expand budget when auto-expansion is enabled
pub const DEFAULT_EXPAND_BUDGET: usize = 5000;

/// Execute a query against the index
pub fn execute_query(
    query: &Query,
    index: &RepoIndex,
    limit_override: Option<usize>,
) -> crate::Result<QueryResult> {
    execute_query_with_options(
        query,
        index,
        QueryOptions {
            limit: limit_override,
            expand_budget: None,
        },
    )
}

/// Execute a query with full options including expand_budget
pub fn execute_query_with_options(
    query: &Query,
    index: &RepoIndex,
    options: QueryOptions,
) -> crate::Result<QueryResult> {
    let default_limit = index.default_limit();
    let effective_limit = options.limit.unwrap_or(default_limit);

    if let Query::References(symbol) = query {
        let mut refs = index.search_references(symbol, effective_limit * 2)?;
        let total_matches = refs.len();
        let truncated = refs.len() > effective_limit;
        refs.truncate(effective_limit);

        let total_tokens = refs.iter().map(|r| estimate_tokens(&r.preview)).sum();

        return Ok(QueryResult {
            handles: Vec::new(),
            ref_handles: Some(refs),
            total_tokens,
            truncated,
            total_matches,
            auto_expanded: false,
            expand_note: None,
        });
    }

    let handles = execute_query_internal(query, index, effective_limit * 2)?;

    let total_matches = handles.len();
    let truncated = handles.len() > effective_limit;

    let mut handles: Vec<Handle> = handles.into_iter().take(effective_limit).collect();
    let total_tokens: usize = handles.iter().map(|h| h.token_count).sum();

    // Auto-expand if budget allows
    let expand_budget = options.expand_budget.unwrap_or(0);
    let (auto_expanded, expand_note) = if expand_budget > 0 {
        if total_tokens <= expand_budget {
            // Expand all handles
            let handle_ids: Vec<String> = handles.iter().map(|h| h.id.to_string()).collect();
            if let Ok(contents) = index.expand(&handle_ids) {
                let content_map: std::collections::HashMap<String, String> =
                    contents.into_iter().collect();
                for handle in &mut handles {
                    if let Some(content) = content_map.get(&handle.id.to_string()) {
                        handle.content = Some(content.clone());
                    }
                }
                (true, None)
            } else {
                (false, Some("Failed to expand handles".to_string()))
            }
        } else {
            (false, Some(format!(
                "Results ({} tokens) exceed expand_budget ({}). Use canopy_expand to retrieve specific handles.",
                total_tokens, expand_budget
            )))
        }
    } else {
        (false, None)
    };

    Ok(QueryResult {
        handles,
        ref_handles: None,
        total_tokens,
        truncated,
        total_matches,
        auto_expanded,
        expand_note,
    })
}

fn execute_query_internal(
    query: &Query,
    index: &RepoIndex,
    limit: usize,
) -> crate::Result<Vec<Handle>> {
    match query {
        Query::Section(heading) => index.search_sections(heading, limit),

        Query::Grep(pattern) => index.fts_search(pattern, limit),

        Query::File(path) => index.get_file(path),

        Query::Code(symbol) => index.search_code(symbol, limit),

        Query::Children(parent) => index.search_children(parent, limit),

        Query::ChildrenNamed(parent, symbol) => index.search_children_named(parent, symbol, limit),

        Query::Definition(symbol) => index.search_definitions(symbol, limit),

        Query::References(symbol) => {
            // References return RefHandles, but for now we convert to regular Handles
            // by returning nodes that contain the reference
            index.search_reference_sources(symbol, limit)
        }

        Query::InFile(glob, subquery) => {
            // Only support grep inside in-file for now
            match subquery.as_ref() {
                Query::Grep(pattern) => index.search_in_files(glob, pattern, limit),
                _ => {
                    // For other queries, filter results by glob
                    let results = execute_query_internal(subquery, index, limit * 2)?;
                    let glob_matcher = globset::Glob::new(glob)
                        .map_err(|e| CanopyError::GlobPattern(e.to_string()))?
                        .compile_matcher();

                    Ok(results
                        .into_iter()
                        .filter(|h| glob_matcher.is_match(&h.file_path))
                        .take(limit)
                        .collect())
                }
            }
        }

        Query::Union(queries) => {
            let mut seen = HashSet::new();
            let mut results = Vec::new();

            for q in queries {
                let handles = execute_query_internal(q, index, limit)?;
                for handle in handles {
                    if seen.insert(handle.id.raw().to_string()) {
                        results.push(handle);
                    }
                }
            }

            results.truncate(limit);
            Ok(results)
        }

        Query::Intersect(queries) => {
            if queries.is_empty() {
                return Ok(Vec::new());
            }

            // Execute first query
            let first_results = execute_query_internal(&queries[0], index, limit * 2)?;
            let mut result_ids: HashSet<String> = first_results
                .iter()
                .map(|h| h.id.raw().to_string())
                .collect();

            // Intersect with remaining queries
            for q in &queries[1..] {
                let handles = execute_query_internal(q, index, limit * 2)?;
                let ids: HashSet<String> = handles.iter().map(|h| h.id.raw().to_string()).collect();
                result_ids = result_ids.intersection(&ids).cloned().collect();
            }

            // Return handles that are in the intersection
            let results: Vec<Handle> = first_results
                .into_iter()
                .filter(|h| result_ids.contains(h.id.raw()))
                .take(limit)
                .collect();

            Ok(results)
        }

        Query::Limit(n, subquery) => {
            let results = execute_query_internal(subquery, index, *n)?;
            Ok(results.into_iter().take(*n).collect())
        }
    }
}

/// S-expression parser for the query DSL
struct QueryParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> QueryParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn parse(&mut self) -> crate::Result<Query> {
        self.skip_whitespace();

        if self.peek() != Some('(') {
            return Err(self.error("Expected '('"));
        }
        self.advance(); // consume '('

        self.skip_whitespace();

        // Parse the operator
        let op = self.parse_identifier()?;

        let query = match op.as_str() {
            "section" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::Section(arg)
            }
            "grep" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::Grep(arg)
            }
            "file" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::File(arg)
            }
            "code" => {
                self.skip_whitespace();
                let arg = self.parse_string()?;
                Query::Code(arg)
            }
            "in-file" => {
                self.skip_whitespace();
                let glob = self.parse_string()?;
                self.skip_whitespace();
                let subquery = self.parse()?;
                Query::InFile(glob, Box::new(subquery))
            }
            "union" => {
                let mut queries = Vec::new();
                loop {
                    self.skip_whitespace();
                    if self.peek() == Some(')') {
                        break;
                    }
                    queries.push(self.parse()?);
                }
                Query::Union(queries)
            }
            "intersect" => {
                let mut queries = Vec::new();
                loop {
                    self.skip_whitespace();
                    if self.peek() == Some(')') {
                        break;
                    }
                    queries.push(self.parse()?);
                }
                Query::Intersect(queries)
            }
            "limit" => {
                self.skip_whitespace();
                let n = self.parse_number()?;
                self.skip_whitespace();
                let subquery = self.parse()?;
                Query::Limit(n, Box::new(subquery))
            }
            "children" => {
                self.skip_whitespace();
                let parent = self.parse_string()?;
                Query::Children(parent)
            }
            "children-named" => {
                self.skip_whitespace();
                let parent = self.parse_string()?;
                self.skip_whitespace();
                let symbol = self.parse_string()?;
                Query::ChildrenNamed(parent, symbol)
            }
            "definition" => {
                self.skip_whitespace();
                let symbol = self.parse_string()?;
                Query::Definition(symbol)
            }
            "references" => {
                self.skip_whitespace();
                let symbol = self.parse_string()?;
                Query::References(symbol)
            }
            _ => return Err(self.error(&format!("Unknown operator: {}", op))),
        };

        self.skip_whitespace();

        if self.peek() != Some(')') {
            return Err(self.error("Expected ')'"));
        }
        self.advance(); // consume ')'

        Ok(query)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn parse_identifier(&mut self) -> crate::Result<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let ident = &self.input[start..self.pos];
        if ident.is_empty() {
            Err(self.error("Expected identifier"))
        } else {
            Ok(ident.to_string())
        }
    }

    fn parse_string(&mut self) -> crate::Result<String> {
        if self.peek() != Some('"') {
            return Err(self.error("Expected '\"'"));
        }
        self.advance(); // consume opening quote

        let mut result = String::new();
        let mut escaped = false;

        loop {
            match self.advance() {
                None => return Err(self.error("Unterminated string")),
                Some('\\') if !escaped => {
                    escaped = true;
                }
                Some('"') if !escaped => {
                    break;
                }
                Some(c) => {
                    if escaped {
                        match c {
                            'n' => result.push('\n'),
                            't' => result.push('\t'),
                            'r' => result.push('\r'),
                            _ => result.push(c),
                        }
                        escaped = false;
                    } else {
                        result.push(c);
                    }
                }
            }
        }

        Ok(result)
    }

    fn parse_number(&mut self) -> crate::Result<usize> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        let num_str = &self.input[start..self.pos];
        num_str.parse().map_err(|_| self.error("Expected number"))
    }

    fn error(&self, message: &str) -> CanopyError {
        CanopyError::QueryParse {
            position: self.pos,
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
