//! Markdown parsing using pulldown-cmark.

use crate::document::{DocumentNode, NodeMetadata, NodeType};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

use super::span_to_line_range;

/// Parse markdown file using pulldown-cmark
pub(crate) fn parse_markdown(source: &str) -> Vec<DocumentNode> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{NodeMetadata, NodeType};

    #[test]
    fn parse_sections_with_headings() {
        let md = "# Introduction\n\nSome intro text.\n\n## Details\n\nMore details here.\n";
        let nodes = parse_markdown(md);

        // Should have: paragraph, section (Introduction), paragraph, section (Details)
        let sections: Vec<_> = nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Section)
            .collect();
        assert_eq!(sections.len(), 2);

        // First section: Introduction (ends when Details heading starts)
        match &sections[0].metadata {
            NodeMetadata::Section { heading, level } => {
                assert_eq!(heading, "Introduction");
                assert_eq!(*level, 1);
            }
            _ => panic!("Expected Section metadata"),
        }

        // Second section: Details (extends to end of document)
        match &sections[1].metadata {
            NodeMetadata::Section { heading, level } => {
                assert_eq!(heading, "Details");
                assert_eq!(*level, 2);
            }
            _ => panic!("Expected Section metadata"),
        }
    }

    #[test]
    fn parse_fenced_code_blocks() {
        let md = "# Example\n\n```rust\nfn main() {}\n```\n\n```\nplain code\n```\n";
        let nodes = parse_markdown(md);

        let code_blocks: Vec<_> = nodes
            .iter()
            .filter(|n| n.node_type == NodeType::CodeBlock)
            .collect();
        assert_eq!(code_blocks.len(), 2);

        // First code block has language annotation
        match &code_blocks[0].metadata {
            NodeMetadata::CodeBlock { language } => {
                assert_eq!(language.as_deref(), Some("rust"));
            }
            _ => panic!("Expected CodeBlock metadata"),
        }

        // Second code block has no language
        match &code_blocks[1].metadata {
            NodeMetadata::CodeBlock { language } => {
                assert!(language.is_none());
            }
            _ => panic!("Expected CodeBlock metadata"),
        }
    }

    #[test]
    fn parse_empty_input() {
        let nodes = parse_markdown("");
        assert!(nodes.is_empty());
    }

    #[test]
    fn parse_paragraphs_and_line_ranges() {
        let md = "First paragraph.\n\nSecond paragraph.\n";
        let nodes = parse_markdown(md);

        let paragraphs: Vec<_> = nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Paragraph)
            .collect();
        assert_eq!(paragraphs.len(), 2);

        // Line ranges should be 1-indexed and non-overlapping
        assert_eq!(paragraphs[0].line_range.0, 1);
        assert!(paragraphs[0].line_range.1 <= paragraphs[1].line_range.0);

        // All paragraph metadata should be NodeMetadata::Paragraph
        assert!(matches!(paragraphs[0].metadata, NodeMetadata::Paragraph));
        assert!(matches!(paragraphs[1].metadata, NodeMetadata::Paragraph));
    }
}
