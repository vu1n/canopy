//! Canopy MCP Server - MCP interface for token-efficient codebase queries

use canopy_core::{FileDiscovery, MatchMode, QueryParams, RepoIndex, Result as CanopyResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());

    // Check for CANOPY_ROOT env var first, then detect from current directory
    let repo_root = if let Ok(root) = std::env::var("CANOPY_ROOT") {
        PathBuf::from(root)
    } else {
        detect_repo_root().unwrap_or_else(|_| std::env::current_dir().unwrap())
    };

    let server = McpServer::new(repo_root);

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
    repo_root: PathBuf,
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

impl McpServer {
    fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
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
                                "description": "Repository path to index (defaults to auto-detected root)"
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
                    "description": "Query indexed content. Preferred: use individual params (pattern, symbol, section, glob). Fallback: use query param for s-expression DSL. Returns handles (references) with optional auto-expansion.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Repository path to query (defaults to auto-detected root)"
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
                        }
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
                                "description": "Repository path (defaults to auto-detected root)"
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
                                "description": "Repository path (defaults to auto-detected root)"
                            }
                        }
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
                                "description": "Repository path (defaults to auto-detected root)"
                            },
                            "glob": {
                                "type": "string",
                                "description": "Glob pattern to invalidate (all files if omitted)"
                            }
                        }
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
            _ => Err((-32602, format!("Unknown tool: {}", name))),
        }
    }

    fn tool_index(&self, args: &Value) -> Result<Value, (i32, String)> {
        let glob = args
            .get("glob")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing 'glob' parameter".to_string()))?;

        let repo_root = self.get_repo_root(args);
        let mut index = self.open_index_at(&repo_root)?;
        let stats = index
            .index(glob)
            .map_err(|e| (-32000, e.to_string()))?;

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
        let repo_root = self.get_repo_root(args);
        let mut index = self.open_index_at(&repo_root)?;

        // Auto-index if no files indexed yet
        let status = index.status().map_err(|e| (-32000, e.to_string()))?;
        if status.files_indexed == 0 {
            let default_glob = index.config().default_glob().to_string();
            index.index(&default_glob).map_err(|e| (-32000, e.to_string()))?;
        }

        // Check if DSL query is provided (fallback path)
        if let Some(query_str) = args.get("query").and_then(|v| v.as_str()) {
            let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
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
                .unwrap_or(5000)
        );

        // Validate that at least one search param is provided
        if params.pattern.is_none()
            && params.patterns.is_none()
            && params.symbol.is_none()
            && params.section.is_none()
        {
            return Err((
                -32602,
                "Must specify one of: pattern, patterns, symbol, section, or query".to_string(),
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

        let repo_root = self.get_repo_root(args);
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
        let repo_root = self.get_repo_root(args);
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

        let repo_root = self.get_repo_root(args);
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

    /// Open index at a specific path (with auto-init)
    fn open_index_at(&self, root: &Path) -> Result<RepoIndex, (i32, String)> {
        // Auto-init if .canopy doesn't exist
        if !root.join(".canopy").exists() {
            RepoIndex::init(root).map_err(|e| (-32000, e.to_string()))?;
        }
        RepoIndex::open(root).map_err(|e| (-32000, e.to_string()))
    }

    /// Get repo root from args, falling back to default
    fn get_repo_root(&self, args: &Value) -> PathBuf {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.repo_root.clone())
    }
}

fn detect_repo_root() -> CanopyResult<PathBuf> {
    let mut current = std::env::current_dir()?;
    loop {
        if current.join(".canopy").exists() || current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Ok(std::env::current_dir()?);
        }
    }
}
