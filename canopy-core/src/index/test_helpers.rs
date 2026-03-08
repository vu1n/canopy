/// Shared test utilities for the index module.
use super::RepoIndex;
use std::fs;
use std::io::Write;
use tempfile::TempDir;

/// Create a test repo with N Rust files containing a function and struct each.
/// Returns the [`TempDir`] guard (dropping it cleans up the directory).
pub(crate) fn setup_repo(n: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    for i in 0..n {
        let path = src.join(format!("file_{}.rs", i));
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            "fn func_{i}() {{ println!(\"hello from {i}\"); }}\nstruct Struct{i} {{ x: i32 }}",
        )
        .unwrap();
    }

    RepoIndex::init(dir.path()).unwrap();
    dir
}
