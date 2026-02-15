//! Document model for parsed files

use std::ops::Range;
use std::path::PathBuf;

/// Byte range in source text (always byte offsets, not char indices)
pub type Span = Range<usize>;

/// Node type enum (stored as integer in DB)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NodeType {
    Section = 0,
    CodeBlock = 1,
    Paragraph = 2,
    Function = 3,
    Class = 4,
    Struct = 5,
    Method = 6,
    Chunk = 7, // For line-based chunking fallback
}

impl NodeType {
    pub fn as_int(self) -> u8 {
        self as u8
    }

    pub fn from_int(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::Section),
            1 => Some(Self::CodeBlock),
            2 => Some(Self::Paragraph),
            3 => Some(Self::Function),
            4 => Some(Self::Class),
            5 => Some(Self::Struct),
            6 => Some(Self::Method),
            7 => Some(Self::Chunk),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Section => "section",
            Self::CodeBlock => "code_block",
            Self::Paragraph => "paragraph",
            Self::Function => "function",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Method => "method",
            Self::Chunk => "chunk",
        }
    }
}

/// A node extracted from a file
#[derive(Debug, Clone)]
pub struct DocumentNode {
    pub node_type: NodeType,
    pub span: Span,
    pub line_range: (usize, usize), // 1-indexed line numbers
    pub metadata: NodeMetadata,
    /// Parent symbol name (e.g., class name for methods)
    pub parent_name: Option<String>,
    /// Handle ID of parent node (if applicable)
    pub parent_handle_id: Option<String>,
    /// Parent node type (if applicable)
    pub parent_node_type: Option<NodeType>,
    /// Parent node span (if applicable)
    pub parent_span: Option<Span>,
}

/// Type-specific metadata
#[derive(Debug, Clone)]
pub enum NodeMetadata {
    Section {
        heading: String,
        level: u8,
    },
    CodeBlock {
        language: Option<String>,
    },
    Paragraph,
    Function {
        name: String,
        signature: Option<String>,
    },
    Class {
        name: String,
    },
    Struct {
        name: String,
    },
    Method {
        name: String,
        class_name: Option<String>,
    },
    Chunk {
        index: usize,
    },
}

impl NodeMetadata {
    /// Serialize metadata to JSON for storage
    pub fn to_json(&self) -> String {
        match self {
            Self::Section { heading, level } => serde_json::json!({
                "type": "section",
                "heading": heading,
                "level": level
            })
            .to_string(),
            Self::CodeBlock { language } => serde_json::json!({
                "type": "code_block",
                "language": language
            })
            .to_string(),
            Self::Paragraph => serde_json::json!({ "type": "paragraph" }).to_string(),
            Self::Function { name, signature } => serde_json::json!({
                "type": "function",
                "name": name,
                "signature": signature
            })
            .to_string(),
            Self::Class { name } => serde_json::json!({
                "type": "class",
                "name": name
            })
            .to_string(),
            Self::Struct { name } => serde_json::json!({
                "type": "struct",
                "name": name
            })
            .to_string(),
            Self::Method { name, class_name } => serde_json::json!({
                "type": "method",
                "name": name,
                "class_name": class_name
            })
            .to_string(),
            Self::Chunk { index } => serde_json::json!({
                "type": "chunk",
                "index": index
            })
            .to_string(),
        }
    }

    /// Deserialize metadata from JSON
    pub fn from_json(json: &str, node_type: NodeType) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;

        match node_type {
            NodeType::Section => Some(Self::Section {
                heading: v.get("heading")?.as_str()?.to_string(),
                level: v.get("level")?.as_u64()? as u8,
            }),
            NodeType::CodeBlock => Some(Self::CodeBlock {
                language: v.get("language").and_then(|l| l.as_str()).map(String::from),
            }),
            NodeType::Paragraph => Some(Self::Paragraph),
            NodeType::Function => Some(Self::Function {
                name: v.get("name")?.as_str()?.to_string(),
                signature: v
                    .get("signature")
                    .and_then(|s| s.as_str())
                    .map(String::from),
            }),
            NodeType::Class => Some(Self::Class {
                name: v.get("name")?.as_str()?.to_string(),
            }),
            NodeType::Struct => Some(Self::Struct {
                name: v.get("name")?.as_str()?.to_string(),
            }),
            NodeType::Method => Some(Self::Method {
                name: v.get("name")?.as_str()?.to_string(),
                class_name: v
                    .get("class_name")
                    .and_then(|s| s.as_str())
                    .map(String::from),
            }),
            NodeType::Chunk => Some(Self::Chunk {
                index: v.get("index")?.as_u64()? as usize,
            }),
        }
    }

    /// Get searchable text content from metadata (for symbol search)
    pub fn searchable_name(&self) -> Option<&str> {
        match self {
            Self::Section { heading, .. } => Some(heading),
            Self::Function { name, .. } => Some(name),
            Self::Class { name } => Some(name),
            Self::Struct { name } => Some(name),
            Self::Method { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// Reference type (call, import, type usage)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefType {
    /// Function/method call
    Call,
    /// Import statement
    Import,
    /// Type reference (type annotation, inheritance)
    TypeRef,
}

impl RefType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Import => "import",
            Self::TypeRef => "type_ref",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "call" => Some(Self::Call),
            "import" => Some(Self::Import),
            "type_ref" => Some(Self::TypeRef),
            _ => None,
        }
    }
}

/// A reference (call, import, type usage) extracted from code
#[derive(Debug, Clone)]
pub struct Reference {
    /// The unqualified name being referenced
    pub name: String,
    /// Optional qualifier (e.g., module path, object name)
    pub qualifier: Option<String>,
    /// Type of reference
    pub ref_type: RefType,
    /// Byte span of the reference in source
    pub span: Span,
    /// Line range (1-indexed)
    pub line_range: (usize, usize),
}

/// Parsed file with nodes and references
#[derive(Debug)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub source: String,
    pub content_hash: [u8; 32],
    pub nodes: Vec<DocumentNode>,
    pub refs: Vec<Reference>,
    pub total_tokens: usize,
}
