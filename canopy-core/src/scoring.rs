//! Handle scoring and budget-aware selection for partial auto-expansion.

use crate::document::NodeType;
use crate::handle::Handle;
use crate::query::split_terms;
use std::collections::HashMap;

const NEARBY_LINE_GAP: usize = 2;
const MAX_EXPANSIONS_PER_FILE: usize = 2;

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
    let mut file_expansion_counts: HashMap<&str, usize> = HashMap::new();

    for (idx, _score, handle_tokens) in ranked {
        if is_near_duplicate_selection(idx, handles, &selected) {
            continue;
        }
        let file_path = handles[idx].file_path.as_str();
        if file_expansion_counts.get(file_path).copied().unwrap_or(0) >= MAX_EXPANSIONS_PER_FILE {
            continue;
        }
        if used_tokens + handle_tokens <= budget {
            selected.push(idx);
            used_tokens += handle_tokens;
            *file_expansion_counts.entry(file_path).or_insert(0) += 1;
        }
    }

    selected.sort_unstable();
    selected
}

fn is_near_duplicate_selection(
    candidate_idx: usize,
    handles: &[Handle],
    selected_indices: &[usize],
) -> bool {
    let candidate = &handles[candidate_idx];
    let (candidate_start, candidate_end) = candidate.line_range;

    selected_indices.iter().any(|selected_idx| {
        let selected = &handles[*selected_idx];
        if selected.file_path != candidate.file_path {
            return false;
        }

        let (selected_start, selected_end) = selected.line_range;
        if candidate_start <= selected_end && selected_start <= candidate_end {
            return true;
        }

        if candidate_end < selected_start {
            selected_start - candidate_end <= NEARBY_LINE_GAP
        } else if selected_end < candidate_start {
            candidate_start - selected_end <= NEARBY_LINE_GAP
        } else {
            false
        }
    })
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
    fn selection_skips_nearby_duplicate_ranges_in_same_file() {
        let scorer = HandleScorer::new("auth");
        let mut first = make_handle("src/auth/a.rs", "auth", NodeType::Function, 50);
        first.line_range = (10, 12);
        let mut nearby = make_handle("src/auth/a.rs", "auth", NodeType::Function, 50);
        nearby.line_range = (13, 15);
        let mut far = make_handle("src/auth/a.rs", "auth", NodeType::Function, 50);
        far.line_range = (40, 45);

        let handles = vec![first, nearby, far];
        let selected = select_for_expansion(&handles, 200, &scorer);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn selection_limits_expansions_per_file() {
        let scorer = HandleScorer::new("auth");
        let handles = vec![
            make_handle("src/auth/a.rs", "auth", NodeType::Function, 40),
            make_handle("src/auth/a.rs", "auth", NodeType::Function, 40),
            make_handle("src/auth/a.rs", "auth", NodeType::Function, 40),
            make_handle("src/auth/b.rs", "auth", NodeType::Function, 40),
        ];
        let selected = select_for_expansion(&handles, 400, &scorer);
        let selected_a: usize = selected
            .iter()
            .filter(|idx| handles[**idx].file_path == "src/auth/a.rs")
            .count();
        assert!(selected_a <= 2);
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
