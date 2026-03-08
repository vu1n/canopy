//! Symbol candidate extraction from handle previews.

use canopy_core::{split_terms, Handle};
use std::collections::{HashMap, HashSet};

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

/// Extract identifier-like symbol candidates from handle previews.
///
/// Scores tokens by identifier heuristics (camelCase, snake_case) and returns
/// the top `limit` candidates, excluding query terms and stop words.
pub fn extract_symbol_candidates_from_handles(
    handles: &[Handle],
    query_text: &str,
    limit: usize,
) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use canopy_core::document::NodeType;
    use canopy_core::handle::HandleSource;

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
    fn skips_short_tokens() {
        let handle = make_handle("a1b2c3", "fn ab() {}");
        let results = extract_symbol_candidates_from_handles(&[handle], "query", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn prefers_identifiers() {
        let handle = make_handle("d4e5f6", "fn handleAuth() { verify_token(ctx); }");
        let results = extract_symbol_candidates_from_handles(&[handle], "test", 5);
        assert!(!results.is_empty());
        assert!(results.contains(&"handleAuth".to_string()));
    }
}
