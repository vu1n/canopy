---
name: canopy-search
description: >
  Semantic code intelligence for large codebases. Find functions, classes, and symbols.
  Use for token-aware retrieval and shared indexes across parallel agents.
user_invocable: false
---

# Canopy: Semantic Code Intelligence

Use canopy when you need to find code symbols (functions, classes, structs) or when working with large codebases (500+ files). For simple text searches in small repos, prefer ripgrep/Grep.

## Tool Decision Matrix

| Finding... | Use |
|------------|-----|
| File by name | `fd` / Glob |
| Text pattern | `rg` / Grep |
| **Function/class definition** | **canopy** |
| Token count before reading | canopy |
| Large repo (1000+ files) | canopy |
| Code structure to rewrite | ast-grep |

## When to Use Canopy

**Use canopy for:**
- Symbol search: `canopy_query(symbol="authenticate")` - find function/class definitions
- Large repos where ripgrep is slow
- When you need token counts before deciding what to read
- Parallel agents that can share an indexed codebase

**Use Grep instead for:**
- Quick text searches mid-task (no index needed)
- Small/medium repos (<500 files)
- Known literal patterns like `console.log`

## Quick Reference

### Index first (once per session)
```
canopy_index(glob="**/*.{rs,py,ts,js,go}")
```

### Symbol search (unique capability)
```
canopy_query(symbol="handleError")
canopy_query(symbol="authenticate", glob="src/**/*.py")
```

### Text search with token awareness
```
canopy_query(pattern="TODO")
canopy_query(pattern="error", glob="src/*.rs")
```

### Multi-pattern search
```
canopy_query(patterns=["TODO", "FIXME"])                    # Match any (OR)
canopy_query(patterns=["auth", "validate"], match="all")    # Match all (AND)
```

### Auto-expand (default: 5000 tokens)
Results are auto-expanded if they fit within the token budget. For large results, use `canopy_expand`:
```
canopy_expand(handle_ids=["h1a2b3c...", "h5d6e7f..."])
```

## API Reference

### canopy_index
Index files for querying.
```
canopy_index(glob="**/*.rs")
```

### canopy_query
Search indexed content. Returns handles with token counts and previews.

**Parameters:**
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
canopy_expand(handle_ids=["h1a2b3c4d5e6"])
```

### canopy_status
Get index statistics (file count, tokens, last indexed).

### canopy_invalidate
Force reindex of files.
```
canopy_invalidate()              # All files
canopy_invalidate(glob="*.rs")   # Only matching files
```

## Key Insight

**Canopy's unique value is semantic symbol search.** Text pattern search is convenient but ripgrep does it well. Use canopy when you need to find code symbols by name - something grep can't do semantically.
