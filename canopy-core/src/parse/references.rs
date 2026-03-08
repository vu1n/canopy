//! Reference extraction (calls, imports) from tree-sitter nodes.

use crate::document::{RefType, Reference};

use super::tree_sitter_parse::{node_text, FileType};

/// Extract references (calls, imports) from a tree-sitter node
pub(crate) fn extract_references(
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
                                line_range: (
                                    node.start_position().row + 1,
                                    node.end_position().row + 1,
                                ),
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
                                line_range: (
                                    node.start_position().row + 1,
                                    node.end_position().row + 1,
                                ),
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
                                    child
                                        .child_by_field_name("name")
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
                                        line_range: (
                                            child.start_position().row + 1,
                                            child.end_position().row + 1,
                                        ),
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
                                line_range: (
                                    node.start_position().row + 1,
                                    node.end_position().row + 1,
                                ),
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
                                line_range: (
                                    node.start_position().row + 1,
                                    node.end_position().row + 1,
                                ),
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
                        let name = path
                            .trim_matches('"')
                            .rsplit('/')
                            .next()
                            .unwrap_or(&path)
                            .to_string();
                        if !name.is_empty() {
                            refs.push(Reference {
                                name,
                                qualifier: Some(path.trim_matches('"').to_string()),
                                ref_type: RefType::Import,
                                span: node.start_byte()..node.end_byte(),
                                line_range: (
                                    node.start_position().row + 1,
                                    node.end_position().row + 1,
                                ),
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
            let property = func_node
                .child_by_field_name("property")
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
                let path = func_node
                    .child_by_field_name("path")
                    .map(|p| node_text(&p, source));
                (node_text(&name, source), path)
            } else {
                (node_text(func_node, source), None)
            }
        }
        "selector_expression" => {
            // Go: obj.Method
            if let Some(field) = func_node.child_by_field_name("field") {
                let operand = func_node
                    .child_by_field_name("operand")
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
                let path = node
                    .child_by_field_name("path")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::RefType;

    /// Walk the tree depth-first, calling extract_references on every node.
    fn collect_refs(
        source: &str,
        language: tree_sitter::Language,
        file_type: FileType,
    ) -> Vec<Reference> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut refs = Vec::new();
        let mut cursor = tree.root_node().walk();
        walk_and_extract(&mut cursor, source, &mut refs, file_type);
        refs
    }

    fn walk_and_extract(
        cursor: &mut tree_sitter::TreeCursor,
        source: &str,
        refs: &mut Vec<Reference>,
        file_type: FileType,
    ) {
        loop {
            let node = cursor.node();
            extract_references(&node, source, refs, file_type);
            if cursor.goto_first_child() {
                walk_and_extract(cursor, source, refs, file_type);
                cursor.goto_parent();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    #[test]
    fn rust_call_and_use_references() {
        let source = "use std::collections::HashMap;\n\nfn main() {\n    let x = foo();\n    let y = bar::baz();\n}\n";
        let refs = collect_refs(source, tree_sitter_rust::LANGUAGE.into(), FileType::Rust);

        // Should find use imports
        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.ref_type == RefType::Import)
            .collect();
        assert!(!imports.is_empty(), "should extract at least one import");
        assert!(
            imports.iter().any(|r| r.name == "HashMap"),
            "should find HashMap import, got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        // Should find function calls
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.ref_type == RefType::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "foo"),
            "should find foo() call, got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls
                .iter()
                .any(|r| r.name == "baz" && r.qualifier.as_deref() == Some("bar")),
            "should find bar::baz() call with qualifier"
        );
    }

    #[test]
    fn javascript_import_and_call_references() {
        let source = "import { useState, useEffect } from 'react';\n\nfunction App() {\n  const [x, setX] = useState(0);\n  console.log(x);\n}\n";
        let refs = collect_refs(
            source,
            tree_sitter_javascript::LANGUAGE.into(),
            FileType::JavaScript,
        );

        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.ref_type == RefType::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "useState"),
            "should find useState import, got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "useEffect"),
            "should find useEffect import"
        );

        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.ref_type == RefType::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "useState"),
            "should find useState() call, got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        // console.log is a member_expression call
        assert!(
            calls
                .iter()
                .any(|r| r.name == "log" && r.qualifier.as_deref() == Some("console")),
            "should find console.log() call with qualifier"
        );
    }

    #[test]
    fn python_import_and_call_references() {
        let source = "from os.path import join\nimport sys\n\ndef main():\n    result = join('a', 'b')\n    sys.exit(0)\n";
        let refs = collect_refs(
            source,
            tree_sitter_python::LANGUAGE.into(),
            FileType::Python,
        );

        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.ref_type == RefType::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "join"),
            "should find 'join' import, got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.ref_type == RefType::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "join"),
            "should find join() call, got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls
                .iter()
                .any(|r| r.name == "exit" && r.qualifier.as_deref() == Some("sys")),
            "should find sys.exit() call with qualifier"
        );
    }

    #[test]
    fn empty_source_yields_no_references() {
        let refs = collect_refs("", tree_sitter_rust::LANGUAGE.into(), FileType::Rust);
        assert!(refs.is_empty());

        let refs = collect_refs(
            "",
            tree_sitter_javascript::LANGUAGE.into(),
            FileType::JavaScript,
        );
        assert!(refs.is_empty());

        let refs = collect_refs("", tree_sitter_python::LANGUAGE.into(), FileType::Python);
        assert!(refs.is_empty());
    }
}
