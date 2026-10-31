//! HTTP API: ingest, compaction, trace fetch, and search endpoints.

pub mod ingest_handler;
pub mod query_handler;
pub mod table_handler;

use slatedb::object_store::ObjectStore;
use std::sync::Arc;
use tracelake_ingest::TraceLakeDb;
use tracelake_query::TraceLakeReader;

/// Shared server state: the hot store, the segment store, and the reader
/// that merges them. Cloned per request; every clone shares the same
/// underlying handles.
#[derive(Clone)]
pub struct AppState {
    pub db: TraceLakeDb,
    pub store: Arc<dyn ObjectStore>,
    pub reader: TraceLakeReader,
    pub base_path: String,
}

impl AppState {
    /// Builds state over one hot store and one segment store; `base_path`
    /// must match the compactor's, e.g. `tracelake/segments`.
    pub fn new(db: TraceLakeDb, store: Arc<dyn ObjectStore>, base_path: &str) -> Self {
        let reader = TraceLakeReader::new(db.clone(), Arc::clone(&store), base_path);
        Self {
            db,
            store,
            reader,
            base_path: base_path.to_string(),
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
            axum::routing::get(table_handler::get_record),
        )
        .route(
            "/v1/tables/{tenant_id}/{namespace}/{table}/query",
            axum::routing::post(table_handler::post_record_query),
        )
        .route("/v1/evals", axum::routing::post(table_handler::post_evals))
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
        let db = tracelake_ingest::TraceLakeDb::open_in_memory("/server-test")
            .await
            .expect("db should open");
        AppState::new(db, Arc::new(InMemory::new()), BASE_PATH)
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
