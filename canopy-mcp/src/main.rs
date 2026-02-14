//! Canopy MCP Server - MCP interface for token-efficient codebase queries

mod predict;

use canopy_core::{FileDiscovery, MatchMode, QueryKind, QueryParams, RepoIndex};
use predict::{extract_extensions_from_glob, extract_query_text, predict_globs};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());

    let server = McpServer::new();

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

struct McpServer;

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

impl McpServer {
    fn new() -> Self {
        Self
    }

    fn handle_request(&self, line: &str) -> Option<String> {
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
                                "description": "Repository path to index (e.g., '/path/to/repo')"
                            },
                            "glob": {
                                "type": "string",
                                "description": "Glob pattern for files to index (e.g., '**/*.rs')"
                            }
                        },
                        "required": ["path", "glob"]
                    }
                },
                {
                    "name": "canopy_query",
                    "description": "Query indexed content. Preferred: use individual params (pattern, symbol, section, glob). Fallback: use query param for s-expression DSL. Returns handles (or ref_handles when kind=reference) with optional auto-expansion.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path to query (e.g., '/path/to/repo')"
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
                                "description": "Maximum number of results (default: 100)"
                            },
                            "expand_budget": {
                                "type": "integer",
                                "description": "Auto-expand results if total tokens fit within budget (default: 5000)"
                            },
                            "query": {
                                "type": "string",
                                "description": "[Fallback] S-expression DSL query. Use params above instead. Examples: (grep \"TODO\"), (section \"auth\"), (in-file \"src/*.rs\" (grep \"error\"))"
                            }
                        },
                        "required": ["path"]
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
                                "description": "Repository path (e.g., '/path/to/repo')"
                            },
                            "handle_ids": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Handle IDs to expand (e.g., ['h1a2b3c4d5e6', 'h5d6e7f8a9b0'])"
                            }
                        },
                        "required": ["path", "handle_ids"]
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
                                "description": "Repository path (e.g., '/path/to/repo')"
                            }
                        },
                        "required": ["path"]
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
                                "description": "Repository path (e.g., '/path/to/repo')"
                            },
                            "glob": {
                                "type": "string",
                                "description": "Glob pattern to invalidate (all files if omitted)"
                            }
                        },
                        "required": ["path"]
                    }
                },
                {
                    "name": "canopy_agent_readme",
                    "description": "Returns usage instructions for AI agents using canopy MCP tools. Call this if unfamiliar with canopy to learn the query workflow, available parameters, response formats, and best practices.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        }))
    }

    fn handle_tools_call(&self, params: &Option<Value>) -> Result<Value, (i32, String)> {
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
            "canopy_expand" => self.tool_expand(&arguments),
            "canopy_status" => self.tool_status(&arguments),
            "canopy_invalidate" => self.tool_invalidate(&arguments),
            "canopy_agent_readme" => self.tool_agent_readme(),
            _ => Err((-32602, format!("Unknown tool: {}", name))),
        }
    }

    fn tool_index(&self, args: &Value) -> Result<Value, (i32, String)> {
        let glob = args
            .get("glob")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing 'glob' parameter".to_string()))?;

        let repo_root = self.get_repo_root(args)?;
        let mut index = self.open_index_at(&repo_root)?;
        let stats = index.index(glob).map_err(|e| (-32000, e.to_string()))?;

        // Include repo_root for debugging
        let mut result = serde_json::to_value(&stats).unwrap();
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "repo_root".to_string(),
                json!(repo_root.display().to_string()),
            );
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&result).unwrap()
            }]
        }))
    }

    fn tool_query(&self, args: &Value) -> Result<Value, (i32, String)> {
        let repo_root = self.get_repo_root(args)?;
        let mut index = self.open_index_at(&repo_root)?;

        // Auto-index: use predictive indexing for large repos
        let default_glob = index.config().default_glob().to_string();
        let status = index.status().map_err(|e| (-32000, e.to_string()))?;

        // Count files to determine if this is a large repo
        // Only count once (when no files indexed yet) to avoid repeated walks
        const LARGE_REPO_THRESHOLD: usize = 1000;
        const MAX_PREDICTIVE_FILES: usize = 500;

        let is_large_repo = if status.files_indexed == 0 {
            let all_files = index.walk_files(&default_glob).unwrap_or_default();
            all_files.len() > LARGE_REPO_THRESHOLD
        } else {
            // If already indexed, check if we indexed less than full repo would have
            // (heuristic: if indexed < threshold, assume it was predictive)
            status.files_indexed < LARGE_REPO_THRESHOLD
        };

        if status.files_indexed == 0 && !is_large_repo {
            // Small repo with no index: full index is fine
            index
                .index(&default_glob)
                .map_err(|e| (-32000, e.to_string()))?;
        } else if is_large_repo {
            // Large repo: always run predictive indexing
            // needs_reindex() will skip already-indexed files, so this is safe
            // for multi-agent fan-out where each agent has different queries
            let query_text = extract_query_text(args);
            let extensions = extract_extensions_from_glob(&default_glob);
            let predicted_globs = predict_globs(&query_text, &extensions);

            eprintln!(
                "[canopy] Large repo, predictive indexing for: {:?}",
                predicted_globs.iter().take(5).collect::<Vec<_>>()
            );

            // Index predicted globs until we hit file cap
            let mut total_indexed = 0;
            for glob in &predicted_globs {
                if total_indexed >= MAX_PREDICTIVE_FILES {
                    break;
                }
                match index.index(glob) {
                    Ok(stats) => {
                        total_indexed += stats.files_indexed;
                    }
                    Err(_e) => {
                        // Glob might not match any files, that's ok
                    }
                }
            }

            if total_indexed > 0 {
                eprintln!("[canopy] Predictively indexed {} new files", total_indexed);
            }

            // If prediction found nothing AND no files exist, fall back to entry points
            let current_status = index.status().map_err(|e| (-32000, e.to_string()))?;
            if current_status.files_indexed == 0 {
                eprintln!("[canopy] No files indexed, adding entry points");
                let _ = index.index("**/main.*");
                let _ = index.index("**/index.*");
                let _ = index.index("**/app.*");
            }
        }

        // Check if DSL query is provided (fallback path)
        if let Some(query_str) = args.get("query").and_then(|v| v.as_str()) {
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let result = index
                .query(query_str, limit)
                .map_err(|e| (-32000, e.to_string()))?;

            return Ok(json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&result).unwrap()
                }]
            }));
        }

        // Build QueryParams from individual params
        let mut params = QueryParams::new();

        // Set pattern or patterns
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

        // Set symbol
        if let Some(symbol) = args.get("symbol").and_then(|v| v.as_str()) {
            params.symbol = Some(symbol.to_string());
        }

        // Set section
        if let Some(section) = args.get("section").and_then(|v| v.as_str()) {
            params.section = Some(section.to_string());
        }

        // Set parent filter
        if let Some(parent) = args.get("parent").and_then(|v| v.as_str()) {
            params.parent = Some(parent.to_string());
        }

        // Set kind
        if let Some(kind) = args.get("kind").and_then(|v| v.as_str()) {
            params.kind = match kind {
                "definition" => QueryKind::Definition,
                "reference" => QueryKind::Reference,
                _ => QueryKind::Any,
            };
        }

        // Set glob filter
        if let Some(glob) = args.get("glob").and_then(|v| v.as_str()) {
            params.glob = Some(glob.to_string());
        }

        // Set match mode
        if let Some(match_mode) = args.get("match").and_then(|v| v.as_str()) {
            params.match_mode = match match_mode {
                "all" => MatchMode::All,
                _ => MatchMode::Any,
            };
        }

        // Set limit
        if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
            params.limit = Some(limit as usize);
        }

        // Set expand_budget (default to 5000 if not specified)
        params.expand_budget = Some(
            args.get("expand_budget")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(5000),
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
                "Must specify one of: pattern, patterns, symbol, section, parent, or query"
                    .to_string(),
            ));
        }

        let result = index
            .query_params(params)
            .map_err(|e| (-32000, e.to_string()))?;

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&result).unwrap()
            }]
        }))
    }

    fn tool_expand(&self, args: &Value) -> Result<Value, (i32, String)> {
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
        let index = self.open_index_at(&repo_root)?;
        let contents = index
            .expand(&handle_ids)
            .map_err(|e| (-32000, e.to_string()))?;

        // Format as readable text
        let text = contents
            .iter()
            .map(|(id, content)| format!("// {}\n{}", id, content))
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(json!({
            "content": [{
                "type": "text",
                "text": text
            }]
        }))
    }

    fn tool_status(&self, args: &Value) -> Result<Value, (i32, String)> {
        let repo_root = self.get_repo_root(args)?;
        let index = self.open_index_at(&repo_root)?;
        let status = index.status().map_err(|e| (-32000, e.to_string()))?;

        // Include repo_root and file_discovery for debugging
        let mut result = serde_json::to_value(&status).unwrap();
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "repo_root".to_string(),
                json!(repo_root.display().to_string()),
            );
            obj.insert(
                "file_discovery".to_string(),
                json!(FileDiscovery::detect().name()),
            );
        }

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&result).unwrap()
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
    fn open_index_at(&self, root: &Path) -> Result<RepoIndex, (i32, String)> {
        // Auto-init if .canopy doesn't exist
        if !root.join(".canopy").exists() {
            RepoIndex::init(root).map_err(|e| (-32000, e.to_string()))?;
        }
        RepoIndex::open(root).map_err(|e| (-32000, e.to_string()))
    }

    /// Get repo root from args (required parameter)
    fn get_repo_root(&self, args: &Value) -> Result<PathBuf, (i32, String)> {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .ok_or_else(|| (-32602, "Missing required 'path' parameter".to_string()))
    }
}
