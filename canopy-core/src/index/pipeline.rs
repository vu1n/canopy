//! Indexing pipeline: sequential and parallel paths, DB insertion, batch flushing.

use super::symbol_cache::SymbolCacheEntry;
use super::RepoIndex;
use crate::document::{NodeType, ParsedFile};
use crate::handle::{generate_preview, HandleId};
use crate::parse::{estimate_tokens, file_mtime, parse_file_with_hash, warm_bpe};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::IndexStats;

/// Cached file metadata for batch skip checks during indexing
struct FileMetaCache {
    mtime: i64,
    hash: [u8; 32],
    indexed_at: i64,
    tokens: usize,
}

impl RepoIndex {
    /// Threshold: batches with <= this many files use sequential indexing
    pub(crate) const SEQUENTIAL_THRESHOLD: usize = 64;
    /// Number of parsed files per DB transaction in pipeline mode
    const BATCH_SIZE: usize = 500;

    /// Index files matching glob pattern.
    ///
    /// Dispatches to `index_sequential` for small batches or `index_pipeline`
    /// for large ones. The threshold is [`SEQUENTIAL_THRESHOLD`](Self::SEQUENTIAL_THRESHOLD).
    pub fn index(&mut self, glob: &str) -> crate::Result<IndexStats> {
        let files = self.walk_files(glob)?;

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let ttl_secs = self.config.ttl_duration().as_secs() as i64;

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

        if candidates.len() <= Self::SEQUENTIAL_THRESHOLD {
            self.index_sequential(&candidates, now_secs, ttl_secs)
        } else {
            self.index_pipeline(&candidates, now_secs, ttl_secs)
        }
    }

