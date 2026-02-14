# Canopy CLI — Agent Instructions

Machine-readable instructions for AI agents using `canopy` CLI from a shell.
Canopy provides token-efficient codebase indexing. You query, get lightweight handles with previews, and expand only what you need.

**Core invariant: Handles are cheap (~25 tokens). Expansion costs tokens. Query first, expand selectively.**

## Mental Model

```
INDEX files  →  QUERY for handles with previews  →  EXPAND selected handles  →  full content
```

- Always use `--json` for machine-parseable output.
- Previews are ~100 bytes (~25 tokens). Each handle includes a `token_count` field showing the cost of expanding it.
- `--expand-budget N` auto-expands results if total tokens fit within N. Default is 0 (no auto-expansion) for CLI. Set to 5000+ for auto-expansion.
- Handle IDs are stable hashes (`h` + 24 hex chars, e.g., `h1a2b3c4d5e6f7890abcdef`). They survive reindexing if content location is unchanged.

## Commands

### Query (primary command)

```bash
canopy query [OPTIONS] [--json] [--root PATH]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--pattern <PAT>` | string | — | FTS5 full-text search |
| `--symbol <SYM>` | string | — | Code symbol (function, class, struct, method) |
| `--parent <PAR>` | string | — | Filter by parent symbol (class name for methods) |
| `--kind <KIND>` | `definition` \| `reference` \| `any` | `any` | Filter result type |
| `--glob <GLOB>` | string | — | File path filter (e.g., `src/**/*.ts`) |
| `--expand-budget <N>` | integer | 0 | Auto-expand if total tokens fit within budget |
| `--limit <N>` | integer | 20 | Max results |

Positional argument accepts s-expression DSL (see below).

**Must provide at least one of**: `--pattern`, `--symbol`, `--parent`, or positional DSL query.

Examples:
```bash
canopy query --symbol AuthController --kind definition --json
canopy query --pattern "authentication" --glob "src/**/*.ts" --json
canopy query --symbol authenticate --kind reference --json
canopy query --parent AuthController --json
canopy query --pattern "error" --expand-budget 5000 --json
canopy query '(intersect (grep "auth") (code "validate"))' --json
```

### Expand

```bash
canopy expand <HANDLE_ID>... [--json] [--root PATH]
```

Pass one or more handle IDs as positional arguments.

```bash
canopy expand h1a2b3c4d5e6f7890abcdef h9876543210abcdef12345678 --json
```

### Index

```bash
canopy index [GLOB] [--json] [--root PATH]
```

Index files matching glob pattern. Uses default glob from config if omitted.

```bash
canopy index "**/*.rs" --json
canopy index --json  # uses default from .canopy/config.toml
```

### Status

```bash
canopy status [--json] [--root PATH]
```

Returns: `files_indexed`, `total_tokens`, `index_size_bytes`, `last_indexed`, `schema_version`.

### Invalidate

```bash
canopy invalidate [GLOB] [--json] [--root PATH]
```

Force reindex. Invalidates all files if glob omitted.

### Init

```bash
canopy init [--root PATH]
```

Creates `.canopy/` directory and `config.toml`. Run once per repo.

### Service Commands

When a `canopy-service` HTTP server is running, the CLI can query it with `--service-url`:

```bash
# Global flags for service integration
canopy --service-url http://localhost:3000 [COMMAND]
canopy --mode service-only --service-url http://localhost:3000 [COMMAND]
```

| Flag | Description |
|------|-------------|
| `--service-url <URL>` | Service base URL (also `CANOPY_SERVICE_URL` env var) |
| `--mode auto` | Merge local + service results (default) — local handles override for dirty files |
| `--mode service-only` | Only query the service, skip local index |

```bash
# List repos registered with the service
canopy --service-url http://localhost:3000 repos

# Trigger reindex on the service
canopy --service-url http://localhost:3000 reindex <repo_id> [--glob "**/*.ts"]

# Service health check
canopy --service-url http://localhost:3000 service-status

# Query via service (auto mode: merges service + local dirty files)
canopy --service-url http://localhost:3000 query --pattern "auth" --json
```

In `auto` mode, the CLI detects locally modified files (via `git status`), builds a local index overlay for dirty files, and merges results — local handles override service handles for files you've changed but haven't committed.

## JSON Output Shapes

All commands support `--json` for structured output.

