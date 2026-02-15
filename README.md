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

## Performance & Methodology

We are not publishing definitive benchmark numbers yet. This section documents the optimization methods and how we measure them.

### Token-Efficiency Methods

1. **Handle-first retrieval**
   `canopy_query` returns compact handles and previews first; full content is fetched only via `canopy_expand`.
2. **Predictive lazy indexing**
   Query intent predicts likely globs so indexing is targeted before search, instead of indexing the entire repo up front.
3. **Feedback-reranked retrieval**
   Query/expand feedback is stored in `.canopy/feedback.db` and reused for reranking:
   - glob ranking (`glob_hit_rate_at_k`)
   - node-type priors (`handle_expand_accept_rate`-driven)
4. **Retrieve -> Local overlay -> Merge execution loop**
   In service mode, results are retrieved from service, then merged with dirty local changes to keep answers fresh without full reindex.
5. **Worst-case budget policy**
   The system is tuned to reduce worst-case token blowups, not just average-case wins:
   - bounded expansion (`expand_budget`)
   - bounded result set (`limit`)
   - explicit tracking of compaction pressure (`max_turns` in swarm tests)

### Theory: Budgeted Retrieval as a Control Loop

Canopy treats code understanding as a budgeted sequential retrieval problem:

1. Start with a broad but cheap probe (`canopy_query`) to get compact evidence (handles + previews).
2. Rank candidate handles by expected usefulness per token.
3. Expand only the top candidates while budget remains.
4. Re-evaluate uncertainty; either iterate or stop.

Informal objective:
- maximize answer utility (grounding, completeness, correctness)
- minimize token cost and tail-risk of runaway context growth

In practice, this means:
- **coarse-to-fine retrieval** instead of full-file ingestion
- **budget-aware stopping** when marginal utility drops
- **feedback-driven ranking** from observed expand behavior

One useful mental model for handle ranking is:

```
score(handle) =
  w_text * lexical_relevance +
  w_type * node_type_prior +
  w_feedback * historical_expand_acceptance
```

where the ranker prefers higher expected utility per token, not just raw relevance.

### Retrieval/Ranking Diagram

```text
User Question
    |
    v
Predictive Scope Selection
  (keywords -> likely globs / symbols)
    |
    v
Initial Query (cheap)
  -> candidate handles + previews + token estimates
    |
    v
Handle Ranking
  (text relevance + type priors + feedback priors)
    |
    v
Budget Gate
  - expand top-k within budget
  - keep strict limit / expand_budget
    |
    +--> unresolved + budget left? ---- yes ----> re-query / re-rank / expand
    |                                           (iterate)
    |
    no
    |
    v
Synthesize Answer
  (grounded in expanded evidence)
```

### Service Mode Merge Diagram

```text
                     +------------------------------+
                     |        canopy-service        |
                     |  pre-indexed repo snapshots  |
                     +---------------+--------------+
                                     |
                                     v
User Question -> canopy-mcp -> Service Query/Expand (generation-tagged handles)
                                     |
                                     v
                           Service Candidate Set
                                     |
                                     v
                    Dirty-File Detector (local working tree)
                                     |
                     +---------------+---------------+
                     |                               |
               clean |                               | dirty
                     v                               v
               use service                    local incremental index
                result as-is                  on dirty subset only
                     |                               |
                     +---------------+---------------+
                                     |
                                     v
                       Local/Service Result Merge
                       (dedupe + freshness preference)
                                     |
                                     v
                           Grounded Final Answer
```

### Feedback Learning Loop Diagram

```text
Query Issued
    |
    v
Handles Returned
    |
    v
Which handles were expanded?
    |
    v
Write events to .canopy/feedback.db
  - query_events
  - query_handles
  - expand_events
    |
    v
Compute derived signals
  - glob_hit_rate_at_k
  - handle_expand_accept_rate
  - node-type priors
    |
    v
Apply priors during future ranking
    |
    v
Better next-query ordering under same token budget
```