    /// Pipeline index path for large batches (> SEQUENTIAL_THRESHOLD files).
    ///
    /// Batch-loads metadata (single SELECT), partitions by mtime/TTL, then spawns
    /// rayon workers for parallel parse+hash with a bounded channel feeding a
    /// single-threaded DB writer.
    fn index_pipeline(
        &mut self,
        candidates: &[(PathBuf, String)],
        now_secs: i64,
        ttl_secs: i64,
    ) -> crate::Result<IndexStats> {
        // Amortize metadata lookup: single SELECT into HashMap vs N per-file queries
        let existing = self.batch_load_metadata()?;

        warm_bpe();

        let mut files_skipped = 0usize;
        let mut skipped_tokens = 0usize;
        let mut to_index: Vec<(PathBuf, String)> = Vec::new();

        for (file_path, relative_path) in candidates {
            if let Some(meta) = existing.get(relative_path.as_str()) {
                let current_mtime = file_mtime(file_path);

                if current_mtime == meta.mtime && (now_secs - meta.indexed_at) < ttl_secs {
                    files_skipped += 1;
                    skipped_tokens += meta.tokens;
                    continue;
                }
            }

            to_index.push((file_path.clone(), relative_path.clone()));
        }

        // Bounded channel (cap 64) provides backpressure so rayon workers don't
        // outpace the single-threaded DB writer.
        let (tx_ch, rx_ch) = crossbeam_channel::bounded::<(String, ParsedFile)>(64);

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

        // thread::scope lets rayon workers borrow `existing` and atomic counters
        let pipeline_result: crate::Result<()> = std::thread::scope(|s| {
            let producer_sender = tx_ch.clone();
            s.spawn(move || {
                to_index.par_iter().for_each_with(
                    producer_sender,
                    |sender, (file_path, relative_path)| {
                        if cancelled_ref.load(Ordering::Relaxed) {
                            return;
                        }

                        // mtime captured before read: prevents TOCTOU where hash
                        // reflects old content but mtime reflects new write
                        let mtime = file_mtime(file_path);

                        let source = match fs::read_to_string(file_path) {
                            Ok(s) => s,
                            Err(_) => return,
                        };

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

                        if cancelled_ref.load(Ordering::Relaxed) {
                            return;
                        }

                        let parsed = parse_file_with_hash(file_path, &source, &config, hash, mtime);
                        if sender.send((relative_path.clone(), parsed)).is_err() {
                            cancelled_ref.store(true, Ordering::Relaxed);
                        }
                    },
                );
            });

            drop(tx_ch);

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

        // Fold in hash-skip counters from rayon workers
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
            let row: Option<(i64, Vec<u8>, i64, i64)> = self
                .conn
                .query_row(
                    "SELECT mtime, content_hash, indexed_at, token_count FROM files WHERE path = ?",
                    params![relative_path],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;

            if let Some((db_mtime, _db_hash, indexed_at, db_tokens)) = &row {
                let current_mtime = file_mtime(file_path);

                if current_mtime == *db_mtime && (now_secs - indexed_at) < ttl_secs {
                    files_skipped += 1;
                    skipped_tokens += *db_tokens as usize;
                    continue;
                }
            }

            let mtime = file_mtime(file_path);

            let source = match fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

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
        symbol_cache: &mut HashMap<String, Vec<SymbolCacheEntry>>,
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

    /// Index a parsed file (single-file path with its own transaction)
    pub(crate) fn index_parsed_file(
        &mut self,
        relative_path: &str,
        parsed: &ParsedFile,
    ) -> crate::Result<()> {
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
        tx.execute("DELETE FROM files WHERE path = ?", params![relative_path])?;
        let mtime = parsed.mtime;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

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

        let mut new_cache_entries: Vec<(String, SymbolCacheEntry)> = Vec::new();

        for node in &parsed.nodes {
            let handle_id = HandleId::new(relative_path, node.node_type, &node.span);
            let node_tokens = estimate_tokens(&parsed.source[node.span.clone()]);

            let name = node.metadata.searchable_name().map(String::from);
            let name_lower = name.as_ref().map(|n| n.to_lowercase());
            let preview = generate_preview(&parsed.source, &node.span, preview_bytes);

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

                // Only definition-like types go in the symbol cache (O(1) lookup path)
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

        // Span→node_id map lets us attribute each reference to its enclosing node
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

        for reference in &parsed.refs {
            let name_lower = reference.name.to_lowercase();

            // Find the smallest enclosing node for this reference
            let source_node_id = super::find_smallest_enclosing_node(&reference.span, &node_spans);

            let preview =
                super::reference_preview(&parsed.source, &reference.span, preview_bytes * 2);

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
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::setup_repo;
    use super::*;
    use std::io::Write;

    #[test]
    fn sequential_threshold_is_64() {
        assert_eq!(RepoIndex::SEQUENTIAL_THRESHOLD, 64);
    }

    #[test]
    fn batch_load_metadata_empty_db() {
        let dir = setup_repo(0);
        let index = RepoIndex::open(dir.path()).unwrap();
        let meta = index.batch_load_metadata().unwrap();
        assert!(meta.is_empty(), "empty index should have no metadata");
    }

    #[test]
    fn batch_load_metadata_after_indexing() {
        let dir = setup_repo(3);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        let meta = index.batch_load_metadata().unwrap();
        assert_eq!(meta.len(), 3, "should have metadata for 3 files");

        // Each entry should have valid data
        for (path, cache) in &meta {
            assert!(
                path.starts_with("src/"),
                "path should be relative: {}",
                path
            );
            assert!(cache.mtime > 0, "mtime should be positive");
            assert!(cache.tokens > 0, "tokens should be positive");
        }
    }

    #[test]
    fn index_stats_fields_correct() {
        let dir = setup_repo(5);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        let stats = index.index("**/*.rs").unwrap();

        assert_eq!(stats.files_indexed, 5);
        assert_eq!(stats.files_skipped, 0);
        assert!(stats.total_tokens > 0);
        assert!(stats.index_size_bytes > 0);
    }

    #[test]
    fn reindex_skips_unchanged_files() {
        let dir = setup_repo(3);
        let mut index = RepoIndex::open(dir.path()).unwrap();

        let stats1 = index.index("**/*.rs").unwrap();
        assert_eq!(stats1.files_indexed, 3);

        let stats2 = index.index("**/*.rs").unwrap();
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_skipped, 3);
        // Token count should be preserved from skipped files
        assert_eq!(stats2.total_tokens, stats1.total_tokens);
    }

    #[test]
    fn index_detects_modified_files() {
        let dir = setup_repo(2);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        // Modify one file (change content AND mtime by sleeping briefly)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let path = dir.path().join("src/file_0.rs");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "fn new_func() {{ /* changed */ }}").unwrap();

        let stats = index.index("**/*.rs").unwrap();
        // At least the modified file should be reindexed
        assert!(
            stats.files_indexed >= 1,
            "should reindex at least modified file, got {}",
            stats.files_indexed
        );
    }
}
