//! File parsing for Markdown and code files

use crate::config::Config;
use crate::document::{DocumentNode, NodeMetadata, NodeType, ParsedFile, RefType, Reference, Span};
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

    let (nodes, refs) = if file_type.is_markdown() {
        (parse_markdown(source), Vec::new())
    } else if file_type.has_tree_sitter_grammar() {
        parse_code_with_tree_sitter(path, source, file_type)
    } else if source.len() > config.indexing.chunk_threshold {
        // Large file without grammar: chunk
        (parse_as_chunks(source, config.indexing.chunk_lines, config.indexing.chunk_overlap), Vec::new())
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
                        parent_name: None,
                        parent_handle_id: None,
                        parent_node_type: None,
                        parent_span: None,
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
                        parent_name: None,
                        parent_handle_id: None,
                        parent_node_type: None,
                        parent_span: None,
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
                        parent_name: None,
                        parent_handle_id: None,
                        parent_node_type: None,
                        parent_span: None,
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
            parent_name: None,
            parent_handle_id: None,
            parent_node_type: None,
            parent_span: None,
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
fn parse_code_with_tree_sitter(path: &Path, source: &str, file_type: FileType) -> (Vec<DocumentNode>, Vec<Reference>) {
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
        _ => return (parse_as_single_node(source), Vec::new()),
    };

    if parser.set_language(&language).is_err() {
        return (parse_as_single_node(source), Vec::new());
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return (parse_as_single_node(source), Vec::new()),
    };

    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    extract_tree_sitter_nodes(&tree.root_node(), source, &mut nodes, &mut refs, file_type, None);

    // If no nodes extracted, fall back to single node
    if nodes.is_empty() {
        return (parse_as_single_node(source), refs);
    }

    (nodes, refs)
}

/// Parent context passed down during tree-sitter traversal
#[derive(Clone)]
struct ParentContext {
    name: String,
    node_type: Option<NodeType>,
    span: Option<Span>,
}

