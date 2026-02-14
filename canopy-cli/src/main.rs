//! Canopy CLI - Command-line interface for token-efficient codebase queries

#[cfg(feature = "service")]
mod client;
mod dirty;
mod merge;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "canopy")]
#[command(about = "Token-efficient codebase queries", long_about = None)]
struct Cli {
    /// Override repo root detection
    #[arg(long, global = true)]
    root: Option<std::path::PathBuf>,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    /// Service URL for remote queries (e.g., http://localhost:3000)
    #[arg(long, global = true, env = "CANOPY_SERVICE_URL")]
    service_url: Option<String>,

    /// Query mode: auto (local+service merge) or service-only
    #[arg(long, global = true, default_value = "auto")]
    mode: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create .canopy/ and config.toml
    Init,

    /// Index files matching glob pattern
    Index {
        /// Glob pattern (default from config)
        glob: Option<String>,
    },

    /// Run query and show handles
    Query {
        /// Query in s-expression format (e.g., "(grep 'error')")
        query: Option<String>,

        /// Text pattern to search (alternative to positional query arg)
        #[arg(short, long)]
        pattern: Option<String>,

        /// Search for code symbol (function, class, struct)
        #[arg(short, long)]
        symbol: Option<String>,

        /// Filter by parent symbol (e.g., class name for methods)
        #[arg(long)]
        parent: Option<String>,

        /// Query kind: definition, reference, or any (default)
        #[arg(short, long, value_parser = ["definition", "reference", "any"])]
        kind: Option<String>,

        /// Filter by file glob pattern
        #[arg(short, long)]
        glob: Option<String>,

        /// Auto-expand if total tokens fit within budget (default: 5000 when specified without value)
        #[arg(long, num_args = 0..=1, default_missing_value = "5000")]
        expand_budget: Option<usize>,

        /// Override default result limit
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Expand handles to content
    Expand {
        /// Handle IDs to expand
        handle_ids: Vec<String>,
    },

    /// Show index stats
    Status,

    /// Force reindex of files
    Invalidate {
        /// Glob pattern to invalidate (all if omitted)
        glob: Option<String>,
    },

    /// List repos registered with the service
    Repos,

    /// Trigger reindex on the service
    Reindex {
        /// Repo ID to reindex
        repo: String,
        /// Glob pattern override
        #[arg(long)]
        glob: Option<String>,
    },

    /// Show service status
    ServiceStatus,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init => cmd_init(cli.root),
        Commands::Index { glob } => cmd_index(cli.root, glob, cli.json),
        Commands::Query {
            query,
            pattern,
            symbol,
            parent,
            kind,
            glob,
            expand_budget,
            limit,
        } => cmd_query(
            cli.root,
            query,
            pattern,
            symbol,
            parent,
            kind,
            glob,
            expand_budget,
            limit,
            cli.json,
            cli.service_url.clone(),
            cli.mode.clone(),
        ),
        Commands::Expand { handle_ids } => {
            cmd_expand(cli.root, &handle_ids, cli.json, cli.service_url.clone())
        }
        Commands::Status => cmd_status(cli.root, cli.json),
        Commands::Invalidate { glob } => cmd_invalidate(cli.root, glob, cli.json),
        Commands::Repos => cmd_repos(cli.service_url, cli.json),
        Commands::Reindex { repo, glob } => cmd_reindex(cli.service_url, repo, glob, cli.json),
        Commands::ServiceStatus => cmd_service_status(cli.service_url, cli.json),
    };

    if let Err(e) = result {
        if cli.json {
            let error_json = match &e {
                canopy_core::CanopyError::ServiceError {
                    code,
                    message,
                    hint,
                } => {
                    serde_json::json!({ "code": code, "message": message, "hint": hint })
                }
                _ => {
                    serde_json::json!({ "code": "error", "message": e.to_string(), "hint": "" })
                }
            };
            eprintln!("{}", serde_json::to_string_pretty(&error_json).unwrap());
        } else {
            eprintln!("Error: {}", e);
        }
        std::process::exit(1);
    }
}

