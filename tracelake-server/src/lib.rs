//! HTTP API: ingest, compaction, trace fetch, and search endpoints.

pub mod ingest_handler;
pub mod limits;
pub mod query_handler;
pub mod store;
pub mod table_handler;

use limits::{Limits, TenantLimiter};
use slatedb::object_store::ObjectStore;
use std::sync::Arc;
use tracelake_ingest::TraceLakeDb;
use tracelake_query::TraceLakeReader;

/// Largest accepted JSON request body. Spans and eval logs are batched, but a
/// single request must not be able to exhaust memory.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// Shared server state: the hot store, the segment store, and the reader
/// that merges them. Cloned per request; every clone shares the same
/// underlying handles.
#[derive(Clone)]
pub struct AppState {
    pub db: TraceLakeDb,
    pub store: Arc<dyn ObjectStore>,
    pub reader: TraceLakeReader,
    pub base_path: String,
    pub limits: Arc<TenantLimiter>,
}

impl AppState {
    /// Builds state over one hot store and one segment store; `base_path`
    /// must match the compactor's, e.g. `tracelake/segments`.
    pub fn new(db: TraceLakeDb, store: Arc<dyn ObjectStore>, base_path: &str) -> Self {
        Self::with_limits(db, store, base_path, Limits::default())
    }

    /// Builds state with explicit read limits.
    pub fn with_limits(
        db: TraceLakeDb,
        store: Arc<dyn ObjectStore>,
        base_path: &str,
        limits: Limits,
    ) -> Self {
        let reader = TraceLakeReader::new(db.clone(), Arc::clone(&store), base_path);
        Self {
            db,
            store,
            reader,
            base_path: base_path.to_string(),
            limits: Arc::new(TenantLimiter::new(limits)),
        }
    }
}

