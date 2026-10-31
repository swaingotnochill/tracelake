//! `get_trace()`: fetch one full trace by id, merging the hot SlateDB store
//! with compacted segments.

use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use thiserror::Error;
use tracelake_compact::{TracePackWriter, read_manifest};
use tracelake_core::{
    BloomFilter, KeyError, SegmentEntry, SegmentFile, SegmentFileKind, Span, Trace, trace_prefix,
};
use tracelake_ingest::TraceLakeDb;

use crate::merge::merge_spans;

/// Errors returned while fetching traces.
#[derive(Debug, Error)]
pub enum QueryError {
    #[error("trace lookup arguments are invalid: {0}")]
    Key(#[from] KeyError),

    #[error("hot store read failed: {0}")]
    Hot(#[from] tracelake_ingest::IngestError),

    #[error("segment read failed: {0}")]
    Segment(#[from] tracelake_compact::CompactError),

    #[error("trace pack decode failed: {0}")]
    Pack(#[from] tracelake_core::TracePackError),

    #[error("record pack decode failed: {0}")]
    RecordPack(#[from] tracelake_core::RecordPackError),

    #[error("bloom filter decode failed: {0}")]
    Bloom(#[from] tracelake_core::BloomError),

    #[error("object store io failed: {0}")]
    ObjectStore(#[from] slatedb::object_store::Error),

    #[error("segment {segment_id} lists no {kind} file")]
    MissingSegmentFile {
        segment_id: String,
        kind: &'static str,
    },

    #[error("segment {segment_id} lists no record pack")]
    MissingRecordPack { segment_id: String },

    #[error("table '{table}' has no registered schema")]
    SchemaMissing { table: String },

    #[error("query is invalid: {reason}")]
    InvalidQuery { reason: String },

    #[error("record does not match its schema: {0}")]
    Record(#[from] tracelake_core::RecordError),
}

/// Reads traces by merging the hot SlateDB store with compacted segments.
/// Cloning shares the hot store and segment store handles.
#[derive(Clone)]
pub struct TraceLakeReader {
    pub(crate) db: TraceLakeDb,
    pub(crate) store: Arc<dyn ObjectStore>,
    pub(crate) packs: TracePackWriter,
    pub(crate) base_path: String,
}

impl TraceLakeReader {
    /// Creates a reader over one hot store and one segment store. The
    /// `base_path` must match the compactor's, e.g. `tracelake/segments`.
    pub fn new(db: TraceLakeDb, store: Arc<dyn ObjectStore>, base_path: impl Into<String>) -> Self {
        Self {
            db,
            packs: TracePackWriter::new(Arc::clone(&store)),
            store,
            base_path: base_path.into(),
        }
    }

    /// Fetches one full trace by id.
    ///
    /// Every published segment for the (tenant, namespace) is checked: the
    /// segment's bloom filter skips packs that cannot hold the trace, and the
    /// pack is read on a possible hit. The spans are then merged with any
    /// newer copies still in the hot store — on a span_id conflict the hot
    /// span wins, and newer segments override older ones.
    ///
    /// Returns `None` when no source holds a single span of the trace.
    pub async fn get_trace(
        &self,
        tenant_id: &str,
        namespace: &str,
        trace_id: &str,
    ) -> Result<Option<Trace>, QueryError> {
        trace_prefix(tenant_id, namespace, trace_id)?;

        let manifest = read_manifest(&self.store, &self.base_path, tenant_id, namespace).await?;

        let mut layers: Vec<Vec<Span>> = Vec::new();
        if let Some(manifest) = manifest {
            for entry in &manifest.segments {
                if let Some(spans) = self.read_segment_trace(entry, trace_id).await? {
                    layers.push(spans);
                }
            }
        }

        layers.push(
            self.db
                .get_trace_spans(tenant_id, namespace, trace_id)
                .await?,
        );

        let spans = merge_spans(&layers);
        if spans.is_empty() {
            return Ok(None);
        }

        Ok(Some(Trace {
            tenant_id: tenant_id.to_string(),
            namespace: namespace.to_string(),
            trace_id: trace_id.to_string(),
            spans,
        }))
    }

    /// Reads one trace from a single segment: the bloom filter is checked
    /// first and the pack is only read on a possible hit. Segments without a
    /// bloom file skip straight to the pack read. Returns `None` when the
    /// segment cannot hold the trace.
    async fn read_segment_trace(
        &self,
        entry: &SegmentEntry,
        trace_id: &str,
    ) -> Result<Option<Vec<Span>>, QueryError> {
        if let Some(bloom_file) = segment_file(entry, SegmentFileKind::Bloom) {
            let bytes = self
                .store
                .get(&Path::from(bloom_file.path.as_str()))
                .await?
                .bytes()
                .await?;
            let bloom = BloomFilter::decode(&bytes)?;

            if !bloom.contains(trace_id.as_bytes()) {
                return Ok(None);
            }
        }

        let pack_file = segment_file(entry, SegmentFileKind::TracePack).ok_or(
            QueryError::MissingSegmentFile {
                segment_id: entry.segment_id.clone(),
                kind: SegmentFileKind::TracePack.extension(),
            },
        )?;

        Ok(self.packs.read_trace(&pack_file.path, trace_id).await?)
    }
}

/// Finds one file of a kind in a segment entry.
fn segment_file(entry: &SegmentEntry, kind: SegmentFileKind) -> Option<&SegmentFile> {
    entry.files.iter().find(|file| file.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use slatedb::object_store::memory::InMemory;
    use tracelake_compact::Compactor;
    use tracelake_core::{Span, SpanStatus, SpanType};

    const BASE_PATH: &str = "tracelake/segments";

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

    /// Compacts `spans` through a fresh hot store onto `store`, simulating a
    /// compactor run whose hot store is afterwards dropped.
    async fn compact_spans(store: &Arc<dyn ObjectStore>, spans: &[Span]) {
        let db = TraceLakeDb::open_in_memory("/compact-source")
            .await
            .expect("compaction source db should open");
        db.put_spans(spans).await.expect("spans should write");
        Compactor::new(Arc::clone(store), BASE_PATH)
            .compact(&db)
            .await
            .expect("compaction should succeed");
    }

    #[tokio::test]
    async fn get_trace_returns_none_when_no_source_holds_the_trace() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = TraceLakeDb::open_in_memory("/test-absent")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-absent")
            .await
            .expect("absent trace should not error");

        assert_eq!(fetched, None);
    }

    #[tokio::test]
    async fn get_trace_reads_hot_spans_before_compaction() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = TraceLakeDb::open_in_memory("/test-hot")
            .await
            .expect("db should open");

        let later = make_span("tenantid-1", "default", "trace-1", "span-b", 2);
        let earlier = make_span("tenantid-1", "default", "trace-1", "span-a", 1);
        let other_trace = make_span("tenantid-1", "default", "trace-2", "span-x", 1);

        db.put_spans(&[later, earlier, other_trace])
            .await
            .expect("spans should write");

        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);
        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-1")
            .await
            .expect("hot trace should fetch")
            .expect("trace should exist");

        assert_eq!(fetched.trace_id, "trace-1");
        assert_eq!(
            fetched
                .spans
                .iter()
                .map(|span| span.span_id.as_str())
                .collect::<Vec<_>>(),
            vec!["span-a", "span-b"]
        );
    }

    #[tokio::test]
    async fn get_trace_reads_compacted_segment_when_hot_store_is_empty() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let spans = vec![
            make_span("tenantid-1", "default", "trace-1", "span-a", 1),
            make_span("tenantid-1", "default", "trace-1", "span-b", 2),
        ];
        compact_spans(&store, &spans).await;

        // A hot store that no longer holds the compacted spans, as after
        // SlateDB drops the compacted SSTs.
        let db = TraceLakeDb::open_in_memory("/test-cold")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-1")
            .await
            .expect("cold trace should fetch")
            .expect("trace should exist");

        assert_eq!(fetched.spans, spans);
    }

    #[tokio::test]
    async fn get_trace_merges_compacted_and_hot_spans() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let compacted = vec![
            make_span("tenantid-1", "default", "trace-1", "span-a", 1),
            make_span("tenantid-1", "default", "trace-1", "span-b", 2),
        ];
        compact_spans(&store, &compacted).await;

        // Span written after the compaction, still only in the hot store.
        let hot = make_span("tenantid-1", "default", "trace-1", "span-c", 3);
        let db = TraceLakeDb::open_in_memory("/test-merge")
            .await
            .expect("db should open");
        db.put_spans(std::slice::from_ref(&hot))
            .await
            .expect("hot span should write");

        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);
        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-1")
            .await
            .expect("merged trace should fetch")
            .expect("trace should exist");

        let mut expected = compacted;
        expected.push(hot);
        assert_eq!(fetched.spans, expected);
    }

    #[tokio::test]
    async fn get_trace_prefers_hot_span_on_span_id_conflict() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let compacted = make_span("tenantid-1", "default", "trace-1", "span-a", 1);
        compact_spans(&store, std::slice::from_ref(&compacted)).await;

        // The same span re-written after compaction with a newer status.
        let mut hot = make_span("tenantid-1", "default", "trace-1", "span-a", 1);
        hot.status = SpanStatus::Error;
        hot.error = Some("retried and failed".to_string());

        let db = TraceLakeDb::open_in_memory("/test-conflict")
            .await
            .expect("db should open");
        db.put_spans(std::slice::from_ref(&hot))
            .await
            .expect("hot span should write");

        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);
        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-1")
            .await
            .expect("conflicting trace should fetch")
            .expect("trace should exist");

        assert_eq!(fetched.spans, vec![hot]);
    }

    #[tokio::test]
    async fn get_trace_bloom_negative_skips_the_pack_read() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let spans = vec![make_span("tenantid-1", "default", "trace-1", "span-a", 1)];
        compact_spans(&store, &spans).await;

        // Remove the pack: any read of it now fails, so a successful lookup
        // for an absent trace proves the bloom filter pruned the pack read.
        let manifest = read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        let pack_path = manifest.segments[0]
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::TracePack)
            .expect("pack file should exist")
            .path
            .clone();
        store
            .delete(&Path::from(pack_path.as_str()))
            .await
            .expect("pack should delete");

