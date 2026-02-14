use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

/// Run a git command and assert it succeeded
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

/// Helper to create a test git repo with known content
fn create_test_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // git init and configure
    git(root, &["init"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // Create a Rust file with known symbols
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

    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);

    // Verify HEAD exists
    let head = git(root, &["rev-parse", "HEAD"]);
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert!(sha.len() >= 40, "Expected valid commit SHA, got: {}", sha);

    dir
}

/// Helper to find a free port
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Helper to wait for the service to be ready
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

#[test]
fn test_service_lifecycle() {
    let repo = create_test_repo();
    let port = free_port();
    let base_url = format!("http://127.0.0.1:{}", port);

    // Start service
    let mut service = Command::new(env!("CARGO_BIN_EXE_canopy-service"))
        .args(["--port", &port.to_string()])
        .spawn()
        .expect("Failed to start canopy-service");

    // Wait for service
    assert!(
        wait_for_service(&base_url, Duration::from_secs(5)),
        "Service failed to start"
    );

    let client = reqwest::blocking::Client::new();

    // 1. Add repo
    let resp: serde_json::Value = client
        .post(&format!("{}/repos/add", base_url))
        .json(&serde_json::json!({
            "path": repo.path().to_string_lossy().to_string(),
            "name": "test-repo"
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();

    let repo_id = resp["repo_id"].as_str().unwrap().to_string();
    assert!(!repo_id.is_empty());

    // 2. Reindex
    let resp: serde_json::Value = client
        .post(&format!("{}/reindex", base_url))
        .json(&serde_json::json!({ "repo": &repo_id }))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["status"].as_str().unwrap(), "indexing");

    // 3. Wait for indexing to complete
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

    // 4. Query -- verify handle metadata
    let resp: serde_json::Value = client
        .post(&format!("{}/query", base_url))
        .json(&serde_json::json!({
            "repo": &repo_id,
            "symbol": "hello_world"
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();

    let handles = resp["handles"].as_array().expect(&format!(
        "Expected handles array in response: {}",
        serde_json::to_string_pretty(&resp).unwrap()
    ));
    assert!(!handles.is_empty(), "Expected at least one handle");
    let handle = &handles[0];
    assert_eq!(
        handle["source"].as_str().unwrap(),
        "service",
        "Handle: {}",
        serde_json::to_string_pretty(handle).unwrap()
    );
    assert!(
        handle["commit_sha"].as_str().is_some(),
        "Expected commit_sha in handle: {}",
        serde_json::to_string_pretty(handle).unwrap()
    );
    assert!(
        handle["generation"].as_u64().is_some(),
        "Expected generation in handle: {}",
        serde_json::to_string_pretty(handle).unwrap()
    );

    let handle_id = handle["id"].as_str().unwrap();
    let generation = handle["generation"].as_u64().unwrap();

    // 5. Expand valid handle
    let resp: serde_json::Value = client
        .post(&format!("{}/expand", base_url))
        .json(&serde_json::json!({
            "repo": &repo_id,
            "handles": [{ "id": handle_id, "generation": generation }]
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();

    let contents = resp["contents"].as_array().unwrap();
    assert!(!contents.is_empty());
    assert!(contents[0]["content"]
        .as_str()
        .unwrap()
        .contains("hello_world"));

    // 6. Expand with stale generation -> 409
    let resp = client
        .post(&format!("{}/expand", base_url))
        .json(&serde_json::json!({
            "repo": &repo_id,
            "handles": [{ "id": handle_id, "generation": generation + 999 }]
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["code"].as_str().unwrap(), "stale_generation");

    // Cleanup
    service.kill().ok();
}
