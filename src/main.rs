use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Method, header::CONNECTION},
    routing::{get, post},
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;

mod convert;
mod database;
mod error;
mod logger;

// Loopback only: the browser reaches the backend exclusively through the
// Next.js proxy, which enforces authentication. Overridable via QIMS_BIND
// (used by test harnesses to run on isolated ports).
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8787";

/// Liveness probe.
async fn health() -> &'static str {
    "ok"
}

/// The stateless half of the service: conversion needs no database, so these
/// routes are generic over the state and work in either mode below.
fn stateless_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/health", get(health))
        .route("/convert", post(convert::convert))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logger::init();

    // Newer deployments keep documents in their own application database and
    // need this binary only for its LibreOffice/poppler `/convert` pipeline,
    // which owns no data. QIMS_CONVERT_ONLY=1 skips the SurrealDB connection
    // entirely so the converter runs standalone; without it, the full legacy
    // stack (documents/users/notifications) is served exactly as before.
    let convert_only = matches!(
        std::env::var("QIMS_CONVERT_ONLY").as_deref(),
        Ok("1") | Ok("true")
    );

    // Permissive CORS so the Next.js dev server can call the API.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers(Any);

    let routes = if convert_only {
        log::info!("convert-only mode: /convert and /health, no database");
        stateless_routes()
    } else {
        let db = database::connect().await?;
        stateless_routes()
            .merge(database::documents::routes())
            .merge(database::users::routes())
            .merge(database::notifications::routes())
            .with_state(db)
    };

    let app = routes
        // Documents can carry base64 images and uploads are whole files, so
        // axum's 2 MB default body limit is far too small.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        // One connection per request: the Next.js proxy pools keep-alive
        // sockets, and a reused socket the server has meanwhile closed
        // surfaces as intermittent ECONNRESET / 500s in the app.
        .layer(SetResponseHeaderLayer::overriding(
            CONNECTION,
            HeaderValue::from_static("close"),
        ))
        .layer(cors);

    let bind_addr =
        std::env::var("QIMS_BIND").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    log::info!("QIMS backend listening on http://{bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
