//! Tool input schema definitions for MCP tool registration.

use serde_json::{json, Value};

pub(crate) const DEFAULT_MCP_QUERY_LIMIT: usize = 16;

/// Shared query parameter JSON schema properties used by canopy_query and canopy_evidence_pack.
pub(crate) fn query_param_properties() -> Value {
    json!({
        "path": {
            "type": "string",
            "description": "Repository path (optional if --root or CANOPY_ROOT is set)"
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
            "description": "[Fallback] S-expression DSL query. Use params above instead."
        }
    })
}

/// Build an inputSchema object merging shared query params with tool-specific extras.
pub(crate) fn query_input_schema(shared: &Value, extras: &[&str]) -> Value {
    let mut props = shared.as_object().cloned().unwrap_or_default();
    for key in extras {
        let schema = match *key {
            "limit" => json!({
                "type": "integer",
                "description": "Maximum number of results (default: 16)"
            }),
            "max_handles" => json!({
                "type": "integer",
                "description": "Maximum ranked handles in evidence pack (default: 8)"
            }),
            "max_per_file" => json!({
                "type": "integer",
                "description": "Maximum handles selected from a single file (default: 2)"
            }),
            "plan" => json!({
                "type": "boolean",
                "description": "Override server-side recursive evidence planning (default: auto: only when confidence is low)"
            }),
            _ => continue,
        };
        props.insert(key.to_string(), schema);
    }
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": []
    })
}
