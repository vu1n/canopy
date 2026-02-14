//! Canopy CLI - Command-line interface for token-efficient codebase queries

use canopy_client::{ClientRuntime, IndexResult, QueryInput, StandalonePolicy};
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
        Commands::Index { glob } => cmd_index(cli.root, glob, cli.json, cli.service_url.as_deref()),
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
            cli.service_url.as_deref(),
        ),
        Commands::Expand { handle_ids } => {
            cmd_expand(cli.root, &handle_ids, cli.json, cli.service_url.as_deref())
        }
        Commands::Status => cmd_status(cli.root, cli.json),
        Commands::Invalidate { glob } => cmd_invalidate(cli.root, glob, cli.json),
        Commands::Repos => cmd_repos(cli.service_url.as_deref(), cli.json),
        Commands::Reindex { repo, glob } => {
            cmd_reindex(cli.service_url.as_deref(), repo, glob, cli.json)
        }
        Commands::ServiceStatus => cmd_service_status(cli.service_url.as_deref(), cli.json),
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

fn make_runtime(service_url: Option<&str>) -> ClientRuntime {
    ClientRuntime::new(service_url, StandalonePolicy::QueryOnly)
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
    service_url: Option<&str>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url);
    let result = runtime.index(&repo_root, glob.as_deref())?;

    match result {
        IndexResult::Local(stats) => {
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
        }
        IndexResult::Service(resp) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "generation": resp.generation,
                        "status": resp.status,
                        "commit_sha": resp.commit_sha,
                    }))
                    .unwrap()
                );
            } else {
                println!("{}: generation {}", "Reindex".green(), resp.generation);
                println!("{}: {}", "Status".blue(), resp.status);
                if let Some(sha) = &resp.commit_sha {
                    println!("{}: {}", "Commit".blue(), sha);
                }
            }
        }
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
    service_url: Option<&str>,
) -> canopy_core::Result<()> {
    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url);

    // Build QueryInput
    let input = if let Some(ref qs) = query_str {
        if pattern.is_none() && symbol.is_none() && parent.is_none() {
            // Pure DSL query
            QueryInput::Dsl(
                qs.clone(),
                canopy_core::QueryOptions {
                    limit,
                    expand_budget,
                },
            )
        } else {
            // Mixed: build params from flags
            QueryInput::Params(build_query_params(
                pattern.as_deref(),
                symbol.as_deref(),
                parent.as_deref(),
                kind.as_deref(),
                glob.as_deref(),
                expand_budget,
                limit,
            )?)
        }
    } else {
        QueryInput::Params(build_query_params(
            pattern.as_deref(),
            symbol.as_deref(),
            parent.as_deref(),
            kind.as_deref(),
            glob.as_deref(),
            expand_budget,
            limit,
        )?)
    };

    let result = runtime.query(&repo_root, input)?;
    print_query_result(&result, json)
}

/// Build QueryParams from CLI arguments
fn build_query_params(
    pattern: Option<&str>,
    symbol: Option<&str>,
    parent: Option<&str>,
    kind: Option<&str>,
    glob: Option<&str>,
    expand_budget: Option<usize>,
    limit: Option<usize>,
) -> canopy_core::Result<canopy_core::QueryParams> {
    use canopy_core::{QueryKind, QueryParams};

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
    service_url: Option<&str>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url);
    let outcome = runtime.expand(&repo_root, handle_ids)?;

    if json {
        let json_val = serde_json::json!({
            "contents": outcome.contents.iter().map(|(id, content)| {
                serde_json::json!({ "handle_id": id, "content": content })
            }).collect::<Vec<_>>(),
            "failed_ids": outcome.failed_ids,
        });
        println!("{}", serde_json::to_string_pretty(&json_val).unwrap());
    } else {
        for (handle_id, content) in &outcome.contents {
            println!("{}", format!("// {}", handle_id).dimmed());
            println!("{}", content);
            println!();
        }
        if !outcome.failed_ids.is_empty() {
            eprintln!(
                "{}: failed to expand: {}",
                "Warning".yellow(),
                outcome.failed_ids.join(", ")
            );
        }
    }
    Ok(())
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

fn cmd_repos(service_url: Option<&str>, json: bool) -> canopy_core::Result<()> {
    use colored::Colorize;

    let runtime = make_runtime(service_url);
    let repos = runtime.list_repos()?;

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

fn cmd_reindex(
    service_url: Option<&str>,
    repo: String,
    glob: Option<String>,
    json: bool,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let runtime = make_runtime(service_url);
    let response = runtime.reindex_by_id(&repo, glob.as_deref())?;

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

fn cmd_service_status(service_url: Option<&str>, json: bool) -> canopy_core::Result<()> {
    use colored::Colorize;

    let runtime = make_runtime(service_url);
    let status = runtime.service_status()?;

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
