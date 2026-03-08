//! File discovery backends: fd, ripgrep, ignore crate.

use super::RepoIndex;
use crate::error::CanopyError;
use ignore::WalkBuilder;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// File discovery backend
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileDiscovery {
    Fd,
    Ripgrep,
    Ignore,
}

/// Cached detection result — avoids repeated process spawns.
static DETECTED_BACKEND: OnceLock<FileDiscovery> = OnceLock::new();

impl FileDiscovery {
    /// Detect the best available file discovery tool (cached after first call).
    pub fn detect() -> Self {
        *DETECTED_BACKEND.get_or_init(Self::probe)
    }

    /// Probe for available tools without caching.
    fn probe() -> Self {
        if Command::new("fd").arg("--version").output().is_ok() {
            return Self::Fd;
        }
        if Command::new("rg").arg("--version").output().is_ok() {
            return Self::Ripgrep;
        }
        Self::Ignore
    }

    /// Get the name of the discovery tool
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fd => "fd",
            Self::Ripgrep => "ripgrep",
            Self::Ignore => "ignore-crate",
        }
    }
}

impl RepoIndex {
    /// Walk files matching glob, respecting .gitignore
    /// Uses fd > ripgrep > ignore crate (in order of preference)
    pub fn walk_files(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let discovery = FileDiscovery::detect();

        match discovery {
            FileDiscovery::Fd => self.walk_files_fd(glob),
            FileDiscovery::Ripgrep => self.walk_files_rg(glob),
            FileDiscovery::Ignore => self.walk_files_ignore(glob),
        }
    }

    /// Walk files using fd (fastest)
    fn walk_files_fd(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let mut cmd = Command::new("fd");
        cmd.arg("--type").arg("f");
        cmd.arg("--hidden"); // Include hidden, let .gitignore handle it

        // Use glob pattern for filtering (supports directory patterns like **/auth/**/*.ts)
        // -p enables full path matching (not just filename)
        cmd.arg("--glob").arg("-p").arg(glob);

        // Add exclusions from config
        for pattern in &self.config.ignore.patterns {
            cmd.arg("--exclude").arg(pattern);
        }

        // Search in repo root
        cmd.arg(&self.repo_root);

        let output = cmd.output().map_err(CanopyError::Io)?;

        if !output.status.success() {
            // Fallback to ignore crate on error
            return self.walk_files_ignore(glob);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<PathBuf> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();

        Ok(files)
    }

    /// Walk files using ripgrep --files
    fn walk_files_rg(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let mut cmd = Command::new("rg");
        cmd.arg("--files");
        cmd.arg("--hidden"); // Include hidden, let .gitignore handle it

        // Use glob pattern for filtering (supports directory patterns like **/auth/**/*.ts)
        cmd.arg("--glob").arg(glob);

        // Add exclusions from config
        for pattern in &self.config.ignore.patterns {
            cmd.arg("--glob").arg(format!("!{}", pattern));
            cmd.arg("--glob").arg(format!("!{}/**", pattern));
        }

        // Search in repo root
        cmd.arg(&self.repo_root);

        let output = cmd.output().map_err(CanopyError::Io)?;

        if !output.status.success() {
            // Fallback to ignore crate on error
            return self.walk_files_ignore(glob);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<PathBuf> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();

        Ok(files)
    }

    /// Walk files using ignore crate (fallback)
    fn walk_files_ignore(&self, glob: &str) -> crate::Result<Vec<PathBuf>> {
        let mut builder = WalkBuilder::new(&self.repo_root);
        builder.hidden(false);
        builder.git_ignore(true);
        builder.git_global(true);
        builder.git_exclude(true);

        // Build glob matcher for inclusion
        let mut glob_builder = globset::GlobSetBuilder::new();
        glob_builder
            .add(globset::Glob::new(glob).map_err(|e| CanopyError::GlobPattern(e.to_string()))?);
        let glob_set = glob_builder
            .build()
            .map_err(|e| CanopyError::GlobPattern(e.to_string()))?;

        // Build glob matcher for custom ignore patterns
        let mut ignore_builder = globset::GlobSetBuilder::new();
        for pattern in &self.config.ignore.patterns {
            let glob_pattern = if pattern.contains('*') || pattern.contains('?') {
                pattern.clone()
            } else {
                format!("**/{}", pattern)
            };
            if let Ok(g) = globset::Glob::new(&glob_pattern) {
                ignore_builder.add(g);
            }
            if let Ok(g) = globset::Glob::new(&format!("**/{}/**", pattern)) {
                ignore_builder.add(g);
            }
        }
        let ignore_set = ignore_builder
            .build()
            .map_err(|e| CanopyError::GlobPattern(e.to_string()))?;

        let mut files = Vec::new();

        for entry in builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();

            if path.is_dir() {
                continue;
            }

            let relative = path.strip_prefix(&self.repo_root).unwrap_or(path);

            if ignore_set.is_match(relative) {
                continue;
            }

            if glob_set.is_match(relative) {
                files.push(path.to_path_buf());
            }
        }

        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn file_discovery_detect_returns_valid_backend() {
        let backend = FileDiscovery::detect();
        // On CI or dev machines, at least one of these should be available
        assert!(
            matches!(
                backend,
                FileDiscovery::Fd | FileDiscovery::Ripgrep | FileDiscovery::Ignore
            ),
            "detect() should return a valid backend"
        );
    }

    #[test]
    fn file_discovery_detect_is_cached() {
        let first = FileDiscovery::detect();
        let second = FileDiscovery::detect();
        assert_eq!(first, second, "repeated detect() calls should return same value");
    }

    #[test]
    fn file_discovery_name_values() {
        assert_eq!(FileDiscovery::Fd.name(), "fd");
        assert_eq!(FileDiscovery::Ripgrep.name(), "ripgrep");
        assert_eq!(FileDiscovery::Ignore.name(), "ignore-crate");
    }

    #[test]
    fn walk_files_finds_matching_files() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();

        // Create some test files
        for name in &["a.rs", "b.rs", "c.txt"] {
            let path = src.join(name);
            let mut f = fs::File::create(&path).unwrap();
            writeln!(f, "// content").unwrap();
        }

        RepoIndex::init(dir.path()).unwrap();
        let index = RepoIndex::open(dir.path()).unwrap();

        let rs_files = index.walk_files("**/*.rs").unwrap();
        assert_eq!(rs_files.len(), 2, "should find 2 .rs files");

        let txt_files = index.walk_files("**/*.txt").unwrap();
        assert_eq!(txt_files.len(), 1, "should find 1 .txt file");

        let all_files = index.walk_files("**/*").unwrap();
        // Should find at least the 3 files we created (plus config.toml in .canopy)
        assert!(all_files.len() >= 3, "should find at least 3 files, got {}", all_files.len());
    }

    #[test]
    fn walk_files_no_matches_returns_empty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();

        let path = src.join("a.rs");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "// content").unwrap();

        RepoIndex::init(dir.path()).unwrap();
        let index = RepoIndex::open(dir.path()).unwrap();

        let files = index.walk_files("**/*.py").unwrap();
        assert!(files.is_empty(), "should find no .py files");
    }
}
