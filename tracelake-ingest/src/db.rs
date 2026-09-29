use slatedb::object_store::{ObjectStore, memory::InMemory};
use slatedb::{Db, WriteBatch};
use tracelake_core::{Span, encode_span_storage_key, trace_prefix};

use std::sync::Arc;

use crate::ingest::IngestError;

/// Thin SlateDB wrapper for writing and reading hot trace spans.
pub struct TraceLakeDb {
    db: Db,
}

impl TraceLakeDb {
    /// Opens a TraceLake database backed by SlateDB's in-memory object store.
    pub async fn open_in_memory(path: &str) -> Result<Self, IngestError> {
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = Db::builder(path, object_store).build().await?;

        Ok(Self { db })
    }

    /// Writes one span under its canonical TraceLake storage key.
    pub async fn put_span(&self, span: &Span) -> Result<(), IngestError> {
        let key = encode_span_storage_key(span)?;
        let value = serde_json::to_vec(span)?;

        self.db.put(key, value).await?;

        Ok(())
    }

    /// Writes multiple spans in one SlateDB batch.
    pub async fn put_spans(&self, spans: &[Span]) -> Result<(), IngestError> {
        let mut batch = WriteBatch::new();

        for span in spans {
            let key = encode_span_storage_key(span)?;
            let value = serde_json::to_vec(span)?;
            batch.put(key, value);
        }

        self.db.write(batch).await?;
        Ok(())
    }

    /// Reads all spans for one trace from the hot SlateDB store in key order.
    pub async fn get_trace_spans(
        &self,
        tenant_id: &str,
        namespace: &str,
        trace_id: &str,
    ) -> Result<Vec<Span>, IngestError> {
        let prefix = trace_prefix(tenant_id, namespace, trace_id)?;
        let mut iter = self.db.scan_prefix(prefix).await?;

        let mut spans = Vec::new();

        while let Some(kv) = iter.next().await? {
            let span: Span = serde_json::from_slice(&kv.value)?;
            spans.push(span);
        }

        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::time::Instant;
    use tracelake_core::{KeyError, Span, SpanStatus, SpanType, TokenUsage};

    fn get_example_span() -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            span_id: "span-1".to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: "gpt-4o".to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 4, 10, 30, 0).unwrap(),
            end_time: Some(Utc.with_ymd_and_hms(2026, 6, 4, 10, 30, 1).unwrap()),
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: Some(json!({ "prompt": "hello" })),
            output: Some(json!({ "text": "world" })),
            tokens: Some(TokenUsage {
                prompt: 10,
                completion: 20,
                total: 30,
            }),
            cost_usd: Some(0.001),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    #[tokio::test]
    async fn put_span_writes_valid_span() {
        let db = TraceLakeDb::open_in_memory("/test-put-span")
            .await
            .expect("db should open");

        let span = get_example_span();

        db.put_span(&span).await.expect("span should write");
    }

    #[tokio::test]
    async fn put_span_rejects_invalid_span_key() {
        let mut span = get_example_span();
        span.tenant_id = "".to_string();

        let db = TraceLakeDb::open_in_memory("/test-put-span")
            .await
            .expect("db should open");

        let error = db
            .put_span(&span)
            .await
            .expect_err("span with empty tenant_id should fail");

        assert!(matches!(
            error,
            IngestError::Key(KeyError::EmptySegment { field: "tenant_id" })
        ));
    }

    #[tokio::test]
    async fn put_span_stores_span_under_encoded_key() {
        let span = get_example_span();
        let key = encode_span_storage_key(&span).expect("key should encode");
        let db = TraceLakeDb::open_in_memory("/test-put-span")
            .await
            .expect("db should open");

        db.put_span(&span).await.expect("span should write");

        let stored = db
            .db
            .get(key)
            .await
            .expect("span should read")
            .expect("span should exist");

        let decoded: Span =
            serde_json::from_slice(&stored).expect("stored span should deserialize");

        assert_eq!(decoded, span);
    }

    #[tokio::test]
    async fn put_spans_writes_multiple_spans_in_one_batch() {
        let db = TraceLakeDb::open_in_memory("/test-put-spans")
            .await
            .expect("db should open");

        let mut first = get_example_span();
        first.span_id = "span-1".to_string();

        let mut second = get_example_span();
        second.span_id = "span-2".to_string();
        second.start_time = Utc
            .with_ymd_and_hms(2026, 6, 9, 20, 53, 1)
            .single()
            .expect("test timestamp should be valid");

        let spans = vec![first.clone(), second.clone()];

        db.put_spans(&spans)
            .await
            .expect("multiple spans should write");

        for span in spans {
            let key = encode_span_storage_key(&span).expect("key should encode");

            let stored = db
                .db
                .get(key)
                .await
                .expect("span should read")
                .expect("span should exist");

            let decoded: Span =
                serde_json::from_slice(&stored).expect("stored span should deserialize");

            assert_eq!(decoded, span);
        }
    }

    #[tokio::test]
    async fn get_trace_spans_read_only_in_key_order() {
        let db = TraceLakeDb::open_in_memory("/test-get-trace-spans")
            .await
            .expect("db should open");

        let mut later = get_example_span();
        later.span_id = "span-later".to_string();
        later.start_time = Utc
            .with_ymd_and_hms(2026, 6, 9, 21, 0, 2)
            .single()
            .expect("test timestamp should be valid");

        let mut earlier = get_example_span();
        earlier.span_id = "span-earlier".to_string();
        earlier.start_time = Utc
            .with_ymd_and_hms(2026, 6, 9, 21, 0, 1)
            .single()
            .expect("test timestamp should be valid");

        let mut other_trace = get_example_span();
        other_trace.span_id = "trace-other".to_string();
        other_trace.trace_id = "trace-2".to_string();

        db.put_spans(&[later.clone(), other_trace, earlier.clone()])
            .await
            .expect("spans should write");

        let spans = db
            .get_trace_spans("tenantid-1", "default", "trace-1")
            .await
            .expect("trace spans should read");

        assert_eq!(spans, vec![earlier, later]);
    }

