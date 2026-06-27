mod cache;
mod config;
mod dash;
mod error;
mod handlers;
mod hls;
mod transmux;
mod upstream;
mod url_utils;

use crate::cache::Cache;
use crate::config::Config;
use crate::handlers::{handle_dash_manifest, handle_hls2dash, handle_rn, handle_ts_init, handle_ts_init_from_playlist, handle_ts_segment_from_playlist, health, AppState};
use axum::{routing::get, Router};
use reqwest::ClientBuilder;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present (best-effort).
    let _ = dotenvy_or_manual_env();

    let config = Config::from_env();

    // Initialize tracing.
    let filter = EnvFilter::try_new(&config.log_level)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    info!(
        version = %config.version,
        port = config.port,
        proxy_base = %config.proxy_base,
        cache_capacity = config.cache_max_capacity,
        upstream_timeout_secs = config.upstream_timeout_secs,
        "starting hls2dash"
    );

    // Build HTTP client.
    let http_client = ClientBuilder::new()
        .timeout(Duration::from_secs(config.upstream_timeout_secs))
        .gzip(true)
        .deflate(true)
        .user_agent("hls2dash/0.1")
        .build()?;

    // Build playlist cache (global TTL = 5 s; per-playlist TTL would require moka Expiry).
    let playlist_cache = Arc::new(Cache::new(config.cache_max_capacity, 5));

    let state = AppState {
        http_client,
        playlist_cache,
        config: config.clone(),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health))
        .route("/rn", get(handle_rn))
        .route("/hls2dash-ts-pl/*path", get(handle_ts_segment_from_playlist))
        .route("/hls2dash-init-pl/*path", get(handle_ts_init_from_playlist))
        .route("/hls2dash-init/*path", get(handle_ts_init))
        .route("/hls2dash/*path", get(handle_hls2dash))
        .route("/dash/*path", get(handle_dash_manifest))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Attempt to read a `.env` file manually without a full dotenv crate.
/// Silently ignores missing or malformed files.
fn dotenvy_or_manual_env() -> std::io::Result<()> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(".env")?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            // Only set if not already in environment.
            if std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
    Ok(())
}
