//! Repository index with SQLite FTS5

mod expand;
mod file_discovery;
mod pipeline;
pub(crate) mod search;
pub(crate) mod symbol_cache;
#[cfg(test)]
mod test_helpers;

pub use file_discovery::FileDiscovery;

use crate::config::{default_config_toml, Config};
use crate::document::NodeType;
use crate::error::CanopyError;
use crate::handle::generate_preview;
use crate::query::{execute_query_with_options, parse_query, QueryOptions, QueryParams, QueryResult};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use symbol_cache::SymbolCacheEntry;

const SCHEMA_VERSION: i32 = 3;

/// Statistics from an indexing operation
#[derive(Debug, Serialize)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub total_tokens: usize,
    pub index_size_bytes: u64,
}

/// Index status information
#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub files_indexed: usize,
    pub total_tokens: usize,
    pub schema_version: i32,
    pub index_size_bytes: u64,
    pub last_indexed: Option<String>,
}

/// Detail record returned when expanding a handle.
pub struct ExpandedHandleDetail {
    pub handle_id: String,
    pub file_path: String,
    pub node_type: NodeType,
    pub token_count: usize,
    pub content: String,
}
type ExpandedHandleDbRow = (String, i64, i64, i64, i64, Vec<u8>);

/// Repository index backed by SQLite
pub struct RepoIndex {
    pub(crate) repo_root: PathBuf,
    pub(crate) conn: Connection,
    pub(crate) config: Config,
    /// Symbol cache: name_lower -> entries (preloaded at open for O(1) lookups)
    pub(crate) symbol_cache: HashMap<String, Vec<SymbolCacheEntry>>,
    /// Reverse index: file_path -> set of symbol name_lower keys in symbol_cache
    pub(crate) symbol_cache_by_file: HashMap<String, HashSet<String>>,
}

impl RepoIndex {
    /// Initialize a new canopy repository
    pub fn init(repo_root: &Path) -> crate::Result<()> {
        let canopy_dir = repo_root.join(".canopy");
        let config_path = canopy_dir.join("config.toml");

        if config_path.exists() {
            return Err(CanopyError::ConfigExists(config_path));
        }

        fs::create_dir_all(&canopy_dir)?;
        fs::write(&config_path, default_config_toml())?;

        // Add .canopy to .gitignore if not present
        update_gitignore(repo_root)?;

        // Create the database
        let db_path = canopy_dir.join("index.db");
        let conn = Connection::open(&db_path)?;
        Self::init_schema(&conn)?;

        Ok(())
    }

    /// Open an existing index, or initialize and then open if `.canopy` doesn't exist yet.
    pub fn open_or_init(repo_root: &Path) -> crate::Result<Self> {
        if !repo_root.join(".canopy").exists() {
            Self::init(repo_root)?;
        }
        Self::open(repo_root)
    }

