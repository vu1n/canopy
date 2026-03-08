//! File parsing for Markdown and code files.
//!
//! Submodules:
//! - `bpe` — BPE token estimation with cached encoder
//! - `markdown` — Markdown parsing via pulldown-cmark
//! - `tree_sitter_parse` — Tree-sitter code parsing and per-language classifiers
//! - `references` — Reference extraction (calls, imports) from AST nodes

mod bpe;
mod markdown;
pub(crate) mod references;
pub(crate) mod tree_sitter_parse;

pub use bpe::{estimate_tokens, warm_bpe};

use crate::config::Config;
use crate::document::{DocumentNode, NodeMetadata, NodeType, ParsedFile, Span};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Detect file type from extension
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Markdown,
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Other,
}

impl FileType {
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("md" | "markdown") => Self::Markdown,
            Some("rs") => Self::Rust,
            Some("py") => Self::Python,
            Some("js" | "jsx" | "mjs" | "cjs") => Self::JavaScript,
            Some("ts" | "tsx" | "mts" | "cts") => Self::TypeScript,
            Some("go") => Self::Go,
            _ => Self::Other,
        }
    }

    pub fn is_markdown(self) -> bool {
        matches!(self, Self::Markdown)
    }

    pub fn has_tree_sitter_grammar(self) -> bool {
        matches!(
            self,
            Self::Rust | Self::Python | Self::JavaScript | Self::TypeScript | Self::Go
        )
    }
}

/// Parse a file and extract nodes
pub fn parse_file(path: &Path, source: &str, config: &Config) -> ParsedFile {
    // Compute content hash
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let content_hash: [u8; 32] = hasher.finalize().into();

    // Capture mtime at call time
    let mtime = file_mtime(path);

    parse_file_with_hash(path, source, config, content_hash, mtime)
}

/// Parse a file with a precomputed content hash and mtime
/// (avoids double-hashing and TOCTOU mtime race in pipeline)
pub fn parse_file_with_hash(
    path: &Path,
    source: &str,
    config: &Config,
    content_hash: [u8; 32],
    mtime: i64,
) -> ParsedFile {
    let file_type = FileType::from_path(path);

    let (nodes, refs) = if file_type.is_markdown() {
        (markdown::parse_markdown(source), Vec::new())
    } else if file_type.has_tree_sitter_grammar() {
        tree_sitter_parse::parse_code_with_tree_sitter(path, source, file_type)
    } else if source.len() > config.indexing.chunk_threshold {
        // Large file without grammar: chunk
        (
            parse_as_chunks(
                source,
                config.indexing.chunk_lines,
                config.indexing.chunk_overlap,
            ),
            Vec::new(),
        )
    } else {
        // Small file without grammar: single node
        (parse_as_single_node(source), Vec::new())
    };

    // Compute total tokens
    let total_tokens = estimate_tokens(source);

    ParsedFile {
        path: path.to_path_buf(),
        source: source.to_string(),
        content_hash,
        nodes,
        refs,
        total_tokens,
        mtime,
    }
}

/// Get file mtime as seconds since UNIX epoch (0 on error)
pub fn file_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Fallback parsers (used by tree_sitter_parse on grammar failures too)
// ---------------------------------------------------------------------------

/// Parse file as line-based chunks (for large files without grammar)
fn parse_as_chunks(source: &str, chunk_lines: usize, chunk_overlap: usize) -> Vec<DocumentNode> {
    let lines: Vec<&str> = source.lines().collect();
    let mut nodes = Vec::new();
    let mut chunk_index = 0;

    let step = chunk_lines.saturating_sub(chunk_overlap).max(1);

    let mut i = 0;
    while i < lines.len() {
        let end = (i + chunk_lines).min(lines.len());
        let chunk_lines_slice = &lines[i..end];

        // Calculate byte span
        let start_byte = lines[..i]
            .iter()
            .map(|l| l.len() + 1) // +1 for newline
            .sum::<usize>();
        let chunk_len: usize = chunk_lines_slice.iter().map(|l| l.len() + 1).sum();
        let end_byte = (start_byte + chunk_len).min(source.len());

        let span = start_byte..end_byte;
        let line_range = (i + 1, end); // 1-indexed

        nodes.push(DocumentNode {
            node_type: NodeType::Chunk,
            span,
            line_range,
            metadata: NodeMetadata::Chunk { index: chunk_index },
            parent_name: None,
            parent_handle_id: None,
            parent_node_type: None,
            parent_span: None,
        });

        chunk_index += 1;
        i += step;
    }

    nodes
}

