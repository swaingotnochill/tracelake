use crate::compactor::{CompactError, TraceGroup};
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::{Span, decode_trace_pack, encode_trace_pack};

/// File name written for every Step 3 pack; real segment numbering arrives
/// with the manifest in Step 4.
const PACK_FILE_NAME: &str = "segment-000001.tracepack";

/// Writes trace packs to an object store and reads traces back from them.
pub struct TracePackWriter {
    store: Arc<dyn ObjectStore>,
}

impl TracePackWriter {
    /// Creates a writer over one object store.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Writes one pack per trace group under
    /// `{base_path}/{tenant_id}/{namespace}/{PACK_FILE_NAME}` and returns the
    /// object paths written, in group order.
    ///
    /// `base_path` must be a relative object store path without a leading `/`.
    pub async fn write_groups(
        &self,
        groups: &[TraceGroup],
        base_path: &str,
    ) -> Result<Vec<String>, CompactError> {
        let mut paths = Vec::with_capacity(groups.len());

        for group in groups {
            let path = pack_path(base_path, &group.tenant_id, &group.namespace);
            let bytes = encode_trace_pack(&group.traces)?;

            self.store
                .put(&Path::from(path.as_str()), bytes.into())
                .await?;

            paths.push(path);
        }

        Ok(paths)
    }

    /// Reads one trace back from a pack object, or `None` when the pack does
    /// not hold the trace.
    pub async fn read_trace(
        &self,
        pack_path: &str,
        trace_id: &str,
    ) -> Result<Option<Vec<Span>>, CompactError> {
        let path = Path::from(pack_path);
        let bytes = self.store.get(&path).await?.bytes().await?;
        let pack = decode_trace_pack(&bytes)?;

        Ok(pack.read_trace(trace_id)?)
    }
}

/// Builds one pack's object path.
fn pack_path(base_path: &str, tenant_id: &str, namespace: &str) -> String {
    format!("{base_path}/{tenant_id}/{namespace}/{PACK_FILE_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect_traces;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Instant;
    use tracelake_core::{SpanStatus, SpanType};
    use tracelake_ingest::TraceLakeDb;

    const BASE_PATH: &str = "tracelake/segments";

    fn scale_span(trace_num: usize, span_num: usize) -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: format!("trace-{trace_num:03}"),
            span_id: format!("span-{span_num:03}"),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: format!("span-{span_num:03}"),
            start_time: Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 0).unwrap()
                + chrono::Duration::nanoseconds(((trace_num * 1_000 + span_num) as i64)),
            end_time: None,
            status: SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: Some(json!({
                "prompt": "The user asks the support agent to explain the refund policy "
                    .repeat(6)
            })),
            output: Some(json!({
                "text": "The agent explains that refunds post within five business days "
                    .repeat(6)
            })),
            tokens: None,
            cost_usd: None,
            model: Some("gpt-4o".to_string()),
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    #[tokio::test]
    async fn ten_k_spans_round_trip_through_one_pack() {
        const TRACE_COUNT: usize = 100;
        const SPANS_PER_TRACE: usize = 100;
        const SPAN_COUNT: usize = TRACE_COUNT * SPANS_PER_TRACE;

        let db = TraceLakeDb::open_in_memory("/test-10k-pack")
            .await
            .expect("db should open");
        let writer = TracePackWriter::new(Arc::new(slatedb::object_store::memory::InMemory::new()));

        let generate_started = Instant::now();
        let spans: Vec<Span> = (0..TRACE_COUNT)
            .flat_map(|trace_num| {
                (0..SPANS_PER_TRACE).map(move |span_num| scale_span(trace_num, span_num))
            })
            .collect();
        let generate_elapsed = generate_started.elapsed();

        let ingest_started = Instant::now();
        db.put_spans(&spans).await.expect("10k spans should write");
        let ingest_elapsed = ingest_started.elapsed();

        let collect_started = Instant::now();
        let groups = collect_traces(&db).await.expect("traces should collect");
        let collect_elapsed = collect_started.elapsed();

        let write_started = Instant::now();
        let paths = writer
            .write_groups(&groups, BASE_PATH)
            .await
            .expect("packs should write");
        let write_elapsed = write_started.elapsed();

        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            format!("{BASE_PATH}/tenantid-1/default/{PACK_FILE_NAME}")
        );

        let mut expected: HashMap<String, Vec<&Span>> = HashMap::new();
        for span in &spans {
            expected
                .entry(span.trace_id.clone())
                .or_default()
                .push(span);
        }

        let read_started = Instant::now();
        let mut total = 0;
        for trace_num in 0..TRACE_COUNT {
            let trace_id = format!("trace-{trace_num:03}");
            let read_back = writer
                .read_trace(&paths[0], &trace_id)
                .await
                .expect("trace should read")
                .unwrap_or_else(|| panic!("trace {trace_id} should be in the pack"));

            let expected_spans = &expected[&trace_id];
            assert_eq!(
                read_back.len(),
                expected_spans.len(),
                "trace {trace_id} span count"
            );

            for (index, span) in read_back.iter().enumerate() {
                assert_eq!(span, expected_spans[index], "trace {trace_id} span {index}");
            }

            total += read_back.len();
        }
        let read_elapsed = read_started.elapsed();

        assert_eq!(total, SPAN_COUNT);

        println!(
            "10K pack check: generate={generate_elapsed:?} ingest={ingest_elapsed:?} \
             collect={collect_elapsed:?} pack_write={write_elapsed:?} \
             read_{TRACE_COUNT}_traces={read_elapsed:?}"
        );
    }
}
