//! Tool implementation methods for the MCP server.

use crate::protocol::McpError;
use crate::schema::DEFAULT_MCP_QUERY_LIMIT;
use crate::McpServer;

use canopy_client::predict::extract_query_text;
use canopy_client::IndexResult;
use canopy_core::feedback::FeedbackStore;
use canopy_core::{MatchMode, QueryParams, RepoIndex};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Wrap a text string into an MCP content response.
fn mcp_text(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }] })
}

/// Serialize a value to JSON text, then wrap as an MCP content response.
fn mcp_json<T: serde::Serialize>(val: &T) -> Result<Value, McpError> {
    let text = serde_json::to_string(val)
        .map_err(|e| McpError::Application(format!("Serialization error: {}", e)))?;
    Ok(mcp_text(text))
}

impl McpServer {
    pub(crate) fn tool_index(&mut self, args: &Value) -> Result<Value, McpError> {
        let glob = args
            .get("glob")
            .and_then(|v| v.as_str())
            .ok_or(McpError::InvalidParams(
                "Missing 'glob' parameter".to_string(),
            ))?;

        let repo_root = self.get_repo_root(args)?;
        let result = self.runtime.index(&repo_root, Some(glob))?;

        let result_json = match result {
            IndexResult::Local(stats) => {
                let mut val = serde_json::to_value(&stats)
                    .map_err(|e| McpError::Application(format!("Serialization error: {}", e)))?;
                if let Some(obj) = val.as_object_mut() {
                    obj.insert(
                        "repo_root".to_string(),
                        json!(repo_root.display().to_string()),
                    );
                }
                val
            }
            IndexResult::Service(resp) => {
                json!({
                    "generation": resp.generation,
                    "status": resp.status,
                    "commit_sha": resp.commit_sha,
                    "repo_root": repo_root.display().to_string(),
                })
            }
        };

        mcp_json(&result_json)
    }

    pub(crate) fn tool_query(&mut self, args: &Value) -> Result<Value, McpError> {
        let repo_root = self.get_repo_root(args)?;
        self.ensure_predictive_index(&repo_root, args)?;

        let params = build_query_params(args)?;
        let result = self.runtime.query(&repo_root, params)?;

        mcp_json(&result)
    }

    pub(crate) fn tool_evidence_pack(&mut self, args: &Value) -> Result<Value, McpError> {
        let repo_root = self.get_repo_root(args)?;
        self.ensure_predictive_index(&repo_root, args)?;

        let params = build_query_params(args)?;
        let max_handles = args
            .get("max_handles")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(8);
        let max_per_file = args
            .get("max_per_file")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(2);
        let plan = args.get("plan").and_then(|v| v.as_bool());

        let pack = self
            .runtime
            .evidence_pack(&repo_root, params, max_handles, max_per_file, plan)?;

        mcp_json(&pack)
    }

