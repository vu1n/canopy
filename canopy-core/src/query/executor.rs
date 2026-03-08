//! Query execution engine.

use crate::error::CanopyError;
use crate::handle::Handle;
use crate::index::RepoIndex;
use crate::parse::estimate_tokens;
use crate::scoring::{select_for_expansion, HandleScorer};
use std::collections::HashSet;

use super::dsl::Query;
use super::params::split_terms;
use super::QueryOptions;
use super::QueryResult;

/// Default expand budget for optional auto-expansion.
pub const DEFAULT_EXPAND_BUDGET: usize = 0;

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
            node_type_priors: None,
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
            expanded_count: 0,
            expanded_tokens: 0,
            expanded_handle_ids: Vec::new(),
        });
    }

    let handles = dedupe_handles(execute_query_internal(query, index, effective_limit * 2)?);

    let total_matches = handles.len();
    let truncated = handles.len() > effective_limit;

    let mut handles: Vec<Handle> = handles.into_iter().take(effective_limit).collect();
    let total_tokens: usize = handles.iter().map(|h| h.token_count).sum();

    let mut expanded_count = 0usize;
    let mut expanded_tokens = 0usize;

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
                (expanded_count, expanded_tokens) = expanded_stats(&handles);
                (true, None)
            } else {
                (false, Some("Failed to expand handles".to_string()))
            }
        } else {
            let query_text = extract_query_terms(query).join(" ");
            let scorer = HandleScorer::new(&query_text)
                .with_node_type_priors(options.node_type_priors.clone());
            let selected = select_for_expansion(&handles, expand_budget, &scorer);

            if selected.is_empty() {
                (
                    false,
                    Some(format!(
                        "Expanded 0/{} handles (0/{} tokens). Use canopy_expand for remaining.",
                        handles.len(),
                        total_tokens
                    )),
                )
            } else {
                let handle_ids: Vec<String> = selected
                    .iter()
                    .map(|idx| handles[*idx].id.to_string())
                    .collect();

                if let Ok(contents) = index.expand(&handle_ids) {
                    let content_map: std::collections::HashMap<String, String> =
                        contents.into_iter().collect();
                    for idx in selected {
                        let id = handles[idx].id.to_string();
                        if let Some(content) = content_map.get(&id) {
                            handles[idx].content = Some(content.clone());
                        }
                    }
                    (expanded_count, expanded_tokens) = expanded_stats(&handles);
                    (
                        false,
                        Some(format!(
                            "Expanded {}/{} handles ({}/{} tokens). Use canopy_expand for remaining.",
                            expanded_count,
                            handles.len(),
                            expanded_tokens,
                            total_tokens
                        )),
                    )
                } else {
                    (false, Some("Failed to expand ranked handles".to_string()))
                }
            }
        }
    } else {
        (false, None)
    };

    let expanded_handle_ids = expanded_handle_ids(&handles);

    Ok(QueryResult {
        handles,
        ref_handles: None,
        total_tokens,
        truncated,
        total_matches,
        auto_expanded,
        expand_note,
        expanded_count,
        expanded_tokens,
        expanded_handle_ids,
    })
}

fn dedupe_handles(handles: Vec<Handle>) -> Vec<Handle> {
    let mut seen = HashSet::new();
    handles
        .into_iter()
        .filter(|h| seen.insert(h.id.to_string()))
        .collect()
}

fn expanded_stats(handles: &[Handle]) -> (usize, usize) {
    let expanded_count = handles.iter().filter(|h| h.content.is_some()).count();
    let expanded_tokens = handles
        .iter()
        .filter(|h| h.content.is_some())
        .map(|h| h.token_count)
        .sum();
    (expanded_count, expanded_tokens)
}

fn expanded_handle_ids(handles: &[Handle]) -> Vec<String> {
    handles
        .iter()
        .filter(|h| h.content.is_some())
        .map(|h| h.id.to_string())
        .collect()
}

