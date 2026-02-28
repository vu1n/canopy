//! Canopy CLI - Command-line interface for token-efficient codebase queries

use canopy_client::{ClientRuntime, IndexResult, QueryInput};
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

    /// API key for service admin routes (also reads CANOPY_API_KEY env var)
    #[arg(long, global = true, env = "CANOPY_API_KEY")]
    api_key: Option<String>,

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
        #[command(flatten)]
        args: QueryArgs,
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

    /// Show local feedback metrics
    FeedbackStats {
        /// Lookback window in days (default: 7)
        #[arg(long)]
        lookback_days: Option<f64>,
    },
}

#[derive(clap::Args)]
struct QueryArgs {
    /// Query in s-expression format (e.g., "(grep 'error')")
    query: Option<String>,

    /// Text pattern to search (alternative to positional query arg)
    #[arg(short, long)]
    pattern: Option<String>,

    /// Multiple text patterns (use with --match to control AND/OR)
    #[arg(long, num_args = 1..)]
    patterns: Option<Vec<String>>,

    /// Search for code symbol (function, class, struct)
    #[arg(short, long)]
    symbol: Option<String>,

    /// Search by markdown section heading
    #[arg(long)]
    section: Option<String>,

    /// Filter by parent symbol (e.g., class name for methods)
    #[arg(long)]
    parent: Option<String>,

    /// Query kind: definition, reference, or any (default)
    #[arg(short, long, value_parser = ["definition", "reference", "any"])]
    kind: Option<String>,

    /// Filter by file glob pattern
    #[arg(short, long)]
    glob: Option<String>,

    /// Multi-pattern match mode: any (default) or all
    #[arg(long, value_name = "MODE", value_parser = ["any", "all"])]
    r#match: Option<String>,

    /// Auto-expand if total tokens fit within budget (default: 5000 when specified without value)
    #[arg(long, num_args = 0..=1, default_missing_value = "5000")]
    expand_budget: Option<usize>,

    /// Override default result limit
    #[arg(long)]
    limit: Option<usize>,
}

fn main() {
    let cli = Cli::parse();

    let api_key = cli.api_key;
    let result = match cli.command {
        Commands::Init => cmd_init(cli.root),
        Commands::Index { glob } => cmd_index(
            cli.root,
            glob,
            cli.json,
            cli.service_url.as_deref(),
            api_key,
        ),
        Commands::Query { args } => cmd_query(
            cli.root,
            args,
            cli.json,
            cli.service_url.as_deref(),
            api_key,
        ),
        Commands::Expand { handle_ids } => cmd_expand(
            cli.root,
            &handle_ids,
            cli.json,
            cli.service_url.as_deref(),
            api_key,
        ),
        Commands::Status => cmd_status(cli.root, cli.json),
        Commands::Invalidate { glob } => cmd_invalidate(cli.root, glob, cli.json),
        Commands::Repos => cmd_repos(cli.service_url.as_deref(), cli.json, api_key),
        Commands::Reindex { repo, glob } => {
            cmd_reindex(cli.service_url.as_deref(), repo, glob, cli.json, api_key)
        }
        Commands::ServiceStatus => {
            cmd_service_status(cli.service_url.as_deref(), cli.json, api_key)
        }
        Commands::FeedbackStats { lookback_days } => {
            cmd_feedback_stats(cli.root, cli.json, lookback_days)
        }
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
            if let Ok(s) = serde_json::to_string_pretty(&error_json) {
                eprintln!("{}", s);
            } else {
                eprintln!("Error: {}", e);
            }
        } else {
            eprintln!("Error: {}", e);
        }
        std::process::exit(1);
    }
}

fn make_runtime(service_url: Option<&str>, api_key: Option<String>) -> ClientRuntime {
    ClientRuntime::new(service_url, api_key)
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
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url, api_key);
    let result = runtime.index(&repo_root, glob.as_deref())?;

    match result {
        IndexResult::Local(stats) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
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
                    }))?
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

fn cmd_query(
    root: Option<std::path::PathBuf>,
    args: QueryArgs,
    json: bool,
    service_url: Option<&str>,
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url, api_key);

    let input = if let Some(ref qs) = args.query {
        if args.pattern.is_none() && args.symbol.is_none() && args.parent.is_none() {
            QueryInput::Dsl(
                qs.clone(),
                canopy_core::QueryOptions {
                    limit: args.limit,
                    expand_budget: args.expand_budget,
                    node_type_priors: None,
                },
            )
        } else {
            QueryInput::Params(build_query_params(&args)?)
        }
    } else {
        QueryInput::Params(build_query_params(&args)?)
    };

    let result = runtime.query(&repo_root, input)?;
    print_query_result(&result, json)
}

