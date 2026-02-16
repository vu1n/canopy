# Canopy — Agent Instructions (MCP + HTTP)

Machine-readable instructions for AI agents using canopy via MCP tools or HTTP service.
Canopy provides token-efficient codebase indexing. You query, get lightweight handles with previews, and expand only what you need.

**Two interfaces**: MCP tools (for Claude Code / stdio-based agents) and HTTP service (for agents making REST calls directly).

**Core invariant: Handles are cheap (~25 tokens). Expansion costs tokens. Query first, expand selectively.**

## Canopy-First Policy

For codebase discovery, use canopy tools as the default retrieval interface.

- Use `canopy_evidence_pack` / `canopy_query` instead of shell search (`find`, `grep`, `rg`) for locating files, symbols, and callsites.
- Use `canopy_expand` for targeted content retrieval after ranking.
- Only fall back to non-canopy search if canopy returns no relevant evidence after one refinement pass (`guidance.recommended_action="refine_query"` and still low signal).

## Mental Model

```
QUERY  →  handles with previews (~100 bytes each)  →  EXPAND selected handles  →  full content
```

- Indexing is automatic. On first query, canopy indexes relevant files. For repos >1000 files, it uses predictive lazy indexing — extracting keywords from your query to index only relevant directories.
- `expand_budget` is deprecated for primary workflows. Prefer `canopy_evidence_pack` + selective `canopy_expand`.
- Handle IDs are stable hashes (`h` + 24 hex chars). They survive reindexing if content location is unchanged.

## Tools

### canopy_query

Search indexed content. Returns handles with previews and token counts.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | yes | — | Absolute path to repo root |
| `pattern` | string | no | — | FTS5 full-text search |
| `patterns` | string[] | no | — | Multiple text patterns |
| `symbol` | string | no | — | Code symbol (function, class, struct, method) |
| `section` | string | no | — | Markdown section heading |
| `parent` | string | no | — | Filter by parent symbol (e.g., class name for methods) |
| `kind` | `"definition"` \| `"reference"` \| `"any"` | no | `"any"` | Filter result type |
| `glob` | string | no | — | File path filter (e.g., `"src/**/*.ts"`) |
| `match` | `"any"` \| `"all"` | no | `"any"` | Multi-pattern mode: OR vs AND |
| `limit` | integer | no | 16 | Max results |
| `expand_budget` | integer | no | 0 | Deprecated: auto-expand toggle |
| `query` | string | no | — | S-expression DSL (fallback, see below) |

**Validation**: Must provide at least one of: `pattern`, `patterns`, `symbol`, `section`, `parent`, or `query`.

**Response** (JSON, pretty-printed in `content[0].text`):

```json
{
  "handles": [
    {
      "id": "h1a2b3c4d5e6f7890abcdef",
      "file_path": "src/auth/controller.ts",
      "node_type": "function",
      "span": { "start": 1024, "end": 2048 },
      "line_range": [42, 78],
      "token_count": 256,
      "preview": "async function authenticate(req, res) { const token = req.headers...",
      "content": "...full content when auto_expanded..."
    }
  ],
  "ref_handles": [
    {
      "file_path": "src/routes/login.ts",
      "span": { "start": 500, "end": 530 },
      "line_range": [15, 15],
      "name": "authenticate",
      "qualifier": "authController",
      "ref_type": "call",
      "source_handle": "h9876543210abcdef12345678",
      "preview": "const result = authController.authenticate(req)"
    }
  ],
  "total_tokens": 1024,
  "total_matches": 5,
  "truncated": false,
  "auto_expanded": true,
  "expand_note": "Results exceed expand_budget. Expand specific handles."
}
```

Notes:
- `ref_handles` only present when `kind="reference"`
- `content` may be present whenever `expanded_count > 0` (including partial auto-expansion)
- `expanded_handle_ids` lists which handles already include `content`; do not re-expand those IDs
- `expand_note` only present when budget exceeded
- `auto_expanded` omitted (false) when not auto-expanded

### canopy_evidence_pack