pub(crate) fn extract_query_terms(query: &Query) -> Vec<String> {
    let mut terms = Vec::new();
    collect_query_terms(query, &mut terms);

    let mut seen = HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    terms
}

fn collect_query_terms(query: &Query, terms: &mut Vec<String>) {
    match query {
        Query::Section(s)
        | Query::Grep(s)
        | Query::File(s)
        | Query::Code(s)
        | Query::Children(s)
        | Query::Definition(s)
        | Query::References(s) => add_terms(s, terms),
        Query::ChildrenNamed(parent, symbol) => {
            add_terms(parent, terms);
            add_terms(symbol, terms);
        }
        Query::InFile(glob, subquery) => {
            add_terms(glob, terms);
            collect_query_terms(subquery, terms);
        }
        Query::Union(queries) | Query::Intersect(queries) => {
            for q in queries {
                collect_query_terms(q, terms);
            }
        }
        Query::Limit(_, q) => collect_query_terms(q, terms),
    }
}

fn add_terms(text: &str, terms: &mut Vec<String>) {
    terms.extend(split_terms(text));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Handle, NodeType, Span};

    fn make_handle(file: &str, span: Span, content: Option<&str>) -> Handle {
        let mut h = Handle::new(
            file.to_string(),
            NodeType::Function,
            span.clone(),
            (1, 10),
            50,
            "preview".to_string(),
        );
        if let Some(c) = content {
            h.content = Some(c.to_string());
        }
        h
    }

    #[test]
    fn dedupe_handles_removes_duplicate_ids() {
        let h1 = make_handle("a.rs", 0..50, None);
        let h2 = make_handle("b.rs", 100..200, None);
        // Same file+span as h1, so same ID
        let h3 = make_handle("a.rs", 0..50, None);

        let deduped = dedupe_handles(vec![h1.clone(), h2.clone(), h3]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].id, h1.id);
        assert_eq!(deduped[1].id, h2.id);
    }

    #[test]
    fn expanded_stats_counts_only_content_handles() {
        let handles = vec![
            make_handle("a.rs", 0..50, Some("fn a() {}")),
            make_handle("b.rs", 100..200, None),
            make_handle("c.rs", 200..300, Some("fn c() {}")),
        ];
        let (count, tokens) = expanded_stats(&handles);
        assert_eq!(count, 2);
        assert_eq!(tokens, 100); // 50 + 50 token_count for the two expanded handles
    }

    #[test]
    fn expanded_handle_ids_returns_only_expanded() {
        let handles = vec![
            make_handle("a.rs", 0..50, Some("content")),
            make_handle("b.rs", 100..200, None),
            make_handle("c.rs", 300..400, Some("more")),
        ];
        let ids = expanded_handle_ids(&handles);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], handles[0].id.to_string());
        assert_eq!(ids[1], handles[2].id.to_string());
    }

    #[test]
    fn extract_query_terms_deduplicates_and_lowercases() {
        let q = Query::Union(vec![
            Query::Grep("Hello World".to_string()),
            Query::Code("hello".to_string()),
        ]);
        let terms = extract_query_terms(&q);
        // "hello" appears twice but should be deduplicated
        assert_eq!(terms, vec!["hello", "world"]);
    }

    #[test]
    fn extract_query_terms_splits_on_non_alphanumeric() {
        let q = Query::Grep("foo-bar_baz.qux".to_string());
        let terms = extract_query_terms(&q);
        // Splits on '-' and '.', keeps '_'
        assert_eq!(terms, vec!["foo", "bar_baz", "qux"]);
    }

    #[test]
    fn extract_query_terms_nested_in_file() {
        let q = Query::Limit(
            5,
            Box::new(Query::InFile(
                "src/**/*.rs".to_string(),
                Box::new(Query::Grep("validate".to_string())),
            )),
        );
        let terms = extract_query_terms(&q);
        assert!(terms.contains(&"src".to_string()));
        assert!(terms.contains(&"rs".to_string()));
        assert!(terms.contains(&"validate".to_string()));
    }
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
