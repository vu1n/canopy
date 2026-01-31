# Canopy: Token-Efficient Codebase Intelligence

A Claude Code plugin providing semantic code search for large codebases. Canopy indexes your code using tree-sitter for AST-aware parsing and SQLite FTS5 for fast full-text search.

## Performance (benchmarked on n8n, 7,600 files)

| Metric | Canopy vs Baseline |
|--------|-------------------|
| Symbol discovery | **2.3x more detailed** output |
| Subsystem analysis | **18% faster, 17% cheaper** |
| Multi-file tracing | **15% faster, 11% cheaper** |
| Simple lookup | Same (no overhead) |
| **Overall cost** | **6% savings** |

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

### Best For:

| Scenario | Why |
|----------|-----|
| **Large codebases (>1000 files)** | Predictive lazy indexing - no blocking on first query |
| **Symbol discovery** | Finds function/class definitions with file:line locations |
| **Cross-file tracing** | Understands execution flows across multiple files |
| **Subsystem analysis** | Indexes relevant paths for faster exploration |
| **Parallel agents** | Shared SQLite index with per-agent symbol cache |

### Skip Canopy For:

| Scenario | Use Instead |
|----------|-------------|
| Known file path | `Read` tool |
| Literal text pattern | `Grep` tool |
| File by name | `Glob` / `fd` |
| Small repos (<500 files) | Native tools |
| Code structure rewrites | `ast-grep` |

### Decision Tree

```
Is repo >1000 files?
  └─ Yes → Use canopy (predictive indexing prevents blocking)
  └─ No → Do you need symbol search or cross-file tracing?
            └─ Yes → Use canopy
            └─ No → Use Grep/Glob/Read
```

## Quick Start

### Symbol search (unique capability)
```
canopy_query(symbol="handleError")
canopy_query(symbol="AuthController", glob="src/**/*.ts")
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

## Key Features

### Predictive Lazy Indexing
For large repos (>1000 files), canopy uses keyword heuristics to predict relevant paths:

```
Query: "How does authentication work?"
         │
         ▼
Extract keywords: "auth", "authentication"
         │
         ▼
Match to globs: "**/auth/**", "**/login/**"
         │
         ▼
Index ~100-500 files (seconds, not minutes)
         │
         ▼
Execute query with token-efficient results
```

### Symbol Cache
O(1) symbol lookups via in-memory HashMap preloaded at index open. Falls back to SQLite FTS5 for cache misses.

### Handle-Based Results
Returns lightweight handles with previews (~100 bytes) instead of full content. Agents expand only what they need, reducing token usage.

### Multi-Agent Support
SQLite with WAL mode + mmap. Each agent has its own cache, shares the persistent index.

## API Reference

### canopy_query
Query indexed content. Returns handles with previews and token counts.

| Param | Type | Description |
|-------|------|-------------|
| `path` | string | Repository path (required) |
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
| `path` | string | Repository path (required) |
| `handle_ids` | array | Handle IDs to expand |

### canopy_status
Get index statistics (file count, tokens, last indexed).

### canopy_index
Index files manually (usually auto-indexes on first query).

| Param | Type | Description |
|-------|------|-------------|
| `path` | string | Repository path (required) |
| `glob` | string | Glob pattern for files to index |

### canopy_invalidate
Force reindex of files.

| Param | Type | Description |
|-------|------|-------------|
| `path` | string | Repository path (required) |
| `glob` | string | Pattern to invalidate (all if omitted) |

## Architecture

```
canopy-core (Rust)
  - SQLite FTS5 index (.canopy/index.db)
  - Tree-sitter parsing (Rust, Python, JS, TS, Go)
  - Symbol cache (HashMap preloaded at open)
  - Predictive path selection

canopy-mcp (MCP Server)
  - JSON-RPC 2.0 over stdio
  - Claude Code plugin integration
  - Keyword-to-glob prediction

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

# Query with params
canopy query --pattern error --glob "src/*.rs"
canopy query --symbol authenticate

# Expand specific handles
canopy expand h1a2b3c4d5e6 h7d8e9f0a1b2

# Check status
canopy status

# Force reindex
canopy invalidate
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
