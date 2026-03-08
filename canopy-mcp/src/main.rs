//! Canopy MCP Server - MCP interface for token-efficient codebase queries

mod protocol;
mod schema;
mod tools;

use canopy_client::ClientRuntime;
use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpError};
use schema::{query_input_schema, query_param_properties};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());

    // Parse --service-url from CLI args (falls back to CANOPY_SERVICE_URL env var)
    let service_url = parse_service_url();
    let api_key = parse_api_key();
    let default_repo_root = parse_root_path();
    let mut server = McpServer::with_service_url(service_url, api_key, default_repo_root);

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

pub(crate) struct McpServer {
    pub(crate) runtime: ClientRuntime,
    pub(crate) default_repo_root: Option<PathBuf>,
}

/// Parse a CLI argument by flag name, falling back to an environment variable.
fn parse_arg(flag: &str, env_var: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let prefix = format!("{flag}=");
    for i in 0..args.len() {
        if args[i] == flag {
            return args.get(i + 1).cloned();
        }
        if let Some(val) = args[i].strip_prefix(&prefix) {
            return Some(val.to_string());
        }
    }
    std::env::var(env_var).ok()
}

fn parse_service_url() -> Option<String> {
    parse_arg("--service-url", "CANOPY_SERVICE_URL")
}

fn parse_root_path() -> Option<PathBuf> {
    parse_arg("--root", "CANOPY_ROOT").map(PathBuf::from)
}

fn parse_api_key() -> Option<String> {
    parse_arg("--api-key", "CANOPY_API_KEY")
}

impl McpServer {
    fn with_service_url(
        service_url: Option<String>,
        api_key: Option<String>,
        default_repo_root: Option<PathBuf>,
    ) -> Self {
        Self {
            runtime: ClientRuntime::new(service_url.as_deref(), api_key),
            default_repo_root,
        }
    }

    fn handle_request(&mut self, line: &str) -> Option<String> {
        let req: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                let err: JsonRpcError = McpError::ParseError(format!("Parse error: {}", e)).into();
                return Some(
                    json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": err.code, "message": err.message }
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
            "notifications/initialized" => return None,
            _ => Err(McpError::MethodNotFound(format!(
                "Method not found: {}",
                req.method
            ))),
        };

        let response = match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(value),
                error: None,
            },
            Err(mcp_err) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(mcp_err.into()),
            },
        };

        Some(serde_json::to_string(&response).unwrap_or_else(|e| {
            format!(
                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"Response serialization failed: {}"}}}}"#,
                e
            )
        }))
    }

    fn handle_initialize(&self, _params: &Option<Value>) -> Result<Value, McpError> {
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

    fn handle_tools_list(&self) -> Result<Value, McpError> {
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
                    "description": "Query indexed content by pattern, symbol, section, or glob. Returns handles with optional auto-expansion.",
                    "inputSchema": query_input_schema(&query_param_properties(), &["limit"]),
                },
                {
                    "name": "canopy_evidence_pack",
                    "description": "Build a compact, ranked evidence pack for a query. Returns handles, file summaries, and guidance on whether to expand or refine.",
                    "inputSchema": query_input_schema(&query_param_properties(), &["max_handles", "max_per_file", "plan"]),
                },
                {
                    "name": "canopy_expand",
                    "description": "Expand handles to full source content.",
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

    fn handle_tools_call(&mut self, params: &Option<Value>) -> Result<Value, McpError> {
        let params = params
            .as_ref()
            .ok_or(McpError::InvalidParams("Missing params".to_string()))?;

        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or(McpError::InvalidParams("Missing tool name".to_string()))?;

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        match name {
            "canopy_index" => self.tool_index(&arguments),
            "canopy_query" => self.tool_query(&arguments),
            "canopy_evidence_pack" => self.tool_evidence_pack(&arguments),
            "canopy_expand" => self.tool_expand(&arguments),
            "canopy_status" => self.tool_status(&arguments),
            "canopy_invalidate" => self.tool_invalidate(&arguments),
            "canopy_agent_readme" => self.tool_agent_readme(),
            _ => Err(McpError::InvalidParams(format!("Unknown tool: {}", name))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server() -> McpServer {
        McpServer::with_service_url(None, None, None)
    }

    #[test]
    fn handle_initialize_returns_protocol_version() {
        let server = test_server();
        let result = server.handle_initialize(&None).unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "canopy-mcp");
    }

    #[test]
    fn handle_tools_list_returns_tools() {
        let server = test_server();
        let result = server.handle_tools_list().unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(tool_names.contains(&"canopy_query"));
        assert!(tool_names.contains(&"canopy_expand"));
    }

    #[test]
    fn handle_request_parse_error_returns_json_rpc_error() {
        let mut server = test_server();
        let resp = server.handle_request("not json").unwrap();
        let parsed: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["error"]["code"], -32700);
    }

    #[test]
    fn handle_request_unknown_method_returns_error() {
        let mut server = test_server();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "unknown/method",
            "params": null
        });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let parsed: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["error"]["code"], -32601);
    }

    #[test]
    fn handle_request_notification_returns_none() {
        let mut server = test_server();
        let req = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": null
        });
        assert!(server.handle_request(&req.to_string()).is_none());
    }

    #[test]
    fn handle_request_initialize_succeeds() {
        let mut server = test_server();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let parsed: Value = serde_json::from_str(&resp).unwrap();
        assert!(parsed["result"].is_object());
        assert_eq!(parsed["id"], 1);
    }
}
