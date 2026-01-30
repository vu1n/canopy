# CLAUDE.md

## Project Overview

Canopy is a token-efficient codebase indexing system for LLM agents. It helps agents explore large codebases without reading entire files by providing handles with previews and selective expansion.

## Architecture

```
canopy/
├── canopy-core/     # Core library
│   └── src/
│       ├── index.rs    # SQLite FTS5 index + symbol cache
│       ├── parse.rs    # Tree-sitter parsing (TS, Python, Rust, etc.)
│       ├── query.rs    # Query DSL and execution
│       ├── handle.rs   # Handle types and preview generation
│       ├── config.rs   # Configuration loading
│       └── document.rs # Parsed document types
├── canopy-mcp/      # MCP server for Claude Code
│   └── src/
│       ├── main.rs     # MCP protocol handler
│       └── predict.rs  # Predictive path selection heuristics
├── canopy-cli/      # Command-line interface
│   └── src/
│       └── main.rs     # CLI commands (init, index, query, expand, status)
└── benchmark/       # A/B testing
    ├── run-ab-test.sh
    └── results/
```

## Key Concepts

### Handles
Lightweight references to code sections with previews (~100 bytes). Agents see handles first, then expand only what they need.

### Predictive Lazy Indexing
For large repos (>1000 files), we don't do full upfront indexing. Instead:
1. Extract keywords from query ("auth" → authentication-related)
2. Map to directory patterns (`**/auth/**`, `**/login/**`)
3. Index only matched paths (~100-500 files)
4. Execute query on partial index

### Symbol Cache
In-memory HashMap preloaded at index open for O(1) symbol lookups. Updated incrementally during indexing.

### SQLite Optimizations
- WAL mode for concurrent access
- mmap (256MB) for memory-mapped reads
- FTS5 for full-text search
- Symbol FTS for fuzzy symbol matching

## Development

```bash
# Build
cargo build

# Test
cargo test

# Run MCP server locally
cargo run -p canopy-mcp -- --root /path/to/repo

# Run CLI
cargo run -p canopy-cli -- query --pattern "auth" --root /path/to/repo
```

## Common Tasks

### Adding a new query type
1. Add variant to `QueryKind` in `canopy-core/src/query.rs`
2. Implement execution in `execute_query()`
3. Add MCP tool parameter handling in `canopy-mcp/src/main.rs`

### Adding predictive keyword mappings
Edit `KEYWORD_PATTERNS` in `canopy-mcp/src/predict.rs`:
```rust
const KEYWORD_PATTERNS: &[(&[&str], &[&str])] = &[
    (&["auth", "login"], &["**/auth/**", "**/login/**"]),
    // Add new mappings here
];
```

### Running benchmarks
```bash
./benchmark/run-ab-test.sh /path/to/large/repo
```

## Code Style

- Use `cargo fmt` before committing
- Avoid over-engineering; keep changes focused
- Update tests for new functionality
- Symbol cache should be updated during `index_parsed_file()` and cleared in `invalidate()`

## Performance Notes

- Symbol cache gives O(1) lookups vs O(log n) B-tree
- mmap pragma provides ~10-20% read speedup on warm index
- Predictive indexing reduces first-query time from 10+ min to <30s on 7600-file repos
- Multi-agent scenarios: each agent has its own cache, falls back to DB on cache miss
