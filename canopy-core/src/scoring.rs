//! Handle scoring and budget-aware selection for partial auto-expansion.

use crate::document::NodeType;
use crate::handle::Handle;
use std::collections::HashMap;

/// Scores handles for expansion relevance and cost-efficiency.
pub struct HandleScorer {
    query_terms: Vec<String>,
    node_type_priors: Option<HashMap<NodeType, f64>>,
}

impl HandleScorer {
    pub fn new(query_text: &str) -> Self {
        let query_terms = split_terms(query_text);
        Self {
            query_terms,
            node_type_priors: None,
        }
    }

    pub fn with_node_type_priors(
        mut self,
        node_type_priors: Option<HashMap<NodeType, f64>>,
    ) -> Self {
        self.node_type_priors = node_type_priors;
        self
    }

    pub fn score(&self, handle: &Handle) -> f64 {
        let haystack = format!(
            "{} {}",
            handle.file_path.to_lowercase(),
            handle.preview.to_lowercase()
        );

        let relevance = if self.query_terms.is_empty() {
            1.0
        } else {
            let hits = self
                .query_terms
                .iter()
                .filter(|term| haystack.contains(term.as_str()))
                .count();
            let raw = hits as f64 / self.query_terms.len() as f64;
            raw.max(0.1)
        };

        let default_type_weight = match handle.node_type {
            NodeType::Function | NodeType::Method => 1.0,
            NodeType::Class | NodeType::Struct => 0.8,
            NodeType::Section => 0.6,
            NodeType::CodeBlock => 0.5,
            NodeType::Chunk => 0.3,
            NodeType::Paragraph => 0.2,
        };
        let type_weight = self
            .node_type_priors
            .as_ref()
            .and_then(|p| p.get(&handle.node_type).copied())
            .unwrap_or(default_type_weight);

        let token_count = handle.token_count.max(1) as f64;
        let cost_efficiency = 1.0 / (1.0 + token_count.ln());

        0.6 * relevance + 0.25 * type_weight + 0.15 * cost_efficiency
    }
}

/// Greedy selection by descending score, constrained by token budget.
///
/// Returns selected indices in original order for stable downstream handling.
pub fn select_for_expansion(
    handles: &[Handle],
    budget: usize,
    scorer: &HandleScorer,
) -> Vec<usize> {
    if budget == 0 || handles.is_empty() {
        return Vec::new();
    }

    let mut ranked: Vec<(usize, f64, usize)> = handles
        .iter()
        .enumerate()
        .map(|(idx, handle)| (idx, scorer.score(handle), handle.token_count))
        .collect();

    // Higher score first; tie-break smaller handles to better pack the budget.
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2)));

    let mut selected = Vec::new();
    let mut used_tokens = 0usize;

    for (idx, _score, handle_tokens) in ranked {
        if used_tokens + handle_tokens <= budget {
            selected.push(idx);
            used_tokens += handle_tokens;
        }
    }

    selected.sort_unstable();
    selected
}

fn split_terms(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Handle, Span};

    fn make_handle(file: &str, preview: &str, node_type: NodeType, token_count: usize) -> Handle {
        Handle::new(
            file.to_string(),
            node_type,
            Span { start: 0, end: 10 },
            (1, 1),
            token_count,
            preview.to_string(),
        )
    }

    #[test]
    fn score_prefers_query_match() {
        let scorer = HandleScorer::new("auth login");
        let matched = make_handle(
            "src/auth/session.rs",
            "login handler",
            NodeType::Function,
            80,
        );
        let unmatched = make_handle("src/db/repo.rs", "sql repository", NodeType::Function, 80);
        assert!(scorer.score(&matched) > scorer.score(&unmatched));
    }

    #[test]
    fn score_prefers_lower_token_cost_when_other_factors_close() {
        let scorer = HandleScorer::new("auth");
        let small = make_handle("src/auth/a.rs", "auth function", NodeType::Function, 40);
        let large = make_handle("src/auth/b.rs", "auth function", NodeType::Function, 400);
        assert!(scorer.score(&small) > scorer.score(&large));
    }

    #[test]
    fn selection_respects_budget() {
        let scorer = HandleScorer::new("auth");
        let handles = vec![
            make_handle("src/auth/a.rs", "auth", NodeType::Function, 100),
            make_handle("src/auth/b.rs", "auth", NodeType::Function, 100),
            make_handle("src/db/c.rs", "db", NodeType::Function, 100),
        ];
        let selected = select_for_expansion(&handles, 200, &scorer);
        let used: usize = selected.iter().map(|i| handles[*i].token_count).sum();
        assert!(used <= 200);
    }

    #[test]
    fn learned_priors_override_default_type_weight() {
        let mut priors = HashMap::new();
        priors.insert(NodeType::Paragraph, 1.0);
        priors.insert(NodeType::Function, 0.0);

        let scorer = HandleScorer::new("auth").with_node_type_priors(Some(priors));
        let paragraph = make_handle("docs/auth.md", "auth flow", NodeType::Paragraph, 80);
        let function = make_handle("src/auth.rs", "auth flow", NodeType::Function, 80);
        assert!(scorer.score(&paragraph) > scorer.score(&function));
    }
}