fn build_query_params(args: &QueryArgs) -> canopy_core::Result<canopy_core::QueryParams> {
    use canopy_core::{MatchMode, QueryParams};

    let mut params = QueryParams::new();
    params.pattern = args.pattern.clone();
    params.patterns = args.patterns.clone();
    params.symbol = args.symbol.clone();
    params.section = args.section.clone();
    params.parent = args.parent.clone();
    params.glob = args.glob.clone();
    params.limit = args.limit;
    params.expand_budget = args.expand_budget;

    if let Some(ref k) = args.kind {
        params.kind = QueryParams::parse_kind(k);
    }

    if let Some(ref m) = args.r#match {
        params.match_mode = match m.as_str() {
            "all" => MatchMode::All,
            _ => MatchMode::Any,
        };
    }

    if !params.has_search_target() {
        return Err(canopy_core::CanopyError::QueryParse {
            position: 0,
            message: "Must provide either a query s-expression or --pattern/--symbol/--parent flag"
                .to_string(),
        });
    }

    Ok(params)
}

/// Print query results in text or JSON format
fn print_query_result(result: &canopy_core::QueryResult, json: bool) -> canopy_core::Result<()> {
    use colored::Colorize;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
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
    if result.expanded_count > 0 {
        println!(
            "({} expanded handles, {} expanded tokens)",
            result.expanded_count, result.expanded_tokens
        );
    }
    Ok(())
}

fn cmd_expand(
    root: Option<std::path::PathBuf>,
    handle_ids: &[String],
    json: bool,
    service_url: Option<&str>,
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url, api_key);
    let outcome = runtime.expand(&repo_root, handle_ids)?;

    if json {
        let json_val = serde_json::json!({
            "contents": outcome.contents.iter().map(|(id, content)| {
                serde_json::json!({ "handle_id": id, "content": content })
            }).collect::<Vec<_>>(),
            "failed_ids": outcome.failed_ids,
        });
        println!("{}", serde_json::to_string_pretty(&json_val)?);
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
        println!("{}", serde_json::to_string_pretty(&status)?);
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

fn cmd_feedback_stats(
    root: Option<std::path::PathBuf>,
    json: bool,
    lookback_days: Option<f64>,
) -> canopy_core::Result<()> {
    use canopy_core::feedback::FeedbackStore;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let store = FeedbackStore::open(&repo_root)?;
    let metrics = store.compute_metrics(lookback_days.unwrap_or(7.0))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "glob_hit_rate_at_k": metrics.glob_hit_rate_at_k,
                "handle_expand_accept_rate": metrics.handle_expand_accept_rate,
                "avg_tokens_per_expand": metrics.avg_tokens_per_expand,
                "sample_count": metrics.sample_count,
            }))?
        );
    } else {
        println!("{}:", "Feedback".blue());
        println!(
            "  {} {:.3}",
            "glob_hit_rate_at_k".green(),
            metrics.glob_hit_rate_at_k
        );
        println!(
            "  {} {:.3}",
            "handle_expand_accept_rate".green(),
            metrics.handle_expand_accept_rate
        );
        println!(
            "  {} {:.1}",
            "avg_tokens_per_expand".green(),
            metrics.avg_tokens_per_expand
        );
        println!("  {} {}", "sample_count".green(), metrics.sample_count);
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

fn cmd_repos(
    service_url: Option<&str>,
    json: bool,
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let runtime = make_runtime(service_url, api_key);
    let repos = runtime.list_repos()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&repos)?);
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
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let runtime = make_runtime(service_url, api_key);
    let response = runtime.reindex_by_id(&repo, glob.as_deref())?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "generation": response.generation,
                "status": response.status,
                "commit_sha": response.commit_sha,
            }))?
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

fn cmd_service_status(
    service_url: Option<&str>,
    json: bool,
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    use colored::Colorize;

    let runtime = make_runtime(service_url, api_key);
    let status = runtime.service_status()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "service": status.service,
                "repos": status.repos,
            }))?
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