        let db = TraceLakeDb::open_in_memory("/test-bloom-skip")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let absent = reader
            .get_trace("tenantid-1", "default", "trace-absent")
            .await
            .expect("bloom-negative lookup must not read the pack");
        assert_eq!(absent, None);

        // The compacted trace itself is now unreadable: its pack is gone, and
        // the bloom filter points at it.
        let present = reader.get_trace("tenantid-1", "default", "trace-1").await;
        assert!(present.is_err(), "bloom-positive lookup must read the pack");
    }

    #[tokio::test]
    async fn get_trace_reads_the_same_trace_across_multiple_segments() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let first = vec![make_span("tenantid-1", "default", "trace-1", "span-a", 1)];
        compact_spans(&store, &first).await;

        let second = vec![make_span("tenantid-1", "default", "trace-1", "span-b", 2)];
        compact_spans(&store, &second).await;

        let manifest = read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        assert_eq!(manifest.segments.len(), 2);

        let db = TraceLakeDb::open_in_memory("/test-multi-segment")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-1")
            .await
            .expect("multi-segment trace should fetch")
            .expect("trace should exist");

        let mut expected = first;
        expected.extend(second);
        assert_eq!(fetched.spans, expected);
    }

    #[tokio::test]
    async fn get_trace_is_isolated_by_namespace() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let wanted = make_span("tenantid-1", "default", "trace-1", "span-a", 1);
        let other_namespace = make_span("tenantid-1", "other", "trace-1", "span-b", 2);
        compact_spans(&store, &[wanted.clone(), other_namespace]).await;

        let db = TraceLakeDb::open_in_memory("/test-namespace-isolation")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let fetched = reader
            .get_trace("tenantid-1", "default", "trace-1")
            .await
            .expect("namespaced trace should fetch")
            .expect("trace should exist");

        assert_eq!(fetched.spans, vec![wanted]);
    }

    #[tokio::test]
    async fn get_trace_rejects_invalid_lookup_arguments() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = TraceLakeDb::open_in_memory("/test-invalid-args")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let error = reader
            .get_trace("", "default", "trace-1")
            .await
            .expect_err("empty tenant_id should fail");

        assert!(matches!(
            error,
            QueryError::Key(KeyError::EmptySegment { field: "tenant_id" })
        ));
    }

    #[tokio::test]
    #[ignore = "compacts 10K spans and times fetches; run in release for meaningful numbers"]
    async fn bench_trace_fetch_latency_hot_and_cold() {
        const TRACE_COUNT: usize = 100;
        const SPANS_PER_TRACE: usize = 100;
        const SPAN_COUNT: usize = TRACE_COUNT * SPANS_PER_TRACE;

        fn bench_span(trace_num: usize, span_num: usize) -> Span {
            let mut span = make_span(
                "tenantid-1",
                "default",
                &format!("trace-{trace_num:03}"),
                &format!("span-{span_num:03}"),
                0,
            );
            span.start_time = Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 0).unwrap()
                + chrono::Duration::nanoseconds((trace_num * 1_000 + span_num) as i64);
            span.input = Some(json!({
                "prompt": "The user asks the support agent to explain the refund policy "
                    .repeat(6)
            }));
            span.output = Some(json!({
                "text": "The agent explains that refunds post within five business days "
                    .repeat(6)
            }));
            span
        }

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // Hot phase: all spans live in SlateDB, no segments exist yet (every
        // fetch reads a manifest that 404s, then scans the hot prefix).
        let spans: Vec<Span> = (0..TRACE_COUNT)
            .flat_map(|trace_num| {
                (0..SPANS_PER_TRACE).map(move |span_num| bench_span(trace_num, span_num))
            })
            .collect();

        let hot_db = TraceLakeDb::open_in_memory("/bench-hot")
            .await
            .expect("hot db should open");
        hot_db.put_spans(&spans).await.expect("spans should write");

        let hot_reader = TraceLakeReader::new(hot_db, Arc::clone(&store), BASE_PATH);
        let hot_started = std::time::Instant::now();
        for trace_num in 0..TRACE_COUNT {
            let fetched = hot_reader
                .get_trace("tenantid-1", "default", &format!("trace-{trace_num:03}"))
                .await
                .expect("hot trace should fetch")
                .expect("trace should exist");
            assert_eq!(fetched.spans.len(), SPANS_PER_TRACE);
        }
        let hot_elapsed = hot_started.elapsed();
        let hot_avg_us = hot_elapsed.as_micros() as f64 / TRACE_COUNT as f64;

        // Cold phase: compact a fresh copy, then read through a hot store that
        // no longer holds the spans (compacted SSTs dropped).
        compact_spans(&store, &spans).await;

        let cold_db = TraceLakeDb::open_in_memory("/bench-cold")
            .await
            .expect("cold db should open");
        let cold_reader = TraceLakeReader::new(cold_db, Arc::clone(&store), BASE_PATH);

        let cold_first_started = std::time::Instant::now();
        let first = cold_reader
            .get_trace("tenantid-1", "default", "trace-042")
            .await
            .expect("cold trace should fetch")
            .expect("trace should exist");
        let cold_first_elapsed = cold_first_started.elapsed();
        assert_eq!(first.spans.len(), SPANS_PER_TRACE);

        let cold_repeat_started = std::time::Instant::now();
        for trace_num in 0..TRACE_COUNT {
            let fetched = cold_reader
                .get_trace("tenantid-1", "default", &format!("trace-{trace_num:03}"))
                .await
                .expect("cold trace should fetch")
                .expect("trace should exist");
            assert_eq!(fetched.spans.len(), SPANS_PER_TRACE);
        }
        let cold_repeat_elapsed = cold_repeat_started.elapsed();
        let cold_repeat_avg_us = cold_repeat_elapsed.as_micros() as f64 / TRACE_COUNT as f64;

        // Bloom-negative probe: manifest GET + bloom GET, no pack read.
        let probe_started = std::time::Instant::now();
        for probe in 0..TRACE_COUNT {
            let absent = cold_reader
                .get_trace("tenantid-1", "default", &format!("absent-{probe:03}"))
                .await
                .expect("absent trace should not error");
            assert_eq!(absent, None);
        }
        let probe_elapsed = probe_started.elapsed();
        let probe_avg_us = probe_elapsed.as_micros() as f64 / TRACE_COUNT as f64;

        println!(
            "trace fetch bench: {TRACE_COUNT} traces x {SPANS_PER_TRACE} spans \
             (~{} B/span), in-memory object store, single thread",
            spans
                .iter()
                .map(|span| serde_json::to_vec(span).unwrap().len())
                .sum::<usize>()
                / SPAN_COUNT
        );
        println!(
            "hot={hot_avg_us:.0}us/trace (memtable scan) \
             cold_first={cold_first_elapsed:?} \
             cold_repeat={cold_repeat_avg_us:.0}us/trace (manifest+bloom+pack GET) \
             bloom_negative={probe_avg_us:.0}us/trace (no pack read)"
        );
    }
}
