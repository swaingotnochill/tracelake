use crate::bloom_writer::build_segment_bloom;
use crate::manifest_writer::{publish_manifest, read_manifest};
use crate::stats_writer::build_segment_stats;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use thiserror::Error;
use tracelake_core::{
    SegmentEntry, SegmentFile, SegmentFileKind, SegmentManifest, Span, Trace, encode_trace_pack,
};
use tracelake_ingest::TraceLakeDb;

/// Errors returned by the compaction pipeline.
#[derive(Debug, Error)]
pub enum CompactError {
    #[error("hot store scan failed: {0}")]
    Scan(#[from] tracelake_ingest::IngestError),

    #[error("trace pack encode failed: {0}")]
    Pack(#[from] tracelake_core::TracePackError),

    #[error("record pack failed: {0}")]
    RecordPack(#[from] tracelake_core::RecordPackError),

    #[error("record does not match its schema: {0}")]
    Record(#[from] tracelake_core::RecordError),

    #[error("schema shape is invalid: {0}")]
    Schema(#[from] tracelake_core::SchemaError),

    #[error("schema cannot evolve: {0}")]
    Evolution(#[from] tracelake_core::EvolutionError),

    #[error("text index failed: {0}")]
    Text(#[from] crate::text_indexer::TextIndexError),

    #[error("table '{table}' has no registered schema")]
    SchemaNotFound { table: String },

    #[error("segment {segment_id} lists no record pack")]
    MissingRecordPack { segment_id: String },

    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),

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
    Ok(group_spans(spans))
}

/// Groups spans, already in storage key order, into one [`TraceGroup`] per
/// (tenant, namespace) run and one [`Trace`] per trace_id run inside it.
///
/// The grouping walks the input once and relies on the key order: every
/// change of tenant, namespace, or trace_id starts a new group or trace.
pub fn group_spans(spans: Vec<Span>) -> Vec<TraceGroup> {
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

    groups
}

/// Compacts the hot store into segments on an object store.
///
/// Each compaction writes one segment per (tenant, namespace): a trace pack,
/// a bloom filter, and stats, then publishes the manifest with a single PUT.
/// Step 4 assumes one compactor per namespace; concurrent compactors would
/// need manifest coordination.
pub struct Compactor {
    store: Arc<dyn ObjectStore>,
    base_path: String,
}

impl Compactor {
    /// Creates a compactor writing under one base path, e.g.
    /// `tracelake/segments`.
    pub fn new(store: Arc<dyn ObjectStore>, base_path: impl Into<String>) -> Self {
        Self {
            store,
            base_path: base_path.into(),
        }
    }

    /// Compacts every span in the hot store, returning the manifest entries
    /// written, one segment per (tenant, namespace).
    pub async fn compact(&self, db: &TraceLakeDb) -> Result<Vec<SegmentEntry>, CompactError> {
        let groups = collect_traces(db).await?;

        let mut written = Vec::with_capacity(groups.len());
        for group in &groups {
            written.push(self.compact_group(group).await?);
        }

        Ok(written)
    }

    async fn compact_group(&self, group: &TraceGroup) -> Result<SegmentEntry, CompactError> {
        let mut manifest = read_manifest(
            &self.store,
            &self.base_path,
            &group.tenant_id,
            &group.namespace,
        )
        .await?
        .unwrap_or_else(|| SegmentManifest::new(&group.tenant_id, &group.namespace));

        let segment_id = tracelake_core::segment_id(manifest.next_segment_number());

        let pack_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::TracePack,
                encode_trace_pack(&group.traces)?,
            )
            .await?;

        let bloom_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Bloom,
                build_segment_bloom(group),
            )
            .await?;

        let text_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Text,
                crate::text_indexer::build_text_index(&group.traces)?,
            )
            .await?;

        let stats = build_segment_stats(group);
        let stats_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Stats,
                serde_json::to_vec(&stats)?,
            )
            .await?;

        let entry = SegmentEntry {
            segment_id: segment_id.clone(),
            trace_count: stats.trace_count,
            span_count: stats.span_count,
            files: vec![pack_file, bloom_file, stats_file, text_file],
            stats,
            record_stats: None,
        };

        manifest.segments.push(entry.clone());
        publish_manifest(&self.store, &self.base_path, &manifest).await?;

        Ok(entry)
    }

    /// Writes one segment file and returns its manifest record.
    async fn put_file(
        &self,
        group: &TraceGroup,
        segment_id: &str,
        kind: SegmentFileKind,
        bytes: Vec<u8>,
    ) -> Result<SegmentFile, CompactError> {
        let path = format!(
            "{}/{}/{}/{segment_id}.{}",
            self.base_path,
            group.tenant_id,
            group.namespace,
            kind.extension()
        );
        let crc32 = crc32fast::hash(&bytes);
        let size_bytes = bytes.len() as u64;

        self.store
            .put(&Path::from(path.as_str()), bytes.into())
            .await?;

        Ok(SegmentFile {
            kind,
            path,
            size_bytes,
            crc32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use tracelake_core::{Span, SpanStatus, SpanType};
    use tracelake_ingest::TraceLakeDb;

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

    /// (tenant, namespace, [(trace_id, [span_id])]) per group.
    type GroupsSummary<'a> = Vec<(&'a str, &'a str, Vec<(&'a str, Vec<&'a str>)>)>;

    fn summarize(groups: &[TraceGroup]) -> GroupsSummary<'_> {
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
                + chrono::Duration::nanoseconds((trace_num * 1_000 + span_num) as i64),
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
            cost_usd: Some(0.001),
            model: Some("gpt-4o".to_string()),
            provider: None,
            error: None,
            tags: vec!["e2e".to_string()],
            scores: Vec::new(),
        }
    }

    async fn store_get(store: &Arc<dyn ObjectStore>, path: &str) -> Vec<u8> {
        store
            .get(&Path::from(path))
            .await
            .expect("file should exist")
            .bytes()
            .await
            .expect("file should read")
            .to_vec()
    }

    #[tokio::test]
    async fn ten_k_span_segment_publishes_pack_bloom_stats_and_manifest() {
        const TRACE_COUNT: usize = 100;
        const SPANS_PER_TRACE: usize = 100;
        const SPAN_COUNT: usize = TRACE_COUNT * SPANS_PER_TRACE;

        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let db = TraceLakeDb::open_in_memory("/test-step4-e2e")
            .await
            .expect("db should open");

        let spans: Vec<Span> = (0..TRACE_COUNT)
            .flat_map(|trace_num| {
                (0..SPANS_PER_TRACE).map(move |span_num| scale_span(trace_num, span_num))
            })
            .collect();
        db.put_spans(&spans).await.expect("10k spans should write");

        let compact_started = std::time::Instant::now();
        let compactor = Compactor::new(Arc::clone(&store), BASE_PATH);
        let entries = compactor
            .compact(&db)
            .await
            .expect("compaction should succeed");
        let compact_elapsed = compact_started.elapsed();

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.segment_id, "segment-000001");
        assert_eq!(entry.trace_count, TRACE_COUNT as u64);
        assert_eq!(entry.span_count, SPAN_COUNT as u64);
        assert_eq!(entry.files.len(), 4);

        let kinds: Vec<_> = entry
            .files
            .iter()
            .map(|file| (file.kind, file.path.as_str()))
            .collect();
        for (kind, path) in &kinds {
            let expected = format!(
                "{BASE_PATH}/tenantid-1/default/segment-000001.{}",
                kind.extension()
            );
            assert_eq!(*path, expected.as_str());
        }

        // Manifest reread from the store matches what compaction published.
        let manifest = read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        assert_eq!(manifest.segments, entries);

        // Bloom: every real trace id is contained, absent ids are almost
        // always rejected (Step 4 verification requirement).
        let bloom_path = entry
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::Bloom)
            .expect("bloom file should exist");
        let bloom = tracelake_core::BloomFilter::decode(&store_get(&store, &bloom_path.path).await)
            .expect("bloom should decode");

        for trace_num in 0..TRACE_COUNT {
            let trace_id = format!("trace-{trace_num:03}");
            assert!(bloom.contains(trace_id.as_bytes()), "{trace_id} missing");
        }
        assert!(bloom.contains(b"span-042"));
        assert!(bloom.contains(b"e2e"));

        let false_positives = (0..1_000)
            .filter(|probe| bloom.contains(format!("absent-{probe:04}").as_bytes()))
            .count();
        assert!(
            false_positives <= 50,
            "bloom false positive rate too high: {false_positives}/1000"
        );

        // Stored pack bytes match the manifest's size and checksum.
        let pack_file = entry
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::TracePack)
            .expect("pack file should exist");
        let stored = store_get(&store, &pack_file.path).await;
        assert_eq!(stored.len() as u64, pack_file.size_bytes);
        assert_eq!(crc32fast::hash(&stored), pack_file.crc32);

        // Stats ranges are exact.
        assert_eq!(entry.stats.span_count, SPAN_COUNT as u64);
        assert_eq!(entry.stats.start_time_min, Some(spans[0].start_time));
        assert_eq!(
            entry.stats.start_time_max,
            Some(spans[SPAN_COUNT - 1].start_time)
        );
        assert_eq!(entry.stats.cost_usd_min, Some(0.001));
        assert_eq!(entry.stats.cost_usd_max, Some(0.001));

        // Traces read back exactly from the published pack.
        let reader = crate::TracePackWriter::new(Arc::clone(&store));
        let trace_spans = reader
            .read_trace(&pack_file.path, "trace-042")
            .await
            .expect("trace should read")
            .expect("trace should exist");
        let expected: Vec<&Span> = spans
            .iter()
            .filter(|span| span.trace_id == "trace-042")
            .collect();
        assert_eq!(trace_spans.len(), expected.len());
        for (index, span) in trace_spans.iter().enumerate() {
            assert_eq!(span, expected[index]);
        }

        let spans_per_second = SPAN_COUNT as f64 / compact_elapsed.as_secs_f64();
        println!("10K segment check: compact={compact_elapsed:?} ({spans_per_second:.0} spans/s)");
    }