Build compact, ranked evidence without snippets or full content.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | yes | — | Absolute path to repo root |
| Same search params as `canopy_query` | — | — | — | `pattern`, `patterns`, `symbol`, `section`, `parent`, `kind`, `glob`, `match`, `query` |
| `max_handles` | integer | no | 8 | Max ranked handles in pack |
| `max_per_file` | integer | no | 2 | Max selected handles per file |
| `plan` | boolean | no | auto (low-confidence only) | Override server-side recursive planning (service mode only) |

Response includes:
- `handles` with id/path/line-range/token-count/score (no snippets)
- `files` grouped by file path
- `expand_suggestion` with best handles to expand first (recently expanded handles are de-prioritized)
- `guidance` with explicit control signals:
  - `stop_querying` (bool): whether to stop retrieval loops
  - `recommended_action`: `refine_query` or `expand_then_answer`
  - `suggested_expand_count`: how many handles to expand before synthesis
  - `max_additional_queries`: retrieval budget before writing
  - `confidence` and `confidence_band`: heuristic trust for current pack
  - `next_step`: direct one-line instruction for the agent

### canopy_expand

Expand handles to full content.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to repo root |
| `handle_ids` | string[] | yes | Handle IDs to expand (e.g., `["h1a2b3c4d5e6f7890abcdef"]`) |

**Response** (plain text in `content[0].text`):

```
// h1a2b3c4d5e6f7890abcdef
async function authenticate(req, res) {
  const token = req.headers.authorization;
  // ... full content ...
}

// h9876543210abcdef12345678
class AuthController {
  // ... full content ...
}
```

### canopy_index

Index files matching a glob pattern. Usually not needed — canopy auto-indexes on first query.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to repo root |
| `glob` | string | yes | Glob pattern (e.g., `"**/*.rs"`) |

### canopy_status

Get index statistics.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to repo root |

**Response**: `files_indexed`, `total_tokens`, `index_size_bytes`, `last_indexed`, `schema_version`, `repo_root`, `file_discovery`

### canopy_invalidate

Force reindex of files. Use when files have changed since last indexing.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to repo root |
| `glob` | string | no | Glob pattern to invalidate (all files if omitted) |

---

## HTTP Service API

For agents making direct HTTP requests to `canopy-service`. The service manages multiple repos with generation-tracked indexing.

**Base URL**: `http://<host>:<port>` (default: `http://127.0.0.1:3000`)

### Setup Workflow

```
1. POST /repos/add  →  register a repo  →  get repo_id
2. POST /reindex    →  index the repo   →  generation bumps when done
3. POST /query      →  search           →  handles with source/generation metadata
4. POST /expand     →  get content      →  pass generation for staleness check
```

### POST /repos/add

Register a repository for indexing.

**Request**:
```json
{ "path": "/absolute/path/to/repo", "name": "my-repo" }
```

**Response** `200`:
```json
{ "repo_id": "uuid-string", "name": "my-repo" }
```

`path` must be a git repository (`.git/` must exist). `name` is optional (defaults to directory name). Save the `repo_id` — you need it for all subsequent calls.

### POST /reindex

Trigger indexing for a registered repo. Async — returns immediately, indexing runs in background.

**Request**:
```json
{ "repo": "<repo_id>", "glob": "**/*.ts" }
```

`glob` is optional (defaults to config). If already indexing, returns `"status": "already_indexing"` (coalesced).

**Response** `200`:
```json
{ "generation": 1, "status": "indexing", "commit_sha": "abc123..." }
```

After indexing completes, `generation` bumps and `status` becomes `"ready"`. Poll `GET /status` to check.

### POST /query

Query a repo. Same QueryParams as MCP, flattened into request body with `repo` field.

**Request**:
```json
{
  "repo": "<repo_id>",
  "pattern": "authentication",
  "kind": "definition",
  "glob": "src/**/*.ts",
  "limit": 20,
  "expand_budget": 1200
}
```

All query parameters from the MCP section above are supported (`pattern`, `patterns`, `symbol`, `section`, `parent`, `kind`, `glob`, `match`, `limit`, `expand_budget`).

**Response** `200`: Same `QueryResult` JSON as MCP (see above), with additional fields on each handle:
- `source`: `"service"` — indicates handle came from the HTTP service
- `commit_sha`: git commit the index was built from
- `generation`: generation counter (pass this to expand for staleness check)