    #[tokio::test]
    async fn get_trace_spans_isolated_by_tenant_and_namespace() {
        let db = TraceLakeDb::open_in_memory("/test-trace-isolation")
            .await
            .expect("db should open");

        let wanted = get_example_span();

        let mut other_tenant = wanted.clone();
        other_tenant.tenant_id = "tenantid-2".to_string();

        let mut other_namespace = wanted.clone();
        other_namespace.namespace = "other".to_string();

        db.put_spans(&[wanted.clone(), other_tenant, other_namespace])
            .await
            .expect("spans should write");

        let spans = db
            .get_trace_spans("tenantid-1", "default", "trace-1")
            .await
            .expect("trace spans should read");

        assert_eq!(spans, vec![wanted]);
    }

    #[tokio::test]
    #[ignore = "writes 100K spans; run manually for ingest scale check"]
    async fn writes_and_reads_100k_spans_grouped_by_traces() {
        const TRACE_COUNT: usize = 100;
        const SPANS_PER_TRACE: usize = 1_000;
        const SPAN_COUNT: usize = TRACE_COUNT * SPANS_PER_TRACE;

        let db = TraceLakeDb::open_in_memory("/test-100k-spans")
            .await
            .expect("db should open");

        let mut base = get_example_span();
        base.input = Some(json!({
            "prompt": "The user asks the support agent to explain the refund policy \
                       for order 8837 and escalate a duplicate charge. "
                .repeat(4)
        }));
        base.output = Some(json!({
            "text": "The agent explains that refunds post within five business days \
                     and opens a dispute for the duplicate charge. "
                .repeat(4)
        }));

        let mut spans = Vec::with_capacity(SPAN_COUNT);

        let generate_started = Instant::now();
        for trace_num in 0..TRACE_COUNT {
            for span_num in 0..SPANS_PER_TRACE {
                let mut span = base.clone();
                span.trace_id = format!("trace-{trace_num:03}");
                span.span_id = format!("span-{span_num:04}");
                span.start_time = base.start_time
                    + chrono::Duration::nanoseconds(
                        (trace_num * SPANS_PER_TRACE + span_num) as i64,
                    );
                spans.push(span);
            }
        }
        let generate_elapsed = generate_started.elapsed();

        // Measure each ingest phase separately so no single total hides the
        // bottleneck: key encoding, serialization, SlateDB put(), and the
        // scan_prefix() readback below.
        let encode_started = Instant::now();
        let keys: Vec<Vec<u8>> = spans
            .iter()
            .map(|span| encode_span_storage_key(span).expect("key should encode"))
            .collect();
        let encode_elapsed = encode_started.elapsed();

        let serialize_started = Instant::now();
        let values: Vec<Vec<u8>> = spans
            .iter()
            .map(|span| serde_json::to_vec(span).expect("span should serialize"))
            .collect();
        let serialize_elapsed = serialize_started.elapsed();

        let value_bytes: usize = values.iter().map(|value| value.len()).sum();
        drop(spans);

        let write_started = Instant::now();
        let mut batch = WriteBatch::new();
        for (key, value) in keys.into_iter().zip(values) {
            batch.put(key, value);
        }
        db.db.write(batch).await.expect("100k spans should write");
        let write_elapsed = write_started.elapsed();

        let read_started = Instant::now();
        let trace_spans = db
            .get_trace_spans("tenantid-1", "default", "trace-042")
            .await
            .expect("trace spans should read");
        let read_elapsed = read_started.elapsed();

        let encode_rate = SPAN_COUNT as f64 / encode_elapsed.as_secs_f64();
        let serialize_rate = SPAN_COUNT as f64 / serialize_elapsed.as_secs_f64();
        let write_rate = SPAN_COUNT as f64 / write_elapsed.as_secs_f64();

        println!(
            "100K ingest check: {TRACE_COUNT} traces x {SPANS_PER_TRACE} spans, \
             ~{} B/span value, one WriteBatch, in-memory object store",
            value_bytes / SPAN_COUNT
        );
        println!(
            "generate={generate_elapsed:?} encode={encode_elapsed:?} ({encode_rate:.0} keys/s) \
             serialize={serialize_elapsed:?} ({serialize_rate:.0} spans/s) \
             write={write_elapsed:?} ({write_rate:.0} puts/s) \
             read_1k_trace={read_elapsed:?}"
        );

        assert_eq!(trace_spans.len(), SPANS_PER_TRACE);

        for (index, span) in trace_spans.iter().enumerate() {
            assert_eq!(span.trace_id, "trace-042");
            assert_eq!(span.span_id, format!("span-{index:04}"));
        }

        // Every trace must read back with an exact span count: 100 x 1000 = 100K.
        for trace_num in 0..TRACE_COUNT {
            let trace_id = format!("trace-{trace_num:03}");
            let trace_all = db
                .get_trace_spans("tenantid-1", "default", &trace_id)
                .await
                .expect("trace spans should read");
            assert_eq!(
                trace_all.len(),
                SPANS_PER_TRACE,
                "trace {trace_id} should hold exactly {SPANS_PER_TRACE} spans"
            );
        }
    }
}
