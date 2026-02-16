//! Canopy MCP Server - MCP interface for token-efficient codebase queries

use canopy_client::predict::extract_query_text;
use canopy_client::{ClientRuntime, IndexResult, QueryInput, StandalonePolicy};
use canopy_core::feedback::FeedbackStore;
use canopy_core::{MatchMode, QueryKind, QueryParams, RepoIndex};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

const DEFAULT_MCP_QUERY_LIMIT: usize = 16;
const DEFAULT_MCP_EXPAND_BUDGET: usize = 0;

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());

    // Parse --service-url from CLI args (falls back to CANOPY_SERVICE_URL env var)
    let service_url = parse_service_url();
    let default_repo_root = parse_root_path();
    let mut server = McpServer::with_service_url(service_url, default_repo_root);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.is_empty() {
            continue;
        }

        let response = server.handle_request(&line);
        if let Some(resp) = response {
            let _ = writeln!(stdout, "{}", resp);
            let _ = stdout.flush();
        }
    }
}

struct McpServer {
    runtime: ClientRuntime,
    default_repo_root: Option<PathBuf>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

/// Parse --service-url from CLI args, falling back to CANOPY_SERVICE_URL env var
fn parse_service_url() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--service-url" {
            return args.get(i + 1).cloned();
        }
        if let Some(val) = args[i].strip_prefix("--service-url=") {
            return Some(val.to_string());
        }
    }
    std::env::var("CANOPY_SERVICE_URL").ok()
}

/// Parse --root from CLI args, falling back to CANOPY_ROOT env var
fn parse_root_path() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--root" {
            if let Some(path) = args.get(i + 1) {
                return Some(PathBuf::from(path));
            }
        }
        if let Some(val) = args[i].strip_prefix("--root=") {
            return Some(PathBuf::from(val));
        }
    }
    std::env::var("CANOPY_ROOT").ok().map(PathBuf::from)
}

impl McpServer {
    fn with_service_url(service_url: Option<String>, default_repo_root: Option<PathBuf>) -> Self {
        Self {
            runtime: ClientRuntime::new(service_url.as_deref(), StandalonePolicy::Predictive),
            default_repo_root,
        }
    }

