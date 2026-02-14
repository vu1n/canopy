//! Dirty file detection and local index overlay

use canopy_core::CanopyError;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

/// Status of a dirty (uncommitted) file
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirtyStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Unmerged,
}

/// A file with uncommitted changes
#[derive(Debug, Clone)]
pub struct DirtyFile {
    pub path: String,
    pub status: DirtyStatus,
}

/// Collection of dirty files with a fingerprint for cache invalidation
#[derive(Debug, Clone)]
pub struct DirtyState {
    pub files: Vec<DirtyFile>,
    pub fingerprint: String,
}

impl DirtyState {
    /// Check if a path is dirty
    pub fn is_dirty(&self, path: &str) -> bool {
        self.files.iter().any(|f| f.path == path)
    }

    /// Get set of dirty file paths
    pub fn dirty_paths(&self) -> std::collections::HashSet<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }

    /// Check if there are any dirty files
    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }
}

/// Detect dirty (uncommitted) files in a git repository
///
/// Uses `git status --porcelain=v2 -z` for reliable parsing.
/// Record types:
/// - `1` = changed entry (modified, added, deleted)
/// - `2` = renamed/copied entry
/// - `u` = unmerged entry
/// - `?` = untracked entry
pub fn detect_dirty(repo_root: &Path) -> canopy_core::Result<DirtyState> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v2", "-z"])
        .current_dir(repo_root)
        .output()
        .map_err(CanopyError::Io)?;

    if !output.status.success() {
        return Err(CanopyError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "git status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files = parse_porcelain_v2(&stdout);
    let fingerprint = compute_fingerprint(&files, repo_root);

    Ok(DirtyState { files, fingerprint })
}

/// Parse git status --porcelain=v2 -z output
///
/// Format (NUL-delimited):
/// - `1 <XY> ...  <path>` — changed entry
/// - `2 <XY> ...  <path>\0<origPath>` — renamed/copied
/// - `u <XY> ...  <path>` — unmerged
/// - `? <path>` — untracked
fn parse_porcelain_v2(output: &str) -> Vec<DirtyFile> {
    let mut files = Vec::new();
    let mut parts = output.split('\0').peekable();

    while let Some(entry) = parts.next() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        if entry.starts_with("1 ") {
            // Changed entry: 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            if let Some(path) = extract_path_from_ordinary(entry) {
                let xy = &entry[2..4];
                let status = classify_xy(xy);
                files.push(DirtyFile { path, status });
            }
        } else if entry.starts_with("2 ") {
            // Renamed/copied: 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>
            if let Some(path) = extract_path_from_renamed(entry) {
                files.push(DirtyFile {
                    path,
                    status: DirtyStatus::Renamed,
                });
                // Skip the origPath (next NUL-delimited field)
                let _ = parts.next();
            }
        } else if entry.starts_with("u ") {
            // Unmerged: u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
            if let Some(path) = extract_path_from_unmerged(entry) {
                files.push(DirtyFile {
                    path,
                    status: DirtyStatus::Unmerged,
                });
            }
        } else if entry.starts_with("? ") {
            // Untracked
            let path = entry[2..].to_string();
            files.push(DirtyFile {
                path,
                status: DirtyStatus::Added,
            });
        }
    }

    files
}

/// Extract path from an ordinary changed entry (type 1)
/// Format: 1 XY sub mH mI mW hH hI path
/// Fields are space-separated, path is the 9th field (index 8)
fn extract_path_from_ordinary(entry: &str) -> Option<String> {
    let fields: Vec<&str> = entry.splitn(9, ' ').collect();
    if fields.len() >= 9 {
        Some(fields[8].to_string())
    } else {
        None
    }
}

/// Extract path from a renamed/copied entry (type 2)
/// Format: 2 XY sub mH mI mW hH hI Xscore path
/// Fields are space-separated, path is the 10th field (index 9)
fn extract_path_from_renamed(entry: &str) -> Option<String> {
    let fields: Vec<&str> = entry.splitn(10, ' ').collect();
    if fields.len() >= 10 {
        Some(fields[9].to_string())
    } else {
        None
    }
}

/// Extract path from an unmerged entry (type u)
/// Format: u XY sub m1 m2 m3 mW h1 h2 h3 path
/// Fields are space-separated, path is the 11th field (index 10)
fn extract_path_from_unmerged(entry: &str) -> Option<String> {
    let fields: Vec<&str> = entry.splitn(11, ' ').collect();
    if fields.len() >= 11 {
        Some(fields[10].to_string())
    } else {
        None
    }
}

/// Classify XY status codes to DirtyStatus
fn classify_xy(xy: &str) -> DirtyStatus {
    let bytes = xy.as_bytes();
    // Check index status (first char) and worktree status (second char)
    // D in either position = deleted
    // A in index = added
    // Otherwise = modified
    if bytes.len() >= 2 {
        if bytes[0] == b'D' || bytes[1] == b'D' {
            return DirtyStatus::Deleted;
        }
        if bytes[0] == b'A' {
            return DirtyStatus::Added;
        }
    }
    DirtyStatus::Modified
}