### POST /expand

Expand handles to full content. Supports generation-based staleness detection.

**Request**:
```json
{
  "repo": "<repo_id>",
  "handles": [
    { "id": "h1a2b3c4d5e6f7890abcdef", "generation": 1 },
    { "id": "h9876543210abcdef12345678" }
  ]
}
```

`generation` on each handle is optional. If provided and stale, returns `409`.

**Response** `200`:
```json
{
  "contents": [
    { "handle_id": "h1a2b3c4d5e6f7890abcdef", "content": "async function..." },
    { "handle_id": "h9876543210abcdef12345678", "content": "class AuthController..." }
  ]
}
```

### GET /repos

List all registered repos.

**Response** `200`: Array of repo shards with `repo_id`, `name`, `repo_root`, `status`, `generation`, `commit_sha`.

### GET /status

Service health and all repo states.

**Response** `200`:
```json
{ "service": "canopy-service", "repos": [...] }
```

### Error Responses

All errors return structured JSON:

```json
{ "code": "error_code", "message": "Human-readable message", "hint": "What to do next" }
```

| Status | Code | Meaning | Recovery |
|--------|------|---------|----------|
| 404 | `not_found` | Repo or handle not found | Check repo_id, re-query for handles |
| 409 | `stale_generation` | Handle generation doesn't match current | Call `POST /reindex`, then re-query |
| 500 | `internal_error` | Server error | Check service logs |

### HTTP vs MCP: When to Use Which

| Scenario | Use |
|----------|-----|
| Agent integrated with Claude Code | MCP tools (automatic) |
| Agent with HTTP client, no MCP | HTTP service |
| Multiple agents sharing one index | HTTP service (shared state, generation tracking) |
| Single agent, local repo | MCP tools (simpler, auto-indexes) |

Feedback note:
- In service mode, expand feedback is recorded on the service side to avoid duplicate client/server expand-event accounting.

---

## Decision Tree

```
GOAL: Find a specific function/class/struct definition
  → canopy_query(path, symbol="AuthController", kind="definition")

GOAL: Find where a symbol is called/imported/used
  → canopy_query(path, symbol="authenticate", kind="reference")
  → Returns ref_handles with caller context

GOAL: Search for a concept across the codebase
  → canopy_query(path, pattern="authentication")

GOAL: Find multiple related terms (OR)
  → canopy_query(path, patterns=["error", "panic", "unwrap"], match="any")

GOAL: Find code matching all terms (AND)
  → canopy_query(path, patterns=["auth", "validate"], match="all")

GOAL: Search within specific files/directories
  → canopy_query(path, pattern="TODO", glob="src/**/*.rs")

GOAL: Explore all methods of a class
  → canopy_query(path, parent="AuthController")

GOAL: Find a specific method within a class
  → canopy_query(path, parent="AuthController", symbol="validate")

GOAL: Search markdown documentation headings
  → canopy_query(path, section="Installation")
```

## Exploration Workflow

**Phase 1 — Orient** (1 call):
```
canopy_status(path)  →  understand what's indexed, repo size
```

**Phase 2 — Discover** (1 call):
```
canopy_evidence_pack(path, pattern="<domain concept>", max_handles=8, max_per_file=2)
```
Identify relevant handles by file path, line range, and score.

Decision gate from response guidance:
- If `guidance.stop_querying=true` and `guidance.recommended_action="expand_then_answer"`, proceed to Phase 3 immediately.
- If `guidance.recommended_action="refine_query"`, run one narrower follow-up query (more specific symbol/glob/terms), then proceed to Phase 3.

**Phase 3 — Expand** (1 call):
```
canopy_expand(path, handle_ids=[...only the relevant ones...])
```
Expand only the minimal handles needed for final synthesis.

**Phase 4 — Trace** (as needed):
```
canopy_query(path, symbol="<name from Phase 3>", kind="reference")  →  find callers
canopy_query(path, parent="<class name>")  →  explore class hierarchy
```

## Stop Rule (Novelty + Guidance)