/// Recursively extract nodes from tree-sitter tree with parent tracking
fn extract_tree_sitter_nodes(
    node: &tree_sitter::Node,
    source: &str,
    nodes: &mut Vec<DocumentNode>,
    refs: &mut Vec<Reference>,
    file_type: FileType,
    parent_ctx: Option<ParentContext>,
) {
    let kind = node.kind();

    // Extract references (calls, imports) from this node
    extract_references(node, source, refs, file_type);

    // Check if this node is a parent-providing node (class, impl, etc.)
    // These provide context for their children but may also be indexed themselves
    let new_parent_ctx = determine_parent_context(node, source, file_type);

    // Determine if this is a node we should index
    let node_info = match file_type {
        FileType::Rust => match kind {
            "function_item" => {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_text(node, "name", source))
                    .unwrap_or_default();
                let sig = extract_signature(node, source, "parameters");
                Some((
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                ))
            }
            "impl_item" => {
                // Extract impl type name (handles generics like impl<T> Foo<T>)
                let name = node.child_by_field_name("type")
                    .map(|t| {
                        if t.kind() == "generic_type" {
                            t.child_by_field_name("type")
                                .map(|inner| node_text(&inner, source))
                                .unwrap_or_else(|| node_text(&t, source))
                        } else {
                            node_text(&t, source)
                        }
                    })
                    .unwrap_or_default();
                Some((NodeType::Struct, NodeMetadata::Struct { name }))
            }
            "struct_item" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                Some((NodeType::Struct, NodeMetadata::Struct { name }))
            }
            "trait_item" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                Some((NodeType::Class, NodeMetadata::Class { name }))
            }
            "mod_item" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                Some((NodeType::Section, NodeMetadata::Section { heading: format!("mod {}", name), level: 1 }))
            }
            _ => None,
        },
        FileType::Python => match kind {
            "function_definition" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                let sig = extract_signature(node, source, "parameters");
                // If inside a class, it's a method
                let (node_type, metadata) = if parent_ctx.is_some() {
                    (NodeType::Method, NodeMetadata::Method {
                        name,
                        class_name: parent_ctx.as_ref().map(|p| p.name.clone()),
                    })
                } else {
                    (NodeType::Function, NodeMetadata::Function {
                        name,
                        signature: sig,
                    })
                };
                Some((node_type, metadata))
            }
            "class_definition" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                Some((NodeType::Class, NodeMetadata::Class { name }))
            }
            _ => None,
        },
        FileType::JavaScript | FileType::TypeScript => match kind {
            "function_declaration" | "arrow_function" | "function" => {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))
                    .unwrap_or_else(|| "<anonymous>".to_string());
                let sig = extract_signature(node, source, "formal_parameters");
                Some((
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                ))
            }
            "class_declaration" | "class" => {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))
                    .unwrap_or_default();
                Some((NodeType::Class, NodeMetadata::Class { name }))
            }
            "method_definition" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                Some((
                    NodeType::Method,
                    NodeMetadata::Method {
                        name,
                        class_name: parent_ctx.as_ref().map(|p| p.name.clone()),
                    },
                ))
            }
            "interface_declaration" | "type_alias_declaration" => {
                let name = find_child_text(node, "type_identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))
                    .unwrap_or_default();
                Some((NodeType::Struct, NodeMetadata::Struct { name }))
            }
            _ => None,
        },
        FileType::Go => match kind {
            "function_declaration" => {
                let name = find_child_text(node, "identifier", source).unwrap_or_default();
                let sig = extract_signature(node, source, "parameter_list");
                Some((
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                ))
            }
            "method_declaration" => {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                // Extract receiver type as parent_name for Go methods
                let receiver_type = extract_go_receiver_type(node, source);
                Some((
                    NodeType::Method,
                    NodeMetadata::Method {
                        name,
                        class_name: receiver_type.clone(),
                    },
                ))
            }
            "type_declaration" => {
                // Look for type_spec child
                find_child_by_kind(node, "type_spec").map(|type_spec| {
                    let name = find_child_text(&type_spec, "type_identifier", source)
                        .unwrap_or_default();
                    (NodeType::Struct, NodeMetadata::Struct { name })
                })
            }
            _ => None,
        },
        _ => None,
    };

    // If this is a node we should index, add it
    if let Some((node_type, metadata)) = node_info {
        let span = node.start_byte()..node.end_byte();
        let line_range = (node.start_position().row + 1, node.end_position().row + 1);

        // For Go methods, use receiver type as parent_name
        let effective_parent = if file_type == FileType::Go && kind == "method_declaration" {
            extract_go_receiver_type(node, source).map(|name| ParentContext {
                name,
                node_type: None,
                span: None,
            })
        } else {
            parent_ctx.clone()
        };

        nodes.push(DocumentNode {
            node_type,
            span,
            line_range,
            metadata,
            parent_name: effective_parent.as_ref().map(|p| p.name.clone()),
            parent_handle_id: None,
            parent_node_type: effective_parent.as_ref().and_then(|p| p.node_type),
            parent_span: effective_parent.as_ref().and_then(|p| p.span.clone()),
        });
    }

    // Always recurse into children with appropriate parent context
    let child_parent_ctx = new_parent_ctx.or(parent_ctx);
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            extract_tree_sitter_nodes(&child, source, nodes, refs, file_type, child_parent_ctx.clone());
        }
    }
}

/// Determine if a node provides parent context for its children
fn determine_parent_context(
    node: &tree_sitter::Node,
    source: &str,
    file_type: FileType,
) -> Option<ParentContext> {
    let kind = node.kind();
    let span = node.start_byte()..node.end_byte();

    match file_type {
        FileType::Rust => {
            if kind == "impl_item" {
                // Extract impl type name (handles generics)
                let name = node.child_by_field_name("type")
                    .map(|t| {
                        if t.kind() == "generic_type" {
                            t.child_by_field_name("type")
                                .map(|inner| node_text(&inner, source))
                                .unwrap_or_else(|| node_text(&t, source))
                        } else {
                            node_text(&t, source)
                        }
                    })?;
                Some(ParentContext {
                    name,
                    node_type: Some(NodeType::Struct),
                    span: Some(span),
                })
            } else if kind == "trait_item" {
                let name = find_child_by_field(node, "name")
                    .map(|n| node_text(&n, source))?;
                Some(ParentContext {
                    name,
                    node_type: Some(NodeType::Class),
                    span: Some(span),
                })
            } else {
                None
            }
        }
        FileType::Python => {
            if kind == "class_definition" {
                let name = find_child_text(node, "identifier", source)?;
                Some(ParentContext {
                    name,
                    node_type: Some(NodeType::Class),
                    span: Some(span),
                })
            } else {
                None
            }
        }
        FileType::JavaScript | FileType::TypeScript => {
            if kind == "class_declaration" || kind == "class" {
                let name = find_child_text(node, "identifier", source)
                    .or_else(|| find_child_by_field(node, "name").map(|n| node_text(&n, source)))?;
                Some(ParentContext {
                    name,
                    node_type: Some(NodeType::Class),
                    span: Some(span),
                })
            } else {
                None
            }
        }
        FileType::Go => {
            // Go doesn't have traditional classes, receiver types are handled per-method
            None
        }
        _ => None,
    }
}