    pub(crate) fn tool_expand(&mut self, args: &Value) -> Result<Value, McpError> {
        let handle_ids: Vec<String> = args
            .get("handle_ids")
            .and_then(|v| v.as_array())
            .ok_or(McpError::InvalidParams(
                "Missing 'handle_ids' parameter".to_string(),
            ))?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        if handle_ids.is_empty() {
            return Err(McpError::InvalidParams(
                "Empty handle_ids array".to_string(),
            ));
        }

        let repo_root = self.get_repo_root(args)?;
        let outcome = self.runtime.expand(&repo_root, &handle_ids)?;

        // Format as readable text
        let mut text = outcome
            .contents
            .iter()
            .map(|(id, content)| format!("// {}\n{}", id, content))
            .collect::<Vec<_>>()
            .join("\n\n");

        // Include failed_ids in response
        if !outcome.failed_ids.is_empty() {
            text.push_str(&format!(
                "\n\n// Failed to expand: {}",
                outcome.failed_ids.join(", ")
            ));
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "failed_ids": outcome.failed_ids
        }))
    }

    pub(crate) fn tool_status(&self, args: &Value) -> Result<Value, McpError> {
        let repo_root = self.get_repo_root(args)?;
        let index = self.open_index_at(&repo_root)?;
        let status = index.status()?;

        let mut result = serde_json::to_value(&status)
            .map_err(|e| McpError::Application(format!("Serialization error: {}", e)))?;
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "repo_root".to_string(),
                json!(repo_root.display().to_string()),
            );
            obj.insert(
                "file_discovery".to_string(),
                json!(canopy_core::FileDiscovery::detect().name()),
            );
            if let Ok(feedback_store) = FeedbackStore::open(&repo_root) {
                if let Ok(metrics) = feedback_store.compute_metrics(7.0) {
                    obj.insert(
                        "feedback".to_string(),
                        json!({
                            "glob_hit_rate_at_k": metrics.glob_hit_rate_at_k,
                            "handle_expand_accept_rate": metrics.handle_expand_accept_rate,
                            "avg_tokens_per_expand": metrics.avg_tokens_per_expand,
                            "sample_count": metrics.sample_count,
                        }),
                    );
                }
            }
        }

        mcp_json(&result)
    }

    pub(crate) fn tool_invalidate(&self, args: &Value) -> Result<Value, McpError> {
        let glob = args.get("glob").and_then(|v| v.as_str());

        let repo_root = self.get_repo_root(args)?;
        let mut index = self.open_index_at(&repo_root)?;
        let count = index.invalidate(glob)?;

        Ok(mcp_text(format!("Invalidated {} files", count)))
    }

    pub(crate) fn tool_agent_readme(&self) -> Result<Value, McpError> {
        Ok(mcp_text(include_str!("../../AGENT-MCP.md")))
    }

    /// In standalone mode, run predictive indexing before a query so relevant files are indexed.
    pub(crate) fn ensure_predictive_index(
        &mut self,
        repo_root: &std::path::Path,
        args: &Value,
    ) -> Result<(), McpError> {
        if !self.runtime.is_service_mode() {
            let query_text = extract_query_text(args);
            let mut index = self.open_index_at(repo_root)?;
            self.runtime
                .predictive_index_for_query(repo_root, &mut index, &query_text)?;
        }
        Ok(())
    }

    /// Open index at a specific path (with auto-init).
    pub(crate) fn open_index_at(&self, root: &std::path::Path) -> Result<RepoIndex, McpError> {
        Ok(RepoIndex::open_or_init(root)?)
    }

    /// Get repo root from args (required parameter)
    pub(crate) fn get_repo_root(&self, args: &Value) -> Result<PathBuf, McpError> {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            return Ok(PathBuf::from(path));
        }
        if let Some(root) = &self.default_repo_root {
            return Ok(root.clone());
        }
        Err(McpError::InvalidParams(
            "Missing 'path' parameter and no default --root/CANOPY_ROOT configured".to_string(),
        ))
    }
}