    /// Open an existing index at `.canopy/index.db`. Returns `NotInitialized` if `.canopy` is missing.
    pub fn open(repo_root: &Path) -> crate::Result<Self> {
        let canopy_dir = repo_root.join(".canopy");
        let config_path = canopy_dir.join("config.toml");
        let db_path = canopy_dir.join("index.db");

        // Load config (use defaults if not present)
        let config = if config_path.exists() {
            Config::load(&config_path)?
        } else {
            // Check if .canopy exists at all
            if !canopy_dir.exists() {
                return Err(CanopyError::NotInitialized);
            }
            Config::default()
        };

        // Open database
        let conn = Connection::open(&db_path)?;

        // Initialize or migrate schema
        Self::init_schema(&conn)?;

        // Load symbol cache for O(1) lookups
        let (symbol_cache, symbol_cache_by_file) = Self::load_symbol_cache(&conn)?;

        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            conn,
            config,
            symbol_cache,
            symbol_cache_by_file,
        })
    }

    fn init_schema(conn: &Connection) -> crate::Result<()> {
        // Enable WAL mode for concurrent access + mmap for faster reads
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -64000;
            PRAGMA foreign_keys = ON;
            PRAGMA mmap_size = 268435456;
            ",
        )?;

        // Check schema version
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        // Fail fast on older schema versions - require reindex
        if version != 0 && version != SCHEMA_VERSION {
            return Err(CanopyError::SchemaVersionMismatch {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }

        if version == 0 {
            // Fresh database, create schema v2
            conn.execute_batch(
                "
                -- File metadata for cache invalidation
                CREATE TABLE IF NOT EXISTS files (
                    id INTEGER PRIMARY KEY,
                    path TEXT UNIQUE NOT NULL,
                    content_hash BLOB NOT NULL,
                    mtime INTEGER NOT NULL,
                    indexed_at INTEGER NOT NULL,
                    token_count INTEGER NOT NULL
                );

                -- Nodes (sections, code blocks, paragraphs, functions, etc.)
                CREATE TABLE IF NOT EXISTS nodes (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
                    handle_id TEXT UNIQUE NOT NULL,
                    node_type INTEGER NOT NULL,
                    start_byte INTEGER NOT NULL,
                    end_byte INTEGER NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    token_count INTEGER NOT NULL,
                    metadata TEXT,
                    -- NEW COLUMNS in v2:
                    name TEXT,
                    name_lower TEXT COLLATE NOCASE,
                    parent_name TEXT,
                    parent_name_lower TEXT COLLATE NOCASE,
                    parent_handle_id TEXT,
                    preview TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_id);
                CREATE INDEX IF NOT EXISTS idx_nodes_handle ON nodes(handle_id);
                CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(node_type);
                CREATE INDEX IF NOT EXISTS idx_nodes_name_lower ON nodes(name_lower);
                CREATE INDEX IF NOT EXISTS idx_nodes_parent_name_lower ON nodes(parent_name_lower);
                CREATE INDEX IF NOT EXISTS idx_nodes_parent_handle ON nodes(parent_handle_id);

                -- FTS5 index for text search
                CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
                    content,
                    tokenize='unicode61'
                );

                -- Mapping from FTS rowid to node
                CREATE TABLE IF NOT EXISTS fts_node_map (
                    fts_rowid INTEGER PRIMARY KEY,
                    node_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE
                );

                -- References table (calls, imports, type refs)
                CREATE TABLE IF NOT EXISTS refs (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    name_lower TEXT COLLATE NOCASE,
                    qualifier TEXT,
                    ref_type TEXT NOT NULL,
                    source_node_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
                    span_start INTEGER NOT NULL,
                    span_end INTEGER NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    preview TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_refs_name_lower ON refs(name_lower);
                CREATE INDEX IF NOT EXISTS idx_refs_type ON refs(ref_type);
                CREATE INDEX IF NOT EXISTS idx_refs_source ON refs(source_node_id);
                CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id);

                -- Symbol FTS for fuzzy symbol search
                CREATE VIRTUAL TABLE IF NOT EXISTS symbol_fts USING fts5(
                    name,
                    tokenize='unicode61'
                );

                -- Mapping from symbol FTS rowid to node
                CREATE TABLE IF NOT EXISTS symbol_fts_map (
                    fts_rowid INTEGER PRIMARY KEY,
                    node_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE
                );

                PRAGMA user_version = 3;
                ",
            )?;
        }

        Ok(())
    }

    /// Get current config
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Query indexed content from a DSL string with full options.
    ///
    /// Prefer [`query_params`](Self::query_params) for structured input from MCP tools.
    pub fn query_with_options(
        &self,
        query_str: &str,
        options: QueryOptions,
    ) -> crate::Result<QueryResult> {
        let query = parse_query(query_str)?;
        execute_query_with_options(&query, self, options)
    }

    /// Query using simplified params API (recommended for MCP tools)
    ///
    /// Example:
    /// ```ignore
    /// let params = QueryParams::pattern("error").with_glob("src/*.rs");
    /// let result = index.query_params(params)?;
    /// ```
    pub fn query_params(&self, params: QueryParams) -> crate::Result<QueryResult> {
        let query = params.to_query()?;
        let options = params.to_options();
        execute_query_with_options(&query, self, options)
    }
}

/// Find the smallest node that encloses the given span
pub(crate) fn find_smallest_enclosing_node(
    ref_span: &std::ops::Range<usize>,
    nodes: &[(std::ops::Range<usize>, i64)],
) -> Option<i64> {
    nodes
        .iter()
        .filter(|(span, _)| span.start <= ref_span.start && ref_span.end <= span.end)
        .min_by_key(|(span, _)| span.end - span.start)
        .map(|(_, id)| *id)
}