/// Compute a fingerprint from dirty files for cache invalidation
///
/// SHA256 of sorted (path, mtime) pairs. If mtime is unavailable, uses 0.
pub fn compute_fingerprint(dirty_files: &[DirtyFile], repo_root: &Path) -> String {
    let mut entries: Vec<(String, u64)> = dirty_files
        .iter()
        .map(|f| {
            let full_path = repo_root.join(&f.path);
            let mtime = std::fs::metadata(&full_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (f.path.clone(), mtime)
        })
        .collect();
    entries.sort();

    let mut hasher = Sha256::new();
    for (path, mtime) in &entries {
        hasher.update(format!("{}:{}\n", path, mtime).as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Rebuild local index for dirty files only
///
/// 1. Invalidate dirty paths in the index
/// 2. Re-index only the dirty files (non-deleted ones)
pub fn rebuild_local_index(
    index: &mut canopy_core::RepoIndex,
    dirty: &DirtyState,
    _repo_root: &Path,
) -> canopy_core::Result<()> {
    if dirty.is_clean() {
        return Ok(());
    }

    // Invalidate all dirty file paths
    for file in &dirty.files {
        // Use glob to invalidate specific files
        index.invalidate(Some(&file.path))?;
    }

    // Re-index non-deleted dirty files
    for file in &dirty.files {
        if file.status != DirtyStatus::Deleted {
            // Index just this file's glob pattern
            index.index(&file.path)?;
        }
    }

    Ok(())
}

/// Path to the cached fingerprint file
fn fingerprint_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join(".canopy").join("dirty_fingerprint")
}

/// Check if the dirty state has changed since last rebuild
pub fn needs_rebuild(dirty: &DirtyState, repo_root: &Path) -> bool {
    let fp_path = fingerprint_path(repo_root);
    match std::fs::read_to_string(&fp_path) {
        Ok(cached) => cached.trim() != dirty.fingerprint,
        Err(_) => true, // No cached fingerprint
    }
}

/// Save the current fingerprint after a successful rebuild
pub fn save_fingerprint(dirty: &DirtyState, repo_root: &Path) -> canopy_core::Result<()> {
    let fp_path = fingerprint_path(repo_root);
    std::fs::write(&fp_path, &dirty.fingerprint)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ordinary_modified() {
        let output = "1 .M N... 100644 100644 100644 abc123 def456 src/main.rs\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].status, DirtyStatus::Modified);
    }

    #[test]
    fn test_parse_ordinary_added() {
        let output = "1 A. N... 000000 100644 100644 0000000 abc123 src/new.rs\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/new.rs");
        assert_eq!(files[0].status, DirtyStatus::Added);
    }

    #[test]
    fn test_parse_ordinary_deleted() {
        let output = "1 D. N... 100644 000000 000000 abc123 0000000 src/old.rs\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/old.rs");
        assert_eq!(files[0].status, DirtyStatus::Deleted);
    }

    #[test]
    fn test_parse_renamed() {
        let output =
            "2 R. N... 100644 100644 100644 abc123 def456 R100 new_name.rs\0old_name.rs\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new_name.rs");
        assert_eq!(files[0].status, DirtyStatus::Renamed);
    }

    #[test]
    fn test_parse_untracked() {
        let output = "? untracked.txt\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "untracked.txt");
        assert_eq!(files[0].status, DirtyStatus::Added);
    }

    #[test]
    fn test_parse_unmerged() {
        let output =
            "u UU N... 100644 100644 100644 100644 abc123 def456 789abc src/conflict.rs\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/conflict.rs");
        assert_eq!(files[0].status, DirtyStatus::Unmerged);
    }

    #[test]
    fn test_parse_multiple() {
        let output = "1 .M N... 100644 100644 100644 abc123 def456 src/a.rs\0? new.txt\01 D. N... 100644 000000 000000 abc123 0000000 src/old.rs\0";
        let files = parse_porcelain_v2(output);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status, DirtyStatus::Modified);
        assert_eq!(files[1].status, DirtyStatus::Added);
        assert_eq!(files[2].status, DirtyStatus::Deleted);
    }

    #[test]
    fn test_parse_empty() {
        let files = parse_porcelain_v2("");
        assert!(files.is_empty());
    }

    #[test]
    fn test_dirty_state_is_dirty() {
        let state = DirtyState {
            files: vec![DirtyFile {
                path: "src/main.rs".to_string(),
                status: DirtyStatus::Modified,
            }],
            fingerprint: "abc".to_string(),
        };
        assert!(state.is_dirty("src/main.rs"));
        assert!(!state.is_dirty("src/other.rs"));
    }

    #[test]
    fn test_dirty_state_is_clean() {
        let state = DirtyState {
            files: vec![],
            fingerprint: "empty".to_string(),
        };
        assert!(state.is_clean());
    }

    #[test]
    fn test_classify_xy() {
        assert_eq!(classify_xy(".M"), DirtyStatus::Modified);
        assert_eq!(classify_xy("M."), DirtyStatus::Modified);
        assert_eq!(classify_xy("A."), DirtyStatus::Added);
        assert_eq!(classify_xy("D."), DirtyStatus::Deleted);
        assert_eq!(classify_xy(".D"), DirtyStatus::Deleted);
    }

    #[test]
    fn test_compute_fingerprint_deterministic() {
        let files = vec![
            DirtyFile {
                path: "b.rs".to_string(),
                status: DirtyStatus::Modified,
            },
            DirtyFile {
                path: "a.rs".to_string(),
                status: DirtyStatus::Modified,
            },
        ];
        let tmp = std::env::temp_dir();
        let fp1 = compute_fingerprint(&files, &tmp);
        let fp2 = compute_fingerprint(&files, &tmp);
        assert_eq!(fp1, fp2); // Same files → same fingerprint
    }
}
