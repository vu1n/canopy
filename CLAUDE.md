# CLAUDE.md

## Project Overview

Canopy is a token-efficient codebase indexing system for LLM agents. It helps agents explore large codebases without reading entire files by providing handles with previews and selective expansion.

## Architecture

```
canopy/
├── canopy-core/     # Core library (pure, no reqwest)
│   └── src/
│       ├── index.rs       # SQLite FTS5 index + symbol cache
│       ├── parse.rs       # Tree-sitter parsing (TS, Python, Rust, etc.)
│       ├── query.rs       # Query DSL and execution
│       ├── handle.rs      # Handle types, HandleSource, preview generation
│       ├── generation.rs  # Generation tracking (Generation, RepoShard, ShardStatus)
│       ├── config.rs      # Configuration loading
│       ├── document.rs    # Parsed document types
│       └── error.rs       # Error types including StaleGeneration, ServiceError
├── canopy-client/   # Shared client runtime (CLI + MCP stay in sync)
│   └── src/
│       ├── runtime.rs        # ClientRuntime: unified mode orchestration
│       ├── service_client.rs # HTTP client for canopy-service (blocking reqwest)
│       ├── dirty.rs          # Git dirty file detection and local index overlay
│       ├── merge.rs          # Result merge logic (local + service)
│       └── predict.rs        # Predictive path selection heuristics
├── canopy-service/  # HTTP service for multi-repo indexing
│   └── src/
│       ├── main.rs     # Axum router, CLI args (--port, --bind)
│       ├── routes.rs   # 7 HTTP endpoints (query, expand, repos/add, repos, status, reindex, metrics)
│       ├── state.rs    # AppState with RwLock<HashMap<String, RepoShard>>
│       └── error.rs    # Structured error envelope {code, message, hint}
├── canopy-mcp/      # MCP server for Claude Code
│   └── src/
│       └── main.rs     # MCP protocol handler (uses ClientRuntime)
├── canopy-cli/      # Command-line interface
│   └── src/
│       └── main.rs     # CLI commands (uses ClientRuntime)
└── benchmark/       # A/B testing + swarm benchmarks
    ├── run-ab-test.sh
    ├── run-swarm-test.sh  # baseline vs canopy vs canopy-service
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

### Service Architecture (v3)

Canopy v3 adds a shared indexing service for multi-agent scenarios:

**canopy-service**: HTTP service that indexes committed code for multiple repos with generation tracking.
- Repos registered via `POST /repos/add`, indexed via `POST /reindex`
- Each repo has a `Generation` counter that bumps on reindex
- Handles stamped with `source: "service"`, `commit_sha`, and `generation`
- Stale generation on expand returns 409 with structured error

**Client dirty overlay**: `canopy-client` detects local uncommitted changes and merges with service results.
- `git status --porcelain=v2` detects dirty files
- Local index rebuilt for dirty files only (fingerprint-cached)
- Merge: drops ALL service handles (and ref_handles) for dirty file paths, keeps local

**Shared contracts** on `Handle`:
- `source: HandleSource` -- `Local` or `Service`
- `commit_sha: Option<String>` -- git commit the index was built from
- `generation: Option<u64>` -- generation counter for staleness detection

### HTTP API

| Endpoint | Method | Description |
|---|---|---|
| `/query` | POST | Query a repo (body: `{ repo, ...QueryParams }`) |
| `/expand` | POST | Expand handles (body: `{ repo, handles: [{id, generation?}] }`) |
| `/repos/add` | POST | Register a repo (body: `{ path, name? }`) |
| `/repos` | GET | List registered repos |
| `/status` | GET | Service health + shard states |
| `/reindex` | POST | Trigger reindex (body: `{ repo, glob? }`) |

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

# Run service
cargo run -p canopy-service -- --port 3000

# CLI with service integration
cargo run -p canopy-cli -- --service-url http://localhost:3000 query --pattern "auth"
cargo run -p canopy-cli -- --service-url http://localhost:3000 query --symbol "Config"
cargo run -p canopy-cli -- --service-url http://localhost:3000 repos
cargo run -p canopy-cli -- --service-url http://localhost:3000 service-status
```

## Common Tasks

### Adding a new query type
1. Add variant to `QueryKind` in `canopy-core/src/query.rs`
2. Implement execution in `execute_query()`
3. Add MCP tool parameter handling in `canopy-mcp/src/main.rs`

### Adding predictive keyword mappings
Edit `KEYWORD_PATTERNS` in `canopy-client/src/predict.rs`:
```rust
const KEYWORD_PATTERNS: &[(&[&str], &[&str])] = &[
    (&["auth", "login"], &["**/auth/**", "**/login/**"]),
    // Add new mappings here
];
```

### Adding a service endpoint
1. Add route handler in `canopy-service/src/routes.rs`
2. Add request/response types in the same file
3. Register route in `canopy-service/src/main.rs` router
4. Add client method in `canopy-client/src/service_client.rs`
5. Add runtime method in `canopy-client/src/runtime.rs`
6. Add CLI subcommand in `canopy-cli/src/main.rs`

### Running benchmarks
```bash
./benchmark/run-ab-test.sh /path/to/large/repo
./benchmark/run-swarm-test.sh /path/to/large/repo  # baseline + canopy + canopy-service
AGENTS=2 MODE=canopy-service ./benchmark/run-swarm-test.sh /path/to/repo
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