/// Generate a preview for a reference span using the containing line
pub(crate) fn reference_preview(
    source: &str,
    span: &std::ops::Range<usize>,
    max_bytes: usize,
) -> String {
    let line_start = source[..span.start.min(source.len())]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_end = source[span.end.min(source.len())..]
        .find('\n')
        .map(|p| span.end + p)
        .unwrap_or(source.len());

    let preview_span = line_start..line_end;
    generate_preview(source, &preview_span, max_bytes)
}

/// Update .gitignore to include .canopy/
fn update_gitignore(repo_root: &Path) -> crate::Result<()> {
    let gitignore_path = repo_root.join(".gitignore");

    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
        if !content
            .lines()
            .any(|line| line.trim() == ".canopy" || line.trim() == ".canopy/")
        {
            let mut file = fs::OpenOptions::new().append(true).open(&gitignore_path)?;
            use std::io::Write;
            writeln!(file, "\n# Canopy index\n.canopy/")?;
        }
    } else {
        fs::write(&gitignore_path, "# Canopy index\n.canopy/\n")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_helpers::setup_repo;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_sequential_path_indexes_small_batch() {
        let dir = setup_repo(5); // well below SEQUENTIAL_THRESHOLD (64)
        let mut index = RepoIndex::open(dir.path()).unwrap();
        let stats = index.index("**/*.rs").unwrap();

        assert_eq!(stats.files_indexed, 5);
        assert_eq!(stats.files_skipped, 0);
        assert!(stats.total_tokens > 0);

        // Reindex should skip all
        let stats2 = index.index("**/*.rs").unwrap();
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_skipped, 5);
    }

    #[test]
    fn test_pipeline_path_indexes_large_batch() {
        // Create > SEQUENTIAL_THRESHOLD files to force pipeline path
        let dir = setup_repo(80);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        let stats = index.index("**/*.rs").unwrap();

        assert_eq!(stats.files_indexed, 80);
        assert_eq!(stats.files_skipped, 0);
        assert!(stats.total_tokens > 0);

        // Reindex should skip all via mtime+TTL fast path
        let stats2 = index.index("**/*.rs").unwrap();
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_skipped, 80);

        // Verify symbols are queryable
        let results = index.search_code("func_0", 10).unwrap();
        assert!(
            !results.is_empty(),
            "should find func_0 after pipeline index"
        );
    }

    #[test]
    fn test_symbol_cache_by_file_consistency() {
        let dir = setup_repo(3);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        // Verify reverse index tracks all files
        assert!(
            !index.symbol_cache_by_file.is_empty(),
            "symbol_cache_by_file should be populated"
        );

        // Every file in reverse index should have matching entries in forward cache
        for (file_path, names) in &index.symbol_cache_by_file {
            for name in names {
                let entries = index
                    .symbol_cache
                    .get(name)
                    .expect("forward cache missing key");
                assert!(
                    entries.iter().any(|e| &e.file_path == file_path),
                    "forward cache for '{}' should contain entry for '{}'",
                    name,
                    file_path
                );
            }
        }

        // Invalidate one file and check consistency
        index.invalidate(Some("src/file_0.rs")).unwrap();

        // Reverse index should no longer have file_0
        assert!(
            !index.symbol_cache_by_file.contains_key("src/file_0.rs"),
            "reverse index should not contain invalidated file"
        );

        // Forward cache should not contain entries for file_0
        for entries in index.symbol_cache.values() {
            assert!(
                !entries.iter().any(|e| e.file_path == "src/file_0.rs"),
                "forward cache should not contain invalidated file entries"
            );
        }
    }

    #[test]
    fn test_mtime_captured_at_read_time() {
        let dir = setup_repo(1);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        // Read the stored mtime from DB
        let stored_mtime: i64 = index
            .conn
            .query_row("SELECT mtime FROM files LIMIT 1", [], |row| row.get(0))
            .unwrap();

        // mtime should be non-zero and reasonable (within last few seconds)
        assert!(stored_mtime > 0, "mtime should be non-zero");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (now - stored_mtime).abs() < 60,
            "mtime should be within last minute, got delta={}",
            now - stored_mtime
        );
    }

    #[test]
    fn test_invalidate_all_clears_reverse_index() {
        let dir = setup_repo(5);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();
        assert!(!index.symbol_cache_by_file.is_empty());

        index.invalidate(None).unwrap();
        assert!(index.symbol_cache.is_empty());
        assert!(index.symbol_cache_by_file.is_empty());
    }
}
