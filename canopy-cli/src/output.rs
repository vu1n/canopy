//! Output formatting for CLI results and errors.

use colored::Colorize;

/// Print query results in text or JSON format.
pub(crate) fn print_query_result(
    result: &canopy_core::QueryResult,
    json: bool,
) -> canopy_core::Result<()> {
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

/// Print a CanopyError in text or structured JSON format, then exit.
pub(crate) fn print_error_and_exit(e: canopy_core::CanopyError, json: bool) -> ! {
    if json {
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
