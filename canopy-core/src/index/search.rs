//! Index search methods — FTS, symbol, section, reference, and file queries.

use crate::document::{NodeType, RefType};
use crate::error::CanopyError;
use crate::handle::{generate_preview, Handle, HandleId, HandleSource, RefHandle};
use rusqlite::params;

use super::symbol_cache::SymbolCacheEntry;
use super::RepoIndex;

/// Shared column list for handle queries — matches the `handle_from_row` column order.
pub(super) const HANDLE_SELECT: &str =
    "n.handle_id, f.path, n.node_type, n.start_byte, n.end_byte, \
     n.line_start, n.line_end, n.token_count, n.preview";

impl RepoIndex {
    /// Execute a handle query and collect results.
    fn query_handles(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
    ) -> crate::Result<Vec<Handle>> {
        let mut stmt = self.conn.prepare(sql)?;
        let handles = collect_row_results(stmt.query_map(params, handle_from_row)?)?;
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

    /// Search for code symbols by name (exact match with fuzzy fallback).
    pub fn search_code(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        let handles = self.search_symbol_exact(symbol, limit)?;
        if handles.is_empty() {
            return self.search_symbol_fuzzy(symbol, limit);
        }
        Ok(handles)
    }

    /// Exact symbol lookup: cache first, then DB fallback.
    fn search_symbol_exact(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
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

        let all_rows: Vec<(String, usize)> = collect_row_results(stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let tokens: i64 = row.get(1)?;
            Ok((path, tokens.max(0) as usize))
        })?)?;
        let matches: Vec<(String, usize)> = all_rows
            .into_iter()
            .filter(|(path, _)| glob_matcher.is_match(path))
            .collect();

        let mut handles = Vec::new();
        for (file_path, token_count) in matches {
            // Read file to get line count and preview
            let full_path = self.repo_root.join(&file_path);
            if let Ok(source) = std::fs::read_to_string(&full_path) {
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

        let all_handles: Vec<Handle> =
            collect_row_results(stmt.query_map(params![escaped], handle_from_row)?)?;
        let handles: Vec<Handle> = all_handles
            .into_iter()
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

    /// Search for symbol definitions (exact match only, no fuzzy fallback).
    pub fn search_definitions(&self, symbol: &str, limit: usize) -> crate::Result<Vec<Handle>> {
        self.search_symbol_exact(symbol, limit)
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

        let raw_rows =
            collect_row_results(stmt.query_map(params![symbol_lower, limit as i64], |row| {
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
                    span_start.max(0) as usize,
                    span_end.max(0) as usize,
                    line_start.max(0) as usize,
                    line_end.max(0) as usize,
                    name,
                    qualifier,
                    ref_type_str,
                    source_handle_id,
                    preview.unwrap_or_else(|| "...".to_string()),
                ))
            })?)?;
        let refs: Vec<RefHandle> = raw_rows
            .into_iter()
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
    let span = (start_byte.max(0) as usize)..(end_byte.max(0) as usize);

    Ok(Handle {
        id: HandleId::from_raw(handle_id),
        file_path,
        node_type,
        span,
        line_range: (line_start.max(0) as usize, line_end.max(0) as usize),
        token_count: token_count.max(0) as usize,
        preview: preview.unwrap_or_else(|| "...".to_string()),
        content: None,
        source: HandleSource::Local,
        commit_sha: None,
        generation: None,
    })
}

/// Collect row results, propagating the first row-level error.
pub(super) fn collect_row_results<T>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
) -> rusqlite::Result<Vec<T>> {
    rows.collect()
}

/// Escape FTS5 special characters
pub(super) fn escape_fts5_query(query: &str) -> String {
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
    use crate::document::NodeType;
    use crate::handle::{HandleId, HandleSource};

    #[test]
    fn escape_fts5_plain_query_unchanged() {
        assert_eq!(escape_fts5_query("hello world"), "hello world");
        assert_eq!(escape_fts5_query("simple"), "simple");
        assert_eq!(escape_fts5_query(""), "");
    }

    #[test]
    fn escape_fts5_special_chars_quoted() {
        // Parentheses
        assert_eq!(escape_fts5_query("fn()"), "\"fn()\"");
        // Asterisk
        assert_eq!(escape_fts5_query("glob*"), "\"glob*\"");
        // Dash (NOT operator in FTS5)
        assert_eq!(escape_fts5_query("self-signed"), "\"self-signed\"");
        // Angle brackets
        assert_eq!(escape_fts5_query("Vec<T>"), "\"Vec<T>\"");
        // Double quotes are escaped by doubling
        assert_eq!(escape_fts5_query("say \"hello\""), "\"say \"\"hello\"\"\"");
    }

    #[test]
    fn code_type_params_returns_four_code_types() {
        let params = code_type_params();
        assert_eq!(params.len(), 4);
        assert_eq!(params[0], NodeType::Function.as_int() as i32); // 3
        assert_eq!(params[1], NodeType::Class.as_int() as i32); // 4
        assert_eq!(params[2], NodeType::Struct.as_int() as i32); // 5
        assert_eq!(params[3], NodeType::Method.as_int() as i32); // 6
    }

    #[test]
    fn handle_from_cache_entry_roundtrip() {
        let entry = SymbolCacheEntry {
            handle_id: "h_test".to_string(),
            file_path: "src/lib.rs".to_string(),
            node_type: NodeType::Function.as_int() as i32,
            start_byte: 10,
            end_byte: 200,
            line_start: 1,
            line_end: 15,
            token_count: 42,
            preview: "fn test()".to_string(),
        };

        let handle = handle_from_cache_entry(&entry);

        assert_eq!(handle.id, HandleId::from_raw("h_test".to_string()));
        assert_eq!(handle.file_path, "src/lib.rs");
        assert_eq!(handle.node_type, NodeType::Function);
        assert_eq!(handle.span, 10..200);
        assert_eq!(handle.line_range, (1, 15));
        assert_eq!(handle.token_count, 42);
        assert_eq!(handle.preview, "fn test()");
        assert!(handle.content.is_none());
        assert_eq!(handle.source, HandleSource::Local);
        assert!(handle.commit_sha.is_none());
        assert!(handle.generation.is_none());
    }

    #[test]
    fn collect_row_results_propagates_first_error() {
        let items: Vec<rusqlite::Result<i32>> = vec![
            Ok(1),
            Err(rusqlite::Error::InvalidParameterName("bad".into())),
            Ok(3),
        ];
        let result = collect_row_results(items.into_iter());
        assert!(result.is_err());
    }

    #[test]
    fn collect_row_results_all_ok() {
        let items: Vec<rusqlite::Result<&str>> = vec![Ok("a"), Ok("b")];
        let result = collect_row_results(items.into_iter());
        assert_eq!(result.unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn collect_row_results_empty_input() {
        let items: Vec<rusqlite::Result<i32>> = vec![];
        let result = collect_row_results(items.into_iter());
        assert!(result.unwrap().is_empty());
    }
}
