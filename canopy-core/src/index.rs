//! Repository index with SQLite FTS5

use crate::config::{Config, DEFAULT_CONFIG};
use crate::document::RefType;
use crate::document::{NodeType, ParsedFile};
use crate::error::CanopyError;
use crate::handle::{generate_preview, Handle, HandleId, HandleSource, RefHandle};
use crate::parse::{estimate_tokens, file_mtime, parse_file_with_hash, warm_bpe};
use crate::query::{
    execute_query, execute_query_with_options, parse_query, QueryOptions, QueryParams, QueryResult,
};
use ignore::WalkBuilder;
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// File discovery backend
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileDiscovery {
    Fd,
    Ripgrep,
    Ignore,
}

impl FileDiscovery {
    /// Detect the best available file discovery tool
    pub fn detect() -> Self {
        // Try fd first
        if Command::new("fd").arg("--version").output().is_ok() {
            return Self::Fd;
        }
        // Try ripgrep
        if Command::new("rg").arg("--version").output().is_ok() {
            return Self::Ripgrep;
        }
        // Fallback to ignore crate
        Self::Ignore
    }

    /// Get the name of the discovery tool
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fd => "fd",
            Self::Ripgrep => "ripgrep",
            Self::Ignore => "ignore-crate",
        }
    }
}

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

/// Symbol cache pair: (name_lower -> entries, file_path -> set of name_lower keys)
type SymbolCachePair = (
    std::collections::HashMap<String, Vec<SymbolCacheEntry>>,
    HashMap<String, HashSet<String>>,
);

/// Cached file metadata for batch skip checks during indexing
struct FileMetaCache {
    mtime: i64,
    hash: [u8; 32],
    indexed_at: i64,
    tokens: usize,
}

/// Expanded handle tuple: (handle_id, file_path, node_type, token_count, content).
pub type ExpandedHandleDetail = (String, String, NodeType, usize, String);
type ExpandedHandleDbRow = (String, i64, i64, i64, i64, Vec<u8>);

/// Cached symbol entry for O(1) lookups
#[derive(Clone)]
struct SymbolCacheEntry {
    handle_id: String,
    file_path: String,
    node_type: i32,
    start_byte: usize,
    end_byte: usize,
    line_start: usize,
    line_end: usize,
    token_count: usize,
    preview: String,
}

/// Repository index backed by SQLite
pub struct RepoIndex {
    repo_root: PathBuf,
    conn: Connection,
    config: Config,
    /// Symbol cache: name_lower -> entries (preloaded at open for O(1) lookups)
    symbol_cache: std::collections::HashMap<String, Vec<SymbolCacheEntry>>,
    /// Reverse index: file_path -> set of symbol name_lower keys in symbol_cache
    symbol_cache_by_file: HashMap<String, HashSet<String>>,
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

