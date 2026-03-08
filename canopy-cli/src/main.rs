//! Canopy CLI - Command-line interface for token-efficient codebase queries

mod commands;
mod output;

use clap::{Parser, Subcommand};

use commands::{
    cmd_expand, cmd_feedback_stats, cmd_index, cmd_init, cmd_invalidate, cmd_query, cmd_reindex,
    cmd_repos, cmd_service_status, cmd_status,
};
use output::print_error_and_exit;

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
pub(crate) struct QueryArgs {
    /// Query in s-expression format (e.g., "(grep 'error')")
    pub(crate) query: Option<String>,

    /// Text pattern to search (alternative to positional query arg)
    #[arg(short, long)]
    pub(crate) pattern: Option<String>,

    /// Multiple text patterns (use with --match to control AND/OR)
    #[arg(long, num_args = 1..)]
    pub(crate) patterns: Option<Vec<String>>,

    /// Search for code symbol (function, class, struct)
    #[arg(short, long)]
    pub(crate) symbol: Option<String>,

    /// Search by markdown section heading
    #[arg(long)]
    pub(crate) section: Option<String>,

    /// Filter by parent symbol (e.g., class name for methods)
    #[arg(long)]
    pub(crate) parent: Option<String>,

    /// Query kind: definition, reference, or any (default)
    #[arg(short, long, value_parser = ["definition", "reference", "any"])]
    pub(crate) kind: Option<String>,

    /// Filter by file glob pattern
    #[arg(short, long)]
    pub(crate) glob: Option<String>,

    /// Multi-pattern match mode: any (default) or all
    #[arg(long, value_name = "MODE", value_parser = ["any", "all"])]
    pub(crate) r#match: Option<String>,

    /// Auto-expand if total tokens fit within budget (default: 5000 when specified without value)
    #[arg(long, num_args = 0..=1, default_missing_value = "5000")]
    pub(crate) expand_budget: Option<usize>,

    /// Override default result limit
    #[arg(long)]
    pub(crate) limit: Option<usize>,
}

fn main() {
    let cli = Cli::parse();

    let json = cli.json;
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
        print_error_and_exit(e, json);
    }
}
