//! TraceLake HTTP server entry point.

use slatedb::object_store::memory::InMemory;
use std::sync::Arc;
use tracelake_ingest::TraceLakeDb;
use tracelake_server::{AppState, build_router};

/// Address the server binds when `TRACELAKE_ADDR` is unset.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Path root under which compacted segments are written.
const DEFAULT_BASE_PATH: &str = "tracelake/segments";

#[tokio::main]
async fn main() {
    let addr: std::net::SocketAddr = std::env::var("TRACELAKE_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_string())
        .parse()
        .expect("TRACELAKE_ADDR must be a valid socket address");

    // Both stores are in-memory for now: hot spans in SlateDB over an
    // InMemory object store, segments on a separate InMemory store.
    // Persistent backing (local disk or S3) comes with deployment wiring.
    let db = TraceLakeDb::open_in_memory("tracelake-hot")
        .await
        .expect("hot store should open");
    let store: Arc<dyn slatedb::object_store::ObjectStore> = Arc::new(InMemory::new());

    let state = AppState::new(db, store, DEFAULT_BASE_PATH);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("server address should bind");

    println!("tracelake listening on http://{addr}");
    println!("endpoints:");
    println!("  POST /v1/spans                                  ingest a batch of spans");
    println!("  POST /v1/compact                                compact the hot store");
    println!("  GET  /v1/traces/{{tenant_id}}/{{namespace}}/{{trace_id}}   fetch one full trace");
    println!("  POST /v1/traces/search                          search by status/model/time");

    axum::serve(listener, app)
        .await
        .expect("server should serve");
}
