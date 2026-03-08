//! Tree-sitter code parsing and per-language node classification.

use crate::document::{DocumentNode, NodeMetadata, NodeType, Reference, Span};

use super::references::extract_references;

pub use super::FileType;

/// Parse code file with tree-sitter
pub(crate) fn parse_code_with_tree_sitter(
    path: &std::path::Path,
    source: &str,
    file_type: FileType,
) -> (Vec<DocumentNode>, Vec<Reference>) {
    let mut parser = tree_sitter::Parser::new();

    // Set language based on file type
    let language = match file_type {
        FileType::Rust => tree_sitter_rust::LANGUAGE.into(),
        FileType::Python => tree_sitter_python::LANGUAGE.into(),
        FileType::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        FileType::TypeScript => {
            // Use TSX for .tsx files, regular TS for others
            if path.extension().is_some_and(|e| e == "tsx") {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        FileType::Go => tree_sitter_go::LANGUAGE.into(),
        _ => return (super::parse_as_single_node(source), Vec::new()),
    };

    if parser.set_language(&language).is_err() {
        return (super::parse_as_single_node(source), Vec::new());
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return (super::parse_as_single_node(source), Vec::new()),
    };

    let mut nodes = Vec::new();
    let mut refs = Vec::new();
    extract_tree_sitter_nodes(
        &tree.root_node(),
        source,
        &mut nodes,
        &mut refs,
        file_type,
        None,
    );

    // If no nodes extracted, fall back to single node
    if nodes.is_empty() {
        return (super::parse_as_single_node(source), refs);
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
        FileType::Rust => classify_rust_node(node, source),
        FileType::Python => classify_python_node(node, source, &parent_ctx),
        FileType::JavaScript | FileType::TypeScript => {
            classify_js_ts_node(node, source, &parent_ctx)
        }
        FileType::Go => classify_go_node(node, source),
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
            extract_tree_sitter_nodes(
                &child,
                source,
                nodes,
                refs,
                file_type,
                child_parent_ctx.clone(),
            );
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
                let name = node.child_by_field_name("type").map(|t| {
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
                let name = node
                    .child_by_field_name("name")
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
                let name = find_child_text(node, "identifier", source).or_else(|| {
                    node.child_by_field_name("name")
                        .map(|n| node_text(&n, source))
                })?;
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
        type_node
            .named_child(0)
            .map(|inner| node_text(&inner, source))
    } else {
        Some(node_text(&type_node, source))
    }
}

// ---------------------------------------------------------------------------
// Per-language node classifiers
// ---------------------------------------------------------------------------

fn classify_rust_node(node: &tree_sitter::Node, source: &str) -> Option<(NodeType, NodeMetadata)> {
    match node.kind() {
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
            let name = node
                .child_by_field_name("type")
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
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
            Some((NodeType::Struct, NodeMetadata::Struct { name }))
        }
        "trait_item" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
            Some((NodeType::Class, NodeMetadata::Class { name }))
        }
        "mod_item" => {
            let name = find_child_text(node, "identifier", source).unwrap_or_default();
            Some((
                NodeType::Section,
                NodeMetadata::Section {
                    heading: format!("mod {}", name),
                    level: 1,
                },
            ))
        }
        _ => None,
    }
}

fn classify_python_node(
    node: &tree_sitter::Node,
    source: &str,
    parent_ctx: &Option<ParentContext>,
) -> Option<(NodeType, NodeMetadata)> {
    match node.kind() {
        "function_definition" => {
            let name = find_child_text(node, "identifier", source).unwrap_or_default();
            let sig = extract_signature(node, source, "parameters");
            if parent_ctx.is_some() {
                Some((
                    NodeType::Method,
                    NodeMetadata::Method {
                        name,
                        class_name: parent_ctx.as_ref().map(|p| p.name.clone()),
                    },
                ))
            } else {
                Some((
                    NodeType::Function,
                    NodeMetadata::Function {
                        name,
                        signature: sig,
                    },
                ))
            }
        }
        "class_definition" => {
            let name = find_child_text(node, "identifier", source).unwrap_or_default();
            Some((NodeType::Class, NodeMetadata::Class { name }))
        }
        _ => None,
    }
}

fn classify_js_ts_node(
    node: &tree_sitter::Node,
    source: &str,
    parent_ctx: &Option<ParentContext>,
) -> Option<(NodeType, NodeMetadata)> {
    match node.kind() {
        "function_declaration" | "arrow_function" | "function" => {
            let name = find_child_text(node, "identifier", source)
                .or_else(|| {
                    node.child_by_field_name("name")
                        .map(|n| node_text(&n, source))
                })
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
                .or_else(|| {
                    node.child_by_field_name("name")
                        .map(|n| node_text(&n, source))
                })
                .unwrap_or_default();
            Some((NodeType::Class, NodeMetadata::Class { name }))
        }
        "method_definition" => {
            let name = node
                .child_by_field_name("name")
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
                .or_else(|| {
                    node.child_by_field_name("name")
                        .map(|n| node_text(&n, source))
                })
                .unwrap_or_default();
            Some((NodeType::Struct, NodeMetadata::Struct { name }))
        }
        _ => None,
    }
}

fn classify_go_node(node: &tree_sitter::Node, source: &str) -> Option<(NodeType, NodeMetadata)> {
    match node.kind() {
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
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
            let receiver_type = extract_go_receiver_type(node, source);
            Some((
                NodeType::Method,
                NodeMetadata::Method {
                    name,
                    class_name: receiver_type,
                },
            ))
        }
        "type_declaration" => find_child_by_kind(node, "type_spec").map(|type_spec| {
            let name = find_child_text(&type_spec, "type_identifier", source).unwrap_or_default();
            (NodeType::Struct, NodeMetadata::Struct { name })
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter helpers
// ---------------------------------------------------------------------------

fn find_child_by_kind<'a>(
    node: &'a tree_sitter::Node,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == kind {
                return Some(child);
            }
        }
    }
    None
}

fn find_child_text(node: &tree_sitter::Node, kind: &str, source: &str) -> Option<String> {
    find_child_by_kind(node, kind).map(|n| node_text(&n, source))
}

pub(crate) fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

fn extract_signature(node: &tree_sitter::Node, source: &str, params_kind: &str) -> Option<String> {
    find_child_by_kind(node, params_kind).map(|n| node_text(&n, source))
}
