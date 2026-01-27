//! File parsing for Markdown and code files

use crate::config::Config;
use crate::document::{DocumentNode, NodeMetadata, NodeType, ParsedFile, Span};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};
use std::path::Path;

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
    let file_type = FileType::from_path(path);

    // Compute content hash
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let content_hash: [u8; 32] = hasher.finalize().into();

    let nodes = if file_type.is_markdown() {
        parse_markdown(source)
    } else if file_type.has_tree_sitter_grammar() {
        parse_code_with_tree_sitter(path, source, file_type)
    } else if source.len() > config.indexing.chunk_threshold {
        // Large file without grammar: chunk
        parse_as_chunks(source, config.indexing.chunk_lines, config.indexing.chunk_overlap)
    } else {
        // Small file without grammar: single node
        parse_as_single_node(source)
    };

    // Compute total tokens
    let total_tokens = estimate_tokens(source);

    ParsedFile {
        path: path.to_path_buf(),
        source: source.to_string(),
        content_hash,
        nodes,
        total_tokens,
    }
}

/// Parse markdown file using pulldown-cmark
fn parse_markdown(source: &str) -> Vec<DocumentNode> {
    let mut nodes = Vec::new();
    let parser = Parser::new(source);

    let mut current_section_start: Option<usize> = None;
    let mut current_heading: Option<(String, u8)> = None;
    let mut in_heading = false;
    let mut heading_text = String::new();
    let mut heading_level = 0u8;

    let mut code_block_start: Option<usize> = None;
    let mut code_block_lang: Option<String> = None;

    let mut para_start: Option<usize> = None;

    for (event, range) in parser.into_offset_iter() {
        let offset = range.start;

        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // End previous section if any
                if let (Some(start), Some((heading, lvl))) =
                    (current_section_start, current_heading.take())
                {
                    let span = start..offset;
                    let line_range = span_to_line_range(source, &span);
                    nodes.push(DocumentNode {
                        node_type: NodeType::Section,
                        span,
                        line_range,
                        metadata: NodeMetadata::Section {
                            heading,
                            level: lvl,
                        },
                    });
                }

                in_heading = true;
                heading_text.clear();
                heading_level = heading_level_to_u8(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                current_section_start = Some(range.start);
                current_heading = Some((heading_text.clone(), heading_level));
            }
            Event::Text(text) if in_heading => {
                heading_text.push_str(&text);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                code_block_start = Some(range.start);
                code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                        let lang = lang.to_string();
                        if lang.is_empty() {
                            None
                        } else {
                            Some(lang)
                        }
                    }
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = code_block_start.take() {
                    let span = start..range.end;
                    let line_range = span_to_line_range(source, &span);
                    nodes.push(DocumentNode {
                        node_type: NodeType::CodeBlock,
                        span,
                        line_range,
                        metadata: NodeMetadata::CodeBlock {
                            language: code_block_lang.take(),
                        },
                    });
                }
            }
            Event::Start(Tag::Paragraph) => {
                para_start = Some(range.start);
            }
            Event::End(TagEnd::Paragraph) => {
                if let Some(start) = para_start.take() {
                    let span = start..range.end;
                    let line_range = span_to_line_range(source, &span);
                    nodes.push(DocumentNode {
                        node_type: NodeType::Paragraph,
                        span,
                        line_range,
                        metadata: NodeMetadata::Paragraph,
                    });
                }
            }
            _ => {}
        }
    }

    // End final section if any
    if let (Some(start), Some((heading, level))) = (current_section_start, current_heading) {
        let span = start..source.len();
        let line_range = span_to_line_range(source, &span);
        nodes.push(DocumentNode {
            node_type: NodeType::Section,
            span,
            line_range,
            metadata: NodeMetadata::Section { heading, level },
        });
    }

    nodes
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Parse code file with tree-sitter
fn parse_code_with_tree_sitter(path: &Path, source: &str, file_type: FileType) -> Vec<DocumentNode> {
    let mut parser = tree_sitter::Parser::new();

    // Set language based on file type
    let language = match file_type {
        FileType::Rust => tree_sitter_rust::LANGUAGE.into(),
        FileType::Python => tree_sitter_python::LANGUAGE.into(),
        FileType::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        FileType::TypeScript => {
            // Use TSX for .tsx files, regular TS for others
            if path.extension().map_or(false, |e| e == "tsx") {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        FileType::Go => tree_sitter_go::LANGUAGE.into(),
        _ => return parse_as_single_node(source),
    };

    if parser.set_language(&language).is_err() {
        return parse_as_single_node(source);
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return parse_as_single_node(source),
    };

    let mut nodes = Vec::new();
    extract_tree_sitter_nodes(&tree.root_node(), source, &mut nodes, file_type);

    // If no nodes extracted, fall back to single node
    if nodes.is_empty() {
        return parse_as_single_node(source);
    }

    nodes
}

/// Recursively extract nodes from tree-sitter tree
fn extract_tree_sitter_nodes(
    node: &tree_sitter::Node,
    source: &str,
    nodes: &mut Vec<DocumentNode>,
    file_type: FileType,
) {
    let kind = node.kind();

    // Determine if this is a node we care about
    let (node_type, metadata) = match file_type {
        FileType::Rust => match kind {
            "function_item" => {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_text(node, "name", source))
                    .unwrap_or_default();
                let sig = extract_signature(node, source, "parameters");
                (
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                )
            }
            "impl_item" | "struct_item" => {
                let name = find_child_by_field(node, "name")
                    .or_else(|| find_child_by_field(node, "type"))
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                (NodeType::Struct, NodeMetadata::Struct { name })
            }
            "trait_item" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                (NodeType::Class, NodeMetadata::Class { name })
            }
            "mod_item" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                (NodeType::Section, NodeMetadata::Section { heading: format!("mod {}", name), level: 1 })
            }
            _ => {
                // Recurse into children
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        extract_tree_sitter_nodes(&child, source, nodes, file_type);
                    }
                }
                return;
            }
        },
        FileType::Python => match kind {
            "function_definition" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                let sig = extract_signature(node, source, "parameters");
                (
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                )
            }
            "class_definition" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                (NodeType::Class, NodeMetadata::Class { name })
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        extract_tree_sitter_nodes(&child, source, nodes, file_type);
                    }
                }
                return;
            }
        },
        FileType::JavaScript | FileType::TypeScript => match kind {
            "function_declaration" | "arrow_function" | "function" => {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))
                    .unwrap_or_else(|| "<anonymous>".to_string());
                let sig = extract_signature(node, source, "formal_parameters");
                (
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                )
            }
            "class_declaration" | "class" => {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))
                    .unwrap_or_default();
                (NodeType::Class, NodeMetadata::Class { name })
            }
            "method_definition" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                (
                    NodeType::Method,
                    NodeMetadata::Method {
                        name,
                        class_name: None,
                    },
                )
            }
            "interface_declaration" | "type_alias_declaration" => {
                let name = find_child_text(node, "type_identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))
                    .unwrap_or_default();
                (NodeType::Struct, NodeMetadata::Struct { name })
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        extract_tree_sitter_nodes(&child, source, nodes, file_type);
                    }
                }
                return;
            }
        },
        FileType::Go => match kind {
            "function_declaration" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                let sig = extract_signature(node, source, "parameter_list");
                (
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                )
            }
            "method_declaration" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                (
                    NodeType::Method,
                    NodeMetadata::Method {
                        name,
                        class_name: None,
                    },
                )
            }
            "type_declaration" => {
                // Look for type_spec child
                if let Some(type_spec) = find_child_by_kind(node, "type_spec") {
                    let name = find_child_text(&type_spec, "type_identifier", source)
                        .unwrap_or_default();
                    (NodeType::Struct, NodeMetadata::Struct { name })
                } else {
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            extract_tree_sitter_nodes(&child, source, nodes, file_type);
                        }
                    }
                    return;
                }
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        extract_tree_sitter_nodes(&child, source, nodes, file_type);
                    }
                }
                return;
            }
        },
        _ => return,
    };

    let span = node.start_byte()..node.end_byte();
    let line_range = (node.start_position().row + 1, node.end_position().row + 1);

    nodes.push(DocumentNode {
        node_type,
        span,
        line_range,
        metadata,
    });

    // Don't recurse into children for top-level nodes we captured
    // (methods inside classes are handled separately)
}

