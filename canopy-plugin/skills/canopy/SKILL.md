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

**Use canopy for:**
- Symbol search: `canopy_query(path=..., symbol="authenticate")` - find function/class definitions
- Large repos (500+ files) where ripgrep is slow
- When you need token counts before deciding what to read

**Use Grep instead for:**
- Quick text searches mid-task (no index needed)
- Small/medium repos (<500 files)
- Known literal patterns

## Tool Decision Matrix

| Finding... | Use |
|------------|-----|
| File by name | `fd` / Glob |
| Text pattern | `rg` / Grep |
| **Function/class definition** | **canopy** |
| Token count before reading | canopy |
| Large repo (500+ files) | canopy |

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

## Key Insight

**Canopy's unique value is semantic symbol search.** Text pattern search is convenient but ripgrep does it well. Use canopy when you need to find code symbols by name - something grep can't do semantically.
