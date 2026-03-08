//! Shared git utilities used by both client and service.

use std::path::Path;
use std::process::Command;

/// Get the HEAD commit SHA for a repo, or None if not a git repo / git unavailable.
pub fn head_commit_sha(repo_root: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn head_commit_sha_on_this_repo() {
        // Use the canopy project root (this is a git repo)
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let sha = head_commit_sha(&repo_root);
        assert!(sha.is_some(), "should return a SHA for the canopy repo");
        let sha = sha.unwrap();
        // Git SHA is 40 hex characters
        assert_eq!(sha.len(), 40, "SHA should be 40 chars, got: {}", sha);
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "SHA should be hex, got: {}",
            sha
        );
    }

    #[test]
    fn head_commit_sha_non_git_dir_returns_none() {
        // /tmp is not a git repo
        let sha = head_commit_sha(Path::new("/tmp"));
        assert!(sha.is_none(), "should return None for non-git directory");
    }

    #[test]
    fn head_commit_sha_nonexistent_dir_returns_none() {
        let sha = head_commit_sha(Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(sha.is_none(), "should return None for nonexistent path");
    }
}
