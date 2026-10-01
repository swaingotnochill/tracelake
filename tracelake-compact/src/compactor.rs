use thiserror::Error;
use tracelake_core::Trace;
use tracelake_ingest::TraceLakeDb;

/// Errors returned by the compaction pipeline.
#[derive(Debug, Error)]
pub enum CompactError {
    #[error("hot store scan failed: {0}")]
    Scan(#[from] tracelake_ingest::IngestError),

    #[error("trace pack encode failed: {0}")]
    Pack(#[from] tracelake_core::TracePackError),

    #[error("object store io failed: {0}")]
    ObjectStore(#[from] slatedb::object_store::Error),
}

/// All traces for one (tenant, namespace) pair; each group becomes one trace
/// pack.
#[derive(Debug)]
pub struct TraceGroup {
    pub tenant_id: String,
    pub namespace: String,
    pub traces: Vec<Trace>,
}

/// Scans the hot store and groups spans into traces per tenant and namespace.
///
/// Spans arrive in storage key order, so groups, traces, and spans all keep
/// that order: tenant, namespace, trace_id, then start time, then span_id.
pub async fn collect_traces(db: &TraceLakeDb) -> Result<Vec<TraceGroup>, CompactError> {
    let spans = db.scan_all_spans().await?;
    let mut groups: Vec<TraceGroup> = Vec::new();

    for span in spans {
        let same_group = groups
            .last()
            .is_some_and(|g| g.tenant_id == span.tenant_id && g.namespace == span.namespace);

        if !same_group {
            groups.push(TraceGroup {
                tenant_id: span.tenant_id.clone(),
                namespace: span.namespace.clone(),
                traces: Vec::new(),
            });
        }

        if let Some(group) = groups.last_mut() {
            let same_trace = group
                .traces
                .last()
                .is_some_and(|t| t.trace_id == span.trace_id);

            if !same_trace {
                group.traces.push(Trace {
                    tenant_id: span.tenant_id.clone(),
                    namespace: span.namespace.clone(),
                    trace_id: span.trace_id.clone(),
                    spans: Vec::new(),
                });
            }

            if let Some(trace) = group.traces.last_mut() {
                trace.spans.push(span);
            }
        }
    }

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use tracelake_core::{Span, SpanStatus, SpanType};

    fn make_span(
        tenant_id: &str,
        namespace: &str,
        trace_id: &str,
        span_id: &str,
        second: u32,
    ) -> Span {
        Span {
            tenant_id: tenant_id.to_string(),
            namespace: namespace.to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: span_id.to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, second).unwrap(),
            end_time: None,
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: Some(json!({ "prompt": "hello" })),
            output: None,
            tokens: None,
            cost_usd: None,
            model: None,
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    fn summarize(groups: &[TraceGroup]) -> Vec<(&str, &str, Vec<(&str, Vec<&str>)>)> {
        groups
            .iter()
            .map(|g| {
                (
                    g.tenant_id.as_str(),
                    g.namespace.as_str(),
                    g.traces
                        .iter()
                        .map(|t| {
                            (
                                t.trace_id.as_str(),
                                t.spans
                                    .iter()
                                    .map(|s| s.span_id.as_str())
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn collect_traces_groups_by_tenant_namespace_and_trace() {
        let db = TraceLakeDb::open_in_memory("/test-collect-traces")
            .await
            .expect("db should open");

        let trace1_later = make_span("tenant-a", "default", "trace-1", "b", 2);
        let trace1_earlier = make_span("tenant-a", "default", "trace-1", "a", 1);
        let trace2 = make_span("tenant-a", "default", "trace-2", "a", 1);
        let prod = make_span("tenant-a", "prod", "trace-1", "a", 1);
        let other_tenant = make_span("tenant-b", "default", "trace-1", "a", 1);

        db.put_spans(&[
            other_tenant.clone(),
            trace1_later.clone(),
            prod.clone(),
            trace1_earlier.clone(),
            trace2.clone(),
        ])
        .await
        .expect("spans should write");

        let groups = collect_traces(&db).await.expect("traces should collect");

        assert_eq!(
            summarize(&groups),
            vec![
                (
                    "tenant-a",
                    "default",
                    vec![("trace-1", vec!["a", "b"]), ("trace-2", vec!["a"]),]
                ),
                ("tenant-a", "prod", vec![("trace-1", vec!["a"])]),
                ("tenant-b", "default", vec![("trace-1", vec!["a"])]),
            ]
        );
    }

    #[tokio::test]
    async fn collect_traces_returns_empty_for_empty_store() {
        let db = TraceLakeDb::open_in_memory("/test-collect-empty")
            .await
            .expect("db should open");

        let groups = collect_traces(&db).await.expect("traces should collect");

        assert!(groups.is_empty());
    }
}
