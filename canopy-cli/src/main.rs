//! Canopy CLI - Command-line interface for token-efficient codebase queries

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
        ),
        Commands::Expand { handle_ids } => cmd_expand(cli.root, &handle_ids, cli.json),
        Commands::Status => cmd_status(cli.root, cli.json),
        Commands::Invalidate { glob } => cmd_invalidate(cli.root, glob, cli.json),
    };

    if let Err(e) = result {
        if cli.json {
            let error_json = serde_json::json!({ "error": e.to_string() });
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
) -> canopy_core::Result<()> {
    use canopy_core::{QueryKind, QueryParams, RepoIndex};
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let index = RepoIndex::open(&repo_root)?;

    // Determine which query path to use
    let result = if let Some(query_str) = query_str {
        // Old DSL path: positional s-expression query
        use canopy_core::QueryOptions;
        let options = QueryOptions {
            limit,
            expand_budget,
        };
        index.query_with_options(&query_str, options)?
    } else if pattern.is_some() || symbol.is_some() || parent.is_some() {
        // New params-based API
        let mut params = QueryParams::new();

        if let Some(p) = pattern {
            params.pattern = Some(p);
        }
        if let Some(s) = symbol {
            params.symbol = Some(s);
        }
        if let Some(p) = parent {
            params.parent = Some(p);
        }
        if let Some(k) = kind {
            params.kind = match k.as_str() {
                "definition" => QueryKind::Definition,
                "reference" => QueryKind::Reference,
                _ => QueryKind::Any,
            };
        }
        if let Some(g) = glob {
            params.glob = Some(g);
        }
        if let Some(l) = limit {
            params.limit = Some(l);
        }
        if let Some(eb) = expand_budget {
            params.expand_budget = Some(eb);
        }

        index.query_params(params)?
    } else {
        return Err(canopy_core::CanopyError::QueryParse {
            position: 0,
            message: "Must provide either a query s-expression or --pattern/--symbol/--parent flag"
                .to_string(),
        });
    };

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
) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    let index = RepoIndex::open(&repo_root)?;
    let contents = index.expand(handle_ids)?;

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
