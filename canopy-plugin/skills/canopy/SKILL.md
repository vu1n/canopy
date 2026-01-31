---
name: canopy
description: >
  Check canopy index status and optionally reindex a repository.
user_invocable: true
arg_description: "<path> - repository path (REQUIRED)"
---

# Canopy: Semantic Code Intelligence

## IMPORTANT: Path is Required

All canopy tools require an explicit `path` parameter. This prevents indexing the wrong directory.

```
canopy_query(path="/Users/vuln/code/myproject", pattern="auth")  # Correct
canopy_query(pattern="auth")                                      # ERROR: missing path
```

## When invoked via /canopy

1. User provides path as argument: `/canopy /path/to/repo`
2. Call `canopy_status(path=<arg>)` to get current index state
3. Display: files indexed, total tokens, repo root, file discovery method
4. Ask user: "Reindex now?" with options for glob pattern

---

## When to Use Canopy

### Canopy Excels At (benchmarked on n8n, 7,600 files):

| Scenario | Improvement | Why |
|----------|-------------|-----|
| **Symbol discovery** | 2.3x more detailed | Finds ~200 endpoints vs ~150 with file:line locations |
| **Subsystem analysis** | 18% faster, 17% cheaper | Cross-file understanding benefits from index |
| **Multi-file tracing** | 15% faster, 11% cheaper | Execution flow tracing uses indexed refs |
| **Large codebases (>1000 files)** | No first-query blocking | Predictive lazy indexing indexes only relevant paths |
| **Parallel agents on same repo** | Shared SQLite index | O(1) symbol lookups via in-memory cache |

### Skip Canopy For:

| Scenario | Why |
|----------|-----|
| **Simple file lookup** | Same speed/cost as baseline |
| **Known file path** | Just use Read tool directly |
| **Small repos (<500 files)** | Overhead not worth it |
| **Quick grep for literal** | Grep is 100ms, canopy is ~1s |

### Decision Tree

```
Is repo >1000 files?
  └─ Yes → Use canopy (predictive indexing prevents blocking)
  └─ No → Do you need symbol search or cross-file tracing?
            └─ Yes → Use canopy
            └─ No → Use Grep/Glob/Read
```

## Quick Reference

### Symbol search (unique capability)
```
canopy_query(path="/path/to/repo", symbol="handleError")
canopy_query(path="/path/to/repo", symbol="AuthController", glob="src/**/*.ts")
```

### Text search with token awareness
```
canopy_query(path="/path/to/repo", pattern="authentication")
canopy_query(path="/path/to/repo", pattern="error", glob="src/*.rs")
```

### Multi-pattern search
```
canopy_query(path="/path/to/repo", patterns=["TODO", "FIXME"])
canopy_query(path="/path/to/repo", patterns=["auth", "validate"], match="all")
```

### Auto-expand (default: 5000 tokens)
Results auto-expand if they fit budget. For large results:
```
canopy_expand(path="/path/to/repo", handle_ids=["h1a2b3c...", "h5d6e7f..."])
```

## API Reference

### canopy_index
Index files for querying. Usually not needed - auto-indexes on first query.
```
canopy_index(path="/path/to/repo", glob="**/*.rs")
```

### canopy_query
Search indexed content. Returns handles with token counts and previews.

**Required:** `path`
**Optional:**
- `pattern` (string): Single text pattern
- `patterns` (array): Multiple patterns
- `symbol` (string): Code symbol (function, class, struct, method)
- `section` (string): Markdown section heading
- `glob` (string): Filter by file glob
- `match` ("any" | "all"): Multi-pattern mode (default: "any")
- `limit` (integer): Max results (default: 100)
- `expand_budget` (integer): Auto-expand if tokens fit (default: 5000)

### canopy_expand
Expand handles to full content.
```
canopy_expand(path="/path/to/repo", handle_ids=["h1a2b3c4d5e6"])
```

### canopy_status
Get index statistics (file count, tokens, last indexed).
```
canopy_status(path="/path/to/repo")
```

### canopy_invalidate
Force reindex of files.
```
canopy_invalidate(path="/path/to/repo")
canopy_invalidate(path="/path/to/repo", glob="*.rs")
```

## Key Features

1. **Predictive Lazy Indexing**: On large repos, indexes only paths relevant to your query keywords (auth → `**/auth/**`, `**/login/**`). No 10-minute blocking on first query.

2. **Symbol Cache**: O(1) lookups via in-memory HashMap preloaded at startup. Falls back to SQLite for cache misses.

3. **Handle-Based Results**: Returns lightweight previews (~100 bytes) instead of full content. Expand only what you need.

4. **Multi-Agent Friendly**: SQLite with WAL mode + mmap. Each agent gets its own cache, shares the persistent index.

## Benchmark Results (n8n, 7,600 files)

| Test | Canopy vs Baseline |
|------|-------------------|
| Symbol search | 2.3x more detailed output |
| Subsystem analysis | 18% faster, 17% cheaper |
| Multi-file tracing | 15% faster, 11% cheaper |
| Simple lookup | Same (no overhead) |
| **Overall** | **6% cost savings** |