    fn handle_request(&mut self, line: &str) -> Option<String> {
        let req: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(
                    json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("Parse error: {}", e) }
                    })
                    .to_string(),
                );
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        let result = match req.method.as_str() {
            "initialize" => self.handle_initialize(&req.params),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => self.handle_tools_call(&req.params),
            "notifications/initialized" => return None, // No response for notifications
            _ => Err((-32601, format!("Method not found: {}", req.method))),
        };

        let response = match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(value),
                error: None,
            },
            Err((code, message)) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(JsonRpcError { code, message }),
            },
        };

        Some(serde_json::to_string(&response).unwrap())
    }

    fn handle_initialize(&self, _params: &Option<Value>) -> Result<Value, (i32, String)> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "canopy-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    fn handle_tools_list(&self) -> Result<Value, (i32, String)> {
        Ok(json!({
            "tools": [
                {
                    "name": "canopy_index",
                    "description": "Index files matching glob pattern for efficient querying",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path to index (optional if --root or CANOPY_ROOT is set)"
                            },
                            "glob": {
                                "type": "string",
                                "description": "Glob pattern for files to index (e.g., '**/*.rs')"
                            }
                        },
                        "required": ["glob"]
                    }
                },
                {
                    "name": "canopy_query",
                    "description": "Query indexed content. Use canopy tools as the primary code-search interface (instead of find/grep/rg) when available. Preferred: use individual params (pattern, symbol, section, glob). Fallback: use query param for s-expression DSL. Returns handles (or ref_handles when kind=reference) with optional auto-expansion.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path to query (optional if --root or CANOPY_ROOT is set)"
                            },
                            "pattern": {
                                "type": "string",
                                "description": "Single text pattern to search (FTS5 search)"
                            },
                            "patterns": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Multiple text patterns to search"
                            },
                            "symbol": {
                                "type": "string",
                                "description": "Code symbol search (function, class, struct, method)"
                            },
                            "section": {
                                "type": "string",
                                "description": "Section heading search (markdown sections)"
                            },
                            "parent": {
                                "type": "string",
                                "description": "Filter by parent symbol (e.g., class name for methods)"
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["definition", "reference", "any"],
                                "description": "Query kind: 'definition' for exact symbol match, 'reference' for usages, 'any' (default)"
                            },
                            "glob": {
                                "type": "string",
                                "description": "File glob filter (e.g., 'src/**/*.rs')"
                            },
                            "match": {
                                "type": "string",
                                "enum": ["any", "all"],
                                "description": "Match mode for multi-pattern: 'any' (OR, default) or 'all' (AND)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results (default: 16)"
                            },
                            "expand_budget": {
                                "type": "integer",
                                "description": "[Deprecated] Auto-expand results if total tokens fit within budget (default: 0, disabled)"
                            },
                            "query": {
                                "type": "string",
                                "description": "[Fallback] S-expression DSL query. Use params above instead. Examples: (grep \"TODO\"), (section \"auth\"), (in-file \"src/*.rs\" (grep \"error\"))"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "canopy_evidence_pack",
                    "description": "Build a compact, ranked evidence set for a task (no snippets). Preferred first step for discovery. Returns guidance.stop_querying/recommended_action/next_step so agents know when to stop querying and start writing.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path to query (optional if --root or CANOPY_ROOT is set)"
                            },
                            "pattern": {
                                "type": "string",
                                "description": "Single text pattern to search (FTS5 search)"
                            },
                            "patterns": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Multiple text patterns to search"
                            },
                            "symbol": {
                                "type": "string",
                                "description": "Code symbol search (function, class, struct, method)"
                            },
                            "section": {
                                "type": "string",
                                "description": "Section heading search (markdown sections)"
                            },
                            "parent": {
                                "type": "string",
                                "description": "Filter by parent symbol (e.g., class name for methods)"
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["definition", "reference", "any"],
                                "description": "Query kind: 'definition' for exact symbol match, 'reference' for usages, 'any' (default)"
                            },
                            "glob": {
                                "type": "string",
                                "description": "File glob filter (e.g., 'src/**/*.rs')"
                            },
                            "match": {
                                "type": "string",
                                "enum": ["any", "all"],
                                "description": "Match mode for multi-pattern: 'any' (OR, default) or 'all' (AND)"
                            },
                            "query": {
                                "type": "string",
                                "description": "[Fallback] S-expression DSL query."
                            },
                            "max_handles": {
                                "type": "integer",
                                "description": "Maximum ranked handles in evidence pack (default: 8)"
                            },
                            "max_per_file": {
                                "type": "integer",
                                "description": "Maximum handles selected from a single file (default: 2)"
                            },
                            "plan": {
                                "type": "boolean",
                                "description": "Override server-side recursive evidence planning (default: auto: only when confidence is low)"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "canopy_expand",
                    "description": "Expand handles to full content. Use after canopy_query to retrieve actual content for specific handles.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path (optional if --root or CANOPY_ROOT is set)"
                            },
                            "handle_ids": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Handle IDs to expand (e.g., ['h1a2b3c4d5e6', 'h5d6e7f8a9b0'])"
                            }
                        },
                        "required": ["handle_ids"]
                    }
                },
                {
                    "name": "canopy_status",
                    "description": "Get index status including file count, token count, and last indexed time",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path (optional if --root or CANOPY_ROOT is set)"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "canopy_invalidate",
                    "description": "Force reindex of files matching glob pattern",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path (optional if --root or CANOPY_ROOT is set)"
                            },
                            "glob": {
                                "type": "string",
                                "description": "Glob pattern to invalidate (all files if omitted)"
                            }
                        },
                        "required": []
                    }
                },
                {
                    "name": "canopy_agent_readme",
                    "description": "Returns optional usage instructions for canopy MCP tools.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        }))
    }

    fn handle_tools_call(&mut self, params: &Option<Value>) -> Result<Value, (i32, String)> {
        let params = params
            .as_ref()
            .ok_or((-32602, "Missing params".to_string()))?;

        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing tool name".to_string()))?;

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        match name {
            "canopy_index" => self.tool_index(&arguments),
            "canopy_query" => self.tool_query(&arguments),
            "canopy_evidence_pack" => self.tool_evidence_pack(&arguments),
            "canopy_expand" => self.tool_expand(&arguments),
            "canopy_status" => self.tool_status(&arguments),
            "canopy_invalidate" => self.tool_invalidate(&arguments),
            "canopy_agent_readme" => self.tool_agent_readme(),
            _ => Err((-32602, format!("Unknown tool: {}", name))),
        }
    }

    fn tool_index(&mut self, args: &Value) -> Result<Value, (i32, String)> {
        let glob = args
            .get("glob")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing 'glob' parameter".to_string()))?;

        let repo_root = self.get_repo_root(args)?;
        let result = self
            .runtime
            .index(&repo_root, Some(glob))
            .map_err(|e| (-32000, e.to_string()))?;

        let result_json = match result {
            IndexResult::Local(stats) => {
                let mut val = serde_json::to_value(&stats).unwrap();
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

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&result_json).unwrap()
            }]
        }))
    }

    fn tool_query(&mut self, args: &Value) -> Result<Value, (i32, String)> {
        let repo_root = self.get_repo_root(args)?;

        // In standalone predictive mode, do predictive indexing before query
        if !self.runtime.is_service_mode() {
            let query_text = extract_query_text(args);
            let mut index = self.open_index_at(&repo_root)?;
            self.runtime
                .predictive_index_for_query(&repo_root, &mut index, &query_text)
                .map_err(|e| (-32000, e.to_string()))?;
        }

        // Build QueryInput from MCP args
        let input = build_query_input(args)?;

        let result = self
            .runtime
            .query(&repo_root, input)
            .map_err(|e| (-32000, e.to_string()))?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&result).unwrap()
            }]
        }))
    }

    fn tool_evidence_pack(&mut self, args: &Value) -> Result<Value, (i32, String)> {
        let repo_root = self.get_repo_root(args)?;

        // In standalone predictive mode, do predictive indexing before query.
        if !self.runtime.is_service_mode() {
            let query_text = extract_query_text(args);
            let mut index = self.open_index_at(&repo_root)?;
            self.runtime
                .predictive_index_for_query(&repo_root, &mut index, &query_text)
                .map_err(|e| (-32000, e.to_string()))?;
        }

        let input = build_query_input(args)?;
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
            .evidence_pack(&repo_root, input, max_handles, max_per_file, plan)
            .map_err(|e| (-32000, e.to_string()))?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&pack).unwrap()
            }]
        }))
    }

    fn tool_expand(&mut self, args: &Value) -> Result<Value, (i32, String)> {
        let handle_ids: Vec<String> = args
            .get("handle_ids")
            .and_then(|v| v.as_array())
            .ok_or((-32602, "Missing 'handle_ids' parameter".to_string()))?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        if handle_ids.is_empty() {
            return Err((-32602, "Empty handle_ids array".to_string()));
        }

        let repo_root = self.get_repo_root(args)?;
        let outcome = self
            .runtime
            .expand(&repo_root, &handle_ids)
            .map_err(|e| (-32000, e.to_string()))?;

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

    fn tool_status(&self, args: &Value) -> Result<Value, (i32, String)> {
        let repo_root = self.get_repo_root(args)?;
        let index = self.open_index_at(&repo_root)?;
        let status = index.status().map_err(|e| (-32000, e.to_string()))?;

        let mut result = serde_json::to_value(&status).unwrap();
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

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&result).unwrap()
            }]
        }))
    }

    fn tool_invalidate(&self, args: &Value) -> Result<Value, (i32, String)> {
        let glob = args.get("glob").and_then(|v| v.as_str());

        let repo_root = self.get_repo_root(args)?;
        let mut index = self.open_index_at(&repo_root)?;
        let count = index
            .invalidate(glob)
            .map_err(|e| (-32000, e.to_string()))?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": format!("Invalidated {} files", count)
            }]
        }))
    }

    fn tool_agent_readme(&self) -> Result<Value, (i32, String)> {
        let readme = include_str!("../../AGENT-MCP.md");
        Ok(json!({
            "content": [{
                "type": "text",
                "text": readme
            }]
        }))
    }

    /// Open index at a specific path (with auto-init)
    fn open_index_at(&self, root: &std::path::Path) -> Result<RepoIndex, (i32, String)> {
        if !root.join(".canopy").exists() {
            RepoIndex::init(root).map_err(|e| (-32000, e.to_string()))?;
        }
        RepoIndex::open(root).map_err(|e| (-32000, e.to_string()))
    }

    /// Get repo root from args (required parameter)
    fn get_repo_root(&self, args: &Value) -> Result<PathBuf, (i32, String)> {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            return Ok(PathBuf::from(path));
        }
        if let Some(root) = &self.default_repo_root {
            return Ok(root.clone());
        }
        Err((
            -32602,
            "Missing 'path' parameter and no default --root/CANOPY_ROOT configured".to_string(),
        ))
    }
}

