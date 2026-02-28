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
