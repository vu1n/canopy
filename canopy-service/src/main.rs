mod error;
mod routes;
mod state;

use axum::routing::{get, post};
use axum::Router;
use state::{AppState, SharedState};
use std::sync::Arc;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .position(|a| a == "--port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let bind: String = std::env::args()
        .position(|a| a == "--bind")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let state: SharedState = Arc::new(AppState::new());

    let app = Router::new()
        .route("/query", post(routes::query))
        .route("/evidence_pack", post(routes::evidence_pack))
        .route("/expand", post(routes::expand))
        .route("/repos/add", post(routes::add_repo))
        .route("/repos", get(routes::list_repos))
        .route("/status", get(routes::status))
        .route("/reindex", post(routes::reindex))
        .route("/metrics", get(routes::metrics))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{}:{}", bind, port);
    eprintln!("canopy-service listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