/// Build QueryInput from MCP JSON-RPC arguments
fn build_query_input(args: &Value) -> Result<QueryInput, (i32, String)> {
    // Check if DSL query is provided (fallback path)
    if let Some(query_str) = args.get("query").and_then(|v| v.as_str()) {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or(Some(DEFAULT_MCP_QUERY_LIMIT));
        return Ok(QueryInput::Dsl(
            query_str.to_string(),
            canopy_core::QueryOptions {
                limit,
                expand_budget: None,
                node_type_priors: None,
            },
        ));
    }

    // Build QueryParams from individual params
    let mut params = QueryParams::new();

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
        params.kind = match kind {
            "definition" => QueryKind::Definition,
            "reference" => QueryKind::Reference,
            _ => QueryKind::Any,
        };
    }

    if let Some(glob) = args.get("glob").and_then(|v| v.as_str()) {
        params.glob = Some(glob.to_string());
    }

    if let Some(match_mode) = args.get("match").and_then(|v| v.as_str()) {
        params.match_mode = match match_mode {
            "all" => MatchMode::All,
            _ => MatchMode::Any,
        };
    }

    params.limit = Some(
        args.get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MCP_QUERY_LIMIT),
    );

    // Set expand_budget (default disabled unless explicitly set)
    params.expand_budget = Some(
        args.get("expand_budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MCP_EXPAND_BUDGET),
    );

    // Validate that at least one search param is provided
    if params.pattern.is_none()
        && params.patterns.is_none()
        && params.symbol.is_none()
        && params.section.is_none()
        && params.parent.is_none()
    {
        return Err((
            -32602,
            "Must specify one of: pattern, patterns, symbol, section, parent, or query".to_string(),
        ));
    }

    Ok(QueryInput::Params(params))
}