### What We Get From These Methods

1. **Lower context bloat by default**
   The default path is query -> shortlist -> selective expand, not bulk file reads.
2. **Better worst-case stability**
   Budget caps and compaction-aware evaluation make runaway contexts easier to detect and control.
3. **Better grounding signals**
   Feedback loops and strict quality checks favor answers with concrete file references and structure.

Use `canopy feedback-stats` to inspect local feedback metrics.

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
Total: 600 tokens (illustrative example)
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
│  canopy-service  │  HTTP service for multi-repo indexing
├─────────────────┤
│  canopy-mcp     │  MCP server for Claude Code
│  canopy-cli     │  Command-line interface
├─────────────────┤
│  canopy-client  │  Shared runtime (service client, dirty overlay, merge, predict)
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

## Operating Modes

Canopy supports two operating modes:

### Standalone Mode (solo dev)

Default when no service URL is configured. Local index with predictive indexing for large repos.

```bash
# CLI
canopy query --pattern "auth"

# MCP (automatic — no config needed)
```

Best for: solo developers, small-medium repos, quick setup.

### Service Mode (teams/swarms)

Shared pre-indexed service for multi-agent scenarios. Set `CANOPY_SERVICE_URL` to enable.

```bash
# Start the service
canopy-service --port 3000

# CLI with service
CANOPY_SERVICE_URL=http://localhost:3000 canopy query --symbol "Config"

# MCP with service (set env in MCP config)
# The MCP server reads CANOPY_SERVICE_URL from its environment
```

Best for: teams, CI, multi-agent swarms, large repos where upfront indexing pays off.

The `canopy-client` crate provides a `ClientRuntime` that handles both modes transparently — CLI and MCP stay in sync without duplicating mode-switching logic.

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

No canonical benchmark claims are published yet. The scripts below are for reproducible local evaluation.

### Swarm Benchmark (multi-agent)

Simulates N concurrent agents exploring a codebase — measures token economy, speed, and quality across parallel workloads.

```bash
# Run with defaults (5 agents, both baseline + canopy)
./benchmark/run-swarm-test.sh /path/to/repo

# Customize via env vars
AGENTS=3 MODE=canopy MAX_TURNS=15 ./benchmark/run-swarm-test.sh /path/to/repo

# Baseline only
AGENTS=5 MODE=baseline ./benchmark/run-swarm-test.sh /path/to/repo

# Explicit mode comparison
MODE=compare COMPARE_MODES="baseline canopy canopy-service" \
AGENTS=4 MAX_TURNS=5 INDEX_TIMEOUT=1200 \
./benchmark/run-swarm-test.sh /path/to/repo
```

Each agent gets a different task (round-robin) to simulate real multi-agent workloads. Results include per-agent metrics and an aggregate comparison table in `benchmark/results/swarm-{date}/summary.md`.

### Measurement Protocol (Methods, Not Claims)

1. **Keep setup controlled**
   Same repo, same model, same tasks, same `MAX_TURNS`, same agent count across modes.
2. **Compare three modes**
   - `baseline`: no canopy MCP
   - `canopy`: local canopy MCP
   - `canopy-service`: canopy MCP backed by `canopy-service`
3. **Track both token views**
   - **Reported tokens**: `input + cache_create + output`
   - **Effective tokens**: `reported + cache_read`
4. **Track worst-case behavior**
   - max agent time
   - compactions (`num_turns >= MAX_TURNS`)
5. **Track answer quality and grounding heuristics**
   - success rate (non-empty, not compacted)
   - grounded outputs (file-path references)
   - structured outputs (headings + bullets/fences)
6. **Verify retrieval path usage**
   - local feedback events: `query_events`, `expand_events`
   - service metrics: `/query` and `/expand` call counts
   - if service query/expand are `0`, the run did not exercise service retrieval

### Single-Agent A/B Test

```bash
./benchmark/run-ab-test.sh /path/to/repo
```

Results are saved to `benchmark/results/`.

---

## License

MIT