/// Extract the receiver type from a Go method declaration
fn extract_go_receiver_type(node: &tree_sitter::Node, source: &str) -> Option<String> {
    // method_declaration has a receiver field
    let receiver = node.child_by_field_name("receiver")?;
    // receiver is a parameter_list containing parameter_declaration(s)
    let param_decl = receiver.named_child(0)?;
    // parameter_declaration has a type field
    let type_node = param_decl.child_by_field_name("type")?;

    // Handle pointer types like *Foo
    if type_node.kind() == "pointer_type" {
        type_node.named_child(0).map(|inner| node_text(&inner, source))
    } else {
        Some(node_text(&type_node, source))
    }
}

/// Extract references (calls, imports) from a tree-sitter node
fn extract_references(
    node: &tree_sitter::Node,
    source: &str,
    refs: &mut Vec<Reference>,
    file_type: FileType,
) {
    let kind = node.kind();

    match file_type {
        FileType::JavaScript | FileType::TypeScript => {
            match kind {
                // Function calls: foo(), obj.foo(), this.foo()
                "call_expression" => {
                    if let Some(func) = node.child_by_field_name("function") {
                        let (name, qualifier) = extract_call_target(&func, source);
                        if !name.is_empty() {
                            refs.push(Reference {
                                name,
                                qualifier,
                                ref_type: RefType::Call,
                                span: node.start_byte()..node.end_byte(),
                                line_range: (node.start_position().row + 1, node.end_position().row + 1),
                            });
                        }
                    }
                }
                // Import statements: import { x } from 'y', import x from 'y'
                "import_statement" => {
                    // Extract imported names
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            extract_import_names(&child, source, refs);
                        }
                    }
                }
                _ => {}
            }
        }
        FileType::Python => {
            match kind {
                // Function calls
                "call" => {
                    if let Some(func) = node.child_by_field_name("function") {
                        let (name, qualifier) = extract_call_target(&func, source);
                        if !name.is_empty() {
                            refs.push(Reference {
                                name,
                                qualifier,
                                ref_type: RefType::Call,
                                span: node.start_byte()..node.end_byte(),
                                line_range: (node.start_position().row + 1, node.end_position().row + 1),
                            });
                        }
                    }
                }
                // Import statements: from x import y, import x
                "import_from_statement" | "import_statement" => {
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            if child.kind() == "dotted_name" || child.kind() == "aliased_import" {
                                let name = if child.kind() == "aliased_import" {
                                    child.child_by_field_name("name")
                                        .map(|n| node_text(&n, source))
                                        .unwrap_or_default()
                                } else {
                                    node_text(&child, source)
                                };
                                if !name.is_empty() {
                                    refs.push(Reference {
                                        name,
                                        qualifier: None,
                                        ref_type: RefType::Import,
                                        span: child.start_byte()..child.end_byte(),
                                        line_range: (child.start_position().row + 1, child.end_position().row + 1),
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        FileType::Rust => {
            match kind {
                // Function calls: foo(), self.foo()
                "call_expression" => {
                    if let Some(func) = node.child_by_field_name("function") {
                        let (name, qualifier) = extract_call_target(&func, source);
                        if !name.is_empty() {
                            refs.push(Reference {
                                name,
                                qualifier,
                                ref_type: RefType::Call,
                                span: node.start_byte()..node.end_byte(),
                                line_range: (node.start_position().row + 1, node.end_position().row + 1),
                            });
                        }
                    }
                }
                // Use statements: use foo::bar
                "use_declaration" => {
                    // Extract the use tree
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            extract_rust_use_names(&child, source, refs);
                        }
                    }
                }
                _ => {}
            }
        }
        FileType::Go => {
            match kind {
                // Function calls: foo(), obj.Foo()
                "call_expression" => {
                    if let Some(func) = node.child_by_field_name("function") {
                        let (name, qualifier) = extract_call_target(&func, source);
                        if !name.is_empty() {
                            refs.push(Reference {
                                name,
                                qualifier,
                                ref_type: RefType::Call,
                                span: node.start_byte()..node.end_byte(),
                                line_range: (node.start_position().row + 1, node.end_position().row + 1),
                            });
                        }
                    }
                }
                // Import statements
                "import_spec" => {
                    // Get the path (string literal)
                    if let Some(path_node) = node.child_by_field_name("path") {
                        let path = node_text(&path_node, source);
                        // Extract the last component as the name
                        let name = path.trim_matches('"').split('/').last()
                            .unwrap_or(&path).to_string();
                        if !name.is_empty() {
                            refs.push(Reference {
                                name,
                                qualifier: Some(path.trim_matches('"').to_string()),
                                ref_type: RefType::Import,
                                span: node.start_byte()..node.end_byte(),
                                line_range: (node.start_position().row + 1, node.end_position().row + 1),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Extract the call target (function name and optional qualifier)
fn extract_call_target(func_node: &tree_sitter::Node, source: &str) -> (String, Option<String>) {
    match func_node.kind() {
        "identifier" => (node_text(func_node, source), None),
        "member_expression" | "attribute" | "field_expression" => {
            // obj.method or obj.attr
            let property = func_node.child_by_field_name("property")
                .or_else(|| func_node.child_by_field_name("attribute"))
                .or_else(|| func_node.child_by_field_name("field"));
            let object = func_node.child_by_field_name("object");

            match (property, object) {
                (Some(p), Some(o)) => (node_text(&p, source), Some(node_text(&o, source))),
                (Some(p), None) => (node_text(&p, source), None),
                _ => (node_text(func_node, source), None),
            }
        }
        "scoped_identifier" => {
            // Rust: module::function
            if let Some(name) = func_node.child_by_field_name("name") {
                let path = func_node.child_by_field_name("path")
                    .map(|p| node_text(&p, source));
                (node_text(&name, source), path)
            } else {
                (node_text(func_node, source), None)
            }
        }
        "selector_expression" => {
            // Go: obj.Method
            if let Some(field) = func_node.child_by_field_name("field") {
                let operand = func_node.child_by_field_name("operand")
                    .map(|o| node_text(&o, source));
                (node_text(&field, source), operand)
            } else {
                (node_text(func_node, source), None)
            }
        }
        _ => (node_text(func_node, source), None),
    }
}

/// Extract import names from JS/TS import clauses
fn extract_import_names(node: &tree_sitter::Node, source: &str, refs: &mut Vec<Reference>) {
    match node.kind() {
        "import_clause" | "named_imports" | "import_specifier" => {
            // Recurse into children
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    extract_import_names(&child, source, refs);
                }
            }
        }
        "identifier" => {
            let name = node_text(node, source);
            if !name.is_empty() && name != "from" && name != "import" {
                refs.push(Reference {
                    name,
                    qualifier: None,
                    ref_type: RefType::Import,
                    span: node.start_byte()..node.end_byte(),
                    line_range: (node.start_position().row + 1, node.end_position().row + 1),
                });
            }
        }
        _ => {}
    }
}

/// Extract use names from Rust use declarations
fn extract_rust_use_names(node: &tree_sitter::Node, source: &str, refs: &mut Vec<Reference>) {
    match node.kind() {
        "use_tree" | "use_list" | "scoped_use_list" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    extract_rust_use_names(&child, source, refs);
                }
            }
        }
        "identifier" => {
            let name = node_text(node, source);
            if !name.is_empty() {
                refs.push(Reference {
                    name,
                    qualifier: None,
                    ref_type: RefType::Import,
                    span: node.start_byte()..node.end_byte(),
                    line_range: (node.start_position().row + 1, node.end_position().row + 1),
                });
            }
        }
        "scoped_identifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let path = node.child_by_field_name("path")
                    .map(|p| node_text(&p, source));
                if !name.is_empty() {
                    refs.push(Reference {
                        name,
                        qualifier: path,
                        ref_type: RefType::Import,
                        span: node.start_byte()..node.end_byte(),
                        line_range: (node.start_position().row + 1, node.end_position().row + 1),
                    });
                }
            }
        }
        _ => {}
    }
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
        let (nodes, _refs) = parse_code_with_tree_sitter(Path::new("test.rs"), source, FileType::Rust);

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
