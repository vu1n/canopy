//! Symbol cache: in-memory O(1) symbol lookups with forward + reverse indices.

use super::RepoIndex;
use crate::document::NodeType;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

/// Cached symbol entry for O(1) lookups
#[derive(Clone)]
pub(crate) struct SymbolCacheEntry {
    pub handle_id: String,
    pub file_path: String,
    pub node_type: i32,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub token_count: usize,
    pub preview: String,
}

/// Symbol cache pair: (name_lower -> entries, file_path -> set of name_lower keys)
pub(crate) type SymbolCachePair = (
    HashMap<String, Vec<SymbolCacheEntry>>,
    HashMap<String, HashSet<String>>,
);

impl RepoIndex {
    /// Load symbol cache from database (preload for fast lookups)
    /// Returns (name_lower -> entries, file_path -> set of name_lower keys)
    pub(crate) fn load_symbol_cache(conn: &Connection) -> crate::Result<SymbolCachePair> {
        let mut cache: HashMap<String, Vec<SymbolCacheEntry>> = HashMap::new();
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

    /// Remove a file's entries from the symbol cache using the reverse index (O(symbols in file))
    pub(crate) fn remove_file_from_symbol_cache(
        symbol_cache: &mut HashMap<String, Vec<SymbolCacheEntry>>,
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
    pub(crate) fn add_to_symbol_cache(
        symbol_cache: &mut HashMap<String, Vec<SymbolCacheEntry>>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(name: &str, file: &str) -> (String, SymbolCacheEntry) {
        (
            name.to_lowercase(),
            SymbolCacheEntry {
                handle_id: format!("h_{name}"),
                file_path: file.to_string(),
                node_type: NodeType::Function.as_int() as i32,
                start_byte: 0,
                end_byte: 100,
                line_start: 1,
                line_end: 10,
                token_count: 50,
                preview: format!("fn {name}()"),
            },
        )
    }

    #[test]
    fn add_and_lookup_symbol_cache() {
        let mut cache = HashMap::new();
        let mut by_file = HashMap::new();
        let entries = vec![make_entry("foo", "src/lib.rs")];
        RepoIndex::add_to_symbol_cache(&mut cache, &mut by_file, entries);
        assert_eq!(cache["foo"].len(), 1);
        assert_eq!(cache["foo"][0].handle_id, "h_foo");
        assert!(by_file["src/lib.rs"].contains("foo"));
    }

    #[test]
    fn remove_file_clears_entries_and_reverse_index() {
        let mut cache = HashMap::new();
        let mut by_file = HashMap::new();
        let entries = vec![
            make_entry("foo", "src/a.rs"),
            make_entry("bar", "src/a.rs"),
            make_entry("baz", "src/b.rs"),
        ];
        RepoIndex::add_to_symbol_cache(&mut cache, &mut by_file, entries);
        assert_eq!(cache.len(), 3);

        RepoIndex::remove_file_from_symbol_cache(&mut cache, &mut by_file, "src/a.rs");
        assert!(!cache.contains_key("foo"));
        assert!(!cache.contains_key("bar"));
        assert!(cache.contains_key("baz"));
        assert!(!by_file.contains_key("src/a.rs"));
        assert!(by_file.contains_key("src/b.rs"));
    }

    #[test]
    fn remove_nonexistent_file_is_noop() {
        let mut cache = HashMap::new();
        let mut by_file = HashMap::new();
        RepoIndex::remove_file_from_symbol_cache(&mut cache, &mut by_file, "no/such/file.rs");
        assert!(cache.is_empty());
    }

    #[test]
    fn add_multiple_entries_same_symbol_name() {
        let mut cache = HashMap::new();
        let mut by_file = HashMap::new();
        let entries = vec![
            make_entry("config", "src/a.rs"),
            make_entry("config", "src/b.rs"),
        ];
        RepoIndex::add_to_symbol_cache(&mut cache, &mut by_file, entries);
        assert_eq!(cache["config"].len(), 2);
    }
}