### Query Result

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
      "content": "...full content when auto-expanded..."
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

- `ref_handles`: only present when `--kind reference`
- `content` on handles: only present when `auto_expanded` is true
- `expand_note`: only present when budget exceeded
- `auto_expanded`: omitted when false

### Handle Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | `h` + 24 hex chars (25 chars total) |
| `file_path` | string | Repo-relative path |
| `node_type` | string | `function`, `class`, `struct`, `method`, `section`, `code_block`, `paragraph`, `chunk` |
| `span` | object | `{ "start": <byte>, "end": <byte> }` |
| `line_range` | array | `[start_line, end_line]` (1-indexed) |
| `token_count` | integer | Approximate token count of full content |
| `preview` | string | ~100 bytes of content, whitespace-collapsed |
| `content` | string? | Full content (only when auto-expanded) |

### RefHandle Fields

| Field | Type | Description |
|-------|------|-------------|
| `file_path` | string | Repo-relative path |
| `name` | string | Referenced symbol name (unqualified) |
| `qualifier` | string? | Object name, module path |
| `ref_type` | string | `call`, `import`, `type_ref` |
| `source_handle` | string? | Handle ID of containing function/class |
| `preview` | string | Context around the reference |

## Decision Tree

```
GOAL: Find a specific function/class/struct definition
  → canopy query --symbol AuthController --kind definition --json

GOAL: Find where a symbol is called/imported
  → canopy query --symbol authenticate --kind reference --json

GOAL: Search for a concept across the codebase
  → canopy query --pattern "authentication" --json

GOAL: Search within specific files
  → canopy query --pattern "TODO" --glob "src/**/*.rs" --json

GOAL: Explore all methods of a class
  → canopy query --parent AuthController --json

GOAL: Find a specific method within a class
  → canopy query --parent AuthController --symbol validate --json

GOAL: Complex composed query
  → canopy query '(in-file "src/**/*.rs" (intersect (grep "auth") (code "validate")))' --json
```

## Exploration Workflow

**Phase 1 — Orient**:
```bash
canopy status --json
```

**Phase 2 — Discover**:
```bash
canopy query --pattern "<domain concept>" --limit 20 --json
```
Parse the JSON. Read `preview` fields to identify relevant handles.

**Phase 3 — Expand** (skip if `auto_expanded` was true):
```bash
canopy expand h1a2b3c4... h5d6e7f8... --json
```

**Phase 4 — Trace**:
```bash
canopy query --symbol "<name>" --kind reference --json   # find callers
canopy query --parent "<class>" --json                   # explore class
```

## S-Expression DSL

Positional argument for complex composed queries. Prefer `--pattern`/`--symbol` flags for simple queries.

| Expression | Description |
|------------|-------------|
| `(grep "pattern")` | FTS5 full-text search |
| `(code "symbol")` | AST symbol search |
| `(definition "symbol")` | Exact symbol definition |
| `(references "symbol")` | Find references |
| `(section "heading")` | Markdown section heading |
| `(file "path")` | Entire file as handle |
| `(children "parent")` | All children of parent symbol |
| `(children-named "parent" "child")` | Named child of parent |
| `(in-file "glob" <query>)` | Restrict to matching files |
| `(union <q1> <q2>)` | Combine results (OR) |
| `(intersect <q1> <q2>)` | Intersection (AND) |
| `(limit N <query>)` | Limit result count |

## Supported Languages

**Full symbol extraction** (tree-sitter): Rust, Python, JavaScript, TypeScript, Go

**Markdown**: Parsed into sections, code blocks, paragraphs

**Other files**: Line-based chunking (50 lines, 10-line overlap). FTS5 search works but no symbol extraction.

## Anti-Patterns

1. **Not using `--json`.** Human-readable output is not structured. Always use `--json` for parsing.
2. **Expanding all handles blindly.** Read previews first. Expand only the handles relevant to your task.
3. **Using canopy for known file paths.** If you know the exact file, read it directly.
4. **Ignoring `truncated`.** If true, you're missing results. Narrow your query or increase `--limit`.
5. **Re-expanding when `auto_expanded` is true.** Content is already in the handles.
6. **Using DSL for simple queries.** `--pattern` and `--symbol` flags are cleaner. Reserve DSL for `union`, `intersect`, and `in-file` compositions.
7. **Not initializing first.** Run `canopy init` before first use. Indexing requires `.canopy/` directory.
