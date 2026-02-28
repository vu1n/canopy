//! Integration tests for the canopy-client service-mode query+dirty merge flow.
//!
//! These tests spin up a real canopy-service, register a test repo, and verify
//! that ClientRuntime correctly queries the service, detects dirty files, and
//! merges results.

use canopy_client::runtime::{ClientRuntime, ExpandOutcome, QueryInput};
use canopy_core::{HandleSource, QueryParams};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn git(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Create a test git repo with two Rust files.
fn create_test_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    git(root, &["init"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        r#"
fn hello_world() {
    println!("Hello, world!");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

struct Config {
    name: String,
    port: u16,
}
"#,
    )
    .unwrap();

    std::fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

pub struct Database {
    url: String,
}
"#,
    )
    .unwrap();

    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);

    dir
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Find the canopy-service binary next to the test binary.
fn service_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("canopy-service");
    path
}

fn wait_for_service(base_url: &str, timeout: Duration) -> bool {
    let client = reqwest::blocking::Client::new();
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if client.get(&format!("{}/status", base_url)).send().is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Start a canopy-service, register+reindex a repo, and return (service, port, repo_id).
struct TestService {
    _process: std::process::Child,
    base_url: String,
    #[allow(dead_code)]
    repo_id: String,
    _repo_dir: TempDir,
    repo_path: std::path::PathBuf,
}

impl TestService {
    fn start(repo_dir: TempDir) -> Self {
        let port = free_port();
        let base_url = format!("http://127.0.0.1:{}", port);
        let bin = service_binary();
        assert!(bin.exists(), "canopy-service binary not found at {:?}", bin);

        let process = Command::new(&bin)
            .args(["--port", &port.to_string()])
            .spawn()
            .expect("Failed to start canopy-service");

        assert!(
            wait_for_service(&base_url, Duration::from_secs(5)),
            "Service failed to start"
        );

        let client = reqwest::blocking::Client::new();

        // Add repo
        let resp: serde_json::Value = client
            .post(&format!("{}/repos/add", base_url))
            .json(&serde_json::json!({
                "path": repo_dir.path().to_string_lossy().to_string(),
                "name": "test-repo"
            }))
            .send()
            .unwrap()
            .json()
            .unwrap();
        let repo_id = resp["repo_id"].as_str().unwrap().to_string();

        // Reindex
        client
            .post(&format!("{}/reindex", base_url))
            .json(&serde_json::json!({ "repo": &repo_id }))
            .send()
            .unwrap();

        // Wait for ready
        let mut ready = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            let resp: serde_json::Value = client
                .get(&format!("{}/repos", base_url))
                .send()
                .unwrap()
                .json()
                .unwrap();
            if let Some(repos) = resp.as_array() {
                if let Some(repo) = repos
                    .iter()
                    .find(|r| r["repo_id"].as_str() == Some(&repo_id))
                {
                    if repo["status"].as_str() == Some("ready") {
                        ready = true;
                        break;
                    }
                }
            }
        }
        assert!(ready, "Repo never became ready");

        let repo_path = repo_dir.path().to_path_buf();
        TestService {
            _process: process,
            base_url,
            repo_id,
            _repo_dir: repo_dir,
            repo_path,
        }
    }

    fn runtime(&self) -> ClientRuntime {
        ClientRuntime::new(Some(&self.base_url), None)
    }
}

impl Drop for TestService {
    fn drop(&mut self) {
        self._process.kill().ok();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_service_query_returns_service_handles() {
    let repo = create_test_repo();
    let svc = TestService::start(repo);
    let mut rt = svc.runtime();
    assert!(rt.is_service_mode());

    let params = QueryParams::symbol("hello_world".to_string());
    let result = rt
        .query(&svc.repo_path, QueryInput::Params(params))
        .expect("query failed");

    assert!(!result.handles.is_empty(), "Expected at least one handle");
    for handle in &result.handles {
        assert_eq!(handle.source, HandleSource::Service);
        assert!(handle.generation.is_some());
        assert!(handle.commit_sha.is_some());
    }
}

#[test]
fn test_dirty_merge_replaces_handles_for_modified_file() {
    let repo = create_test_repo();
    let svc = TestService::start(repo);
    let mut rt = svc.runtime();

    // Modify src/main.rs locally (without committing)
    std::fs::write(
        svc.repo_path.join("src/main.rs"),
        r#"
fn hello_world() {
    println!("Hello, modified world!");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn new_local_function() {
    // only exists in the dirty working tree
}

struct Config {
    name: String,
    port: u16,
}
"#,
    )
    .unwrap();

    // Query for a symbol in the modified file
    let params = QueryParams::pattern("hello_world".to_string());
    let result = rt
        .query(&svc.repo_path, QueryInput::Params(params))
        .expect("query failed");

    // Handles for the dirty file (src/main.rs) should be Local
    let main_handles: Vec<_> = result
        .handles
        .iter()
        .filter(|h| h.file_path.contains("main.rs"))
        .collect();
    for h in &main_handles {
        assert_eq!(
            h.source,
            HandleSource::Local,
            "Dirty file handle should be Local, got {:?} for {}",
            h.source,
            h.file_path
        );
    }
}

#[test]
fn test_dirty_merge_keeps_service_handles_for_clean_files() {
    let repo = create_test_repo();
    let svc = TestService::start(repo);
    let mut rt = svc.runtime();

    // Modify only main.rs
    std::fs::write(
        svc.repo_path.join("src/main.rs"),
        r#"
fn hello_world() {
    println!("Modified!");
}
"#,
    )
    .unwrap();

    // Query for a symbol in the clean file (lib.rs)
    let params = QueryParams::symbol("multiply".to_string());
    let result = rt
        .query(&svc.repo_path, QueryInput::Params(params))
        .expect("query failed");

    // Handles for the clean file (src/lib.rs) should be Service
    let lib_handles: Vec<_> = result
        .handles
        .iter()
        .filter(|h| h.file_path.contains("lib.rs"))
        .collect();
    assert!(!lib_handles.is_empty(), "Expected handles from lib.rs");
    for h in &lib_handles {
        assert_eq!(
            h.source,
            HandleSource::Service,
            "Clean file handle should be Service, got {:?} for {}",
            h.source,
            h.file_path
        );
    }
}

#[test]
fn test_expand_service_handles() {
    let repo = create_test_repo();
    let svc = TestService::start(repo);
    let mut rt = svc.runtime();

    // Query to get handles
    let params = QueryParams::symbol("Config".to_string());
    let result = rt
        .query(&svc.repo_path, QueryInput::Params(params))
        .expect("query failed");
    assert!(!result.handles.is_empty());

    // Expand the first handle
    let handle_ids: Vec<String> = result.handles.iter().map(|h| h.id.to_string()).collect();
    let outcome: ExpandOutcome = rt
        .expand(&svc.repo_path, &handle_ids)
        .expect("expand failed");

    assert!(
        !outcome.contents.is_empty(),
        "Expected expanded content, got none"
    );
    // The expanded content should contain the Config struct
    let has_config = outcome
        .contents
        .iter()
        .any(|(_, content)| content.contains("Config"));
    assert!(has_config, "Expanded content should contain Config");
}
