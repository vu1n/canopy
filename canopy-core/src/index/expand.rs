//! Handle expansion, index status, and invalidation.

use crate::document::NodeType;
use crate::error::CanopyError;
use crate::handle::HandleId;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{ExpandedHandleDbRow, ExpandedHandleDetail, IndexStatus, RepoIndex, SCHEMA_VERSION};

impl RepoIndex {
    /// Expand handles to full content
    pub fn expand(&self, handle_ids: &[String]) -> crate::Result<Vec<(String, String)>> {
        let expanded = self.expand_with_details(handle_ids)?;
        Ok(expanded
            .into_iter()
            .map(|d| (d.handle_id, d.content))
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
            let source = std::fs::read_to_string(&full_path)?;

            let mut hasher = Sha256::new();
            hasher.update(source.as_bytes());
            let current_hash: [u8; 32] = hasher.finalize().into();

            if db_hash != current_hash.as_slice() {
                return Err(CanopyError::StaleIndex {
                    path: PathBuf::from(path),
                });
            }

            // Extract content (clamp i64 → usize to avoid wrapping on corrupt DB data)
            let start = start.max(0) as usize;
            let end = (end.max(0) as usize).min(source.len());
            let content = source[start..end].to_string();
            let node_type = NodeType::from_int(node_type_int as u8).unwrap_or(NodeType::Chunk);

            results.push(ExpandedHandleDetail {
                handle_id: handle_id.to_string(),
                file_path: path,
                node_type,
                token_count: token_count.max(0) as usize,
                content,
            });
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
        let index_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

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
            files_indexed: files_indexed.max(0) as usize,
            total_tokens: total_tokens.max(0) as usize,
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
                let rows: Vec<(i64, String)> = super::search::collect_row_results(
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?,
                )?;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::test_helpers::setup_repo;

    #[test]
    fn expand_indexed_handle_returns_content() {
        let dir = setup_repo(2);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        // Search for a known function to get its handle
        let handles = index.search_code("func_0", 10).unwrap();
        assert!(!handles.is_empty(), "should find func_0 after indexing");

        let handle_id = handles[0].id.to_string();
        let expanded = index.expand(&[handle_id.clone()]).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].0, handle_id);
        assert!(
            expanded[0].1.contains("func_0"),
            "expanded content should contain func_0, got: {}",
            expanded[0].1
        );
    }

    #[test]
    fn expand_with_details_returns_metadata() {
        let dir = setup_repo(1);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        let handles = index.search_code("func_0", 10).unwrap();
        assert!(!handles.is_empty());

        let handle_id = handles[0].id.to_string();
        let details = index.expand_with_details(&[handle_id.clone()]).unwrap();
        assert_eq!(details.len(), 1);

        let detail = &details[0];
        assert_eq!(detail.handle_id, handle_id);
        assert!(detail.file_path.contains("file_0.rs"));
        assert_eq!(detail.node_type, NodeType::Function);
        assert!(detail.token_count > 0);
        assert!(detail.content.contains("func_0"));
    }

    #[test]
    fn expand_nonexistent_handle_returns_error() {
        let dir = setup_repo(1);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        // A valid HandleId format but one that does not exist in the DB
        let fake_id = "src/fake.rs::Function::0:100";
        let result = index.expand(&[fake_id.to_string()]);
        assert!(
            result.is_err(),
            "expanding a nonexistent handle should fail"
        );
    }

    #[test]
    fn status_reports_indexed_files() {
        let dir = setup_repo(3);
        let mut index = RepoIndex::open(dir.path()).unwrap();

        // Before indexing: zero files
        let status = index.status().unwrap();
        assert_eq!(status.files_indexed, 0);
        assert_eq!(status.total_tokens, 0);
        assert_eq!(status.schema_version, SCHEMA_VERSION);
        assert!(status.last_indexed.is_none());

        // After indexing
        index.index("**/*.rs").unwrap();
        let status = index.status().unwrap();
        assert_eq!(status.files_indexed, 3);
        assert!(status.total_tokens > 0);
        assert!(status.last_indexed.is_some());
        assert!(status.index_size_bytes > 0);
    }

    #[test]
    fn invalidate_glob_removes_matching_files() {
        let dir = setup_repo(3);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        let status_before = index.status().unwrap();
        assert_eq!(status_before.files_indexed, 3);

        let removed = index.invalidate(Some("**/file_0.rs")).unwrap();
        assert_eq!(removed, 1);

        let status_after = index.status().unwrap();
        assert_eq!(status_after.files_indexed, 2);
    }

    #[test]
    fn invalidate_all_clears_everything() {
        let dir = setup_repo(4);
        let mut index = RepoIndex::open(dir.path()).unwrap();
        index.index("**/*.rs").unwrap();

        let removed = index.invalidate(None).unwrap();
        assert_eq!(removed, 4);

        let status = index.status().unwrap();
        assert_eq!(status.files_indexed, 0);
        assert_eq!(status.total_tokens, 0);
    }
}
