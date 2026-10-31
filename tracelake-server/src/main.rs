//! TraceLake HTTP server entry point.

use slatedb::object_store::local::LocalFileSystem;
use std::sync::Arc;
use tracelake_ingest::TraceLakeDb;
use tracelake_server::{AppState, build_router};

/// Address the server binds when `TRACELAKE_ADDR` is unset. Loopback is the
/// safe default because the alpha has no authentication.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// Directory that holds the hot store and compacted segments.
const DEFAULT_DATA_DIR: &str = "./tracelake-data";

/// Path roots inside the data directory.
const HOT_PATH: &str = "hot";
const SEGMENT_PATH: &str = "segments";

#[tokio::main]
async fn main() {
    let addr: std::net::SocketAddr = std::env::var("TRACELAKE_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_string())
        .parse()
        .expect("TRACELAKE_ADDR must be a valid socket address");

    let data_dir = std::env::var("TRACELAKE_DATA_DIR").unwrap_or_else(|_| DEFAULT_DATA_DIR.into());
    std::fs::create_dir_all(&data_dir).expect("data directory should be creatable");
    let store: Arc<dyn slatedb::object_store::ObjectStore> =
        Arc::new(LocalFileSystem::new_with_prefix(&data_dir).expect("data directory should open"));

    let db = TraceLakeDb::open(Arc::clone(&store), HOT_PATH)
        .await
        .expect("hot store should open");

    let state = AppState::new(db, store, SEGMENT_PATH);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("server address should bind");

    println!("tracelake listening on http://{addr}");
    println!("data directory: {data_dir}");
    println!("endpoints:");
    println!("  POST /v1/spans                                  ingest a batch of spans");
    println!("  POST /v1/compact                                compact the hot store");
    println!("  GET  /v1/traces/{{tenant_id}}/{{namespace}}/{{trace_id}}   fetch one full trace");
    println!("  POST /v1/traces/search                          search by status/model/time");

    axum::serve(listener, app)
        .await
        .expect("server should serve");
}
