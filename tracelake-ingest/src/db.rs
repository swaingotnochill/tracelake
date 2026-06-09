use slatedb::object_store::{ObjectStore, memory::InMemory};
use slatedb::{Db, DbWriteOps, WriteBatch};
use tracelake_core::{Span, encode_span_storage_key, trace_prefix};

use std::sync::Arc;

use crate::ingest::IngestError;

pub struct TraceLakeDb {
    db: Db,
}

impl TraceLakeDb {
    pub async fn open_in_memory(path: &str) -> Result<Self, IngestError> {
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = Db::builder(path, object_store).build().await?;

        Ok(Self { db })
    }

    pub async fn put_span(&self, span: &Span) -> Result<(), IngestError> {
        let key = encode_span_storage_key(span)?;
        let value = serde_json::to_vec(span)?;

        self.db.put(key, value).await?;

        Ok(())
    }

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

    pub async fn get_trace_spans(
        &self,
        tenant_id: String,
        namespace: String,
        trace_id: String,
    ) -> Result<Vec<Span>, IngestError> {
        let prefix = trace_prefix(&tenant_id, &namespace, &trace_id)?;
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
            .get_trace_spans(
                "tenantid-1".to_string(),
                "default".to_string(),
                "trace-1".to_string(),
            )
            .await
            .expect("trace spans should read");

        assert_eq!(spans, vec![earlier, later]);
    }
}
