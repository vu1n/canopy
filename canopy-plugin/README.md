# Canopy: Token-Efficient Codebase Intelligence

A Claude Code plugin providing semantic code search for large codebases. Canopy indexes your code using tree-sitter for AST-aware parsing and SQLite FTS5 for fast full-text search.

## Installation

```bash
# Clone and build
git clone <repo-url>
cd canopy
cargo build --release -p canopy-mcp

# Copy binary to plugin
cp target/release/canopy-mcp canopy-plugin/bin/

# Load plugin in Claude Code
claude --plugin-dir ./canopy-plugin
```

## When to Use Canopy

| Finding... | Use |
|------------|-----|
| File by name | `fd` / Glob |
| Text pattern | `rg` / Grep |
| **Function/class definition** | **canopy** |
| Token count before reading | canopy |
| Large repo (1000+ files) | canopy |
| Code structure to rewrite | ast-grep |

**Use canopy when you need:**
- Semantic symbol search (functions, classes, structs)
- Token-aware retrieval for large results
- A persistent index that survives across queries

**Use Grep instead for:**
- Quick text searches mid-task
- Small repos (<500 files)
- Known literal patterns

## Quick Start

### Index your codebase
```
canopy_index(glob="**/*.{rs,py,ts,js,go}")
```

### Symbol search (unique capability)
```
canopy_query(symbol="handleError")
canopy_query(symbol="authenticate", glob="src/**/*.py")
```

### Text search with auto-expansion
```
canopy_query(pattern="TODO")
canopy_query(pattern="error", glob="src/*.rs", expand_budget=5000)
```

### Multi-pattern search
```
canopy_query(patterns=["TODO", "FIXME"])                    # Match any (OR)
canopy_query(patterns=["auth", "validate"], match="all")    # Match all (AND)
```

## API Reference

### canopy_index
Index files for querying.

| Param | Type | Description |
|-------|------|-------------|
| `glob` | string | Glob pattern for files to index |

### canopy_query
Query indexed content. Returns handles with previews and token counts.

| Param | Type | Description |
|-------|------|-------------|
| `pattern` | string | Single text pattern to search |
| `patterns` | array | Multiple patterns |
| `symbol` | string | Code symbol (function, class, struct, method) |
| `section` | string | Markdown section heading |
| `glob` | string | Filter by file glob |
| `match` | "any" \| "all" | Multi-pattern mode (default: "any") |
| `limit` | integer | Max results (default: 100) |
| `expand_budget` | integer | Auto-expand if tokens fit (default: 5000) |

### canopy_expand
Expand handles to full content.

| Param | Type | Description |
|-------|------|-------------|
| `handle_ids` | array | Handle IDs to expand |

### canopy_status
Get index statistics (file count, tokens, last indexed).

### canopy_invalidate
Force reindex of files.

| Param | Type | Description |
|-------|------|-------------|
| `glob` | string | Pattern to invalidate (all if omitted) |

## Architecture

```
canopy-core (Rust)
  - SQLite FTS5 index (.canopy/index.db)
  - Tree-sitter parsing (Rust, Python, JS, TS, Go)
  - Token-aware chunking (functions, classes, sections)
  - Handle-based retrieval

canopy-mcp (MCP Server)
  - JSON-RPC 2.0 over stdio
  - Claude Code plugin integration

canopy-cli (Optional)
  - Command-line interface
  - Same functionality as MCP tools
```

## CLI Usage

```bash
# Initialize a repository
canopy init

# Index files
canopy index "**/*.rs"

# Query with DSL (backward compatible)
canopy query "(grep 'error')"

# Query with params (preferred)
canopy query --pattern error --glob "src/*.rs"
canopy query --symbol authenticate

# Expand specific handles
canopy expand h1a2b3c4d5e6 h7d8e9f0a1b2

# Check status
canopy status

# Force reindex
canopy invalidate
```

## File Structure

```
canopy-plugin/
├── .claude-plugin/
│   └── plugin.json       # Plugin manifest
├── .mcp.json             # MCP server config
├── skills/
│   └── canopy-search/
│       └── SKILL.md      # Usage instructions
├── bin/
│   └── canopy-mcp        # Pre-built binary
└── README.md
```

## Supported Languages

Tree-sitter parsing for semantic extraction:
- Rust
- Python
- JavaScript
- TypeScript
- Go

Other files use line-based chunking with configurable overlap.

## Configuration

Create `.canopy/config.toml` to customize:

```toml
[core]
ttl = "1h"              # Cache TTL
default_result_limit = 100

[indexing]
default_glob = "**/*.{rs,py,js,ts,tsx,jsx,go,md,txt,json,yaml,yml,toml}"
chunk_lines = 50        # Lines per chunk for non-AST files
chunk_overlap = 10      # Overlap between chunks
preview_bytes = 100     # Preview length

[ignore]
patterns = ["node_modules", "target", ".git"]
```
