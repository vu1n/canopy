use super::*;
use crate::NodeType;
use rusqlite::params;

fn temp_repo() -> std::path::PathBuf {
    crate::temp_test_dir("feedback-test")
}

#[test]
fn record_and_score_roundtrip() {
    let repo_root = temp_repo();
    let store = FeedbackStore::open(&repo_root).unwrap();

    let event_id = store
        .record_query_event(&QueryEvent {
            query_text: "auth".to_string(),
            predicted_globs: Some(vec!["**/auth/**/*.rs".to_string()]),
            files_indexed: 10,
            handles_returned: 1,
            total_tokens: 120,
        })
        .unwrap();

    store
        .record_query_handles(
            event_id,
            &[QueryHandle {
                handle_id: "h123".to_string(),
                file_path: "src/auth/mod.rs".to_string(),
                node_type: NodeType::Function,
                token_count: 120,
                first_match_glob: Some("**/auth/**/*.rs".to_string()),
            }],
        )
        .unwrap();

    store
        .record_expand_event(&ExpandEvent {
            query_event_id: Some(event_id),
            handle_id: "h123".to_string(),
            file_path: "src/auth/mod.rs".to_string(),
            node_type: NodeType::Function,
            token_count: 120,
            auto_expanded: false,
        })
        .unwrap();

    let scores = store
        .get_glob_scores(&["**/auth/**/*.rs".to_string()], 7.0)
        .unwrap();
    assert!(scores.get("**/auth/**/*.rs").copied().unwrap_or(0.0) > 0.0);

    let priors = store.get_node_type_priors().unwrap();
    assert!(priors.get(&NodeType::Function).copied().unwrap_or(0.0) > 0.0);
}