fn cmd_init(root: Option<std::path::PathBuf>) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    RepoIndex::init(&repo_root)?;

    println!("{} .canopy/config.toml", "Created".green());
    println!("{} .canopy/ to .gitignore", "Added".green());
    Ok(())
}

fn cmd_index(
    root: Option<std::path::PathBuf>,
    glob: Option<String>,
    json: bool,
) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let mut index = RepoIndex::open(&repo_root)?;
    let default_glob = index.config().default_glob().to_string();
    let glob = glob.as_deref().unwrap_or(&default_glob);
    let stats = index.index(glob)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&stats).unwrap());
    } else {
        println!(
            "{}: {} files ({} tokens)",
            "Indexed".green(),
            stats.files_indexed,
            stats.total_tokens
        );
        println!(
            "{}: {} files (cache hit)",
            "Skipped".yellow(),
            stats.files_skipped
        );
        println!(
            "{}: .canopy/index.db ({:.1} MB)",
            "Index".blue(),
            stats.index_size_bytes as f64 / 1_000_000.0
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_query(
    root: Option<std::path::PathBuf>,
    query_str: Option<String>,
    pattern: Option<String>,
    symbol: Option<String>,
    parent: Option<String>,
    kind: Option<String>,
    glob: Option<String>,
    expand_budget: Option<usize>,
    limit: Option<usize>,
    json: bool,
    service_url: Option<String>,
    mode: String,
) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;

    // Build QueryParams from CLI args (shared between local and service paths)
    let params = build_query_params(
        query_str.as_deref(),
        pattern.as_deref(),
        symbol.as_deref(),
        parent.as_deref(),
        kind.as_deref(),
        glob.as_deref(),
        expand_budget,
        limit,
    )?;

    // Determine query mode
    let result = if mode == "service-only" {
        // Service-only mode: only query the service
        #[cfg(feature = "service")]
        {
            let url = require_service_url(&service_url)?;
            let client = client::ServiceClient::new(&url);
            let repo_id = repo_root.to_string_lossy().to_string();
            client.query(&repo_id, params)?
        }
        #[cfg(not(feature = "service"))]
        {
            let _ = service_url;
            return Err(canopy_core::CanopyError::ServiceError {
                code: "feature_disabled".to_string(),
                message: "Service feature is not enabled".to_string(),
                hint: "Rebuild with --features service".to_string(),
            });
        }
    } else if service_url.is_some() {
        // Auto mode with service: merge local + service results
        #[cfg(feature = "service")]
        {
            let url = service_url.as_ref().unwrap();
            let client = client::ServiceClient::new(url);
            let repo_id = repo_root.to_string_lossy().to_string();

            // Detect dirty files
            let dirty_state = dirty::detect_dirty(&repo_root)?;
            let dirty_paths = dirty_state.dirty_paths();

            // Rebuild local index for dirty files if needed
            if !dirty_state.is_clean() && dirty::needs_rebuild(&dirty_state, &repo_root) {
                let mut index = RepoIndex::open(&repo_root)?;
                dirty::rebuild_local_index(&mut index, &dirty_state, &repo_root)?;
                dirty::save_fingerprint(&dirty_state, &repo_root)?;
            }

            // Query local index
            let local_result = {
                let index = RepoIndex::open(&repo_root)?;
                if let Some(ref qs) = query_str {
                    let options = canopy_core::QueryOptions {
                        limit,
                        expand_budget,
                    };
                    index.query_with_options(qs, options)?
                } else {
                    index.query_params(params.clone())?
                }
            };

            // Query service
            let service_result = match client.query(&repo_id, params) {
                Ok(result) => result,
                Err(e) => {
                    // Service query failed, fall back to local-only
                    if !json {
                        eprintln!(
                            "{}: service query failed ({}), using local results only",
                            "Warning".yellow(),
                            e
                        );
                    }
                    return print_query_result(&local_result, json);
                }
            };

            // Merge results
            merge::merge_results(local_result, service_result, &dirty_paths)
        }
        #[cfg(not(feature = "service"))]
        {
            // Fall through to local-only
            let index = RepoIndex::open(&repo_root)?;
            if let Some(ref qs) = query_str {
                let options = canopy_core::QueryOptions {
                    limit,
                    expand_budget,
                };
                index.query_with_options(qs, options)?
            } else {
                index.query_params(params)?
            }
        }
    } else {
        // No service URL: local-only query (original behavior)
        let index = RepoIndex::open(&repo_root)?;

        if let Some(query_str) = query_str {
            // Old DSL path: positional s-expression query
            let options = canopy_core::QueryOptions {
                limit,
                expand_budget,
            };
            index.query_with_options(&query_str, options)?
        } else {
            index.query_params(params)?
        }
    };

    print_query_result(&result, json)
}

/// Build QueryParams from CLI arguments
fn build_query_params(
    _query_str: Option<&str>,
    pattern: Option<&str>,
    symbol: Option<&str>,
    parent: Option<&str>,
    kind: Option<&str>,
    glob: Option<&str>,
    expand_budget: Option<usize>,
    limit: Option<usize>,
) -> canopy_core::Result<canopy_core::QueryParams> {
    use canopy_core::{QueryKind, QueryParams};

    // If using the old DSL path, return a default params (won't be used for local)
    if _query_str.is_some() && pattern.is_none() && symbol.is_none() && parent.is_none() {
        // For service calls with s-expression, convert to pattern search
        // The s-expression will be used for local queries directly
        let mut params = QueryParams::new();
        // Extract a rough pattern from the s-expression for service queries
        // This is a best-effort fallback
        if let Some(qs) = _query_str {
            params.pattern = Some(qs.to_string());
        }
        return Ok(params);
    }

    if pattern.is_none() && symbol.is_none() && parent.is_none() {
        return Err(canopy_core::CanopyError::QueryParse {
            position: 0,
            message: "Must provide either a query s-expression or --pattern/--symbol/--parent flag"
                .to_string(),
        });
    }

    let mut params = QueryParams::new();

    if let Some(p) = pattern {
        params.pattern = Some(p.to_string());
    }
    if let Some(s) = symbol {
        params.symbol = Some(s.to_string());
    }
    if let Some(p) = parent {
        params.parent = Some(p.to_string());
    }
    if let Some(k) = kind {
        params.kind = match k {
            "definition" => QueryKind::Definition,
            "reference" => QueryKind::Reference,
            _ => QueryKind::Any,
        };
    }
    if let Some(g) = glob {
        params.glob = Some(g.to_string());
    }
    if let Some(l) = limit {
        params.limit = Some(l);
    }
    if let Some(eb) = expand_budget {
        params.expand_budget = Some(eb);
    }

    Ok(params)
}

/// Print query results in text or JSON format
fn print_query_result(result: &canopy_core::QueryResult, json: bool) -> canopy_core::Result<()> {
    use colored::Colorize;

    if json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else if let Some(refs) = &result.ref_handles {
        for reference in refs {
            let qualifier = reference
                .qualifier
                .as_ref()
                .map(|q| format!("{}.", q))
                .unwrap_or_default();
            let source = reference
                .source_handle
                .as_ref()
                .map(|h| format!(" in {}", h.to_string().cyan()))
                .unwrap_or_default();
            println!(
                "{}: {}:{}-{} {}{} ({}) {:?}",
                "ref".cyan(),
                reference.file_path,
                reference.line_range.0,
                reference.line_range.1,
                qualifier,
                reference.name,
                reference.ref_type.as_str(),
                reference.preview
            );
            if !source.is_empty() {
                println!("{}", source);
            }
        }
    } else {
        for handle in &result.handles {
            if let Some(content) = &handle.content {
                // Auto-expanded: show full content
                println!(
                    "{}: {}:{}-{} [{} tokens]",
                    handle.id.to_string().cyan(),
                    handle.file_path,
                    handle.line_range.0,
                    handle.line_range.1,
                    handle.token_count,
                );
                println!("{}", content);
                println!();
            } else {
                // Not expanded: show preview
                println!(
                    "{}: {}:{}-{} [{} tokens] {:?}",
                    handle.id.to_string().cyan(),
                    handle.file_path,
                    handle.line_range.0,
                    handle.line_range.1,
                    handle.token_count,
                    handle.preview
                );
            }
        }
    }

    let shown = result
        .ref_handles
        .as_ref()
        .map(|r| r.len())
        .unwrap_or_else(|| result.handles.len());
    if result.truncated {
        println!(
            "... ({} showing {} of {} results)",
            "truncated".yellow(),
            shown,
            result.total_matches
        );
    }
    if let Some(note) = &result.expand_note {
        println!("{}: {}", "Note".yellow(), note);
    }
    println!(
        "({} results, {} tokens{})",
        shown,
        result.total_tokens,
        if result.auto_expanded {
            ", auto-expanded"
        } else {
            ""
        }
    );
    Ok(())
}

fn cmd_expand(
    root: Option<std::path::PathBuf>,
    handle_ids: &[String],
    json: bool,
    service_url: Option<String>,
) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;

    // Try local expand first
    let index = RepoIndex::open(&repo_root)?;
    match index.expand(handle_ids) {
        Ok(contents) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&contents).unwrap());
            } else {
                for (handle_id, content) in contents {
                    println!("{}", format!("// {}", handle_id).dimmed());
                    println!("{}", content);
                    println!();
                }
            }
            Ok(())
        }
        Err(canopy_core::CanopyError::HandleNotFound(_)) if service_url.is_some() => {
            // Local expand failed with HandleNotFound, try service
            #[cfg(feature = "service")]
            {
                let url = service_url.as_ref().unwrap();
                let client = client::ServiceClient::new(url);
                let repo_id = repo_root.to_string_lossy().to_string();
                let contents = client.expand(&repo_id, handle_ids, None)?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&contents).unwrap());
                } else {
                    for (handle_id, content) in contents {
                        println!("{}", format!("// {} (service)", handle_id).dimmed());
                        println!("{}", content);
                        println!();
                    }
                }
                Ok(())
            }
            #[cfg(not(feature = "service"))]
            {
                Err(canopy_core::CanopyError::ServiceError {
                    code: "feature_disabled".to_string(),
                    message: "Service feature is not enabled".to_string(),
                    hint: "Rebuild with --features service".to_string(),
                })
            }
        }
        Err(e) => Err(e),
    }
}

