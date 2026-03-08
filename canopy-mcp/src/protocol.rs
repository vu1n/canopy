//! JSON-RPC protocol types and MCP error definitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Serialize)]
pub(crate) struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Serialize)]
pub(crate) struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

/// Structured error type for MCP JSON-RPC responses.
#[derive(Debug)]
pub(crate) enum McpError {
    /// -32700: Invalid JSON
    ParseError(String),
    /// -32601: Unknown method
    MethodNotFound(String),
    /// -32602: Missing or invalid parameters
    InvalidParams(String),
    /// -32000: Application-level error (index, query, expand failures)
    Application(String),
}

impl From<McpError> for JsonRpcError {
    fn from(e: McpError) -> Self {
        let (code, message) = match e {
            McpError::ParseError(m) => (-32700, m),
            McpError::MethodNotFound(m) => (-32601, m),
            McpError::InvalidParams(m) => (-32602, m),
            McpError::Application(m) => (-32000, m),
        };
        JsonRpcError { code, message }
    }
}

impl From<canopy_core::CanopyError> for McpError {
    fn from(e: canopy_core::CanopyError) -> Self {
        McpError::Application(e.to_string())
    }
}