    #[tokio::test]
    async fn second_compaction_appends_new_segment_to_manifest() {
        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let compactor = Compactor::new(Arc::clone(&store), BASE_PATH);

        let first_db = TraceLakeDb::open_in_memory("/test-step4-first")
            .await
            .expect("db should open");
        let first_span = scale_span(0, 0);
        first_db
            .put_spans(std::slice::from_ref(&first_span))
            .await
            .expect("span should write");

        let first_entries = compactor
            .compact(&first_db)
            .await
            .expect("first compaction should succeed");
        assert_eq!(first_entries.len(), 1);
        assert_eq!(first_entries[0].segment_id, "segment-000001");

        let second_db = TraceLakeDb::open_in_memory("/test-step4-second")
            .await
            .expect("db should open");
        let mut second_span = scale_span(500, 0);
        second_span.trace_id = "trace-500".to_string();
        second_db
            .put_spans(&[second_span.clone()])
            .await
            .expect("span should write");

        let second_entries = compactor
            .compact(&second_db)
            .await
            .expect("second compaction should succeed");
        assert_eq!(second_entries.len(), 1);
        assert_eq!(second_entries[0].segment_id, "segment-000002");

        let manifest = read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        assert_eq!(manifest.segments.len(), 2);
        assert_eq!(manifest.next_segment_number(), 3);

        // Each pack holds only its own compaction batch.
        let reader = crate::TracePackWriter::new(Arc::clone(&store));
        let first_pack = &manifest.segments[0].files[0];
        let second_pack = &manifest.segments[1].files[0];
        assert_eq!(
            reader
                .read_trace(&first_pack.path, "trace-500")
                .await
                .expect("lookup should succeed"),
            None
        );
        assert_eq!(
            reader
                .read_trace(&second_pack.path, "trace-000")
                .await
                .expect("lookup should succeed"),
            None
        );
    }
}