fn cmd_status(root: Option<std::path::PathBuf>, json: bool) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let index = RepoIndex::open(&repo_root)?;
    let status = index.status()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status).unwrap());
    } else {
        println!(
            "{}: .canopy/index.db ({:.1} MB)",
            "Index".blue(),
            status.index_size_bytes as f64 / 1_000_000.0
        );
        println!("{}: {} indexed", "Files".blue(), status.files_indexed);
        println!("{}: {}", "Tokens".blue(), status.total_tokens);
        println!("{}: v{}", "Schema".blue(), status.schema_version);
        if let Some(last) = status.last_indexed {
            println!("{}: {}", "Last indexed".blue(), last);
        }
    }
    Ok(())
}

fn cmd_invalidate(
    root: Option<std::path::PathBuf>,
    glob: Option<String>,
    json: bool,
) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let mut index = RepoIndex::open(&repo_root)?;
    let count = index.invalidate(glob.as_deref())?;

    if json {
        println!("{}", serde_json::json!({ "files_removed": count }));
    } else {
        println!(
            "{}: {} files removed from index",
            "Invalidated".yellow(),
            count
        );
    }
    Ok(())
}

fn cmd_repos(service_url: Option<String>, json: bool) -> canopy_core::Result<()> {
    #[cfg(feature = "service")]
    {
        use colored::Colorize;

        let url = require_service_url(&service_url)?;
        let client = client::ServiceClient::new(&url);
        let repos = client.list_repos()?;

        if json {
            println!("{}", serde_json::to_string_pretty(&repos).unwrap());
        } else {
            if repos.is_empty() {
                println!("No repos registered with the service.");
            } else {
                for repo in &repos {
                    let status_str = format!("{:?}", repo.status).to_lowercase();
                    let gen = format!("gen {}", repo.generation);
                    let sha = repo
                        .commit_sha
                        .as_ref()
                        .map(|s| format!(" @ {}", &s[..8.min(s.len())]))
                        .unwrap_or_default();
                    println!(
                        "{}: {} [{}] ({}{}){}",
                        repo.repo_id.cyan(),
                        repo.name,
                        status_str,
                        gen,
                        sha,
                        if repo.repo_root != repo.name {
                            format!(" — {}", repo.repo_root.dimmed())
                        } else {
                            String::new()
                        }
                    );
                }
            }
            println!("({} repos)", repos.len());
        }
        Ok(())
    }
    #[cfg(not(feature = "service"))]
    {
        let _ = (service_url, json);
        Err(canopy_core::CanopyError::ServiceError {
            code: "feature_disabled".to_string(),
            message: "Service feature is not enabled".to_string(),
            hint: "Rebuild with --features service".to_string(),
        })
    }
}

