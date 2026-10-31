//! TraceLake HTTP server entry point.

use std::sync::Arc;
use std::time::Duration;
use tracelake_ingest::TraceLakeDb;
use tracelake_server::limits::Limits;
use tracelake_server::store::StoreSettings;
use tracelake_server::{AppState, build_router};

/// Address the server binds when `TRACELAKE_ADDR` is unset. Loopback is the
/// safe default because the alpha has no authentication.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() {
    let addr: std::net::SocketAddr = std::env::var("TRACELAKE_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_string())
        .parse()
        .expect("TRACELAKE_ADDR must be a valid socket address");

    let settings = StoreSettings::from_env().expect("store settings should be valid");
    let store = settings.open().expect("object store should open");

    let db = TraceLakeDb::open(Arc::clone(&store), &settings.hot_path())
        .await
        .expect("hot store should open");

    let limits = Limits {
        query_timeout: Duration::from_millis(env_u64("TRACELAKE_QUERY_TIMEOUT_MS", 30_000)),
        max_concurrent_per_tenant: env_u64("TRACELAKE_TENANT_CONCURRENCY", 16) as usize,
    };
    let state = AppState::with_limits(db, store, &settings.segment_path(), limits);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("server address should bind");

    println!("tracelake listening on http://{addr}");
    println!("store: {}", settings.describe());
    println!(
        "limits: query timeout {} ms, {} concurrent reads per tenant",
        limits.query_timeout.as_millis(),
        limits.max_concurrent_per_tenant
    );
    println!("endpoints:");
    println!("  POST /v1/spans                                  ingest a batch of spans");
    println!("  POST /v1/compact                                compact the hot store");
    println!("  GET  /v1/traces/{{tenant_id}}/{{namespace}}/{{trace_id}}   fetch one full trace");
    println!("  POST /v1/traces/search                          search by status/model/time");

    axum::serve(listener, app)
        .await
        .expect("server should serve");
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
