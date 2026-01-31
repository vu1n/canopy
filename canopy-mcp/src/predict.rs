//! Query-driven path prediction for lazy indexing

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
}
