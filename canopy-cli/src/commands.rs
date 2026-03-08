//! Command implementations for the Canopy CLI.

use canopy_client::{ClientRuntime, IndexResult};
use canopy_core::QueryParams;

use crate::output::print_query_result;
use crate::QueryArgs;

pub(crate) fn make_runtime(service_url: Option<&str>, api_key: Option<String>) -> ClientRuntime {
    ClientRuntime::new(service_url, api_key)
}

pub(crate) fn detect_repo_root(
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

pub(crate) fn cmd_init(root: Option<std::path::PathBuf>) -> canopy_core::Result<()> {
    use canopy_core::RepoIndex;
    use colored::Colorize;

    let repo_root = detect_repo_root(root)?;
    RepoIndex::init(&repo_root)?;

    println!("{} .canopy/config.toml", "Created".green());
    println!("{} .canopy/ to .gitignore", "Added".green());
    Ok(())
}

pub(crate) fn cmd_index(
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

pub(crate) fn cmd_query(
    root: Option<std::path::PathBuf>,
    args: QueryArgs,
    json: bool,
    service_url: Option<&str>,
    api_key: Option<String>,
) -> canopy_core::Result<()> {
    let repo_root = detect_repo_root(root)?;
    let mut runtime = make_runtime(service_url, api_key);

    let params = if let Some(ref qs) = args.query {
        if args.pattern.is_none() && args.symbol.is_none() && args.parent.is_none() {
            // Warn if structured flags are set but will be ignored in DSL mode
            let ignored: Vec<&str> = [
                args.section.as_ref().map(|_| "--section"),
                args.glob.as_ref().map(|_| "--glob"),
                args.kind.as_ref().map(|_| "--kind"),
                args.r#match.as_ref().map(|_| "--match"),
                args.patterns.as_ref().map(|_| "--patterns"),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !ignored.is_empty() {
                eprintln!(
                    "Warning: {} ignored in DSL query mode. Use structured flags (--pattern/--symbol) or encode in the query expression.",
                    ignored.join(", ")
                );
            }
            let mut params = QueryParams::new();
            params.dsl = Some(qs.clone());
            params.limit = args.limit;
            params.expand_budget = args.expand_budget;
            params
        } else {
            build_query_params(&args)?
        }
    } else {
        build_query_params(&args)?
    };

    let result = runtime.query(&repo_root, params)?;
    print_query_result(&result, json)
}

pub(crate) fn build_query_params(
    args: &QueryArgs,
) -> canopy_core::Result<canopy_core::QueryParams> {
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
        params.match_mode = MatchMode::parse(m);
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

pub(crate) fn cmd_expand(
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

pub(crate) fn cmd_status(root: Option<std::path::PathBuf>, json: bool) -> canopy_core::Result<()> {
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

pub(crate) fn cmd_feedback_stats(
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

pub(crate) fn cmd_invalidate(
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

pub(crate) fn cmd_repos(
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

pub(crate) fn cmd_reindex(
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

pub(crate) fn cmd_service_status(
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
