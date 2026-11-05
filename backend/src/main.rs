//! Onchain Footprint — realtime Hyperliquid perp order-flow terminal.
//! Rust full-stack: axum backend + Leptos (wasm) frontend.

mod api;
mod clientws;
mod engine;
mod hl;

use std::path::PathBuf;

use axum::routing::{get, post};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

use engine::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hyperliquid=warn,hyper=warn,tungstenite=warn".into()),
        )
        .init();

    let rest = hl::rest::HlRest::new("https://api.hyperliquid.xyz/info");
    let (sub_tx, sub_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(rest, sub_tx);

    // spawn background engine tasks
    tokio::spawn(hl::ws::run_upstream(state.clone(), sub_rx));
    tokio::spawn(hl::ws::run_default_set(state.clone()));
    tokio::spawn(hl::ws::run_candle_gapfill(state.clone()));
    tokio::spawn(engine::bookmap::run_book_poll(state.clone()));
    tokio::spawn(engine::tasks::run_assets_poll(state.clone()));
    tokio::spawn(engine::tasks::run_enrichment(state.clone()));
    tokio::spawn(engine::tasks::run_traders_push(state.clone()));
    tokio::spawn(engine::tasks::run_radar_push(state.clone()));
    tokio::spawn(engine::tasks::run_opps_push(state.clone()));
    tokio::spawn(engine::tasks::run_persist(state.clone()));
    tokio::spawn(engine::tasks::run_shutdown_flush(state.clone()));

    // static frontend (built by trunk into dist/)
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("frontend")
        .join("dist");

    let app = Router::new()
        .route("/api/assets", get(api::assets))
        .route("/api/footprint", get(api::footprint))
        .route("/api/level", get(api::level))
        .route("/api/traders", get(api::traders))
        .route("/api/trader/{addr}", get(api::trader_detail))
        .route("/api/tuning", get(api::tuning_get).post(api::tuning_set))
        .route("/api/tuning/reset", post(api::tuning_reset))
        .route("/api/radar", get(api::radar))
        .route("/api/cascades", get(api::cascades))
        .route("/api/opps", get(api::opps))
        .route("/api/bookmap", get(api::bookmap))
        .route("/api/health", get(api::health))
        .route("/ws", get(clientws::ws_handler))
        .fallback_service(ServeDir::new(&dist))
        .layer(CompressionLayer::new())
        .with_state(state.clone());

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Onchain Footprint listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))?;
    Ok(())
}
