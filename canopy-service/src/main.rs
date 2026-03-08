mod error;
mod evidence;
mod feedback_recording;
mod metrics;
mod routes;
mod state;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use clap::Parser;
use state::{AppState, SharedState};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "canopy-service")]
#[command(about = "HTTP service for canopy multi-repo indexing")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "3000")]
    port: u16,

    /// Bind address
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// API key for admin routes (also reads CANOPY_API_KEY env var)
    #[arg(long, env = "CANOPY_API_KEY")]
    api_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Require API key when binding beyond localhost
    let is_localhost = args.bind == "127.0.0.1" || args.bind == "::1" || args.bind == "localhost";
    if !is_localhost && args.api_key.is_none() {
        error!(
            bind = %args.bind,
            "binding to non-localhost requires an API key — set CANOPY_API_KEY or pass --api-key"
        );
        std::process::exit(1);
    }

    let state: SharedState = Arc::new(AppState::new());

    // Query routes: read-only data surface
    let query_routes = Router::new()
        .route("/query", post(routes::query))
        .route("/evidence_pack", post(routes::evidence_pack))
        .route("/expand", post(routes::expand));

    // Admin routes: repo management and operational control
    let admin_routes = Router::new()
        .route("/repos/add", post(routes::add_repo))
        .route("/repos", get(routes::list_repos))
        .route("/reindex", post(routes::reindex));

    // Health/metrics: always public (no sensitive data)
    let ops_routes = Router::new()
        .route("/status", get(routes::status))
        .route("/metrics", get(metrics::metrics));

    // Apply API key guard to query + admin routes when configured.
    // ops_routes remain public (health/metrics contain no sensitive data).
    let guarded_routes = Router::new().merge(query_routes).merge(admin_routes);
    let guarded_routes = if let Some(ref key) = args.api_key {
        let key = key.clone();
        guarded_routes.layer(axum::middleware::from_fn(move |req, next| {
            let expected = key.clone();
            api_key_guard(req, next, expected)
        }))
    } else {
        guarded_routes
    };

    let app = Router::new()
        .merge(guarded_routes)
        .merge(ops_routes)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024)) // 2 MB
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{}:{}", args.bind, args.port);
    if args.api_key.is_some() {
        info!(addr = %addr, "listening (admin routes require API key)");
    } else {
        warn!(
            addr = %addr,
            "listening — no API key configured, admin routes are unprotected"
        );
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn api_key_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
    expected_key: String,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let provided = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());

    match provided {
        Some(key) if key == expected_key => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            axum::Json(canopy_core::ErrorEnvelope::new(
                "unauthorized",
                "Missing or invalid API key",
                "Set the X-Api-Key header to the configured CANOPY_API_KEY",
            )),
        )
            .into_response(),
    }
}