Do not rely on fixed turn counts. Stop retrieval when marginal evidence gain is low.

1. Start with `canopy_evidence_pack`.
2. Follow `guidance`:
   - `expand_then_answer` + `stop_querying=true`: expand suggested handles and write.
   - `refine_query`: run one narrower evidence query.
3. After each additional evidence query, compare with prior pack:
   - If new handles are mostly repeats or from the same files, stop querying.
   - If no meaningful new symbols/files appear, stop querying.
4. Expand only `guidance.suggested_expand_count` handles first; expand more only if contradictions remain.

## Token Budget Management

| Scenario | Strategy |
|----------|----------|
| Exploratory broad search | Use `canopy_evidence_pack` first, then expand selectively. |
| Known target, want full content | Expand only top suggested handles first, then iterate. |
| Preview-only scan | Stay on `canopy_evidence_pack` without expand calls. |
| Result count too high | `truncated=true`. Narrow query with `glob`, more specific `pattern`, or lower `limit`. |

## S-Expression DSL

Fallback for complex composed queries. Use the params API (above) for simple queries.

| Expression | Description |
|------------|-------------|
| `(grep "pattern")` | FTS5 full-text search |
| `(code "symbol")` | AST symbol search |
| `(definition "symbol")` | Exact symbol definition |
| `(references "symbol")` | Find references to symbol |
| `(section "heading")` | Markdown section heading |
| `(file "path")` | Entire file as handle |
| `(children "parent")` | All children of parent symbol |
| `(children-named "parent" "child")` | Named child of parent |
| `(in-file "glob" <query>)` | Restrict query to matching files |
| `(union <q1> <q2>)` | Combine results (OR) |
| `(intersect <q1> <q2>)` | Intersection (AND) |
| `(limit N <query>)` | Limit result count |

Example: `canopy_query(path, query='(in-file "src/**/*.rs" (intersect (grep "auth") (code "validate")))')`

## Supported Languages

**Full symbol extraction** (tree-sitter): Rust, Python, JavaScript, TypeScript, Go

**Markdown**: Parsed into sections, code blocks, paragraphs

**Other files**: Line-based chunking (50 lines, 10-line overlap). FTS5 search works but no symbol extraction.

**Node types**: `function`, `class`, `struct`, `method`, `section`, `code_block`, `paragraph`, `chunk`

**Reference types** (in ref_handles): `call`, `import`, `type_ref`

## Error Recovery

### MCP Errors

| Error | Cause | Action |
|-------|-------|--------|
| HandleNotFound | Invalid or stale handle ID | Re-query to get fresh handles |
| StaleIndex | File modified since indexing | `canopy_invalidate(path)` then re-query |
| QueryParse | Invalid s-expression or no search params | Ensure at least one search param is provided |
| GlobPattern | Invalid glob syntax | Use `**/*.ext` style patterns |

### HTTP Service Errors

| Status | Code | Cause | Action |
|--------|------|-------|--------|
| 404 | `not_found` | Unknown repo_id or handle | Check repo_id with `GET /repos`, re-query for handles |
| 409 | `stale_generation` | Repo reindexed since handle was issued | `POST /reindex`, wait for ready, re-query |
| 500 | `internal_error` | Server error | Retry or check service logs |

## Anti-Patterns

1. **Expanding all handles blindly.** Check `auto_expanded` first. If false, read previews and expand only relevant handles.
2. **Manually indexing large repos.** Predictive indexing handles this automatically. Calling `canopy_index` on a 10k-file repo with `**/*` wastes time.
3. **Using shell `find`/`grep`/`rg` as first-line discovery when canopy is available.** This bypasses ranking and drives context bloat. Start with canopy retrieval.
4. **Ignoring `truncated`.** If true, you're missing results. Narrow your query or increase `limit`.
5. **Re-expanding handles that already include `content`.** Check `expanded_handle_ids` first; expanding those again is redundant.
6. **Using DSL for simple queries.** The params API (`pattern`, `symbol`, etc.) is cleaner. Reserve DSL for `union`, `intersect`, and `in-file` compositions.
7. **Relying on broad auto-expand.** Use evidence packs and explicit expands to avoid context bloat.
