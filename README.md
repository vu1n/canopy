# Canopy

Token-efficient codebase indexing and querying for LLM agents.

## Quick Start with Claude Code

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/vu1n/canopy/main/install.sh | sh
```

This installs `canopy` + `canopy-mcp` to `~/.local/bin/` and configures Claude Code automatically.

Options:
- `--no-claude-setup`: skip Claude Code MCP configuration.
- `--prefix /usr/local`: install to a custom location.

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

- "Find the AuthController class" -> `canopy_query(symbol="AuthController")`
- "How does authentication work?" -> `canopy_query(pattern="auth")`
- "List all API endpoints" -> `canopy_query(symbol="Controller")` plus selective expands

Typical agent flow:
1. Index relevant paths based on query intent (predictive lazy indexing).
2. Return compact handles + previews instead of full files.
3. Expand only handles needed for the final answer.

---

## When to Use Canopy

### Best For

| Scenario | Why |
|----------|-----|
| Large codebases (>1000 files) | Predictive indexing avoids blocking on full upfront index |
| Symbol discovery | Finds function/class definitions with file:line anchors |
| Cross-file tracing | Follows execution paths across files |
| Subsystem analysis | Scopes indexing/query to relevant areas |
| Parallel agents | Shared index behavior is better for concurrent exploration |

### Skip Canopy For

| Scenario | Use Instead |
|----------|-------------|
| Known file path | `Read` tool directly |
| Literal text pattern | `Grep` tool |
| File by name | `Glob` / `fd` |
| Small repos (<500 files) | Native tools are usually enough |

---

## How Canopy Works

Canopy treats repository understanding as a budgeted retrieval loop: broad cheap retrieval first, then selective expansion under explicit token and turn constraints.

### Token-Efficiency Methods

1. Handle-first retrieval
   `canopy_query` returns compact handles and previews first; full content is fetched with `canopy_expand`.
2. Predictive lazy indexing
   Query intent predicts likely globs so indexing is targeted before search.
3. Feedback-reranked retrieval
   Query/expand feedback in `.canopy/feedback.db` reranks future retrieval:
   - glob ranking (`glob_hit_rate_at_k`)
   - node-type priors (`handle_expand_accept_rate`)
4. Retrieve -> local overlay -> merge (service mode)
   Service results are merged with local dirty-file overlays to keep answers fresh without full reindex.
5. Worst-case budget policy
   Retrieval is constrained by `expand_budget`, `limit`, and turn budget (`MAX_TURNS` in swarm tests).

### Theory: Budgeted Retrieval Control Loop

Informal objective:
- maximize answer utility (grounding, coverage, correctness)
- minimize token cost and tail-risk of runaway context growth

Mental model for ranking:

```text
score(handle) =
  w_text * lexical_relevance +
  w_type * node_type_prior +
  w_feedback * historical_expand_acceptance
```

The ranker optimizes expected utility per token, not just raw relevance.

### Diagram: Retrieval and Ranking Loop

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
    +--> unresolved + budget left? -- yes --> re-query / re-rank / expand
    |                                       (iterate)
    |
    no
    |
    v
Synthesize Answer
  (grounded in expanded evidence)
```

### Diagram: Service Mode Merge Loop

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
            use service result               local incremental index
               directly                      on dirty subset only
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

### Diagram: Feedback Learning Loop

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

### Token Economy (Illustrative)

Traditional approach:
```text
Agent reads file1.ts (500 tokens)
Agent reads file2.ts (800 tokens)
Agent reads file3.ts (600 tokens)
Total: 1900 tokens
```

Canopy approach:
```text
Agent queries "auth" -> 10 handles with previews (200 tokens)
Agent expands 2 relevant handles (400 tokens)
Total: 600 tokens
```

Use `canopy feedback-stats` to inspect local retrieval feedback metrics.

---

## MCP Tools

Once configured, Claude Code has these tools:

### `canopy_query`
Search indexed content. Returns handles with previews and token counts.

```text
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
| `match` | `any` \| `all` | Multi-pattern mode |
| `limit` | integer | Max results (default: 16) |
| `expand_budget` | integer | Deprecated auto-expand toggle (default: 0, disabled) |

### `canopy_evidence_pack`
Build a compact ranked evidence set (no snippets) to minimize context bloat before expanding.

```text
canopy_evidence_pack(pattern="authentication", max_handles=8, max_per_file=2)
```

Response includes `guidance.stop_querying`, `guidance.recommended_action`, and `guidance.next_step`
so agents can transition from retrieval to synthesis without custom prompt rules.

### `canopy_expand`
Expand handles to full content.

```text
canopy_expand(handle_ids=["h1a2b3c...", "h5d6e7f..."])
```

### `canopy_status`
Get index statistics.

### `canopy_invalidate`
Force reindex of files.

### `canopy_agent_readme`
Return usage guidance for agents/tool callers.

This guidance is canopy-first: prefer canopy retrieval over ad-hoc `find`/`grep`/`rg`
for discovery, then expand selectively for synthesis.

---

## CLI Usage

```bash
# Initialize canopy in a repository
canopy init

# Index files (automatic on first query)
canopy index

# Query the codebase
canopy query --pattern "authentication"
canopy query --symbol "AuthController"

# Expand handles to full content
canopy expand <handle_id>

# Check index status
canopy status

# Local feedback metrics
canopy feedback-stats
```

---

## Operating Modes

Canopy supports two modes through the shared `canopy-client` runtime.

### Standalone Mode

Default when no service URL is configured.

```bash
canopy query --pattern "auth"
```

Best for: solo developers, small/medium repos, quick setup.

### Service Mode

For multi-agent and team workflows with shared indexing.

```bash
# Install and start service
curl -fsSL https://raw.githubusercontent.com/vu1n/canopy/main/install-service.sh | sh
canopy-service --port 3000

# Register and reindex repo
curl -X POST localhost:3000/repos/add -H 'Content-Type: application/json' \
  -d '{"path": "/path/to/repo", "name": "my-repo"}'
curl -X POST localhost:3000/reindex -H 'Content-Type: application/json' \
  -d '{"repo": "<repo-id>"}'

# Query via service
CANOPY_SERVICE_URL=http://localhost:3000 canopy query --symbol "Config"
```

Service mode features:
- Generation tracking for stale-handle safety.
- Dirty-file local overlay merge for freshness.
- Handle metadata (`source`, `commit_sha`, `generation`).

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

```text
+-----------------+
| canopy-service  |  HTTP service for multi-repo indexing
+-----------------+
| canopy-mcp      |  MCP server for Claude Code
| canopy-cli      |  Command-line interface
+-----------------+
| canopy-client   |  Shared runtime (service client, dirty overlay, merge, predict)
+-----------------+
| canopy-core     |  Indexing and query engine
|  - index.rs     |  SQLite FTS5 + symbol cache + mmap
|  - parse.rs     |  Tree-sitter parsing
|  - query.rs     |  Query DSL and execution
|  - handle.rs    |  HandleSource + generation metadata
+-----------------+
```

---

## Benchmarking

No canonical benchmark claims are published yet.

For local evaluation:

```bash
# Multi-agent comparison
MODE=compare COMPARE_MODES="baseline canopy canopy-service" \
AGENTS=4 MAX_TURNS=5 INDEX_TIMEOUT=1200 \
./benchmark/run-swarm-test.sh /path/to/repo

# Single-agent A/B
./benchmark/run-ab-test.sh /path/to/repo
```

Detailed protocol, metric definitions, and troubleshooting live in `docs/benchmarking.md`.

Design principles, anti-drift guardrails, and divergence logging live in `docs/design-anchors.md`.

---

## License

MIT