/// Parse file as single node (for small files without grammar)
fn parse_as_single_node(source: &str) -> Vec<DocumentNode> {
    if source.is_empty() {
        return Vec::new();
    }

    let line_count = source.lines().count().max(1);

    vec![DocumentNode {
        node_type: NodeType::Chunk,
        span: 0..source.len(),
        line_range: (1, line_count),
        metadata: NodeMetadata::Chunk { index: 0 },
        parent_name: None,
        parent_handle_id: None,
        parent_node_type: None,
        parent_span: None,
    }]
}

/// Convert byte span to line range (1-indexed)
fn span_to_line_range(source: &str, span: &Span) -> (usize, usize) {
    let start_line = source[..span.start.min(source.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1;

    let end_line = source[..span.end.min(source.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1;

    (start_line, end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_markdown() {
        let source = r#"# Heading 1

Some paragraph text.

## Heading 2

```rust
fn main() {}
```

More text.
"#;
        let nodes = markdown::parse_markdown(source);

        // Should have sections, paragraphs, and code blocks
        assert!(nodes
            .iter()
            .any(|n| matches!(n.node_type, NodeType::Section)));
        assert!(nodes
            .iter()
            .any(|n| matches!(n.node_type, NodeType::Paragraph)));
        assert!(nodes
            .iter()
            .any(|n| matches!(n.node_type, NodeType::CodeBlock)));
    }

    #[test]
    fn test_parse_rust_code() {
        let source = r#"
fn main() {
    println!("Hello");
}

struct Foo {
    bar: i32,
}

impl Foo {
    fn new() -> Self {
        Self { bar: 0 }
    }
}
"#;
        let (nodes, _refs) = tree_sitter_parse::parse_code_with_tree_sitter(
            Path::new("test.rs"),
            source,
            FileType::Rust,
        );

        // Should have function and struct
        assert!(nodes
            .iter()
            .any(|n| matches!(n.node_type, NodeType::Function)));
        assert!(nodes
            .iter()
            .any(|n| matches!(n.node_type, NodeType::Struct)));
    }

    #[test]
    fn test_parse_chunks() {
        let source = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let nodes = parse_as_chunks(&source, 10, 2);

        assert!(!nodes.is_empty());
        assert!(nodes.iter().all(|n| matches!(n.node_type, NodeType::Chunk)));
    }

    #[test]
    fn test_file_type_detection() {
        assert_eq!(
            FileType::from_path(Path::new("README.md")),
            FileType::Markdown
        );
        assert_eq!(
            FileType::from_path(Path::new("src/main.rs")),
            FileType::Rust
        );
        assert_eq!(
            FileType::from_path(Path::new("app.tsx")),
            FileType::TypeScript
        );
        assert_eq!(
            FileType::from_path(Path::new("script.js")),
            FileType::JavaScript
        );
        assert_eq!(FileType::from_path(Path::new("main.go")), FileType::Go);
        assert_eq!(FileType::from_path(Path::new("data.csv")), FileType::Other);
    }

    #[test]
    fn test_estimate_tokens() {
        let text = "Hello, world!";
        let tokens = estimate_tokens(text);
        assert!(tokens > 0);
        assert!(tokens < text.len()); // Should be fewer tokens than chars
    }
}