    /// Load symbol cache from database (preload for fast lookups)
    /// Returns (name_lower -> entries, file_path -> set of name_lower keys)
    fn load_symbol_cache(conn: &Connection) -> crate::Result<SymbolCachePair> {
        let mut cache: std::collections::HashMap<String, Vec<SymbolCacheEntry>> =
            std::collections::HashMap::new();
        let mut by_file: HashMap<String, HashSet<String>> = HashMap::new();

        // Only load code symbols (function, class, struct, method)
        let mut stmt = conn.prepare(
            "SELECT n.name_lower, n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.name_lower IS NOT NULL
               AND n.node_type IN (?, ?, ?, ?)",
        )?;

        let rows = stmt.query_map(
            params![
                NodeType::Function.as_int() as i32,
                NodeType::Class.as_int() as i32,
                NodeType::Struct.as_int() as i32,
                NodeType::Method.as_int() as i32,
            ],
            |row| {
                let name_lower: String = row.get(0)?;
                let handle_id: String = row.get(1)?;
                let file_path: String = row.get(2)?;
                let node_type: i32 = row.get(3)?;
                let start_byte: i64 = row.get(4)?;
                let end_byte: i64 = row.get(5)?;
                let line_start: i64 = row.get(6)?;
                let line_end: i64 = row.get(7)?;
                let token_count: i64 = row.get(8)?;
                let preview: Option<String> = row.get(9)?;

                Ok((
                    name_lower,
                    SymbolCacheEntry {
                        handle_id,
                        file_path,
                        node_type,
                        start_byte: start_byte as usize,
                        end_byte: end_byte as usize,
                        line_start: line_start as usize,
                        line_end: line_end as usize,
                        token_count: token_count as usize,
                        preview: preview.unwrap_or_else(|| "...".to_string()),
                    },
                ))
            },
        )?;

        for (name, entry) in rows.flatten() {
            by_file
                .entry(entry.file_path.clone())
                .or_default()
                .insert(name.clone());
            cache.entry(name).or_default().push(entry);
        }

        Ok((cache, by_file))
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

    /// Threshold: batches with <= this many files use sequential indexing
    const SEQUENTIAL_THRESHOLD: usize = 64;
    /// Number of parsed files per DB transaction in pipeline mode
    const BATCH_SIZE: usize = 500;

    /// Index files matching glob pattern
    pub fn index(&mut self, glob: &str) -> crate::Result<IndexStats> {
        // Walk files respecting .gitignore
        let files = self.walk_files(glob)?;

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let ttl_secs = self.config.ttl_duration().as_secs() as i64;

        // ── Decide path before loading metadata ──
        // Build candidate list with relative paths
        let candidates: Vec<(PathBuf, String)> = files
            .iter()
            .map(|file_path| {
                let relative_path = file_path
                    .strip_prefix(&self.repo_root)
                    .unwrap_or(file_path)
                    .to_string_lossy()
                    .to_string();
                (file_path.clone(), relative_path)
            })
            .collect();

        // For small batches, use per-file queries instead of full table scan
        if candidates.len() <= Self::SEQUENTIAL_THRESHOLD {
            return self.index_sequential(&candidates, now_secs, ttl_secs);
        }

        // ── Pipeline path: batch-load metadata (amortized over large set) ──
        let existing = self.batch_load_metadata()?;

        // Eagerly init BPE encoder before parallel work
        warm_bpe();

        // Partition: mtime+TTL fast-skip vs needs-reindex
        let mut files_skipped = 0usize;
        let mut skipped_tokens = 0usize;
        let mut to_index: Vec<(PathBuf, String)> = Vec::new();

        for (file_path, relative_path) in &candidates {
            if let Some(meta) = existing.get(relative_path) {
                let current_mtime = fs::metadata(file_path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                if current_mtime == meta.mtime && (now_secs - meta.indexed_at) < ttl_secs {
                    files_skipped += 1;
                    skipped_tokens += meta.tokens;
                    continue;
                }
            }

            to_index.push((file_path.clone(), relative_path.clone()));
        }

        // ── Pipeline path (large batch) ──
        let (tx_ch, rx_ch) = crossbeam_channel::bounded::<(String, ParsedFile)>(64);

        // Clone immutable data for rayon workers
        let config = self.config.clone();
        let existing_ref = &existing;

        let hash_skipped_count = AtomicUsize::new(0);
        let hash_skipped_tokens = AtomicUsize::new(0);
        let hash_skipped_count_ref = &hash_skipped_count;
        let hash_skipped_tokens_ref = &hash_skipped_tokens;

        // Cancellation flag: set by writer on DB error so producers stop early
        let cancelled = AtomicBool::new(false);
        let cancelled_ref = &cancelled;

        let mut files_indexed = 0usize;
        let mut indexed_tokens = 0usize;

        // Use std::thread::scope so we can share references to `existing` and atomics
        let pipeline_result: crate::Result<()> = std::thread::scope(|s| {
            // Producer thread: runs rayon par_iter internally
            let producer_sender = tx_ch.clone();
            s.spawn(move || {
                to_index.par_iter().for_each_with(
                    producer_sender,
                    |sender, (file_path, relative_path)| {
                        // Check cancellation before doing expensive work
                        if cancelled_ref.load(Ordering::Relaxed) {
                            return;
                        }

                        // Capture mtime before read to avoid TOCTOU race
                        let mtime = file_mtime(file_path);

                        let source = match fs::read_to_string(file_path) {
                            Ok(s) => s,
                            Err(_) => return,
                        };

                        // Hash-based skip
                        let mut hasher = Sha256::new();
                        hasher.update(source.as_bytes());
                        let hash: [u8; 32] = hasher.finalize().into();

                        if let Some(meta) = existing_ref.get(relative_path.as_str()) {
                            if meta.hash == hash {
                                hash_skipped_count_ref.fetch_add(1, Ordering::Relaxed);
                                hash_skipped_tokens_ref.fetch_add(meta.tokens, Ordering::Relaxed);
                                return;
                            }
                        }

                        // Check cancellation again before parsing
                        if cancelled_ref.load(Ordering::Relaxed) {
                            return;
                        }

                        let parsed = parse_file_with_hash(file_path, &source, &config, hash, mtime);
                        if sender.send((relative_path.clone(), parsed)).is_err() {
                            // Writer disconnected — signal other workers to stop
                            cancelled_ref.store(true, Ordering::Relaxed);
                        }
                    },
                );
                // producer_sender dropped here → receiver sees disconnect
            });

            // Drop our copy so only the producer thread's sender keeps the channel alive
            drop(tx_ch);

            // Writer: calling thread (owns &mut self)
            let preview_bytes = self.config.indexing.preview_bytes;
            let mut batch: Vec<(String, ParsedFile)> = Vec::with_capacity(Self::BATCH_SIZE);

            for item in rx_ch.iter() {
                batch.push(item);
                if batch.len() >= Self::BATCH_SIZE {
                    let result = Self::flush_batch(
                        &mut self.conn,
                        &mut self.symbol_cache,
                        &mut self.symbol_cache_by_file,
                        &mut batch,
                        preview_bytes,
                        &mut files_indexed,
                        &mut indexed_tokens,
                    );
                    if let Err(e) = result {
                        // Signal producers to stop, drain channel to unblock them
                        cancelled.store(true, Ordering::Relaxed);
                        drop(rx_ch);
                        return Err(e);
                    }
                }
            }
            if !batch.is_empty() {
                Self::flush_batch(
                    &mut self.conn,
                    &mut self.symbol_cache,
                    &mut self.symbol_cache_by_file,
                    &mut batch,
                    preview_bytes,
                    &mut files_indexed,
                    &mut indexed_tokens,
                )?;
            }

            Ok(())
        });

        pipeline_result?;

        // Incorporate hash-skipped counts
        files_skipped += hash_skipped_count.load(Ordering::Relaxed);
        skipped_tokens += hash_skipped_tokens.load(Ordering::Relaxed);

        let db_path = self.repo_root.join(".canopy").join("index.db");
        let index_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

        Ok(IndexStats {
            files_indexed,
            files_skipped,
            total_tokens: indexed_tokens + skipped_tokens,
            index_size_bytes,
        })
    }

    /// Sequential index path for small batches (≤ SEQUENTIAL_THRESHOLD files).
    /// Uses per-file DB queries instead of batch_load_metadata to avoid full table scan.
    fn index_sequential(
        &mut self,
        candidates: &[(PathBuf, String)],
        now_secs: i64,
        ttl_secs: i64,
    ) -> crate::Result<IndexStats> {
        warm_bpe();

        let mut files_indexed = 0usize;
        let mut files_skipped = 0usize;
        let mut indexed_tokens = 0usize;
        let mut skipped_tokens = 0usize;

        for (file_path, relative_path) in candidates {
            // Per-file metadata lookup (cheap for small batches)
            let row: Option<(i64, Vec<u8>, i64, i64)> = self
                .conn
                .query_row(
                    "SELECT mtime, content_hash, indexed_at, token_count FROM files WHERE path = ?",
                    params![relative_path],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;

            if let Some((db_mtime, _db_hash, indexed_at, db_tokens)) = &row {
                // Fast path: mtime unchanged + TTL valid
                let current_mtime = fs::metadata(file_path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                if current_mtime == *db_mtime && (now_secs - indexed_at) < ttl_secs {
                    files_skipped += 1;
                    skipped_tokens += *db_tokens as usize;
                    continue;
                }
            }

            // Capture mtime before read to avoid TOCTOU race
            let mtime = file_mtime(file_path);

            let source = match fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Hash-based skip
            let mut hasher = Sha256::new();
            hasher.update(source.as_bytes());
            let hash: [u8; 32] = hasher.finalize().into();

            if let Some((_db_mtime, db_hash, _indexed_at, db_tokens)) = &row {
                if db_hash.as_slice() == hash.as_slice() {
                    files_skipped += 1;
                    skipped_tokens += *db_tokens as usize;
                    continue;
                }
            }

            let parsed = parse_file_with_hash(file_path, &source, &self.config, hash, mtime);
            self.index_parsed_file(relative_path, &parsed)?;
            files_indexed += 1;
            indexed_tokens += parsed.total_tokens;
        }

        let db_path = self.repo_root.join(".canopy").join("index.db");
        let index_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

        Ok(IndexStats {
            files_indexed,
            files_skipped,
            total_tokens: indexed_tokens + skipped_tokens,
            index_size_bytes,
        })
    }

    /// Batch-load file metadata from DB for fast skip checks
    fn batch_load_metadata(&self) -> crate::Result<HashMap<String, FileMetaCache>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime, content_hash, indexed_at, token_count FROM files")?;

        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let mtime: i64 = row.get(1)?;
            let hash_blob: Vec<u8> = row.get(2)?;
            let indexed_at: i64 = row.get(3)?;
            let tokens: i64 = row.get(4)?;

            let mut hash = [0u8; 32];
            if hash_blob.len() == 32 {
                hash.copy_from_slice(&hash_blob);
            }

            Ok((
                path,
                FileMetaCache {
                    mtime,
                    hash,
                    indexed_at,
                    tokens: tokens as usize,
                },
            ))
        })?;

        let mut map = HashMap::new();
        for row in rows {
            let (path, meta) = row?;
            map.insert(path, meta);
        }
        Ok(map)
    }

    /// Flush a batch of parsed files in a single transaction
    fn flush_batch(
        conn: &mut Connection,
        symbol_cache: &mut std::collections::HashMap<String, Vec<SymbolCacheEntry>>,
        symbol_cache_by_file: &mut HashMap<String, HashSet<String>>,
        batch: &mut Vec<(String, ParsedFile)>,
        preview_bytes: usize,
        files_indexed: &mut usize,
        indexed_tokens: &mut usize,
    ) -> crate::Result<()> {
        let mut all_new_entries: Vec<(String, Vec<(String, SymbolCacheEntry)>)> = Vec::new();

        let tx = conn.transaction()?;
        for (relative_path, parsed) in batch.drain(..) {
            let entries =
                Self::index_parsed_file_in_tx(&tx, &relative_path, &parsed, preview_bytes)?;
            *files_indexed += 1;
            *indexed_tokens += parsed.total_tokens;
            all_new_entries.push((relative_path, entries));
        }
        tx.commit()?;

        // Apply cache only after successful commit
        for (relative_path, entries) in all_new_entries {
            Self::remove_file_from_symbol_cache(symbol_cache, symbol_cache_by_file, &relative_path);
            Self::add_to_symbol_cache(symbol_cache, symbol_cache_by_file, entries);
        }

        Ok(())
    }

    /// Walk files matching glob, respecting .gitignore
    /// Uses fd > ripgrep > ignore crate (in order of preference)
    pub fn walk_files(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let discovery = FileDiscovery::detect();

        match discovery {
            FileDiscovery::Fd => self.walk_files_fd(glob),
            FileDiscovery::Ripgrep => self.walk_files_rg(glob),
            FileDiscovery::Ignore => self.walk_files_ignore(glob),
        }
    }

    /// Walk files using fd (fastest)
    fn walk_files_fd(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let mut cmd = Command::new("fd");
        cmd.arg("--type").arg("f");
        cmd.arg("--hidden"); // Include hidden, let .gitignore handle it

        // Use glob pattern for filtering (supports directory patterns like **/auth/**/*.ts)
        // -p enables full path matching (not just filename)
        cmd.arg("--glob").arg("-p").arg(glob);

        // Add exclusions from config
        for pattern in &self.config.ignore.patterns {
            cmd.arg("--exclude").arg(pattern);
        }

        // Search in repo root
        cmd.arg(&self.repo_root);

        let output = cmd.output().map_err(CanopyError::Io)?;

        if !output.status.success() {
            // Fallback to ignore crate on error
            return self.walk_files_ignore(glob);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<PathBuf> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();

        Ok(files)
    }

    /// Walk files using ripgrep --files
    fn walk_files_rg(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let mut cmd = Command::new("rg");
        cmd.arg("--files");
        cmd.arg("--hidden"); // Include hidden, let .gitignore handle it

        // Use glob pattern for filtering (supports directory patterns like **/auth/**/*.ts)
        cmd.arg("--glob").arg(glob);

        // Add exclusions from config
        for pattern in &self.config.ignore.patterns {
            cmd.arg("--glob").arg(format!("!{}", pattern));
            cmd.arg("--glob").arg(format!("!{}/**", pattern));
        }

        // Search in repo root
        cmd.arg(&self.repo_root);

        let output = cmd.output().map_err(CanopyError::Io)?;

        if !output.status.success() {
            // Fallback to ignore crate on error
            return self.walk_files_ignore(glob);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<PathBuf> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();

        Ok(files)
    }

    /// Walk files using ignore crate (fallback)
    fn walk_files_ignore(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let mut builder = WalkBuilder::new(&self.repo_root);
        builder.hidden(false);
        builder.git_ignore(true);
        builder.git_global(true);
        builder.git_exclude(true);

        // Build glob matcher for inclusion
        let mut glob_builder = globset::GlobSetBuilder::new();
        glob_builder
            .add(globset::Glob::new(glob).map_err(|e| CanopyError::GlobPattern(e.to_string()))?);
        let glob_set = glob_builder
            .build()
            .map_err(|e| CanopyError::GlobPattern(e.to_string()))?;

        // Build glob matcher for custom ignore patterns
        let mut ignore_builder = globset::GlobSetBuilder::new();
        for pattern in &self.config.ignore.patterns {
            let glob_pattern = if pattern.contains('*') || pattern.contains('?') {
                pattern.clone()
            } else {
                format!("**/{}", pattern)
            };
            if let Ok(g) = globset::Glob::new(&glob_pattern) {
                ignore_builder.add(g);
            }
            if let Ok(g) = globset::Glob::new(&format!("**/{}/**", pattern)) {
                ignore_builder.add(g);
            }
        }
        let ignore_set = ignore_builder
            .build()
            .map_err(|e| CanopyError::GlobPattern(e.to_string()))?;

        let mut files = Vec::new();

        for entry in builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if path.is_dir() {
                continue;
            }

            let relative = path.strip_prefix(&self.repo_root).unwrap_or(path);

            if ignore_set.is_match(relative) {
                continue;
            }

            if glob_set.is_match(relative) {
                files.push(path.to_path_buf());
            }
        }

        Ok(files)
    }

    /// Remove a file's entries from the symbol cache using the reverse index (O(symbols in file))
    fn remove_file_from_symbol_cache(
        symbol_cache: &mut std::collections::HashMap<String, Vec<SymbolCacheEntry>>,
        symbol_cache_by_file: &mut HashMap<String, HashSet<String>>,
        file_path: &str,
    ) {
        if let Some(names) = symbol_cache_by_file.remove(file_path) {
            for name in &names {
                if let Some(entries) = symbol_cache.get_mut(name) {
                    entries.retain(|e| e.file_path != file_path);
                    if entries.is_empty() {
                        symbol_cache.remove(name);
                    }
                }
            }
        }
    }

    /// Add new symbol cache entries and update the reverse index
    fn add_to_symbol_cache(
        symbol_cache: &mut std::collections::HashMap<String, Vec<SymbolCacheEntry>>,
        symbol_cache_by_file: &mut HashMap<String, HashSet<String>>,
        entries: Vec<(String, SymbolCacheEntry)>,
    ) {
        for (name_lower, entry) in entries {
            symbol_cache_by_file
                .entry(entry.file_path.clone())
                .or_default()
                .insert(name_lower.clone());
            symbol_cache.entry(name_lower).or_default().push(entry);
        }
    }

    /// Index a parsed file (single-file path with its own transaction)
    fn index_parsed_file(&mut self, relative_path: &str, parsed: &ParsedFile) -> crate::Result<()> {
        let preview_bytes = self.config.indexing.preview_bytes;
        let tx = self.conn.transaction()?;
        let entries = Self::index_parsed_file_in_tx(&tx, relative_path, parsed, preview_bytes)?;
        tx.commit()?;

        Self::remove_file_from_symbol_cache(
            &mut self.symbol_cache,
            &mut self.symbol_cache_by_file,
            relative_path,
        );
        Self::add_to_symbol_cache(
            &mut self.symbol_cache,
            &mut self.symbol_cache_by_file,
            entries,
        );

        Ok(())
    }

    /// Index a parsed file within an existing transaction.
    /// Returns symbol cache entries to be applied after commit.
    fn index_parsed_file_in_tx(
        tx: &rusqlite::Transaction<'_>,
        relative_path: &str,
        parsed: &ParsedFile,
        preview_bytes: usize,
    ) -> crate::Result<Vec<(String, SymbolCacheEntry)>> {
        // Delete existing entry
        tx.execute("DELETE FROM files WHERE path = ?", params![relative_path])?;

        // Use mtime captured at read time (avoids TOCTOU race)
        let mtime = parsed.mtime;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Insert file
        tx.execute(
            "INSERT INTO files (path, content_hash, mtime, indexed_at, token_count)
             VALUES (?, ?, ?, ?, ?)",
            params![
                relative_path,
                parsed.content_hash.as_slice(),
                mtime,
                now,
                parsed.total_tokens as i64
            ],
        )?;

        let file_id = tx.last_insert_rowid();

        // Collect symbols for cache update
        let mut new_cache_entries: Vec<(String, SymbolCacheEntry)> = Vec::new();

        // Insert nodes
        for node in &parsed.nodes {
            let handle_id = HandleId::new(relative_path, node.node_type, &node.span);
            let node_tokens = estimate_tokens(&parsed.source[node.span.clone()]);

            // Extract name from metadata for fast lookup
            let name = node.metadata.searchable_name().map(String::from);
            let name_lower = name.as_ref().map(|n| n.to_lowercase());

            // Generate preview at index time
            let preview = generate_preview(&parsed.source, &node.span, preview_bytes);

            // Parent fields
            let parent_name: Option<&str> = node.parent_name.as_deref();
            let parent_name_lower = parent_name.map(|p| p.to_lowercase());
            let parent_handle_id = match (node.parent_node_type, node.parent_span.as_ref()) {
                (Some(parent_node_type), Some(parent_span)) => Some(
                    HandleId::new(relative_path, parent_node_type, parent_span)
                        .raw()
                        .to_string(),
                ),
                _ => None,
            };

            tx.execute(
                "INSERT INTO nodes (file_id, handle_id, node_type, start_byte, end_byte,
                                   line_start, line_end, token_count, metadata,
                                   name, name_lower, parent_name, parent_name_lower,
                                   parent_handle_id, preview)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    file_id,
                    handle_id.raw(),
                    node.node_type.as_int() as i32,
                    node.span.start as i64,
                    node.span.end as i64,
                    node.line_range.0 as i64,
                    node.line_range.1 as i64,
                    node_tokens as i64,
                    node.metadata.to_json(),
                    name,
                    name_lower.clone(),
                    parent_name,
                    parent_name_lower,
                    parent_handle_id,
                    preview.clone()
                ],
            )?;

            let node_id = tx.last_insert_rowid();

            // Index content in FTS5
            let content = &parsed.source[node.span.clone()];
            tx.execute(
                "INSERT INTO content_fts (content) VALUES (?)",
                params![content],
            )?;

            let fts_rowid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO fts_node_map (fts_rowid, node_id) VALUES (?, ?)",
                params![fts_rowid, node_id],
            )?;

            // Index symbol name in symbol_fts for fuzzy search
            if let Some(ref sym_name) = name {
                tx.execute(
                    "INSERT INTO symbol_fts (name) VALUES (?)",
                    params![sym_name],
                )?;
                let symbol_fts_rowid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO symbol_fts_map (fts_rowid, node_id) VALUES (?, ?)",
                    params![symbol_fts_rowid, node_id],
                )?;

                // Collect code symbols for cache
                if matches!(
                    node.node_type,
                    NodeType::Function | NodeType::Class | NodeType::Struct | NodeType::Method
                ) {
                    if let Some(ref nl) = name_lower {
                        new_cache_entries.push((
                            nl.clone(),
                            SymbolCacheEntry {
                                handle_id: handle_id.raw().to_string(),
                                file_path: relative_path.to_string(),
                                node_type: node.node_type.as_int() as i32,
                                start_byte: node.span.start,
                                end_byte: node.span.end,
                                line_start: node.line_range.0,
                                line_end: node.line_range.1,
                                token_count: node_tokens,
                                preview: preview.clone(),
                            },
                        ));
                    }
                }
            }
        }

        // Build a map of spans to node IDs for reference source mapping
        let node_spans: Vec<(std::ops::Range<usize>, i64)> = parsed
            .nodes
            .iter()
            .filter_map(|node| {
                let handle_id = HandleId::new(relative_path, node.node_type, &node.span);
                let node_id: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM nodes WHERE handle_id = ?",
                        params![handle_id.raw()],
                        |row| row.get(0),
                    )
                    .ok();
                node_id.map(|id| (node.span.clone(), id))
            })
            .collect();

        // Insert references
        for reference in &parsed.refs {
            let name_lower = reference.name.to_lowercase();

            // Find the smallest enclosing node for this reference
            let source_node_id = find_smallest_enclosing_node(&reference.span, &node_spans);

            let preview = reference_preview(&parsed.source, &reference.span, preview_bytes * 2);

            tx.execute(
                "INSERT INTO refs (file_id, name, name_lower, qualifier, ref_type,
                                  source_node_id, span_start, span_end, line_start, line_end, preview)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    file_id,
                    reference.name,
                    name_lower,
                    reference.qualifier,
                    reference.ref_type.as_str(),
                    source_node_id,
                    reference.span.start as i64,
                    reference.span.end as i64,
                    reference.line_range.0 as i64,
                    reference.line_range.1 as i64,
                    preview,
                ],
            )?;
        }

        Ok(new_cache_entries)
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
                return Err(CanopyError::StaleIndex { path });
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

    /// FTS5 search (used by query executor)
    pub fn fts_search(&self, query: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        // Escape FTS5 special characters
        let escaped = escape_fts5_query(query);

        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM content_fts fts
             JOIN fts_node_map m ON fts.rowid = m.fts_rowid
             JOIN nodes n ON m.node_id = n.id
             JOIN files f ON n.file_id = f.id
             WHERE fts.content MATCH ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(params![escaped, limit as i64], |row| {
                let handle_id: String = row.get(0)?;
                let file_path: String = row.get(1)?;
                let node_type_int: i32 = row.get(2)?;
                let start_byte: i64 = row.get(3)?;
                let end_byte: i64 = row.get(4)?;
                let line_start: i64 = row.get(5)?;
                let line_end: i64 = row.get(6)?;
                let token_count: i64 = row.get(7)?;
                let preview: Option<String> = row.get(8)?;

                Ok((
                    handle_id,
                    file_path,
                    node_type_int,
                    start_byte as usize,
                    end_byte as usize,
                    line_start as usize,
                    line_end as usize,
                    token_count as usize,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Chunk);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
    }

    /// Get all nodes of a specific type
    pub fn get_nodes_by_type(
        &self,
        node_type: NodeType,
        limit: usize,
    ) -> crate::Result<Vec<Handle>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.node_type = ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(params![node_type.as_int() as i32, limit as i64], |row| {
                let handle_id: String = row.get(0)?;
                let file_path: String = row.get(1)?;
                let node_type_int: i32 = row.get(2)?;
                let start_byte: i64 = row.get(3)?;
                let end_byte: i64 = row.get(4)?;
                let line_start: i64 = row.get(5)?;
                let line_end: i64 = row.get(6)?;
                let token_count: i64 = row.get(7)?;
                let preview: Option<String> = row.get(8)?;

                Ok((
                    handle_id,
                    file_path,
                    node_type_int,
                    start_byte as usize,
                    end_byte as usize,
                    line_start as usize,
                    line_end as usize,
                    token_count as usize,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Chunk);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
    }

    /// Search for sections by heading (fuzzy match)
    pub fn search_sections(&self, heading: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let pattern = format!("%{}%", heading.to_lowercase());

        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.node_type = ?
               AND LOWER(json_extract(n.metadata, '$.heading')) LIKE ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(
                params![NodeType::Section.as_int() as i32, pattern, limit as i64],
                |row| {
                    let handle_id: String = row.get(0)?;
                    let file_path: String = row.get(1)?;
                    let node_type_int: i32 = row.get(2)?;
                    let start_byte: i64 = row.get(3)?;
                    let end_byte: i64 = row.get(4)?;
                    let line_start: i64 = row.get(5)?;
                    let line_end: i64 = row.get(6)?;
                    let token_count: i64 = row.get(7)?;
                    let preview: Option<String> = row.get(8)?;

                    Ok((
                        handle_id,
                        file_path,
                        node_type_int,
                        start_byte as usize,
                        end_byte as usize,
                        line_start as usize,
                        line_end as usize,
                        token_count as usize,
                        preview.unwrap_or_else(|| "...".to_string()),
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Section);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
    }

    /// Search for code symbols by name
    pub fn search_code(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let symbol_lower = symbol.to_lowercase();

        // Fast path: check symbol cache first (O(1) lookup)
        if let Some(entries) = self.symbol_cache.get(&symbol_lower) {
            let handles: Vec<Handle> = entries
                .iter()
                .take(limit)
                .map(|e| {
                    let node_type =
                        NodeType::from_int(e.node_type as u8).unwrap_or(NodeType::Function);
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
                })
                .collect();

            if !handles.is_empty() {
                return Ok(handles);
            }
        }

        // Slow path: database query (for symbols not in cache, e.g., newly indexed)
        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.node_type IN (?, ?, ?, ?)
               AND n.name_lower = ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(
                params![
                    NodeType::Function.as_int() as i32,
                    NodeType::Class.as_int() as i32,
                    NodeType::Struct.as_int() as i32,
                    NodeType::Method.as_int() as i32,
                    symbol_lower,
                    limit as i64
                ],
                |row| {
                    let handle_id: String = row.get(0)?;
                    let file_path: String = row.get(1)?;
                    let node_type_int: i32 = row.get(2)?;
                    let start_byte: i64 = row.get(3)?;
                    let end_byte: i64 = row.get(4)?;
                    let line_start: i64 = row.get(5)?;
                    let line_end: i64 = row.get(6)?;
                    let token_count: i64 = row.get(7)?;
                    let preview: Option<String> = row.get(8)?;

                    Ok((
                        handle_id,
                        file_path,
                        node_type_int,
                        start_byte as usize,
                        end_byte as usize,
                        line_start as usize,
                        line_end as usize,
                        token_count as usize,
                        preview.unwrap_or_else(|| "...".to_string()),
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Function);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        if handles.is_empty() {
            return self.search_symbol_fuzzy(symbol, limit);
        }

        Ok(handles)
    }

    fn search_symbol_fuzzy(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let escaped = escape_fts5_query(symbol);
        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM symbol_fts fts
             JOIN symbol_fts_map m ON fts.rowid = m.fts_rowid
             JOIN nodes n ON m.node_id = n.id
             JOIN files f ON n.file_id = f.id
             WHERE fts.name MATCH ?
               AND n.node_type IN (?, ?, ?, ?)
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(
                params![
                    escaped,
                    NodeType::Function.as_int() as i32,
                    NodeType::Class.as_int() as i32,
                    NodeType::Struct.as_int() as i32,
                    NodeType::Method.as_int() as i32,
                    limit as i64
                ],
                |row| {
                    let handle_id: String = row.get(0)?;
                    let file_path: String = row.get(1)?;
                    let node_type_int: i32 = row.get(2)?;
                    let start_byte: i64 = row.get(3)?;
                    let end_byte: i64 = row.get(4)?;
                    let line_start: i64 = row.get(5)?;
                    let line_end: i64 = row.get(6)?;
                    let token_count: i64 = row.get(7)?;
                    let preview: Option<String> = row.get(8)?;

                    Ok((
                        handle_id,
                        file_path,
                        node_type_int,
                        start_byte as usize,
                        end_byte as usize,
                        line_start as usize,
                        line_end as usize,
                        token_count as usize,
                        preview.unwrap_or_else(|| "...".to_string()),
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Function);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
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

        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM content_fts fts
             JOIN fts_node_map m ON fts.rowid = m.fts_rowid
             JOIN nodes n ON m.node_id = n.id
             JOIN files f ON n.file_id = f.id
             WHERE fts.content MATCH ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(params![escaped], |row| {
                let handle_id: String = row.get(0)?;
                let file_path: String = row.get(1)?;
                let node_type_int: i32 = row.get(2)?;
                let start_byte: i64 = row.get(3)?;
                let end_byte: i64 = row.get(4)?;
                let line_start: i64 = row.get(5)?;
                let line_end: i64 = row.get(6)?;
                let token_count: i64 = row.get(7)?;
                let preview: Option<String> = row.get(8)?;

                Ok((
                    handle_id,
                    file_path,
                    node_type_int,
                    start_byte as usize,
                    end_byte as usize,
                    line_start as usize,
                    line_end as usize,
                    token_count as usize,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, file_path, _, _, _, _, _, _, _)| glob_matcher.is_match(file_path))
            .take(limit)
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Chunk);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
    }

    /// Search for children of a parent symbol
    pub fn search_children(&self, parent: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let parent_lower = parent.to_lowercase();

        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.parent_name_lower = ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(params![parent_lower, limit as i64], |row| {
                let handle_id: String = row.get(0)?;
                let file_path: String = row.get(1)?;
                let node_type_int: i32 = row.get(2)?;
                let start_byte: i64 = row.get(3)?;
                let end_byte: i64 = row.get(4)?;
                let line_start: i64 = row.get(5)?;
                let line_end: i64 = row.get(6)?;
                let token_count: i64 = row.get(7)?;
                let preview: Option<String> = row.get(8)?;

                Ok((
                    handle_id,
                    file_path,
                    node_type_int,
                    start_byte as usize,
                    end_byte as usize,
                    line_start as usize,
                    line_end as usize,
                    token_count as usize,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Method);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
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

        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.parent_name_lower = ?
               AND n.name_lower = ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(params![parent_lower, symbol_lower, limit as i64], |row| {
                let handle_id: String = row.get(0)?;
                let file_path: String = row.get(1)?;
                let node_type_int: i32 = row.get(2)?;
                let start_byte: i64 = row.get(3)?;
                let end_byte: i64 = row.get(4)?;
                let line_start: i64 = row.get(5)?;
                let line_end: i64 = row.get(6)?;
                let token_count: i64 = row.get(7)?;
                let preview: Option<String> = row.get(8)?;

                Ok((
                    handle_id,
                    file_path,
                    node_type_int,
                    start_byte as usize,
                    end_byte as usize,
                    line_start as usize,
                    line_end as usize,
                    token_count as usize,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Method);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
    }

    /// Search for symbol definitions (exact match on name_lower)
    pub fn search_definitions(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let symbol_lower = symbol.to_lowercase();

        // Fast path: check symbol cache first (O(1) lookup)
        if let Some(entries) = self.symbol_cache.get(&symbol_lower) {
            let handles: Vec<Handle> = entries
                .iter()
                .take(limit)
                .map(|e| {
                    let node_type =
                        NodeType::from_int(e.node_type as u8).unwrap_or(NodeType::Function);
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
                })
                .collect();

            if !handles.is_empty() {
                return Ok(handles);
            }
        }

        // Slow path: database query
        let mut stmt = self.conn.prepare(
            "SELECT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM nodes n
             JOIN files f ON n.file_id = f.id
             WHERE n.name_lower = ?
               AND n.node_type IN (?, ?, ?, ?)
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(
                params![
                    symbol_lower,
                    NodeType::Function.as_int() as i32,
                    NodeType::Class.as_int() as i32,
                    NodeType::Struct.as_int() as i32,
                    NodeType::Method.as_int() as i32,
                    limit as i64
                ],
                |row| {
                    let handle_id: String = row.get(0)?;
                    let file_path: String = row.get(1)?;
                    let node_type_int: i32 = row.get(2)?;
                    let start_byte: i64 = row.get(3)?;
                    let end_byte: i64 = row.get(4)?;
                    let line_start: i64 = row.get(5)?;
                    let line_end: i64 = row.get(6)?;
                    let token_count: i64 = row.get(7)?;
                    let preview: Option<String> = row.get(8)?;

                    Ok((
                        handle_id,
                        file_path,
                        node_type_int,
                        start_byte as usize,
                        end_byte as usize,
                        line_start as usize,
                        line_end as usize,
                        token_count as usize,
                        preview.unwrap_or_else(|| "...".to_string()),
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Function);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
    }

    /// Search for source nodes containing references to a symbol
    pub fn search_reference_sources(
        &self,
        symbol: &str,
        limit: usize,
    ) -> crate::Result<Vec<Handle>> {
        let symbol_lower = symbol.to_lowercase();

        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte,
                    n.line_start, n.line_end, n.token_count, n.preview
             FROM refs r
             JOIN nodes n ON r.source_node_id = n.id
             JOIN files f ON n.file_id = f.id
             WHERE r.name_lower = ?
             LIMIT ?",
        )?;

        let handles: Vec<Handle> = stmt
            .query_map(params![symbol_lower, limit as i64], |row| {
                let handle_id: String = row.get(0)?;
                let file_path: String = row.get(1)?;
                let node_type_int: i32 = row.get(2)?;
                let start_byte: i64 = row.get(3)?;
                let end_byte: i64 = row.get(4)?;
                let line_start: i64 = row.get(5)?;
                let line_end: i64 = row.get(6)?;
                let token_count: i64 = row.get(7)?;
                let preview: Option<String> = row.get(8)?;

                Ok((
                    handle_id,
                    file_path,
                    node_type_int,
                    start_byte as usize,
                    end_byte as usize,
                    line_start as usize,
                    line_end as usize,
                    token_count as usize,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    handle_id,
                    file_path,
                    node_type_int,
                    start,
                    end,
                    line_start,
                    line_end,
                    tokens,
                    preview,
                )| {
                    let node_type =
                        NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Function);
                    let span = start..end;

                    Handle {
                        id: HandleId::from_raw(handle_id),
                        file_path,
                        node_type,
                        span,
                        line_range: (line_start, line_end),
                        token_count: tokens,
                        preview,
                        content: None,
                        source: HandleSource::Local,
                        commit_sha: None,
                        generation: None,
                    }
                },
            )
            .collect();

        Ok(handles)
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
