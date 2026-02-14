//! Generation tracking types for canopy-service

use serde::{Deserialize, Serialize};

/// Monotonically increasing generation counter for staleness detection
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Generation(u64);

impl Generation {
    /// Create a new generation starting at 0
    pub fn new() -> Self {
        Self(0)
    }

    /// Create a generation from a raw value
    pub fn from_value(value: u64) -> Self {
        Self(value)
    }

    /// Increment and return the next generation
    pub fn next(&self) -> Self {
        Self(self.0 + 1)
    }

    /// Get the raw u64 value
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl Default for Generation {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Status of a repository shard in the service
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShardStatus {
    /// Registered but not yet indexed
    Pending,
    /// Currently indexing
    Indexing,
    /// Index is ready for queries
    Ready,
    /// Indexing failed
    Error,
}

impl Default for ShardStatus {
    fn default() -> Self {
        Self::Pending
    }
}

/// A repository shard managed by the service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoShard {
    /// Unique identifier for this repo registration
    pub repo_id: String,
    /// Absolute path to the repo root
    pub repo_root: String,
    /// Human-friendly name (defaults to directory name)
    pub name: String,
    /// Git commit SHA the index was built from
    pub commit_sha: Option<String>,
    /// Current generation counter
    pub generation: Generation,
    /// Current status
    pub status: ShardStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generation_new() {
        let gen = Generation::new();
        assert_eq!(gen.value(), 0);
    }

    #[test]
    fn test_generation_next() {
        let gen = Generation::new();
        let next = gen.next();
        assert_eq!(next.value(), 1);
        assert_eq!(next.next().value(), 2);
    }

    #[test]
    fn test_generation_from_value() {
        let gen = Generation::from_value(42);
        assert_eq!(gen.value(), 42);
    }

    #[test]
    fn test_generation_ordering() {
        let g1 = Generation::from_value(1);
        let g2 = Generation::from_value(2);
        assert!(g1 < g2);
    }

    #[test]
    fn test_generation_serialize() {
        let gen = Generation::from_value(5);
        let json = serde_json::to_string(&gen).unwrap();
        assert_eq!(json, "5");
        let back: Generation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, gen);
    }

    #[test]
    fn test_shard_status_serialize() {
        let status = ShardStatus::Ready;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"ready\"");
    }

    #[test]
    fn test_repo_shard_roundtrip() {
        let shard = RepoShard {
            repo_id: "abc123".to_string(),
            repo_root: "/tmp/repo".to_string(),
            name: "my-repo".to_string(),
            commit_sha: Some("deadbeef".to_string()),
            generation: Generation::from_value(3),
            status: ShardStatus::Ready,
        };
        let json = serde_json::to_string(&shard).unwrap();
        let back: RepoShard = serde_json::from_str(&json).unwrap();
        assert_eq!(back.repo_id, "abc123");
        assert_eq!(back.generation.value(), 3);
    }
}
