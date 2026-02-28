//! Query-driven path prediction for lazy indexing
//!
//! Prediction policy constants live here alongside the prediction logic.
//! Service-side evidence constants live in `canopy-service/src/evidence.rs`.

use canopy_core::feedback::FeedbackStore;

/// Repos with more files than this use predictive (scoped) indexing instead of full index.
pub const LARGE_REPO_THRESHOLD: usize = 1000;

/// Maximum files to index in a single predictive pass.
pub const MAX_PREDICTIVE_FILES: usize = 500;

/// Keyword to directory pattern mappings
/// Patterns use ** for recursive matching, will be combined with extensions
const KEYWORD_PATTERNS: &[(&[&str], &[&str])] = &[
    // Auth-related
    (
        &[
            "auth",
            "login",
            "logout",
            "session",
            "jwt",
            "oauth",
            "password",
            "credential",
        ],
        &[
            "**/auth/**",
            "**/login/**",
            "**/session/**",
            "**/authentication/**",
        ],
    ),
    // Database-related
    (
        &[
            "database",
            "db",
            "query",
            "sql",
            "orm",
            "repository",
            "migration",
        ],
        &[
            "**/db/**",
            "**/database/**",
            "**/repositories/**",
            "**/repo/**",
        ],
    ),
    // API-related
    (
        &[
            "api",
            "endpoint",
            "route",
            "controller",
            "handler",
            "rest",
            "graphql",
        ],
        &[
            "**/api/**",
            "**/routes/**",
            "**/controllers/**",
            "**/handlers/**",
            "**/endpoints/**",
        ],
    ),
    // Config-related
    (
        &["config", "configuration", "env", "settings", "options"],
        &["**/config/**", "**/settings/**", "**/conf/**"],
    ),
    // Middleware
    (
        &["middleware", "interceptor", "filter", "guard"],
        &[
            "**/middleware/**",
            "**/middlewares/**",
            "**/interceptors/**",
            "**/guards/**",
        ],
    ),
    // Workflow/execution
    (
        &[
            "workflow",
            "execution",
            "engine",
            "runner",
            "worker",
            "job",
            "queue",
        ],
        &[
            "**/workflow/**",
            "**/workflows/**",
            "**/execution/**",
            "**/engine/**",
            "**/workers/**",
            "**/jobs/**",
        ],
    ),
    // Core/shared
    (
        &["core", "shared", "common", "utils", "helpers", "lib"],
        &[
            "**/core/**",
            "**/shared/**",
            "**/common/**",
            "**/utils/**",
            "**/lib/**",
        ],
    ),
    // Service layer
    (
        &["service", "services"],
        &["**/services/**", "**/service/**"],
    ),
];

/// Predict glob patterns based on query keywords
/// Returns patterns like "**/auth/**/*.ts" ready for walk_files()
pub fn predict_globs(query: &str, extensions: &[String]) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let mut globs = Vec::new();
    let mut matched_any = false;

    // Match keywords to directory patterns
    for (keywords, patterns) in KEYWORD_PATTERNS {
        if keywords.iter().any(|k| query_lower.contains(k)) {
            matched_any = true;
            for pattern in *patterns {
                for ext in extensions {
                    // Pattern like "**/auth/**" + "ts" -> "**/auth/**/*.ts"
                    globs.push(format!("{}/*.{}", pattern, ext));
                }
            }
        }
    }

    // Always include entry points (main, index, app, server)
    for ext in extensions {
        globs.push(format!("**/main.{}", ext));
        globs.push(format!("**/index.{}", ext));
        globs.push(format!("**/app.{}", ext));
        globs.push(format!("**/server.{}", ext));
    }

    // If no keywords matched, fall back to src/** only
    if !matched_any {
        for ext in extensions {
            globs.push(format!("src/**/*.{}", ext));
            globs.push(format!("packages/**/*.{}", ext));
        }
    }

    globs.sort();
    globs.dedup();
    globs
}

/// Predict glob patterns and rerank using feedback-derived glob scores.
///
/// Falls back to static order if score lookup fails or no feedback exists.
pub fn predict_globs_with_feedback(
    query: &str,
    extensions: &[String],
    feedback: &FeedbackStore,
) -> Vec<String> {
    let mut globs = predict_globs(query, extensions);
    let base_order: std::collections::HashMap<String, usize> = globs
        .iter()
        .enumerate()
        .map(|(idx, g)| (g.clone(), idx))
        .collect();

    let scores = match feedback.get_glob_scores(&globs, 7.0) {
        Ok(s) => s,
        Err(_) => return globs,
    };

    if scores.is_empty() {
        return globs;
    }

    globs.sort_by(|a, b| {
        let sa = scores.get(a).copied().unwrap_or(0.0);
        let sb = scores.get(b).copied().unwrap_or(0.0);
        sb.total_cmp(&sa).then_with(|| {
            let ia = base_order.get(a).copied().unwrap_or(usize::MAX);
            let ib = base_order.get(b).copied().unwrap_or(usize::MAX);
            ia.cmp(&ib)
        })
    });

    globs
}