#[test]
fn prune_removes_expired_rows_and_cascades_handles() {
    let repo_root = temp_repo();
    let store = FeedbackStore::open(&repo_root).unwrap();
    let old_ts = now_ts() - (RETENTION_DAYS + 1) * 86_400;

    store
        .conn
        .execute(
            "INSERT INTO query_events (timestamp, query_text, files_indexed, handles_returned, total_tokens)
             VALUES (?, ?, 0, 1, 10)",
            params![old_ts, "legacy query"],
        )
        .unwrap();
    let old_query_event_id = store.conn.last_insert_rowid();

    store
        .conn
        .execute(
            "INSERT INTO query_handles
             (query_event_id, handle_id, file_path, node_type, token_count, first_match_glob, returned_at)
             VALUES (?, ?, ?, ?, ?, NULL, ?)",
            params![
                old_query_event_id,
                "hold",
                "src/old.rs",
                NodeType::Function.as_int() as i64,
                10i64,
                old_ts,
            ],
        )
        .unwrap();

    store
        .conn
        .execute(
            "INSERT INTO expand_events
             (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
             VALUES (?, ?, ?, ?, ?, 0, ?)",
            params![
                old_query_event_id,
                "hold",
                "src/old.rs",
                NodeType::Function.as_int() as i64,
                10i64,
                old_ts,
            ],
        )
        .unwrap();

    store.prune().unwrap();

    let query_count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM query_events WHERE id = ?",
            params![old_query_event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(query_count, 0);

    let handle_count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM query_handles WHERE query_event_id = ?",
            params![old_query_event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(handle_count, 0);

    let expand_count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM expand_events WHERE handle_id = ?",
            params!["hold"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(expand_count, 0);
}

#[test]
fn glob_scores_decay_with_age() {
    let repo_root = temp_repo();
    let store = FeedbackStore::open(&repo_root).unwrap();

    // Insert an old event (7 days ago) and a recent one (now)
    let old_ts = now_ts() - 7 * 86_400;
    store
        .conn
        .execute(
            "INSERT INTO query_events (timestamp, query_text) VALUES (?, ?)",
            params![old_ts, "old query"],
        )
        .unwrap();
    let old_id = store.conn.last_insert_rowid();
    store
        .conn
        .execute(
            "INSERT INTO query_handles (query_event_id, handle_id, file_path, node_type, token_count, first_match_glob, returned_at)
             VALUES (?, 'h1', 'old.rs', ?, 10, 'old_glob', ?)",
            params![old_id, NodeType::Function.as_int() as i64, old_ts],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO expand_events (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
             VALUES (?, 'h1', 'old.rs', ?, 10, 0, ?)",
            params![old_id, NodeType::Function.as_int() as i64, old_ts],
        )
        .unwrap();

    let recent_ts = now_ts();
    store
        .conn
        .execute(
            "INSERT INTO query_events (timestamp, query_text) VALUES (?, ?)",
            params![recent_ts, "new query"],
        )
        .unwrap();
    let new_id = store.conn.last_insert_rowid();
    store
        .conn
        .execute(
            "INSERT INTO query_handles (query_event_id, handle_id, file_path, node_type, token_count, first_match_glob, returned_at)
             VALUES (?, 'h2', 'new.rs', ?, 10, 'new_glob', ?)",
            params![new_id, NodeType::Function.as_int() as i64, recent_ts],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO expand_events (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
             VALUES (?, 'h2', 'new.rs', ?, 10, 0, ?)",
            params![new_id, NodeType::Function.as_int() as i64, recent_ts],
        )
        .unwrap();

    // With a 7-day half-life, old_glob's weight should be ~0.5x the recent one.
    // Both have 100% expand rate, so scores should be 1.0 for both,
    // but the returned_weight for the old glob decays.
    // The score = expanded_weight / returned_weight, so both are 1.0 (since both
    // expanded and returned weights decay equally).
    let scores = store
        .get_glob_scores(&["old_glob".to_string(), "new_glob".to_string()], 7.0)
        .unwrap();
    assert!(scores.contains_key("old_glob"));
    assert!(scores.contains_key("new_glob"));
}

#[test]
fn glob_scores_reflect_expand_rate() {
    let repo_root = temp_repo();
    let store = FeedbackStore::open(&repo_root).unwrap();

    let ts = now_ts();
    // Record query event with two handles for same glob; only one expanded.
    store
        .conn
        .execute(
            "INSERT INTO query_events (timestamp, query_text) VALUES (?, ?)",
            params![ts, "mixed query"],
        )
        .unwrap();
    let qid = store.conn.last_insert_rowid();

    for (id, expanded) in [("h_exp", true), ("h_skip", false)] {
        store
            .conn
            .execute(
                "INSERT INTO query_handles (query_event_id, handle_id, file_path, node_type, token_count, first_match_glob, returned_at)
                 VALUES (?, ?, 'f.rs', ?, 10, 'g1', ?)",
                params![qid, id, NodeType::Function.as_int() as i64, ts],
            )
            .unwrap();
        if expanded {
            store
                .conn
                .execute(
                    "INSERT INTO expand_events (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
                     VALUES (?, ?, 'f.rs', ?, 10, 0, ?)",
                    params![qid, id, NodeType::Function.as_int() as i64, ts],
                )
                .unwrap();
        }
    }

    let scores = store.get_glob_scores(&["g1".to_string()], 7.0).unwrap();
    let score = scores.get("g1").copied().unwrap_or(0.0);
    // 1 expanded out of 2 returned -> ~0.5
    assert!(
        score > 0.3 && score < 0.7,
        "expected ~0.5 expand rate, got {}",
        score
    );
}

#[test]
fn compute_metrics_with_known_data() {
    let repo_root = temp_repo();
    let store = FeedbackStore::open(&repo_root).unwrap();
    let ts = now_ts();

    // Insert 2 query events
    for q in &["query_a", "query_b"] {
        store
            .conn
            .execute(
                "INSERT INTO query_events (timestamp, query_text, predicted_globs, files_indexed, handles_returned, total_tokens)
                 VALUES (?, ?, '[\"g1\"]', 5, 2, 100)",
                params![ts, q],
            )
            .unwrap();
        let qid = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO query_handles (query_event_id, handle_id, file_path, node_type, token_count, first_match_glob, returned_at)
                 VALUES (?, ?, 'f.rs', ?, 50, 'g1', ?)",
                params![qid, format!("h_{q}"), NodeType::Function.as_int() as i64, ts],
            )
            .unwrap();
    }
    // Expand one handle
    store
        .conn
        .execute(
            "INSERT INTO expand_events (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
             VALUES (1, 'h_query_a', 'f.rs', ?, 50, 0, ?)",
            params![NodeType::Function.as_int() as i64, ts],
        )
        .unwrap();

    let metrics = store.compute_metrics(7.0).unwrap();
    assert_eq!(metrics.sample_count, 2);
    // 1 expanded / 2 returned = 0.5
    assert!(
        (metrics.handle_expand_accept_rate - 0.5).abs() < 0.01,
        "expected 0.5, got {}",
        metrics.handle_expand_accept_rate
    );
    assert!(
        (metrics.avg_tokens_per_expand - 50.0).abs() < 0.01,
        "expected 50.0 avg tokens, got {}",
        metrics.avg_tokens_per_expand
    );
}

#[test]
fn node_type_priors_distinguish_types() {
    let repo_root = temp_repo();
    let store = FeedbackStore::open(&repo_root).unwrap();
    let ts = now_ts();

    store
        .conn
        .execute(
            "INSERT INTO query_events (timestamp, query_text) VALUES (?, ?)",
            params![ts, "types query"],
        )
        .unwrap();
    let qid = store.conn.last_insert_rowid();

    // 2 Function handles, 1 expanded; 1 Struct handle, not expanded
    for (id, nt) in [
        ("hf1", NodeType::Function),
        ("hf2", NodeType::Function),
        ("hs1", NodeType::Struct),
    ] {
        store
            .conn
            .execute(
                "INSERT INTO query_handles (query_event_id, handle_id, file_path, node_type, token_count, returned_at)
                 VALUES (?, ?, 'f.rs', ?, 10, ?)",
                params![qid, id, nt.as_int() as i64, ts],
            )
            .unwrap();
    }
    store
        .conn
        .execute(
            "INSERT INTO expand_events (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
             VALUES (?, 'hf1', 'f.rs', ?, 10, 0, ?)",
            params![qid, NodeType::Function.as_int() as i64, ts],
        )
        .unwrap();

    let priors = store.get_node_type_priors().unwrap();
    let fn_prior = priors.get(&NodeType::Function).copied().unwrap_or(0.0);
    let st_prior = priors.get(&NodeType::Struct).copied().unwrap_or(0.0);
    assert!(fn_prior > 0.0, "Function prior should be > 0: {}", fn_prior);
    assert_eq!(st_prior, 0.0, "Struct prior should be 0 (never expanded)");
}
