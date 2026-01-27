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

Example response:
```
Canopy index status for /Users/vuln/code/myproject:
- Files indexed: 47 (125k tokens)
- File discovery: fd
- Last indexed: 5 minutes ago

Would you like to reindex? [Yes - default glob] [Yes - custom glob] [No]
```

---

## When to Use Canopy

Canopy is a **time-for-tokens tradeoff**: ~36% slower but ~45% fewer tokens (~44% cost savings).

### Canopy Excels At:
| Scenario | Why |
|----------|-----|
| **Multi-iteration exploration** | Overhead amortizes; by iter 3, same speed + fewer tokens |
| **Deep code review / analysis** | Multiple queries benefit from indexed search |
| **Large codebases (500+ files)** | Index beats repeated file scans |
| **Semantic symbol search** | `canopy_query(symbol="authenticate")` finds definitions |
| **Parallel agents on same repo** | Shared index, reduced redundant file reads |

### Skip Canopy For:
| Scenario | Why |
|----------|-----|
| **Quick single lookup** | Index overhead not worth it |
| **Time-critical tasks** | ~36% slower than native tools |
| **Small repos (<200 files)** | Native tools are fast enough |

### The Iteration Effect (from benchmarks)
```
Iteration 1: Canopy ~94% slower (cold start, index building)
Iteration 2: Canopy ~16% slower (context building)
Iteration 3: Canopy reaches parity (token savings accumulate)
```

**Rule of thumb**: If you expect 3+ search iterations, consider Canopy for token savings.

## Quick Reference

### Index first (once per session)
```
canopy_index(path="/path/to/repo", glob="**/*.{rs,py,ts,js,go}")
```

### Symbol search (unique capability)
```
canopy_query(path="/path/to/repo", symbol="handleError")
canopy_query(path="/path/to/repo", symbol="authenticate", glob="src/**/*.py")
```

### Text search with token awareness
```
canopy_query(path="/path/to/repo", pattern="TODO")
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
Index files for querying.
```
canopy_index(path="/path/to/repo", glob="**/*.rs")
```
**Required:** `path`, `glob`

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
**Required:** `path`, `handle_ids`

### canopy_status
Get index statistics (file count, tokens, last indexed).
```
canopy_status(path="/path/to/repo")
```
**Required:** `path`

### canopy_invalidate
Force reindex of files.
```
canopy_invalidate(path="/path/to/repo")
canopy_invalidate(path="/path/to/repo", glob="*.rs")
```
**Required:** `path`

## Key Insights

1. **Time-for-tokens tradeoff**: ~36% slower but ~45% fewer tokens (~44% cost savings)

2. **Iteration effect**: Canopy overhead amortizes. By iteration 3, it reaches speed parity while maintaining token savings.

3. **Unique capability**: Semantic symbol search - find function/class definitions by name

4. **Decision rule**: Expect 3+ search iterations? Consider Canopy for token/cost savings.