/// Builds the full API router.
pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/v1/spans", axum::routing::post(ingest_handler::post_spans))
        .route(
            "/v1/compact",
            axum::routing::post(ingest_handler::post_compact),
        )
        .route(
            "/v1/traces/{tenant_id}/{namespace}/{trace_id}",
            axum::routing::get(query_handler::get_trace),
        )
        .route(
            "/v1/traces/search",
            axum::routing::post(query_handler::post_search),
        )
        .route(
            "/v1/traces/{tenant_id}/{namespace}/{trace_id}",
            axum::routing::delete(ingest_handler::delete_trace),
        )
        .route(
            "/v1/retention",
            axum::routing::post(ingest_handler::post_retention),
        )
        .route(
            "/v1/tables/{tenant_id}/{namespace}/{table}/schema",
            axum::routing::put(table_handler::put_table_schema)
                .get(table_handler::get_table_schema),
        )
        .route(
            "/v1/tables/{tenant_id}/{namespace}/{table}/records",
            axum::routing::post(table_handler::post_records),
        )
        .route(
            "/v1/tables/{tenant_id}/{namespace}/{table}/records/{record_id}",
            axum::routing::get(table_handler::get_record).delete(table_handler::delete_record),
        )
        .route(
            "/v1/tables/{tenant_id}/{namespace}/{table}/query",
            axum::routing::post(table_handler::post_record_query),
        )
        .route(
            "/v1/tables/{tenant_id}/{namespace}/{table}/aggregate",
            axum::routing::post(table_handler::post_record_aggregate),
        )
        .route(
            "/v1/spans/aggregate",
            axum::routing::post(query_handler::post_span_aggregate),
        )
        .route("/v1/evals", axum::routing::post(table_handler::post_evals))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use chrono::{TimeZone, Utc};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use slatedb::object_store::memory::InMemory;
    use tower::ServiceExt;
    use tracelake_core::{Span, SpanStatus, SpanType};

    const BASE_PATH: &str = "tracelake/segments";

    fn example_span(trace_id: &str, span_id: &str, nanos: i64) -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: span_id.to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 0).unwrap()
                + chrono::Duration::nanoseconds(nanos),
            end_time: None,
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: Some(json!({ "prompt": "hello" })),
            output: None,
            tokens: None,
            cost_usd: None,
            model: Some("gpt-4o".to_string()),
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    async fn test_state() -> AppState {
        test_state_with_limits(crate::limits::Limits::default()).await
    }

    async fn test_state_with_limits(limits: crate::limits::Limits) -> AppState {
        let db = tracelake_ingest::TraceLakeDb::open_in_memory("/server-test")
            .await
            .expect("db should open");
        AppState::with_limits(db, Arc::new(InMemory::new()), BASE_PATH, limits)
    }

    async fn call(
        app: axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(
                body.map(|value| value.to_string()).unwrap_or_default(),
            ))
            .expect("request should build");

        let response = app.oneshot(request).await.expect("request should complete");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("body should be json")
        };

        (status, value)
    }

    #[tokio::test]
    async fn end_to_end_ingest_fetch_search_compact_fetch() {
        let app = build_router(test_state().await);

        let mut failed = example_span("trace-1", "span-a", 1);
        failed.status = SpanStatus::Error;
        failed.error = Some("model timeout".to_string());
        let ok_later = example_span("trace-1", "span-b", 2);
        let other_trace = example_span("trace-2", "span-a", 3);

        // 1. Ingest.
        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/spans",
            Some(json!({ "spans": [failed, ok_later, other_trace] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "written": 3 }));

        // 2. Fetch before compaction: hot only.
        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["trace_id"], "trace-1");
        assert_eq!(body["spans"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["spans"][0]["span_id"], "span-a");

        // 3. Missing trace: 404.
        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-absent",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 4. Search for failed traces before compaction.
        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/traces/search",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "status": "error"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(1));
        assert_eq!(body[0]["trace_id"], "trace-1");
        assert_eq!(body[0]["error"], "model timeout");

        // 5. Text search before compaction uses the hot-store index.
        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/traces/search",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "text_query": "timeout"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(1));
        assert_eq!(body[0]["trace_id"], "trace-1");

        // 6. Compact: spans move to a segment (hot copies remain; the gap
        // known from Step 4).
        let (status, body) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["segments"], json!(["segment-000001"]));

        // 7. Fetch after compaction: hot + segment merge must dedup to the
        // same trace.
        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spans"].as_array().map(Vec::len), Some(2));

        // 8. Search after compaction: same answer through the merge path.
        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/traces/search",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "status": "error"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(1));
        assert_eq!(body[0]["span_count"], 2);

        // 9. Text search after compaction reads the segment index.
        let (status, body) = call(
            app,
            "POST",
            "/v1/traces/search",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "text_query": "timeout"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(1));
        assert_eq!(body[0]["trace_id"], "trace-1");
    }

    #[tokio::test]
    async fn invalid_span_is_rejected_with_400() {
        let app = build_router(test_state().await);

        let mut invalid = example_span("trace-1", "span-a", 1);
        invalid.tenant_id = String::new();

        let (status, body) = call(
            app,
            "POST",
            "/v1/spans",
            Some(json!({ "spans": [invalid] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("tenant_id"));
    }

    #[tokio::test]
    async fn unsafe_identifiers_are_rejected_before_touching_storage() {
        let app = build_router(test_state().await);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/evals",
            Some(json!({
                "tenant_id": "..",
                "namespace": "default",
                "log": { "eval": {}, "samples": [] }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("tenant_id"));

        let (status, _) = call(
            app,
            "GET",
            "/v1/tables/%2E%2E/default/findings/schema",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tenants_and_namespaces_are_isolated() {
        let app = build_router(test_state().await);

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/spans",
            Some(json!({ "spans": [example_span("trace-isolated", "span-a", 1)] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-2/default/trace-isolated",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/traces/search",
            Some(json!({ "tenant_id": "tenantid-2", "namespace": "default" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(0));

        let schema = json!({
            "version": 1,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true }
            ]
        });
        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(schema),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [{ "finding_id": "f-1" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/tables/tenantid-2/default/findings/schema",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = call(
            app,
            "POST",
            "/v1/tables/tenantid-2/default/findings/query",
            Some(json!({ "limit": 10 })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn over_limit_queries_are_rejected_with_429() {
        let app = build_router(
            test_state_with_limits(crate::limits::Limits {
                query_timeout: std::time::Duration::from_secs(5),
                max_concurrent_per_tenant: 0,
            })
            .await,
        );

        for (method, uri, body) in [
            ("GET", "/v1/traces/tenantid-1/default/trace-1", None),
            (
                "POST",
                "/v1/spans/aggregate",
                Some(json!({
                    "tenant_id": "tenantid-1",
                    "namespace": "default",
                    "metrics": ["count"]
                })),
            ),
            (
                "POST",
                "/v1/tables/tenantid-1/default/findings/query",
                Some(json!({ "limit": 10 })),
            ),
        ] {
            let (status, _) = call(app.clone(), method, uri, body).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{method} {uri}");
        }

        let (status, _) = call(
            build_router(test_state().await),
            "GET",
            "/v1/traces/tenantid-1/default/trace-absent",
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a normal request still reaches the database"
        );
    }

    #[tokio::test]
    async fn malformed_body_is_rejected_with_400() {
        let app = build_router(test_state().await);

        let request = Request::builder()
            .method("POST")
            .uri("/v1/spans")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .expect("request should build");

        let response = app.oneshot(request).await.expect("request should complete");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    fn eval_log_fixture() -> Value {
        json!({
            "eval": {
                "eval_id": "eval-1",
                "task": "gsm8k",
                "model": "openai/gpt-4o",
                "status": "success"
            },
            "samples": [
                {
                    "id": "1",
                    "epoch": 1,
                    "input": "What is 2 + 2?",
                    "target": "4",
                    "output": { "completion": "4" },
                    "scores": { "accuracy": { "value": 1.0 } },
                    "metadata": { "trace_id": "trace-1" },
                    "model_usage": { "openai/gpt-4o": { "total_tokens": 15 } },
                    "total_time": 1.5,
                    "completed_at": "2026-10-01T12:00:02+00:00"
                },
                {
                    "id": "2",
                    "epoch": 1,
                    "input": "What is 9 * 9?",
                    "target": "81",
                    "scores": { "accuracy": { "value": 0.0 } },
                    "error": "model timeout"
                }
            ]
        })
    }

    #[tokio::test]
    async fn eval_logs_are_stored_and_queried_through_custom_schemas() {
        let app = build_router(test_state().await);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/evals",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "evals",
                "log": eval_log_fixture()
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["written"], 2);
        assert_eq!(body["table"], "inspect_ai_samples");

        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/tables/tenantid-1/evals/inspect_ai_samples/schema",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["primary_key"], "sample_id");

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/evals/inspect_ai_samples/query",
            Some(json!({
                "filter": { "ge": { "field": "score", "value": 0.5 } },
                "projection": ["task", "score", "trace_id"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["records"][0]["sample_id"], "1@1");
        assert_eq!(body["records"][0]["score"], 1.0);
        assert_eq!(body["records"][0]["trace_id"], "trace-1");
        assert!(body["records"][0].get("input").is_none());

        let (status, body) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["record_segments"], json!(["segment-000001"]));

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/evals/inspect_ai_samples/query",
            Some(json!({
                "filter": { "eq": { "field": "task", "value": "gsm8k" } },
                "projection": ["score"],
                "limit": 10
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["scanned_segments"], 1);

        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/tables/tenantid-1/evals/inspect_ai_samples/records/2@1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "error");
        assert_eq!(body["error"], "model timeout");
    }

    #[tokio::test]
    async fn span_aggregation_groups_by_model_and_computes_percentiles() {
        let app = build_router(test_state().await);
        let mut spans = Vec::new();

        for (index, (model, latency_ms)) in [
            ("gpt-4o", 100),
            ("gpt-4o", 200),
            ("claude-3", 500),
            ("claude-3", 1000),
        ]
        .into_iter()
        .enumerate()
        {
            let mut span = example_span(&format!("trace-{index}"), "span-a", index as i64);
            span.model = Some(model.to_string());
            span.end_time = Some(span.start_time + chrono::Duration::milliseconds(latency_ms));
            spans.push(span);
        }

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/spans",
            Some(json!({ "spans": spans })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/spans/aggregate",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "group_by": ["model"],
                "metrics": [
                    "count",
                    { "p95": { "field": "latency_ms" } },
                    { "sum": { "field": "cost_usd" } }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);

        let gpt = rows
            .iter()
            .find(|row| row["model"] == "gpt-4o")
            .expect("gpt-4o row");
        assert_eq!(gpt["count"], 2);
        let p95 = gpt["p95_latency_ms"].as_f64().expect("p95");
        assert!((100.0..=200.0).contains(&p95), "p95 was {p95}");

        let claude = rows
            .iter()
            .find(|row| row["model"] == "claude-3")
            .expect("claude row");
        assert_eq!(claude["count"], 2);
    }

    #[tokio::test]
    async fn record_aggregation_reads_column_files() {
        let app = build_router(test_state().await);

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/evals",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "evals",
                "log": eval_log_fixture()
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/evals/inspect_ai_samples/aggregate",
            Some(json!({
                "group_by": ["task"],
                "metrics": [
                    "count",
                    { "avg": { "field": "score" } },
                    { "p50": { "field": "total_tokens" } }
                ]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = body["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["task"], "gsm8k");
        assert_eq!(rows[0]["count"], 2);
        assert_eq!(rows[0]["avg_score"], 0.5);
    }

    #[tokio::test]
    async fn aggregation_reads_segments_written_before_a_schema_change() {
        let app = build_router(test_state().await);

        let v1 = json!({
            "version": 1,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true },
                { "name": "severity", "field_type": "string", "required": true }
            ]
        });
        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(v1),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [{ "finding_id": "f-1", "severity": "high" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let v2 = json!({
            "version": 2,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true },
                { "name": "severity", "field_type": "string", "required": true },
                { "name": "confidence", "field_type": "f64", "required": false }
            ]
        });
        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(v2),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [
                { "finding_id": "f-2", "severity": "low", "confidence": 0.9 }
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app,
            "POST",
            "/v1/tables/tenantid-1/default/findings/aggregate",
            Some(json!({
                "group_by": ["severity"],
                "metrics": ["count", { "avg": { "field": "confidence" } }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let rows = body["rows"].as_array().expect("rows");
        let high = rows
            .iter()
            .find(|row| row["severity"] == "high")
            .expect("high row");
        assert_eq!(high["count"], 1);
        assert!(high["avg_confidence"].is_null());

        let low = rows
            .iter()
            .find(|row| row["severity"] == "low")
            .expect("low row");
        assert_eq!(low["count"], 1);
        assert_eq!(low["avg_confidence"], 0.9);
    }

    #[tokio::test]
    async fn repeat_compaction_writes_nothing_and_does_not_duplicate_results() {
        let app = build_router(test_state().await);

        let mut first = example_span("trace-1", "span-a", 1);
        first.model = Some("gpt-4o".to_string());
        first.end_time = Some(first.start_time + chrono::Duration::milliseconds(100));
        let mut second = example_span("trace-1", "span-b", 2);
        second.model = Some("gpt-4o".to_string());
        second.end_time = Some(second.start_time + chrono::Duration::milliseconds(200));

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/spans",
            Some(json!({ "spans": [first, second] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let schema = json!({
            "version": 1,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true }
            ]
        });
        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(schema),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [{ "finding_id": "f-1" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["segments"], json!(["segment-000001"]));
        assert_eq!(body["record_segments"], json!(["segment-000001"]));

        let (status, body) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["segments"], json!([]));
        assert_eq!(body["record_segments"], json!([]));

        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spans"].as_array().map(Vec::len), Some(2));

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/spans/aggregate",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "metrics": ["count"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"][0]["count"], 2);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/query",
            Some(json!({ "limit": 10 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(1));

        let (status, body) = call(
            app,
            "POST",
            "/v1/tables/tenantid-1/default/findings/aggregate",
            Some(json!({ "metrics": ["count"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"][0]["count"], 1);
    }

    #[tokio::test]
    async fn deletes_are_visible_in_every_read_path_after_compaction() {
        let app = build_router(test_state().await);

        let mut gone = example_span("trace-gone", "span-a", 1);
        gone.status = SpanStatus::Error;
        gone.model = Some("gpt-4o".to_string());
        gone.output = Some(json!({ "text": "hallucination in the answer" }));
        gone.end_time = Some(gone.start_time + chrono::Duration::milliseconds(100));

        let mut kept = example_span("trace-keep", "span-a", 2);
        kept.model = Some("gpt-4o".to_string());
        kept.end_time = Some(kept.start_time + chrono::Duration::milliseconds(200));

        let schema = json!({
            "version": 1,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true }
            ]
        });
        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(schema),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/spans",
            Some(json!({ "spans": [gone, kept] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [
                { "finding_id": "f-gone" },
                { "finding_id": "f-keep" }
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "DELETE",
            "/v1/traces/tenantid-1/default/trace-gone",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            app.clone(),
            "DELETE",
            "/v1/tables/tenantid-1/default/findings/records/f-gone",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // A second compaction persists the tombstones in the manifests.
        let (status, body) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["segments"], json!([]));

        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-gone",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-keep",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spans"].as_array().map(Vec::len), Some(1));

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/traces/search",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "status": "error"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(0));

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/traces/search",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "text_query": "hallucination"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().map(Vec::len), Some(0));

        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/tables/tenantid-1/default/findings/records/f-gone",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/query",
            Some(json!({ "limit": 10 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["records"].as_array().expect("records");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["finding_id"], "f-keep");

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/spans/aggregate",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "metrics": ["count"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["rows"][0]["count"], 1);

        let (status, body) = call(
            app,
            "POST",
            "/v1/tables/tenantid-1/default/findings/aggregate",
            Some(json!({ "metrics": ["count"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"][0]["count"], 1);
    }

    #[tokio::test]
    async fn retention_deletes_traces_before_a_cutoff() {
        let app = build_router(test_state().await);

        let old = example_span("trace-old", "span-a", 0);
        let new = example_span("trace-new", "span-a", 5_000_000_000);

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/spans",
            Some(json!({ "spans": [old, new] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/retention",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "before": "2026-06-16T10:00:02Z"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted_traces"], 1);

        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-old",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-new",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "GET",
            "/v1/traces/tenantid-1/default/trace-old",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = call(
            app,
            "POST",
            "/v1/spans/aggregate",
            Some(json!({
                "tenant_id": "tenantid-1",
                "namespace": "default",
                "metrics": ["count"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"][0]["count"], 1);
    }

    #[tokio::test]
    async fn custom_tables_accept_records_only_after_schema_registration() {
        let app = build_router(test_state().await);
        let schema = json!({
            "version": 1,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true },
                { "name": "severity", "field_type": "string", "required": true },
                { "name": "confidence", "field_type": "f64", "required": false }
            ]
        });
        let records = json!({
            "records": [
                { "finding_id": "f-1", "severity": "high", "confidence": 0.9 },
                { "finding_id": "f-2", "severity": "low", "confidence": 0.2 }
            ]
        });

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(records.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(schema.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(records),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["written"], 2);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/query",
            Some(json!({
                "filter": { "and": [
                    { "eq": { "field": "severity", "value": "high" } },
                    { "gt": { "field": "confidence", "value": 0.5 } }
                ]}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["records"][0]["finding_id"], "f-1");

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [{ "finding_id": "f-3", "severity": "high", "nope": 1 }] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, failure) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(json!({
                "version": 1,
                "table": "findings",
                "primary_key": "finding_id",
                "fields": [
                    { "name": "finding_id", "field_type": "string", "required": true },
                    { "name": "severity", "field_type": "i64", "required": true }
                ]
            })),
        )
        .await;
        assert_eq!(
            failure["error"],
            "field 'severity' changed type from string to i64"
        );
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn an_evolved_schema_reads_segments_written_under_the_old_one() {
        let app = build_router(test_state().await);

        let v1 = json!({
            "version": 1,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true },
                { "name": "severity", "field_type": "string", "required": true }
            ]
        });
        let (status, _) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(v1),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [{ "finding_id": "f-1", "severity": "high" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let v2 = json!({
            "version": 2,
            "table": "findings",
            "primary_key": "finding_id",
            "fields": [
                { "name": "finding_id", "field_type": "string", "required": true },
                { "name": "severity", "field_type": "string", "required": true },
                { "name": "confidence", "field_type": "f64", "required": false },
                {
                    "name": "source",
                    "field_type": "string",
                    "required": true,
                    "default": "manual"
                }
            ]
        });
        let (status, body) = call(
            app.clone(),
            "PUT",
            "/v1/tables/tenantid-1/default/findings/schema",
            Some(v2),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"], 2);

        let (status, body) = call(
            app.clone(),
            "GET",
            "/v1/tables/tenantid-1/default/findings/records/f-1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "manual");
        assert!(body.get("confidence").is_none());

        let (status, _) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/records",
            Some(json!({ "records": [
                { "finding_id": "f-2", "severity": "high", "confidence": 0.9, "source": "scanner" }
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/query",
            Some(json!({
                "filter": { "ge": { "field": "confidence", "value": 0.5 } },
                "projection": ["severity", "confidence", "source"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["records"][0]["finding_id"], "f-2");

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/query",
            Some(json!({
                "filter": { "eq": { "field": "source", "value": "manual" } }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["records"][0]["finding_id"], "f-1");
        assert_eq!(body["records"][0]["source"], "manual");

        let (status, _) = call(app.clone(), "POST", "/v1/compact", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            "POST",
            "/v1/tables/tenantid-1/default/findings/query",
            Some(json!({ "limit": 10 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["records"].as_array().map(Vec::len), Some(2));
    }
}