fn find_child_by_kind<'a>(node: &'a tree_sitter::Node, kind: &str) -> Option<tree_sitter::Node<'a>> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == kind {
                return Some(child);
            }
        }
    }
    None
}

fn find_child_by_field<'a>(node: &'a tree_sitter::Node, field: &str) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}

fn find_child_text(node: &tree_sitter::Node, kind: &str, source: &str) -> Option<String> {
    find_child_by_kind(node, kind).map(|n| node_text(&n, source))
}

fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

fn extract_signature(node: &tree_sitter::Node, source: &str, params_kind: &str) -> Option<String> {
    find_child_by_kind(node, params_kind).map(|n| node_text(&n, source))
}

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

/// Estimate token count using tiktoken-rs
pub fn estimate_tokens(text: &str) -> usize {
    // Use cl100k_base encoding (GPT-4/Claude compatible)
    tiktoken_rs::cl100k_base()
        .map(|bpe| bpe.encode_with_special_tokens(text).len())
        .unwrap_or_else(|_| {
            // Fallback: rough estimate of 4 chars per token
            text.len() / 4
        })
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
        let nodes = parse_markdown(source);

        // Should have sections, paragraphs, and code blocks
        assert!(nodes.iter().any(|n| matches!(n.node_type, NodeType::Section)));
        assert!(nodes.iter().any(|n| matches!(n.node_type, NodeType::Paragraph)));
        assert!(nodes.iter().any(|n| matches!(n.node_type, NodeType::CodeBlock)));
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
        let nodes = parse_code_with_tree_sitter(Path::new("test.rs"), source, FileType::Rust);

        // Should have function and struct
        assert!(nodes.iter().any(|n| matches!(n.node_type, NodeType::Function)));
        assert!(nodes.iter().any(|n| matches!(n.node_type, NodeType::Struct)));
    }

    #[test]
    fn test_parse_chunks() {
        let source = (0..100).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let nodes = parse_as_chunks(&source, 10, 2);

        assert!(!nodes.is_empty());
        assert!(nodes.iter().all(|n| matches!(n.node_type, NodeType::Chunk)));
    }

    #[test]
    fn test_file_type_detection() {
        assert_eq!(FileType::from_path(Path::new("README.md")), FileType::Markdown);
        assert_eq!(FileType::from_path(Path::new("src/main.rs")), FileType::Rust);
        assert_eq!(FileType::from_path(Path::new("app.tsx")), FileType::TypeScript);
        assert_eq!(FileType::from_path(Path::new("script.js")), FileType::JavaScript);
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