/// Build [`QueryParams`] from MCP JSON-RPC arguments.
///
/// Supports two paths:
/// - `"query"` key → `QueryParams` with `dsl` field set
/// - Individual keys (`pattern`, `symbol`, etc.) → `QueryParams`
pub(crate) fn build_query_params(args: &Value) -> Result<QueryParams, McpError> {
    let mut params = QueryParams::new();

    // DSL query takes precedence
    if let Some(query_str) = args.get("query").and_then(|v| v.as_str()) {
        params.dsl = Some(query_str.to_string());
        params.limit = Some(
            args.get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(DEFAULT_MCP_QUERY_LIMIT),
        );
        return Ok(params);
    }

    if let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) {
        params.pattern = Some(pattern.to_string());
    } else if let Some(patterns_arr) = args.get("patterns").and_then(|v| v.as_array()) {
        let patterns: Vec<String> = patterns_arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !patterns.is_empty() {
            params.patterns = Some(patterns);
        }
    }

    if let Some(symbol) = args.get("symbol").and_then(|v| v.as_str()) {
        params.symbol = Some(symbol.to_string());
    }

    if let Some(section) = args.get("section").and_then(|v| v.as_str()) {
        params.section = Some(section.to_string());
    }

    if let Some(parent) = args.get("parent").and_then(|v| v.as_str()) {
        params.parent = Some(parent.to_string());
    }

    if let Some(kind) = args.get("kind").and_then(|v| v.as_str()) {
        params.kind = QueryParams::parse_kind(kind);
    }

    if let Some(glob) = args.get("glob").and_then(|v| v.as_str()) {
        params.glob = Some(glob.to_string());
    }

    if let Some(match_mode) = args.get("match").and_then(|v| v.as_str()) {
        params.match_mode = MatchMode::parse(match_mode);
    }

    params.limit = Some(
        args.get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MCP_QUERY_LIMIT),
    );

    if !params.has_search_target() {
        return Err(McpError::InvalidParams(
            "Must specify one of: pattern, patterns, symbol, section, parent, or query".to_string(),
        ));
    }

    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_query_params_pattern() {
        let args = json!({"pattern": "auth"});
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.pattern.as_deref(), Some("auth"));
        assert_eq!(p.limit, Some(DEFAULT_MCP_QUERY_LIMIT));
    }

    #[test]
    fn build_query_params_patterns_array() {
        let args = json!({"patterns": ["foo", "bar"]});
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.patterns, Some(vec!["foo".into(), "bar".into()]));
        assert!(p.pattern.is_none());
    }

    #[test]
    fn build_query_params_symbol() {
        let args = json!({"symbol": "Config"});
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.symbol.as_deref(), Some("Config"));
    }

    #[test]
    fn build_query_params_section() {
        let args = json!({"section": "imports"});
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.section.as_deref(), Some("imports"));
    }

    #[test]
    fn build_query_params_combined_fields() {
        let args = json!({
            "symbol": "Config",
            "glob": "src/**/*.rs",
            "kind": "function",
            "limit": 5
        });
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.symbol.as_deref(), Some("Config"));
        assert_eq!(p.glob.as_deref(), Some("src/**/*.rs"));
        assert_eq!(p.limit, Some(5));
    }

    #[test]
    fn build_query_params_dsl_query() {
        let args = json!({"query": "(and (pattern \"foo\") (symbol \"bar\"))"});
        let p = build_query_params(&args).unwrap();
        assert_eq!(
            p.dsl.as_deref(),
            Some("(and (pattern \"foo\") (symbol \"bar\"))")
        );
        assert_eq!(p.limit, Some(DEFAULT_MCP_QUERY_LIMIT));
    }

    #[test]
    fn build_query_params_dsl_with_custom_limit() {
        let args = json!({"query": "(pattern \"x\")", "limit": 3});
        let p = build_query_params(&args).unwrap();
        assert!(p.dsl.is_some());
        assert_eq!(p.limit, Some(3));
    }

    #[test]
    fn build_query_params_no_search_target_fails() {
        let args = json!({"limit": 10});
        match build_query_params(&args) {
            Err(McpError::InvalidParams(msg)) => {
                assert!(msg.contains("Must specify one of"));
            }
            Err(_) => panic!("expected InvalidParams"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_query_params_empty_object_fails() {
        let args = json!({});
        assert!(build_query_params(&args).is_err());
    }

    #[test]
    fn build_query_params_match_mode() {
        let args = json!({"pattern": "test", "match": "exact"});
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.match_mode, MatchMode::parse("exact"));
    }

    #[test]
    fn build_query_params_parent() {
        let args = json!({"parent": "MyClass"});
        let p = build_query_params(&args).unwrap();
        assert_eq!(p.parent.as_deref(), Some("MyClass"));
    }
}
