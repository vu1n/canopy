//! Handle types for referencing content without expansion

use crate::document::RefType;
use crate::{CanopyError, NodeType, Span};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::{self, Display};
use std::str::FromStr;

/// Stable handle ID: hash of (file_path, node_type, span)
/// Survives reindex as long as content location unchanged
/// Displayed with 'h' prefix (e.g., "h1a2b3c4d5e6"), stored without prefix
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HandleId(String); // hex-encoded hash prefix (internal, no 'h')

impl HandleId {
    /// Create a new handle ID from file path, node type, and span
    pub fn new(file_path: &str, node_type: NodeType, span: &Span) -> Self {
        let input = format!(
            "{}:{}:{}-{}",
            file_path,
            node_type.as_int(),
            span.start,
            span.end
        );
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();
        Self(hex::encode(&hash[..12])) // 24-char hex prefix (12 bytes)
    }

    /// Get the raw ID without prefix
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// Create a HandleId from a raw string (crate-internal use)
    pub(crate) fn from_raw(raw: String) -> Self {
        Self(raw)
    }
}

impl Display for HandleId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "h{}", self.0) // Add 'h' prefix for display
    }
}

impl FromStr for HandleId {
    type Err = CanopyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Accept both "h1a2b3c4d5e6" and "1a2b3c4d5e6"
        let s = s.strip_prefix('h').unwrap_or(s);

        // Validate it's hex
        if !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CanopyError::InvalidHandle(format!(
                "Invalid handle ID: {}",
                s
            )));
        }

        Ok(HandleId(s.to_string()))
    }
}

/// A handle representing a reference to content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handle {
    pub id: HandleId,
    pub file_path: String, // Repo-relative
    pub node_type: NodeType,
    pub span: Span,                 // Byte range in file
    pub line_range: (usize, usize), // For display (1-indexed)
    pub token_count: usize,
    pub preview: String,
    /// Full content, populated when expand_budget is set and results fit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl Handle {
    /// Create a new handle
    pub fn new(
        file_path: String,
        node_type: NodeType,
        span: Span,
        line_range: (usize, usize),
        token_count: usize,
        preview: String,
    ) -> Self {
        let id = HandleId::new(&file_path, node_type, &span);
        Self {
            id,
            file_path,
            node_type,
            span,
            line_range,
            token_count,
            preview,
            content: None,
        }
    }

    /// Set content on the handle (for auto-expansion)
    pub fn with_content(mut self, content: String) -> Self {
        self.content = Some(content);
        self
    }
}

// Serialize NodeType as string for JSON output
impl Serialize for NodeType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NodeType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "section" => Ok(NodeType::Section),
            "code_block" => Ok(NodeType::CodeBlock),
            "paragraph" => Ok(NodeType::Paragraph),
            "function" => Ok(NodeType::Function),
            "class" => Ok(NodeType::Class),
            "struct" => Ok(NodeType::Struct),
            "method" => Ok(NodeType::Method),
            "chunk" => Ok(NodeType::Chunk),
            _ => Err(serde::de::Error::custom(format!(
                "Unknown node type: {}",
                s
            ))),
        }
    }
}

/// Handle for a reference (call, import, type usage)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefHandle {
    /// File path (repo-relative)
    pub file_path: String,
    /// Byte span of the reference
    pub span: Span,
    /// Line range (1-indexed)
    pub line_range: (usize, usize),
    /// The referenced name (unqualified)
    pub name: String,
    /// Optional qualifier (e.g., object name, module path)
    pub qualifier: Option<String>,
    /// Type of reference
    pub ref_type: RefType,
    /// Handle of the containing function/class (if any)
    pub source_handle: Option<HandleId>,
    /// Preview text around the reference
    pub preview: String,
}

// Serialize RefType as string for JSON output
impl Serialize for RefType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RefType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        RefType::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("Unknown ref type: {}", s)))
    }
}

/// UTF-8 safe string extraction
pub fn safe_slice(s: &str, start: usize, end: usize) -> &str {
    let len = s.len();
    let start = start.min(len);
    let end = end.min(len);

    // Find valid UTF-8 boundaries
    let start = (start..len).find(|&i| s.is_char_boundary(i)).unwrap_or(len);
    let end = (0..=end)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);

    if start >= end {
        ""
    } else {
        &s[start..end]
    }
}

/// Preview generation with char-boundary safety
pub fn generate_preview(source: &str, span: &Span, max_bytes: usize) -> String {
    let content = safe_slice(source, span.start, span.end);

    // Take first max_bytes, finding char boundary
    let preview_end = max_bytes.min(content.len());
    let preview = safe_slice(content, 0, preview_end);

    // Clean up whitespace
    let preview = preview.trim();

    // Collapse multiple whitespace into single space
    let preview: String = preview.split_whitespace().collect::<Vec<_>>().join(" ");

    if content.len() > max_bytes {
        format!("{}...", preview)
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_id_creation() {
        let id1 = HandleId::new("src/main.rs", NodeType::Function, &(100..200));
        let id2 = HandleId::new("src/main.rs", NodeType::Function, &(100..200));
        let id3 = HandleId::new("src/main.rs", NodeType::Function, &(100..201));

        assert_eq!(id1, id2); // Same inputs = same ID
        assert_ne!(id1, id3); // Different span = different ID
    }

    #[test]
    fn test_handle_id_display() {
        let id = HandleId::new("test.rs", NodeType::Section, &(0..10));
        let displayed = id.to_string();
        assert!(displayed.starts_with('h'));
        assert_eq!(displayed.len(), 25); // 'h' + 24 hex chars
    }

    #[test]
    fn test_handle_id_parse() {
        let id = HandleId::new("test.rs", NodeType::Section, &(0..10));
        let displayed = id.to_string();

        // Parse with prefix
        let parsed: HandleId = displayed.parse().unwrap();
        assert_eq!(id, parsed);

        // Parse without prefix
        let parsed2: HandleId = id.raw().parse().unwrap();
        assert_eq!(id, parsed2);
    }

    #[test]
    fn test_safe_slice() {
        let s = "Hello, 世界!";
        assert_eq!(safe_slice(s, 0, 5), "Hello");
        assert_eq!(safe_slice(s, 7, 13), "世界"); // Multi-byte chars
        assert_eq!(safe_slice(s, 0, 100), s); // Beyond end
        assert_eq!(safe_slice(s, 8, 10), ""); // Mid-char boundaries
    }

    #[test]
    fn test_generate_preview() {
        let source = "fn main() {\n    println!(\"Hello\");\n}";
        let span = 0..source.len();

        let preview = generate_preview(source, &span, 20);
        assert!(preview.len() <= 23); // 20 + "..."
        assert!(preview.ends_with("..."));

        let short_preview = generate_preview(source, &span, 100);
        assert!(!short_preview.ends_with("..."));
    }
}
