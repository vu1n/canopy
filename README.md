# Canopy

Token-efficient codebase indexing and querying for LLM agents.

## Overview

Canopy provides semantic code indexing that helps LLM agents explore large codebases efficiently. Instead of reading entire files, agents get **handles** with previews and selectively expand only what they need.

### Key Features

- **Predictive Lazy Indexing**: Query-driven indexing that predicts relevant paths from keywords, avoiding full upfront indexing on large repos
- **Handle-Based Results**: Returns lightweight handles with previews (~100 bytes) instead of full content
- **Selective Expansion**: Agents expand only the handles they need, reducing token usage
- **Symbol Cache**: O(1) symbol lookups via in-memory cache preloaded at startup
- **SQLite + mmap**: Fast persistent storage with memory-mapped access

## Performance

Tested on n8n (7,600+ files):

| Metric | Baseline | Canopy | Improvement |
|--------|----------|--------|-------------|
| Cost | $1.26 | $1.18 | **6% savings** |
| Symbol Search Quality | 178 lines | 414 lines | **2.3x more detail** |
| First Query | Blocks on full index | Instant (predictive) | **No blocking** |

## Installation

```bash
# Build from source
cargo build --release

# The binaries will be in target/release/
# - canopy        (CLI)
# - canopy-mcp    (MCP server)
```

## Usage

### CLI

```bash
# Initialize canopy in a repository
canopy init

# Index files (automatic on first query)
canopy index

# Query the codebase
canopy query --pattern "authentication"
canopy query --symbol "AuthController"
canopy query --section "API"

# Check index status
canopy status

# Expand handles to full content
canopy expand <handle_id>
```

### MCP Server

Add to your Claude Code MCP config:

```json
{
  "mcpServers": {
    "canopy": {
      "command": "/path/to/canopy-mcp",
      "args": ["--root", "/path/to/repo"]
    }
  }
}
```

Available MCP tools:
- `canopy_query` - Search with patterns, symbols, sections, globs
- `canopy_expand` - Expand handles to full content
- `canopy_status` - Get index status

## Architecture

```
┌─────────────────┐
│  canopy-mcp     │  MCP server for Claude Code
├─────────────────┤
│  canopy-cli     │  Command-line interface
├─────────────────┤
│  canopy-core    │  Core indexing and query engine
│  ├─ index.rs    │  SQLite FTS5 + symbol cache
│  ├─ parse.rs    │  Tree-sitter parsing
│  ├─ query.rs    │  Query DSL and execution
│  └─ predict.rs  │  Predictive path selection
└─────────────────┘
```

### Predictive Lazy Indexing

For large repos (>1000 files), canopy uses keyword heuristics to predict relevant paths:

```
Query: "How does authentication work?"
         │
         ▼
┌─────────────────────────────┐
│  Extract keywords           │  → "auth", "authentication"
└─────────────────────────────┘
         │
         ▼
┌─────────────────────────────┐
│  Match to glob patterns     │  → "**/auth/**", "**/login/**"
└─────────────────────────────┘
         │
         ▼
┌─────────────────────────────┐
│  Index matched paths        │  → ~100-500 files (seconds)
└─────────────────────────────┘
         │
         ▼
┌─────────────────────────────┐
│  Execute query              │  → Token-efficient results
└─────────────────────────────┘
```

### Token Economy

Traditional approach:
```
Agent reads file1.ts (500 tokens)
Agent reads file2.ts (800 tokens)
Agent reads file3.ts (600 tokens)
Total: 1900 tokens
```

Canopy approach:
```
Agent queries "auth" → 10 handles with previews (200 tokens)
Agent expands 2 relevant handles (400 tokens)
Total: 600 tokens (68% reduction)
```

## Configuration

Create `.canopy/config.toml`:

```toml
[core]
default_result_limit = 20

[indexing]
default_glob = "**/*.{ts,tsx,js,jsx,py,rs,go}"
preview_bytes = 100
ttl = "24h"

[ignore]
patterns = ["node_modules", ".git", "dist", "build", "__pycache__"]
```

## Benchmarking

```bash
# Run A/B test comparing baseline vs canopy
./benchmark/run-ab-test.sh /path/to/repo
```

Results are saved to `benchmark/results/`.

## License

MIT
