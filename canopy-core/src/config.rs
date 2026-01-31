//! Configuration for canopy

use crate::CanopyError;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Default configuration as TOML
pub const DEFAULT_CONFIG: &str = r#"# Canopy Configuration

[core]
# Time-to-live for cache entries (e.g., "1h", "30m", "1d")
ttl = "1h"
# Token encoding (cl100k_base is GPT-4/Claude compatible)
encoding = "cl100k_base"
# Default maximum results returned by queries
default_result_limit = 100

[indexing]
# Default glob pattern for indexing
# Add more extensions as needed for your project
default_glob = "**/*.{rs,py,js,ts,tsx,jsx,go,md,txt,json,yaml,yml,toml}"
# Files above this size (bytes) are chunked instead of parsed as single node
chunk_threshold = 1000000
# Number of lines per chunk
chunk_lines = 50
# Overlap between chunks
chunk_overlap = 10
# Maximum bytes for preview text
preview_bytes = 100

[fts]
# FTS5 tokenizer (unicode61 without stemming is better for code)
tokenizer = "unicode61"

[ignore]
# Additional patterns to ignore (beyond .gitignore)
patterns = [
    ".git",
    ".canopy",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "*.min.js",
    "*.min.css",
    ".DS_Store",
    "*.lock",
    "package-lock.json",
    "Cargo.lock",
]
"#;

/// Canopy configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub core: CoreConfig,
    #[serde(default)]
    pub indexing: IndexingConfig,
    #[serde(default)]
    pub fts: FtsConfig,
    #[serde(default)]
    pub ignore: IgnoreConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    #[serde(default = "default_ttl")]
    pub ttl: String,
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(default = "default_result_limit")]
    pub default_result_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    #[serde(default = "default_glob")]
    pub default_glob: String,
    #[serde(default = "default_chunk_threshold")]
    pub chunk_threshold: usize,
    #[serde(default = "default_chunk_lines")]
    pub chunk_lines: usize,
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap: usize,
    #[serde(default = "default_preview_bytes")]
    pub preview_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsConfig {
    #[serde(default = "default_tokenizer")]
    pub tokenizer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreConfig {
    #[serde(default = "default_ignore_patterns")]
    pub patterns: Vec<String>,
}

// Default value functions
fn default_ttl() -> String {
    "1h".to_string()
}
fn default_encoding() -> String {
    "cl100k_base".to_string()
}
fn default_result_limit() -> usize {
    100
}
fn default_glob() -> String {
    "**/*.{rs,py,js,ts,tsx,jsx,go,md,txt,json,yaml,yml,toml}".to_string()
}
fn default_chunk_threshold() -> usize {
    1_000_000
}
fn default_chunk_lines() -> usize {
    50
}
fn default_chunk_overlap() -> usize {
    10
}
fn default_preview_bytes() -> usize {
    100
}
fn default_tokenizer() -> String {
    "unicode61".to_string()
}
fn default_ignore_patterns() -> Vec<String> {
    vec![
        ".git".to_string(),
        ".canopy".to_string(),
        "node_modules".to_string(),
        "target".to_string(),
        "__pycache__".to_string(),
    ]
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            ttl: default_ttl(),
            encoding: default_encoding(),
            default_result_limit: default_result_limit(),
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            default_glob: default_glob(),
            chunk_threshold: default_chunk_threshold(),
            chunk_lines: default_chunk_lines(),
            chunk_overlap: default_chunk_overlap(),
            preview_bytes: default_preview_bytes(),
        }
    }
}

impl Default for FtsConfig {
    fn default() -> Self {
        Self {
            tokenizer: default_tokenizer(),
        }
    }
}

impl Default for IgnoreConfig {
    fn default() -> Self {
        Self {
            patterns: default_ignore_patterns(),
        }
    }
}

impl Config {
    /// Load config from a TOML file
    pub fn load(path: &Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_toml(&content)
    }

    /// Parse config from TOML string
    pub fn from_toml(content: &str) -> crate::Result<Self> {
        toml::from_str(content).map_err(|e| CanopyError::ConfigParse(e.to_string()))
    }

    /// Get TTL as Duration
    pub fn ttl_duration(&self) -> Duration {
        parse_duration(&self.core.ttl).unwrap_or(Duration::from_secs(3600))
    }

    /// Get the default glob pattern
    pub fn default_glob(&self) -> &str {
        &self.indexing.default_glob
    }
}

/// Parse duration string (e.g., "1h", "30m", "1d")
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str.parse().ok()?;

    match unit {
        "s" => Some(Duration::from_secs(num)),
        "m" => Some(Duration::from_secs(num * 60)),
        "h" => Some(Duration::from_secs(num * 3600)),
        "d" => Some(Duration::from_secs(num * 86400)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_parses() {
        let config = Config::from_toml(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.core.ttl, "1h");
        assert_eq!(config.core.default_result_limit, 100);
        assert_eq!(config.indexing.chunk_threshold, 1_000_000);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("2d"), Some(Duration::from_secs(172800)));
        assert_eq!(parse_duration("invalid"), None);
    }

    #[test]
    fn test_ttl_duration() {
        let config = Config::from_toml(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.ttl_duration(), Duration::from_secs(3600));
    }
}