/// Extract extensions from a glob pattern like "**/*.{ts,tsx,js}"
pub fn extract_extensions_from_glob(glob: &str) -> Vec<String> {
    // Look for {ext1,ext2} or *.ext patterns
    if let Some(brace_start) = glob.find('{') {
        if let Some(brace_end) = glob.find('}') {
            let exts = &glob[brace_start + 1..brace_end];
            return exts.split(',').map(|s| s.trim().to_string()).collect();
        }
    }
    // Single extension: **/*.ts
    if let Some(dot_pos) = glob.rfind("*.") {
        let ext = &glob[dot_pos + 2..];
        if !ext.contains('/') && !ext.contains('*') {
            return vec![ext.to_string()];
        }
    }
    // Default fallback
    vec![
        "ts".to_string(),
        "js".to_string(),
        "py".to_string(),
        "rs".to_string(),
    ]
}

/// Extract query text from MCP args for prediction
pub fn extract_query_text(args: &serde_json::Value) -> String {
    // Try multiple fields that might contain searchable text
    let fields = ["pattern", "symbol", "section", "query"];
    for field in fields {
        if let Some(val) = args.get(field).and_then(|v| v.as_str()) {
            if !val.is_empty() {
                return val.to_string();
            }
        }
    }
    // Also check patterns array
    if let Some(patterns) = args.get("patterns").and_then(|v| v.as_array()) {
        let texts: Vec<&str> = patterns.iter().filter_map(|v| v.as_str()).collect();
        if !texts.is_empty() {
            return texts.join(" ");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use canopy_core::feedback::{ExpandEvent, QueryEvent, QueryHandle};
    use canopy_core::NodeType;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo() -> std::path::PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("canopy-predict-test-{ts}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn test_predict_auth_query() {
        let globs = predict_globs("How does authentication work?", &["ts".to_string()]);
        assert!(globs.iter().any(|g| g.contains("auth")));
        assert!(globs.iter().any(|g| g.contains("login")));
    }

    #[test]
    fn test_predict_database_query() {
        let globs = predict_globs("database query optimization", &["ts".to_string()]);
        assert!(globs
            .iter()
            .any(|g| g.contains("db") || g.contains("database")));
    }

    #[test]
    fn test_fallback_on_no_keywords() {
        let globs = predict_globs("explain this", &["ts".to_string()]);
        // Should include src/** and entry points
        assert!(globs.iter().any(|g| g.contains("src/")));
        assert!(globs.iter().any(|g| g.contains("main.ts")));
    }

    #[test]
    fn test_extract_extensions() {
        assert_eq!(
            extract_extensions_from_glob("**/*.{ts,tsx}"),
            vec!["ts", "tsx"]
        );
        assert_eq!(extract_extensions_from_glob("**/*.rs"), vec!["rs"]);
    }

    #[test]
    fn test_extract_query_text_pattern() {
        let args = serde_json::json!({ "pattern": "authentication" });
        assert_eq!(extract_query_text(&args), "authentication");
    }

    #[test]
    fn test_extract_query_text_patterns() {
        let args = serde_json::json!({ "patterns": ["auth", "login"] });
        assert_eq!(extract_query_text(&args), "auth login");
    }

    #[test]
    fn test_extract_query_text_empty() {
        let args = serde_json::json!({});
        assert_eq!(extract_query_text(&args), "");
    }

    #[test]
    fn test_predict_with_feedback_reranks() {
        let repo_root = temp_repo();
        let store = FeedbackStore::open(&repo_root).unwrap();

        let good_glob = "**/auth/**/*.rs".to_string();
        let bad_glob = "**/db/**/*.rs".to_string();

        // Seed strong signal for auth glob
        for i in 0..3 {
            let event_id = store
                .record_query_event(&QueryEvent {
                    query_text: "auth db".to_string(),
                    predicted_globs: Some(vec![good_glob.clone(), bad_glob.clone()]),
                    files_indexed: 1,
                    handles_returned: 1,
                    total_tokens: 50,
                })
                .unwrap();

            let handle_id = format!("hgood{i}");
            store
                .record_query_handles(
                    event_id,
                    &[QueryHandle {
                        handle_id: handle_id.clone(),
                        file_path: format!("src/auth/{i}.rs"),
                        node_type: NodeType::Function,
                        token_count: 50,
                        first_match_glob: Some(good_glob.clone()),
                    }],
                )
                .unwrap();
            store
                .record_expand_event(&ExpandEvent {
                    query_event_id: Some(event_id),
                    handle_id,
                    file_path: format!("src/auth/{i}.rs"),
                    node_type: NodeType::Function,
                    token_count: 50,
                    auto_expanded: false,
                })
                .unwrap();
        }

        // Seed non-expanded signal for db glob
        for i in 0..3 {
            let event_id = store
                .record_query_event(&QueryEvent {
                    query_text: "auth db".to_string(),
                    predicted_globs: Some(vec![good_glob.clone(), bad_glob.clone()]),
                    files_indexed: 1,
                    handles_returned: 1,
                    total_tokens: 50,
                })
                .unwrap();

            store
                .record_query_handles(
                    event_id,
                    &[QueryHandle {
                        handle_id: format!("hbad{i}"),
                        file_path: format!("src/db/{i}.rs"),
                        node_type: NodeType::Function,
                        token_count: 50,
                        first_match_glob: Some(bad_glob.clone()),
                    }],
                )
                .unwrap();
        }

        let reranked = predict_globs_with_feedback("auth db", &["rs".to_string()], &store);

        let good_pos = reranked
            .iter()
            .position(|g| g == &good_glob)
            .unwrap_or(usize::MAX);
        let bad_pos = reranked
            .iter()
            .position(|g| g == &bad_glob)
            .unwrap_or(usize::MAX);
        assert!(good_pos < bad_pos);
    }
}