fn cmd_reindex(
    service_url: Option<String>,
    repo: String,
    glob: Option<String>,
    json: bool,
) -> canopy_core::Result<()> {
    #[cfg(feature = "service")]
    {
        use colored::Colorize;

        let url = require_service_url(&service_url)?;
        let client = client::ServiceClient::new(&url);
        let response = client.reindex(&repo, glob)?;

        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "generation": response.generation,
                    "status": response.status,
                    "commit_sha": response.commit_sha,
                }))
                .unwrap()
            );
        } else {
            println!("{}: generation {}", "Reindex".green(), response.generation);
            println!("{}: {}", "Status".blue(), response.status);
            if let Some(sha) = &response.commit_sha {
                println!("{}: {}", "Commit".blue(), sha);
            }
        }
        Ok(())
    }
    #[cfg(not(feature = "service"))]
    {
        let _ = (service_url, repo, glob, json);
        Err(canopy_core::CanopyError::ServiceError {
            code: "feature_disabled".to_string(),
            message: "Service feature is not enabled".to_string(),
            hint: "Rebuild with --features service".to_string(),
        })
    }
}

fn cmd_service_status(service_url: Option<String>, json: bool) -> canopy_core::Result<()> {
    #[cfg(feature = "service")]
    {
        use colored::Colorize;

        let url = require_service_url(&service_url)?;
        let client = client::ServiceClient::new(&url);
        let status = client.status()?;

        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "service": status.service,
                    "repos": status.repos,
                }))
                .unwrap()
            );
        } else {
            println!("{}: {}", "Service".green(), status.service);
            println!("{}: {} repos", "Repos".blue(), status.repos.len());
            for repo in &status.repos {
                let status_str = format!("{:?}", repo.status).to_lowercase();
                println!(
                    "  {} — {} [{}] gen {}",
                    repo.name.cyan(),
                    repo.repo_root.dimmed(),
                    status_str,
                    repo.generation
                );
            }
        }
        Ok(())
    }
    #[cfg(not(feature = "service"))]
    {
        let _ = (service_url, json);
        Err(canopy_core::CanopyError::ServiceError {
            code: "feature_disabled".to_string(),
            message: "Service feature is not enabled".to_string(),
            hint: "Rebuild with --features service".to_string(),
        })
    }
}

/// Require a service URL, returning an error if not configured
#[cfg(feature = "service")]
fn require_service_url(service_url: &Option<String>) -> canopy_core::Result<String> {
    service_url
        .as_ref()
        .cloned()
        .ok_or_else(|| canopy_core::CanopyError::ServiceError {
            code: "no_service_url".to_string(),
            message: "No service URL configured".to_string(),
            hint: "Pass --service-url or set CANOPY_SERVICE_URL".to_string(),
        })
}

fn detect_repo_root(
    override_path: Option<std::path::PathBuf>,
) -> canopy_core::Result<std::path::PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }

    // Walk up from current directory looking for .canopy or .git
    let mut current = std::env::current_dir()?;
    loop {
        if current.join(".canopy").exists() || current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            // No parent, use current directory
            return Ok(std::env::current_dir()?);
        }
    }
}
