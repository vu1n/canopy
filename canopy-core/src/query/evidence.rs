//! Evidence pack types and builder.

use crate::document::NodeType;
use crate::handle::HandleSource;
use crate::scoring::HandleScorer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::QueryResult;

/// Compact evidence view derived from query results.
///
/// Intentionally excludes full snippets/content to keep context payloads small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePack {
    pub query_text: String,
    pub total_matches: usize,
    pub truncated: bool,
    pub selected_count: usize,
    pub selected_tokens: usize,
    pub handles: Vec<EvidenceHandle>,
    pub files: Vec<EvidenceFileSummary>,
    /// Suggested first handles to expand for deeper context.
    pub expand_suggestion: Vec<String>,
    /// Action guidance so agents can stop exploring and start synthesis.
    #[serde(default)]
    pub guidance: EvidenceGuidance,
}

impl EvidencePack {
    /// Reorder expand suggestions so recently-expanded handles come last.
    ///
    /// If all suggestions are recently expanded, backfill from pack handles.
    /// The `is_recent` predicate determines whether a handle ID was recently expanded.
    pub fn reorder_expand_suggestions(&mut self, is_recent: impl Fn(&str) -> bool) {
        if self.expand_suggestion.is_empty() {
            return;
        }

        let mut fresh = Vec::new();
        let mut repeated = Vec::new();
        for id in &self.expand_suggestion {
            if is_recent(id) {
                repeated.push(id.clone());
            } else {
                fresh.push(id.clone());
            }
        }

        if repeated.is_empty() {
            return;
        }

        if fresh.is_empty() {
            for handle in &self.handles {
                if fresh.len() >= self.expand_suggestion.len() {
                    break;
                }
                if is_recent(&handle.id) {
                    continue;
                }
                if !fresh.iter().any(|id| id == &handle.id) {
                    fresh.push(handle.id.clone());
                }
            }
        }

        if !fresh.is_empty() {
            fresh.extend(repeated);
            fresh.truncate(self.expand_suggestion.len());
            self.expand_suggestion = fresh;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceHandle {
    pub id: String,
    pub file_path: String,
    pub node_type: NodeType,
    pub line_range: (usize, usize),
    pub token_count: usize,
    pub source: HandleSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceFileSummary {
    pub file_path: String,
    pub handle_ids: Vec<String>,
    pub total_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAction {
    RefineQuery,
    ExpandThenAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceConfidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceGuidance {
    /// Heuristic confidence score in [0, 1].
    pub confidence: f64,
    pub confidence_band: EvidenceConfidence,
    /// True when agents should stop issuing more queries for this task.
    pub stop_querying: bool,
    pub recommended_action: EvidenceAction,
    /// Suggested number of handles to expand before writing final output.
    pub suggested_expand_count: usize,
    /// Additional evidence_pack calls recommended before synthesis.
    pub max_additional_queries: usize,
    /// Brief explanation for action selection.
    pub rationale: String,
    /// One-line instruction intended for direct agent consumption.
    pub next_step: String,
}

impl Default for EvidenceGuidance {
    fn default() -> Self {
        Self {
            confidence: 0.0,
            confidence_band: EvidenceConfidence::Low,
            stop_querying: false,
            recommended_action: EvidenceAction::RefineQuery,
            suggested_expand_count: 0,
            max_additional_queries: 2,
            rationale: "No evidence handles were selected for this query.".to_string(),
            next_step:
                "Refine the query with more specific symbols, paths, or terms before expanding."
                    .to_string(),
        }
    }
}

/// Build a compact, ranked evidence pack from a query result.
///
/// This keeps model context small by returning metadata and handle IDs only.
pub fn build_evidence_pack(
    result: &QueryResult,
    query_text: &str,
    max_handles: usize,
    max_per_file: usize,
) -> EvidencePack {
    if result.handles.is_empty() || max_handles == 0 || max_per_file == 0 {
        let guidance = EvidenceGuidance::default();
        return EvidencePack {
            query_text: query_text.to_string(),
            total_matches: result.total_matches,
            truncated: result.truncated,
            selected_count: 0,
            selected_tokens: 0,
            handles: Vec::new(),
            files: Vec::new(),
            expand_suggestion: Vec::new(),
            guidance,
        };
    }

    let scorer = HandleScorer::new(query_text);
    let mut ranked: Vec<(usize, f64)> = result
        .handles
        .iter()
        .enumerate()
        .map(|(idx, h)| (idx, scorer.score(h)))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.total_cmp(&a.1).then_with(|| {
            result.handles[a.0]
                .token_count
                .cmp(&result.handles[b.0].token_count)
        })
    });

    let mut file_counts: HashMap<&str, usize> = HashMap::new();
    let mut selected: Vec<(usize, f64)> = Vec::new();
    for (idx, score) in ranked {
        if selected.len() >= max_handles {
            break;
        }
        let file = result.handles[idx].file_path.as_str();
        if file_counts.get(file).copied().unwrap_or(0) >= max_per_file {
            continue;
        }
        selected.push((idx, score));
        *file_counts.entry(file).or_insert(0) += 1;
    }

    let handles: Vec<EvidenceHandle> = selected
        .iter()
        .map(|(idx, score)| {
            let h = &result.handles[*idx];
            EvidenceHandle {
                id: h.id.to_string(),
                file_path: h.file_path.clone(),
                node_type: h.node_type,
                line_range: h.line_range,
                token_count: h.token_count,
                source: h.source.clone(),
                commit_sha: h.commit_sha.clone(),
                generation: h.generation,
                score: *score,
            }
        })
        .collect();

    let selected_tokens = handles.iter().map(|h| h.token_count).sum();

    let mut file_index: HashMap<String, usize> = HashMap::new();
    let mut files: Vec<EvidenceFileSummary> = Vec::new();
    for handle in &handles {
        if let Some(idx) = file_index.get(&handle.file_path).copied() {
            files[idx].handle_ids.push(handle.id.clone());
            files[idx].total_tokens += handle.token_count;
        } else {
            let idx = files.len();
            files.push(EvidenceFileSummary {
                file_path: handle.file_path.clone(),
                handle_ids: vec![handle.id.clone()],
                total_tokens: handle.token_count,
            });
            file_index.insert(handle.file_path.clone(), idx);
        }
    }

    let expand_suggestion = handles
        .iter()
        .take(6)
        .map(|h| h.id.clone())
        .collect::<Vec<_>>();
    let guidance = build_evidence_guidance(
        &selected,
        handles.len(),
        files.len(),
        result.total_matches,
        result.truncated,
        max_handles.max(1),
    );

    EvidencePack {
        query_text: query_text.to_string(),
        total_matches: result.total_matches,
        truncated: result.truncated,
        selected_count: handles.len(),
        selected_tokens,
        handles,
        files,
        expand_suggestion,
        guidance,
    }
}

fn build_evidence_guidance(
    selected: &[(usize, f64)],
    selected_count: usize,
    file_count: usize,
    total_matches: usize,
    truncated: bool,
    max_handles: usize,
) -> EvidenceGuidance {
    if selected_count == 0 {
        return EvidenceGuidance {
            rationale: "No ranked evidence available for this query.".to_string(),
            next_step:
                "Refine query terms and rerun canopy_evidence_pack; do not expand empty handles."
                    .to_string(),
            ..Default::default()
        };
    }

    let top_n = selected.len().min(3);
    let avg_top_score = selected.iter().take(top_n).map(|(_, s)| *s).sum::<f64>() / top_n as f64;
    let file_coverage = (file_count.min(3) as f64) / 3.0;
    let fill_ratio = (selected_count as f64 / max_handles as f64).min(1.0);
    let match_signal = if total_matches == 0 {
        0.0
    } else if total_matches < 3 {
        0.4
    } else {
        1.0
    };
    let truncation_penalty = if truncated { 0.10 } else { 0.0 };
    let confidence =
        (0.55 * avg_top_score + 0.20 * file_coverage + 0.15 * fill_ratio + 0.10 * match_signal
            - truncation_penalty)
            .clamp(0.0, 1.0);

    let confidence_band = if confidence < 0.35 {
        EvidenceConfidence::Low
    } else if confidence < 0.70 {
        EvidenceConfidence::Medium
    } else {
        EvidenceConfidence::High
    };

    let stop_querying = confidence >= 0.55 || (selected_count >= 4 && file_count >= 2);
    let suggested_expand_count = if confidence >= 0.75 {
        selected_count.min(2)
    } else if confidence >= 0.50 {
        selected_count.min(3)
    } else {
        selected_count.min(4)
    }
    .max(1);

    let (max_additional_queries, rationale, next_step) = if stop_querying {
        (
            0,
            format!(
                "Ranked evidence is sufficient ({} handles across {} files, {:.2} confidence).",
                selected_count, file_count, confidence
            ),
            format!(
                "Expand {} suggested handles, then write the final answer. Only re-query if expansions contradict the task.",
                suggested_expand_count
            ),
        )
    } else {
        (
            1,
            format!(
                "Evidence is partial ({} handles across {} files, {:.2} confidence).",
                selected_count, file_count, confidence
            ),
            format!(
                "Run at most one narrower canopy_evidence_pack query, then expand {} handles and write the answer.",
                suggested_expand_count
            ),
        )
    };

    EvidenceGuidance {
        confidence,
        confidence_band,
        stop_querying,
        recommended_action: if stop_querying {
            EvidenceAction::ExpandThenAnswer
        } else {
            EvidenceAction::RefineQuery
        },
        suggested_expand_count,
        max_additional_queries,
        rationale,
        next_step,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Handle, NodeType, Span};

    fn make_handle(file: &str, node_type: NodeType, span: Span, tokens: usize, preview: &str) -> Handle {
        Handle::new(file.to_string(), node_type, span, (1, 10), tokens, preview.to_string())
    }

    fn make_query_result(handles: Vec<Handle>) -> QueryResult {
        let total_tokens = handles.iter().map(|h| h.token_count).sum();
        let total_matches = handles.len();
        QueryResult {
            handles,
            total_tokens,
            total_matches,
            ..QueryResult::default()
        }
    }

    #[test]
    fn build_evidence_pack_empty_handles_returns_default_guidance() {
        let result = make_query_result(vec![]);
        let pack = build_evidence_pack(&result, "some query", 10, 3);

        assert_eq!(pack.selected_count, 0);
        assert_eq!(pack.selected_tokens, 0);
        assert!(pack.handles.is_empty());
        assert!(pack.files.is_empty());
        assert!(pack.expand_suggestion.is_empty());
        assert_eq!(pack.guidance.recommended_action, EvidenceAction::RefineQuery);
        assert!(!pack.guidance.stop_querying);
    }

    #[test]
    fn build_evidence_pack_respects_max_per_file() {
        // 4 handles from the same file, max_per_file=2 should keep only 2
        let handles = (0..4)
            .map(|i| {
                let start = i * 100;
                let end = start + 50;
                make_handle("src/auth.rs", NodeType::Function, start..end, 40, "auth function code")
            })
            .collect();
        let result = make_query_result(handles);
        let pack = build_evidence_pack(&result, "auth", 10, 2);

        assert_eq!(pack.selected_count, 2);
        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].handle_ids.len(), 2);
    }

    #[test]
    fn build_evidence_pack_file_summaries_aggregate_correctly() {
        let handles = vec![
            make_handle("src/a.rs", NodeType::Function, 0..50, 30, "fn alpha"),
            make_handle("src/b.rs", NodeType::Function, 0..50, 50, "fn beta"),
            make_handle("src/a.rs", NodeType::Method, 100..200, 70, "fn gamma"),
        ];
        let result = make_query_result(handles);
        let pack = build_evidence_pack(&result, "test", 10, 5);

        assert_eq!(pack.selected_count, 3);
        assert_eq!(pack.files.len(), 2);

        let file_a = pack.files.iter().find(|f| f.file_path == "src/a.rs").unwrap();
        assert_eq!(file_a.handle_ids.len(), 2);
        assert_eq!(file_a.total_tokens, 100); // 30 + 70
    }

    #[test]
    fn guidance_confidence_bands_are_correct() {
        // Zero selected -> default guidance
        let g0 = build_evidence_guidance(&[], 0, 0, 0, false, 10);
        assert_eq!(g0.confidence_band, EvidenceConfidence::Low);
        assert!(!g0.stop_querying);
        assert_eq!(g0.recommended_action, EvidenceAction::RefineQuery);

        // High scores, multiple files, good fill -> High confidence
        let selected: Vec<(usize, f64)> = vec![(0, 0.95), (1, 0.90), (2, 0.85), (3, 0.80)];
        let g_high = build_evidence_guidance(&selected, 4, 3, 10, false, 4);
        assert!(g_high.confidence >= 0.70, "expected High band, got {:.2}", g_high.confidence);
        assert_eq!(g_high.confidence_band, EvidenceConfidence::High);
        assert!(g_high.stop_querying);
        assert_eq!(g_high.recommended_action, EvidenceAction::ExpandThenAnswer);
        assert_eq!(g_high.max_additional_queries, 0);

        // Low scores, single file, sparse matches -> Low/Medium
        let selected_low: Vec<(usize, f64)> = vec![(0, 0.15)];
        let g_low = build_evidence_guidance(&selected_low, 1, 1, 1, true, 10);
        assert!(g_low.confidence < 0.35, "expected Low band, got {:.2}", g_low.confidence);
        assert_eq!(g_low.confidence_band, EvidenceConfidence::Low);
    }

    #[test]
    fn reorder_expand_suggestions_demotes_recent() {
        let handles = vec![
            EvidenceHandle {
                id: "a".to_string(),
                file_path: "a.rs".to_string(),
                node_type: NodeType::Function,
                line_range: (1, 5),
                token_count: 10,
                source: HandleSource::Local,
                commit_sha: None,
                generation: None,
                score: 0.9,
            },
            EvidenceHandle {
                id: "b".to_string(),
                file_path: "b.rs".to_string(),
                node_type: NodeType::Function,
                line_range: (1, 5),
                token_count: 10,
                source: HandleSource::Local,
                commit_sha: None,
                generation: None,
                score: 0.8,
            },
        ];

        let mut pack = EvidencePack {
            query_text: "test".to_string(),
            total_matches: 2,
            truncated: false,
            selected_count: 2,
            selected_tokens: 20,
            handles,
            files: Vec::new(),
            expand_suggestion: vec!["a".to_string(), "b".to_string()],
            guidance: EvidenceGuidance::default(),
        };

        // "a" was recently expanded, so it should be demoted
        pack.reorder_expand_suggestions(|id| id == "a");

        assert_eq!(pack.expand_suggestion[0], "b");
        assert_eq!(pack.expand_suggestion[1], "a");
    }
}
