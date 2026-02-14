# Canopy

Token-efficient codebase indexing and querying for LLM agents.

## Quick Start with Claude Code

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/vu1n/canopy/main/install.sh | sh
```

This installs `canopy` + `canopy-mcp` to `~/.local/bin/` and configures Claude Code automatically.

**Options:**
- `--no-claude-setup` — skip Claude Code MCP configuration
- `--prefix /usr/local` — install to a custom location

<details>
<summary>Build from source</summary>

```bash
git clone https://github.com/vu1n/canopy.git
cd canopy
make install        # builds + installs canopy + canopy-mcp
make setup-claude   # configures Claude Code
```

</details>

### 2. Use It

Claude Code now has access to canopy tools. Ask questions like:

- *"Find the AuthController class"* → Uses `canopy_query(symbol="AuthController")`
- *"How does authentication work?"* → Uses `canopy_query(pattern="auth")` with predictive indexing
- *"List all API endpoints"* → Uses `canopy_query(symbol="Controller")` and expands results

The agent automatically:
1. Indexes relevant paths based on your query (predictive lazy indexing)
2. Returns handles with previews instead of full files
3. Expands only the handles it needs

---

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
| Known file path | `Read` tool directly |
| Literal text pattern | `Grep` tool |
| File by name | `Glob` / `fd` |
| Small repos (<500 files) | Native tools are fast enough |

### Decision Tree

```
Is repo >1000 files?
  └─ Yes → Use canopy (predictive indexing prevents blocking)
  └─ No → Do you need symbol search or cross-file tracing?
            └─ Yes → Use canopy
            └─ No → Use Grep/Glob/Read
```

---

## Performance

Tested on n8n (7,600+ files):

| Test | Canopy vs Baseline |
|------|-------------------|
| Symbol discovery | **2.3x more detailed** (414 vs 178 lines) |
| Subsystem analysis | **18% faster, 17% cheaper** |
| Multi-file tracing | **15% faster, 11% cheaper** |
| Simple lookup | Same (no overhead) |
| **Overall** | **6% cost savings** |

---

## How It Works

### Predictive Lazy Indexing

For large repos, canopy predicts which paths are relevant based on query keywords:

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
│  Index matched paths        │  → ~100-500 files (seconds, not minutes)
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

---

## MCP Tools

Once configured, Claude Code has these tools:

### canopy_query
Search indexed content. Returns handles with previews and token counts.

```
canopy_query(pattern="authentication")
canopy_query(symbol="AuthController", glob="src/**/*.ts")
canopy_query(patterns=["TODO", "FIXME"], match="any")
```

| Param | Type | Description |
|-------|------|-------------|
| `pattern` | string | Text pattern to search |
| `patterns` | array | Multiple patterns |
| `symbol` | string | Code symbol (function, class, struct, method) |
| `section` | string | Markdown section heading |
| `glob` | string | Filter by file glob |
| `match` | "any" \| "all" | Multi-pattern mode (default: "any") |
| `limit` | integer | Max results (default: 100) |
| `expand_budget` | integer | Auto-expand if tokens fit (default: 5000) |

### canopy_expand
Expand handles to full content.

```
canopy_expand(handle_ids=["h1a2b3c...", "h5d6e7f..."])
```

### canopy_status
Get index statistics.

### canopy_invalidate
Force reindex of files.

---

## CLI Usage

The CLI is also available for manual use:

```bash
# Initialize canopy in a repository
canopy init

# Index files (automatic on first query)
canopy index

# Query the codebase
canopy query --pattern "authentication"
canopy query --symbol "AuthController"

# Check index status
canopy status

# Expand handles to full content
canopy expand <handle_id>

# Service integration (v3)
canopy-service --port 3000                              # Start service
canopy --service-url http://localhost:3000 repos         # List repos
canopy --service-url http://localhost:3000 reindex <id>  # Trigger reindex
canopy --service-url http://localhost:3000 query --symbol "Config"  # Query via service
```

---

## Configuration

Create `.canopy/config.toml` in your repo:

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

---

## Architecture

```
┌─────────────────┐
│  canopy-service  │  HTTP service for multi-repo indexing (v3)
├─────────────────┤
│  canopy-mcp     │  MCP server for Claude Code
├─────────────────┤
│  canopy-cli     │  CLI with service integration + dirty overlay
├─────────────────┤
│  canopy-core    │  Core indexing and query engine
│  ├─ index.rs    │  SQLite FTS5 + symbol cache + mmap
│  ├─ parse.rs    │  Tree-sitter parsing
│  ├─ query.rs    │  Query DSL and execution
│  ├─ handle.rs   │  HandleSource, generation tracking
│  └─ generation  │  Generation, RepoShard, ShardStatus
└─────────────────┘
```

---

## Service Mode (v3)

Canopy v3 adds a shared HTTP service for multi-agent scenarios:

```bash
# Install the service
curl -fsSL https://raw.githubusercontent.com/vu1n/canopy/main/install-service.sh | sh

# Start the service
canopy-service --port 3000

# Register and index a repo
curl -X POST localhost:3000/repos/add -H 'Content-Type: application/json' \
  -d '{"path": "/path/to/repo", "name": "my-repo"}'
curl -X POST localhost:3000/reindex -H 'Content-Type: application/json' \
  -d '{"repo": "<repo-id>"}'

# CLI auto-merges local + service results
CANOPY_SERVICE_URL=http://localhost:3000 canopy query --symbol "Config"
```

Features:
- **Generation tracking**: Each reindex bumps a generation counter; stale expands return 409
- **Dirty overlay**: CLI detects uncommitted changes and merges local results with service
- **Handle metadata**: Handles include `source` (local/service), `commit_sha`, `generation`

---

## Benchmarking

```bash
# Run A/B test comparing baseline vs canopy
./benchmark/run-ab-test.sh /path/to/repo
```

Results are saved to `benchmark/results/`.

---

## License

MIT
