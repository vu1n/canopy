//! Repository index with SQLite FTS5

mod file_discovery;
mod pipeline;
pub(crate) mod symbol_cache;

pub use file_discovery::FileDiscovery;

use crate::config::{Config, DEFAULT_CONFIG};
use crate::document::{NodeType, RefType};
use crate::error::CanopyError;
use crate::handle::{generate_preview, Handle, HandleId, HandleSource, RefHandle};
use crate::query::{
    execute_query, execute_query_with_options, parse_query, QueryOptions, QueryParams, QueryResult,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use symbol_cache::SymbolCacheEntry;

const SCHEMA_VERSION: i32 = 3;

/// Shared column list for handle queries — matches the `handle_from_row` column order.
const HANDLE_SELECT: &str = "n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte, \
     n.line_start, n.line_end, n.token_count, n.preview";

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

/// Expanded handle tuple: (handle_id, file_path, node_type, token_count, content).
pub type ExpandedHandleDetail = (String, String, NodeType, usize, String);
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
        fs::write(&config_path, DEFAULT_CONFIG)?;

        // Add .canopy to .gitignore if not present
        update_gitignore(repo_root)?;

        // Create the database
        let db_path = canopy_dir.join("index.db");
        let conn = Connection::open(&db_path)?;
        Self::init_schema(&conn)?;

        Ok(())
    }

    /// Open or create index at .canopy/index.db
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

    /// Initialize database schema
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

    /// Query indexed content
    pub fn query(&self, query_str: &str, limit: Option<usize>) -> crate::Result<QueryResult> {
        let query = parse_query(query_str)?;
        execute_query(&query, self, limit)
    }

    /// Query indexed content with full options including expand_budget
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

    /// Expand handles to full content
    pub fn expand(&self, handle_ids: &[String]) -> crate::Result<Vec<(String, String)>> {
        let expanded = self.expand_with_details(handle_ids)?;
        Ok(expanded
            .into_iter()
            .map(|(handle_id, _file_path, _node_type, _token_count, content)| (handle_id, content))
            .collect())
    }

    /// Expand handles to full content, including file path for analytics.
    pub fn expand_with_paths(
        &self,
        handle_ids: &[String],
    ) -> crate::Result<Vec<(String, String, String)>> {
        let expanded = self.expand_with_details(handle_ids)?;
        Ok(expanded
            .into_iter()
            .map(
                |(handle_id, file_path, _node_type, _token_count, content)| {
                    (handle_id, file_path, content)
                },
            )
            .collect())
    }

    /// Expand handles to full content with metadata for feedback/analytics.
    pub fn expand_with_details(
        &self,
        handle_ids: &[String],
    ) -> crate::Result<Vec<ExpandedHandleDetail>> {
        let mut results = Vec::new();

        for handle_id_str in handle_ids {
            let handle_id: HandleId = handle_id_str.parse()?;

            // Get node info
            let row: Option<ExpandedHandleDbRow> = self
                .conn
                .query_row(
                    "SELECT f.path, n.start_byte, n.end_byte, n.node_type, n.token_count, f.content_hash
                     FROM nodes n
                     JOIN files f ON n.file_id = f.id
                     WHERE n.handle_id = ?",
                    params![handle_id.raw()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .optional()?;

            let Some((path, start, end, node_type_int, token_count, db_hash)) = row else {
                return Err(CanopyError::HandleNotFound(handle_id.to_string()));
            };

            // Read file and verify hash
            let full_path = self.repo_root.join(&path);
            let source = fs::read_to_string(&full_path)?;

            let mut hasher = Sha256::new();
            hasher.update(source.as_bytes());
            let current_hash: [u8; 32] = hasher.finalize().into();

            if db_hash != current_hash.as_slice() {
                return Err(CanopyError::StaleIndex {
                    path: PathBuf::from(path),
                });
            }

            // Extract content
            let start = start as usize;
            let end = (end as usize).min(source.len());
            let content = source[start..end].to_string();
            let node_type = NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Chunk);

            results.push((
                handle_id.to_string(),
                path,
                node_type,
                token_count as usize,
                content,
            ));
        }

        Ok(results)
    }

    /// Get index status
    pub fn status(&self) -> crate::Result<IndexStatus> {
        let files_indexed: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;

        let total_tokens: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(token_count), 0) FROM files",
            [],
            |row| row.get(0),
        )?;

        let last_indexed: Option<i64> = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM files", [], |row| row.get(0))
            .optional()?
            .flatten();

        let db_path = self.repo_root.join(".canopy").join("index.db");
        let index_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

        let last_indexed_str = last_indexed.map(|ts| {
            let duration = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - ts;

            if duration < 60 {
                format!("{} seconds ago", duration)
            } else if duration < 3600 {
                format!("{} minutes ago", duration / 60)
            } else if duration < 86400 {
                format!("{} hours ago", duration / 3600)
            } else {
                format!("{} days ago", duration / 86400)
            }
        });

        Ok(IndexStatus {
            files_indexed: files_indexed as usize,
            total_tokens: total_tokens as usize,
            schema_version: SCHEMA_VERSION,
            index_size_bytes,
            last_indexed: last_indexed_str,
        })
    }

    /// Invalidate cached entries
    pub fn invalidate(&mut self, glob: Option<&str>) -> crate::Result<usize> {
        match glob {
            Some(pattern) => {
                // Build glob matcher
                let glob_matcher = globset::Glob::new(pattern)
                    .map_err(|e| CanopyError::GlobPattern(e.to_string()))?
                    .compile_matcher();

                // Get all file paths
                let mut stmt = self.conn.prepare("SELECT id, path FROM files")?;
                let rows: Vec<(i64, String)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect();

                let mut count = 0;
                let mut deleted_paths: Vec<String> = Vec::new();
                for (id, path) in rows {
                    if glob_matcher.is_match(&path) {
                        self.conn
                            .execute("DELETE FROM files WHERE id = ?", params![id])?;
                        deleted_paths.push(path);
                        count += 1;
                    }
                }

                // Clean up orphaned symbol_fts rows (symbol_fts_map rows are removed via FK)
                self.conn.execute(
                    "DELETE FROM symbol_fts WHERE rowid NOT IN (SELECT fts_rowid FROM symbol_fts_map)",
                    [],
                )?;

                // Remove invalidated entries from symbol cache
                for path in &deleted_paths {
                    Self::remove_file_from_symbol_cache(
                        &mut self.symbol_cache,
                        &mut self.symbol_cache_by_file,
                        path,
                    );
                }

                Ok(count)
            }
            None => {
                // Delete all
                let count: i64 = self
                    .conn
                    .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;

                self.conn.execute("DELETE FROM files", [])?;
                self.conn.execute("DELETE FROM content_fts", [])?;
                self.conn.execute("DELETE FROM fts_node_map", [])?;
                self.conn.execute("DELETE FROM refs", [])?;
                self.conn.execute("DELETE FROM symbol_fts", [])?;
                self.conn.execute("DELETE FROM symbol_fts_map", [])?;

                // Clear symbol cache
                self.symbol_cache.clear();
                self.symbol_cache_by_file.clear();

                Ok(count as usize)
            }
        }
    }

    /// Execute a handle query and collect results.
    fn query_handles(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
    ) -> crate::Result<Vec<Handle>> {
        let mut stmt = self.conn.prepare(sql)?;
        let handles = stmt
            .query_map(params, handle_from_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(handles)
    }

    /// FTS5 search (used by query executor)
    pub fn fts_search(&self, query: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let escaped = escape_fts5_query(query);
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM content_fts fts
                 JOIN fts_node_map m ON fts.rowid = m.fts_rowid
                 JOIN nodes n ON m.node_id = n.id
                 JOIN files f ON n.file_id = f.id
                 WHERE fts.content MATCH ?
                 LIMIT ?"
            ),
            &[&escaped as &dyn rusqlite::types::ToSql, &limit],
        )
    }

    /// Get all nodes of a specific type
    pub fn get_nodes_by_type(
        &self,
        node_type: NodeType,
        limit: usize,
    ) -> crate::Result<Vec<Handle>> {
        let nt = node_type.as_int() as i32;
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM nodes n JOIN files f ON n.file_id = f.id
                 WHERE n.node_type = ? LIMIT ?"
            ),
            &[&nt as &dyn rusqlite::types::ToSql, &limit],
        )
    }

    /// Search for sections by heading (fuzzy match)
    pub fn search_sections(&self, heading: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let pattern = format!("%{}%", heading.to_lowercase());
        let nt = NodeType::Section.as_int() as i32;
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM nodes n JOIN files f ON n.file_id = f.id
                 WHERE n.node_type = ?
                   AND LOWER(json_extract(n.metadata, '$.heading')) LIKE ?
                 LIMIT ?"
            ),
            &[&nt as &dyn rusqlite::types::ToSql, &pattern, &limit],
        )
    }

    /// Search for code symbols by name
    pub fn search_code(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let symbol_lower = symbol.to_lowercase();

        // Fast path: check symbol cache first (O(1) lookup)
        if let Some(entries) = self.symbol_cache.get(&symbol_lower) {
            let handles: Vec<Handle> = entries
                .iter()
                .take(limit)
                .map(handle_from_cache_entry)
                .collect();
            if !handles.is_empty() {
                return Ok(handles);
            }
        }

        // Slow path: database query (for symbols not in cache, e.g., newly indexed)
        let code_types = code_type_params();
        let limit_i64 = limit as i64;
        let handles = self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM nodes n JOIN files f ON n.file_id = f.id
                 WHERE n.node_type IN (?, ?, ?, ?) AND n.name_lower = ?
                 LIMIT ?"
            ),
            &[
                &code_types[0] as &dyn rusqlite::types::ToSql,
                &code_types[1],
                &code_types[2],
                &code_types[3],
                &symbol_lower,
                &limit_i64,
            ],
        )?;

        if handles.is_empty() {
            return self.search_symbol_fuzzy(symbol, limit);
        }

        Ok(handles)
    }

    fn search_symbol_fuzzy(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let escaped = escape_fts5_query(symbol);
        let code_types = code_type_params();
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM symbol_fts fts
                 JOIN symbol_fts_map m ON fts.rowid = m.fts_rowid
                 JOIN nodes n ON m.node_id = n.id
                 JOIN files f ON n.file_id = f.id
                 WHERE fts.name MATCH ? AND n.node_type IN (?, ?, ?, ?)
                 LIMIT ?"
            ),
            &[
                &escaped as &dyn rusqlite::types::ToSql,
                &code_types[0],
                &code_types[1],
                &code_types[2],
                &code_types[3],
                &limit,
            ],
        )
    }

    /// Get file as a single handle
    pub fn get_file(&self, path_pattern: &str) -> crate::Result<Vec<Handle>> {
        let glob_matcher = globset::Glob::new(path_pattern)
            .map_err(|e| CanopyError::GlobPattern(e.to_string()))?
            .compile_matcher();

        let mut stmt = self
            .conn
            .prepare("SELECT f.path, f.token_count FROM files f")?;

        let matches: Vec<(String, usize)> = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let tokens: i64 = row.get(1)?;
                Ok((path, tokens as usize))
            })?
            .filter_map(|r| r.ok())
            .filter(|(path, _)| glob_matcher.is_match(path))
            .collect();

        let mut handles = Vec::new();
        for (file_path, token_count) in matches {
            // Read file to get line count and preview
            let full_path = self.repo_root.join(&file_path);
            if let Ok(source) = fs::read_to_string(&full_path) {
                let line_count = source.lines().count().max(1);
                let span = 0..source.len();
                let preview = generate_preview(&source, &span, self.config.indexing.preview_bytes);

                handles.push(Handle {
                    id: HandleId::new(&file_path, NodeType::Chunk, &span),
                    file_path,
                    node_type: NodeType::Chunk,
                    span,
                    line_range: (1, line_count),
                    token_count,
                    preview,
                    content: None,
                    source: HandleSource::Local,
                    commit_sha: None,
                    generation: None,
                });
            }
        }

        Ok(handles)
    }

    /// Search within specific files (in-file query)
    pub fn search_in_files(
        &self,
        glob: &str,
        fts_query: &str,
        limit: usize,
    ) -> crate::Result<Vec<Handle>> {
        let glob_matcher = globset::Glob::new(glob)
            .map_err(|e| CanopyError::GlobPattern(e.to_string()))?
            .compile_matcher();
        let escaped = escape_fts5_query(fts_query);

        // Can't use query_handles here — need post-query glob + take(limit) filtering
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {HANDLE_SELECT}
             FROM content_fts fts
             JOIN fts_node_map m ON fts.rowid = m.fts_rowid
             JOIN nodes n ON m.node_id = n.id
             JOIN files f ON n.file_id = f.id
             WHERE fts.content MATCH ?"
        ))?;

        let handles: Vec<Handle> = stmt
            .query_map(params![escaped], handle_from_row)?
            .filter_map(|r| r.ok())
            .filter(|h| glob_matcher.is_match(&h.file_path))
            .take(limit)
            .collect();

        Ok(handles)
    }

    /// Search for children of a parent symbol
    pub fn search_children(&self, parent: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let parent_lower = parent.to_lowercase();
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM nodes n JOIN files f ON n.file_id = f.id
                 WHERE n.parent_name_lower = ? LIMIT ?"
            ),
            &[&parent_lower as &dyn rusqlite::types::ToSql, &limit],
        )
    }

    /// Search for named children of a parent symbol
    pub fn search_children_named(
        &self,
        parent: &str,
        symbol: &str,
        limit: usize,
    ) -> crate::Result<Vec<Handle>> {
        let parent_lower = parent.to_lowercase();
        let symbol_lower = symbol.to_lowercase();
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM nodes n JOIN files f ON n.file_id = f.id
                 WHERE n.parent_name_lower = ? AND n.name_lower = ?
                 LIMIT ?"
            ),
            &[
                &parent_lower as &dyn rusqlite::types::ToSql,
                &symbol_lower,
                &limit,
            ],
        )
    }

    /// Search for symbol definitions (exact match on name_lower)
    pub fn search_definitions(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let symbol_lower = symbol.to_lowercase();

        // Fast path: check symbol cache first (O(1) lookup)
        if let Some(entries) = self.symbol_cache.get(&symbol_lower) {
            let handles: Vec<Handle> = entries
                .iter()
                .take(limit)
                .map(handle_from_cache_entry)
                .collect();
            if !handles.is_empty() {
                return Ok(handles);
            }
        }

        // Slow path: database query
        let code_types = code_type_params();
        let limit_i64 = limit as i64;
        self.query_handles(
            &format!(
                "SELECT {HANDLE_SELECT}
                 FROM nodes n JOIN files f ON n.file_id = f.id
                 WHERE n.name_lower = ? AND n.node_type IN (?, ?, ?, ?)
                 LIMIT ?"
            ),
            &[
                &symbol_lower as &dyn rusqlite::types::ToSql,
                &code_types[0],
                &code_types[1],
                &code_types[2],
                &code_types[3],
                &limit_i64,
            ],
        )
    }

    /// Search for source nodes containing references to a symbol
    pub fn search_reference_sources(
        &self,
        symbol: &str,
        limit: usize,
    ) -> crate::Result<Vec<Handle>> {
        let symbol_lower = symbol.to_lowercase();
        let limit = limit as i64;
        self.query_handles(
            &format!(
                "SELECT DISTINCT {HANDLE_SELECT}
                 FROM refs r
                 JOIN nodes n ON r.source_node_id = n.id
                 JOIN files f ON n.file_id = f.id
                 WHERE r.name_lower = ? LIMIT ?"
            ),
            &[&symbol_lower as &dyn rusqlite::types::ToSql, &limit],
        )
    }

    /// Search for references to a symbol (returns RefHandles)
    pub fn search_references(&self, symbol: &str, limit: usize) -> crate::Result<Vec<RefHandle>> {
        let symbol_lower = symbol.to_lowercase();

        let mut stmt = self.conn.prepare(
            "SELECT f.path, r.span_start, r.span_end, r.line_start, r.line_end,
                    r.name, r.qualifier, r.ref_type, n.handle_id, r.preview
             FROM refs r
             JOIN files f ON r.file_id = f.id
             LEFT JOIN nodes n ON r.source_node_id = n.id
             WHERE r.name_lower = ?
             LIMIT ?",
        )?;

        let refs: Vec<RefHandle> = stmt
            .query_map(params![symbol_lower, limit as i64], |row| {
                let file_path: String = row.get(0)?;
                let span_start: i64 = row.get(1)?;
                let span_end: i64 = row.get(2)?;
                let line_start: i64 = row.get(3)?;
                let line_end: i64 = row.get(4)?;
                let name: String = row.get(5)?;
                let qualifier: Option<String> = row.get(6)?;
                let ref_type_str: String = row.get(7)?;
                let source_handle_id: Option<String> = row.get(8)?;
                let preview: Option<String> = row.get(9)?;

                Ok((
                    file_path,
                    span_start as usize,
                    span_end as usize,
                    line_start as usize,
                    line_end as usize,
                    name,
                    qualifier,
                    ref_type_str,
                    source_handle_id,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    file_path,
                    span_start,
                    span_end,
                    line_start,
                    line_end,
                    name,
                    qualifier,
                    ref_type_str,
                    source_handle_id,
                    preview,
                )| {
                    let ref_type = RefType::parse(&ref_type_str).unwrap_or(RefType::Call);
                    let span = span_start..span_end;

                    RefHandle {
                        file_path,
                        span,
                        line_range: (line_start, line_end),
                        name,
                        qualifier,
                        ref_type,
                        source_handle: source_handle_id.map(HandleId::from_raw),
                        preview,
                    }
                },
            )
            .collect();

        Ok(refs)
    }

    /// Get default result limit
    pub fn default_limit(&self) -> usize {
        self.config.core.default_result_limit
    }
}

/// The four code-like node types used by symbol search queries.
fn code_type_params() -> [i32; 4] {
    [
        NodeType::Function.as_int() as i32,
        NodeType::Class.as_int() as i32,
        NodeType::Struct.as_int() as i32,
        NodeType::Method.as_int() as i32,
    ]
}

/// Construct a Handle from a SymbolCacheEntry (used by cache fast paths).
fn handle_from_cache_entry(e: &SymbolCacheEntry) -> Handle {
    let node_type = NodeType::from_int(e.node_type as u8).unwrap_or(NodeType::Function);
    let span = e.start_byte..e.end_byte;
    Handle {
        id: HandleId::from_raw(e.handle_id.clone()),
        file_path: e.file_path.clone(),
        node_type,
        span,
        line_range: (e.line_start, e.line_end),
        token_count: e.token_count,
        preview: e.preview.clone(),
        content: None,
        source: HandleSource::Local,
        commit_sha: None,
        generation: None,
    }
}

/// Construct a Handle from a standard 9-column DB row:
/// (handle_id, path, node_type, start_byte, end_byte, line_start, line_end, token_count, preview)
fn handle_from_row(row: &rusqlite::Row) -> rusqlite::Result<Handle> {
    let handle_id: String = row.get(0)?;
    let file_path: String = row.get(1)?;
    let node_type_int: i32 = row.get(2)?;
    let start_byte: i64 = row.get(3)?;
    let end_byte: i64 = row.get(4)?;
    let line_start: i64 = row.get(5)?;
    let line_end: i64 = row.get(6)?;
    let token_count: i64 = row.get(7)?;
    let preview: Option<String> = row.get(8)?;

    let node_type = NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Chunk);
    let span = (start_byte as usize)..(end_byte as usize);

    Ok(Handle {
        id: HandleId::from_raw(handle_id),
        file_path,
        node_type,
        span,
        line_range: (line_start as usize, line_end as usize),
        token_count: token_count as usize,
        preview: preview.unwrap_or_else(|| "...".to_string()),
        content: None,
        source: HandleSource::Local,
        commit_sha: None,
        generation: None,
    })
}

/// Find the smallest node that encloses the given span
fn find_smallest_enclosing_node(
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
fn reference_preview(source: &str, span: &std::ops::Range<usize>, max_bytes: usize) -> String {
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

/// Escape FTS5 special characters
fn escape_fts5_query(query: &str) -> String {
    // For simple queries, wrap in quotes if it contains special chars
    // FTS5 special chars: " ( ) - * < >
    if query.contains(['"', '(', ')', '-', '*', '<', '>']) {
        // Quote the entire query for literal search
        format!("\"{}\"", query.replace('"', "\"\""))
    } else {
        query.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Create a test repo with N Rust files and return the temp dir
    fn setup_repo(n: usize) -> TempDir {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();

        for i in 0..n {
            let path = src.join(format!("file_{}.rs", i));
            let mut f = fs::File::create(&path).unwrap();
            writeln!(
                f,
                "fn func_{i}() {{ println!(\"hello from {i}\"); }}\nstruct Struct{i} {{ x: i32 }}",
            )
            .unwrap();
        }

        RepoIndex::init(dir.path()).unwrap();
        dir
    }

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
