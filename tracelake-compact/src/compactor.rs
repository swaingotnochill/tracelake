use crate::bloom_writer::build_segment_bloom;
use crate::manifest_writer::{publish_manifest, read_manifest};
use crate::stats_writer::build_segment_stats;
use slatedb::object_store::{ObjectStore, path::Path};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tracelake_core::{
    BloomFilter, SegmentEntry, SegmentFile, SegmentFileKind, SegmentManifest, Span, Tombstone,
    TombstoneKey, Trace, decode_tombstone_key, decode_trace_pack, encode_span_storage_key,
    encode_trace_pack,
};
use tracelake_ingest::TraceLakeDb;

/// Errors returned by the compaction pipeline.
#[derive(Debug, Error)]
pub enum CompactError {
    #[error("identifier is not path safe: {0}")]
    Key(#[from] tracelake_core::KeyError),

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

    #[error("column file failed: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),

    #[error("bloom filter failed: {0}")]
    Bloom(#[from] tracelake_core::BloomError),

    #[error("parquet failed: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),

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

/// Hot trace tombstones grouped by (tenant, namespace), with storage keys.
type NamespaceTombstones = HashMap<(String, String), Vec<(Vec<u8>, Tombstone)>>;

fn tombstone_values(tombstones: &[(Vec<u8>, Tombstone)]) -> Vec<Tombstone> {
    tombstones
        .iter()
        .map(|(_, tombstone)| tombstone.clone())
        .collect()
}

fn tombstone_keys(tombstones: &[(Vec<u8>, Tombstone)]) -> Vec<Vec<u8>> {
    tombstones.iter().map(|(key, _)| key.clone()).collect()
}

fn deleted_ids(tombstones: &NamespaceTombstones) -> HashMap<(String, String), HashSet<String>> {
    tombstones
        .iter()
        .map(|(namespace, values)| {
            (
                namespace.clone(),
                values
                    .iter()
                    .map(|(_, tombstone)| tombstone.id.clone())
                    .collect(),
            )
        })
        .collect()
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

    /// Compacts every span written since the last compaction, applies
    /// tombstones, and returns the segments written, one per namespace.
    pub async fn compact(&self, db: &TraceLakeDb) -> Result<Vec<SegmentEntry>, CompactError> {
        let tombstones = self.collect_tombstones(db).await?;
        let deleted = deleted_ids(&tombstones);
        let (groups, watermarks) = self.collect_pending(db, &deleted).await?;

        let mut written = Vec::with_capacity(groups.len());
        let mut handled: HashSet<(String, String)> = HashSet::new();

        for group in &groups {
            let namespace = (group.tenant_id.clone(), group.namespace.clone());
            let watermark = watermarks.get(&namespace).map(String::as_str);
            let namespace_tombstones = tombstones
                .get(&namespace)
                .map(Vec::as_slice)
                .unwrap_or_default();
            written.push(
                self.compact_group(db, group, watermark, namespace_tombstones)
                    .await?,
            );
            handled.insert(namespace);
        }

        for (namespace, namespace_tombstones) in &tombstones {
            if handled.contains(namespace) {
                continue;
            }
            self.persist_tombstones(db, namespace, namespace_tombstones)
                .await?;
        }

        Ok(written)
    }

    /// Hot trace tombstones grouped by namespace, with their storage keys.
    async fn collect_tombstones(
        &self,
        db: &TraceLakeDb,
    ) -> Result<NamespaceTombstones, CompactError> {
        let mut tombstones: NamespaceTombstones = HashMap::new();

        for (key, tombstone) in db.scan_all_tombstones().await? {
            if let TombstoneKey::Trace {
                tenant_id,
                namespace,
                ..
            } = decode_tombstone_key(&key)?
            {
                tombstones
                    .entry((tenant_id, namespace))
                    .or_default()
                    .push((key, tombstone));
            }
        }

        Ok(tombstones)
    }

    /// Reads spans above each namespace's watermark and below no tombstone, and
    /// groups the survivors. The last key kept per namespace becomes the next
    /// watermark.
    async fn collect_pending(
        &self,
        db: &TraceLakeDb,
        deleted: &HashMap<(String, String), HashSet<String>>,
    ) -> Result<(Vec<TraceGroup>, HashMap<(String, String), String>), CompactError> {
        let mut known: HashMap<(String, String), Option<String>> = HashMap::new();
        let mut last: HashMap<(String, String), String> = HashMap::new();
        let mut kept = Vec::new();

        for (key, span) in db.scan_all_spans_with_keys().await? {
            let namespace = (span.tenant_id.clone(), span.namespace.clone());

            if deleted
                .get(&namespace)
                .is_some_and(|ids| ids.contains(&span.trace_id))
            {
                continue;
            }

            if !known.contains_key(&namespace) {
                let manifest = read_manifest(
                    &self.store,
                    &self.base_path,
                    &span.tenant_id,
                    &span.namespace,
                )
                .await?;
                known.insert(
                    namespace.clone(),
                    manifest.and_then(|manifest| manifest.watermark),
                );
            }

            if let Some(watermark) = known.get(&namespace).and_then(Option::as_deref)
                && key.as_str() <= watermark
            {
                continue;
            }

            last.insert(namespace, key);
            kept.push(span);
        }

        Ok((group_spans(kept), last))
    }

    /// Persists tombstones for a namespace with no new spans, then removes the
    /// hot tombstone keys.
    async fn persist_tombstones(
        &self,
        db: &TraceLakeDb,
        namespace: &(String, String),
        tombstones: &[(Vec<u8>, Tombstone)],
    ) -> Result<(), CompactError> {
        let (tenant_id, namespace_name) = namespace;
        let mut manifest = read_manifest(&self.store, &self.base_path, tenant_id, namespace_name)
            .await?
            .unwrap_or_else(|| SegmentManifest::new(tenant_id, namespace_name));

        manifest.apply_tombstones(&tombstone_values(tombstones));
        publish_manifest(&self.store, &self.base_path, &manifest).await?;

        db.delete_keys(&tombstone_keys(tombstones)).await?;
        self.rewrite_segments(tenant_id, namespace_name).await?;
        Ok(())
    }

    async fn compact_group(
        &self,
        db: &TraceLakeDb,
        group: &TraceGroup,
        watermark: Option<&str>,
        tombstones: &[(Vec<u8>, Tombstone)],
    ) -> Result<SegmentEntry, CompactError> {
        let mut manifest = read_manifest(
            &self.store,
            &self.base_path,
            &group.tenant_id,
            &group.namespace,
        )
        .await?
        .unwrap_or_else(|| SegmentManifest::new(&group.tenant_id, &group.namespace));

        let segment_id = tracelake_core::segment_id(manifest.next_segment_number());
        let entry = self.write_segment_files(group, &segment_id).await?;

        manifest.segments.push(entry.clone());
        manifest.watermark = watermark.map(str::to_string);
        manifest.apply_tombstones(&tombstone_values(tombstones));
        publish_manifest(&self.store, &self.base_path, &manifest).await?;

        let keys = group
            .traces
            .iter()
            .flat_map(|trace| trace.spans.iter())
            .map(encode_span_storage_key)
            .collect::<Result<Vec<_>, _>>()?;
        db.delete_keys(&keys).await?;
        db.delete_keys(&tombstone_keys(tombstones)).await?;

        self.rewrite_segments(&group.tenant_id, &group.namespace)
            .await?;

        Ok(entry)
    }

    /// Writes every file of one segment and returns its manifest entry.
    async fn write_segment_files(
        &self,
        group: &TraceGroup,
        segment_id: &str,
    ) -> Result<SegmentEntry, CompactError> {
        let pack_file = self
            .put_file(
                group,
                segment_id,
                SegmentFileKind::TracePack,
                encode_trace_pack(&group.traces)?,
            )
            .await?;

        let bloom_file = self
            .put_file(
                group,
                segment_id,
                SegmentFileKind::Bloom,
                build_segment_bloom(group),
            )
            .await?;

        let text_file = self
            .put_file(
                group,
                segment_id,
                SegmentFileKind::Text,
                crate::text_indexer::build_text_index(&group.traces)?,
            )
            .await?;

        let spans: Vec<Span> = group
            .traces
            .iter()
            .flat_map(|trace| trace.spans.iter().cloned())
            .collect();
        let columns_file = self
            .put_file(
                group,
                segment_id,
                SegmentFileKind::Columns,
                crate::column_writer::encode_parquet(&crate::column_writer::build_span_batch(
                    &spans,
                )?)?,
            )
            .await?;

        let stats = build_segment_stats(group);
        let stats_file = self
            .put_file(
                group,
                segment_id,
                SegmentFileKind::Stats,
                serde_json::to_vec(&stats)?,
            )
            .await?;

        Ok(SegmentEntry {
            segment_id: segment_id.to_string(),
            trace_count: stats.trace_count,
            span_count: stats.span_count,
            files: vec![pack_file, bloom_file, stats_file, text_file, columns_file],
            stats,
            record_stats: None,
        })
    }

    /// Rewrites segments that still hold tombstoned traces, then deletes the
    /// old files. A segment that becomes empty is removed from the manifest.
    async fn rewrite_segments(&self, tenant_id: &str, namespace: &str) -> Result<(), CompactError> {
        let Some(mut manifest) =
            read_manifest(&self.store, &self.base_path, tenant_id, namespace).await?
        else {
            return Ok(());
        };
        if manifest.deleted.is_empty() {
            return Ok(());
        }

        let deleted: HashSet<String> = manifest
            .deleted
            .iter()
            .map(|tombstone| tombstone.id.clone())
            .collect();
        let mut next_segment = manifest.next_segment_number();
        let mut segments = Vec::with_capacity(manifest.segments.len());
        let mut removed_files = Vec::new();
        let mut changed = false;

        for entry in &manifest.segments {
            if !self.segment_may_hold(entry, &deleted).await? {
                segments.push(entry.clone());
                continue;
            }

            let traces = self.read_pack_traces(entry, tenant_id, namespace).await?;
            let kept: Vec<Trace> = traces
                .into_iter()
                .filter(|trace| !deleted.contains(&trace.trace_id))
                .collect();

            if kept.is_empty() {
                removed_files.extend(entry.files.iter().map(|file| file.path.clone()));
                changed = true;
                continue;
            }

            let segment_id = tracelake_core::segment_id(next_segment);
            next_segment += 1;
            let group = TraceGroup {
                tenant_id: tenant_id.to_string(),
                namespace: namespace.to_string(),
                traces: kept,
            };
            let replacement = self.write_segment_files(&group, &segment_id).await?;
            removed_files.extend(entry.files.iter().map(|file| file.path.clone()));
            segments.push(replacement);
            changed = true;
        }

        if !changed {
            return Ok(());
        }

        manifest.segments = segments;
        publish_manifest(&self.store, &self.base_path, &manifest).await?;
        self.delete_files(&removed_files).await?;

        Ok(())
    }

    /// Whether a segment's bloom filter names any tombstoned trace. A segment
    /// without a bloom file is always rewritten.
    async fn segment_may_hold(
        &self,
        entry: &SegmentEntry,
        deleted: &HashSet<String>,
    ) -> Result<bool, CompactError> {
        let Some(bloom_file) = entry
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::Bloom)
        else {
            return Ok(true);
        };

        let bytes = self
            .store
            .get(&Path::from(bloom_file.path.as_str()))
            .await?
            .bytes()
            .await?;
        let bloom = BloomFilter::decode(&bytes)?;

        Ok(deleted
            .iter()
            .any(|trace_id| bloom.contains(trace_id.as_bytes())))
    }

    async fn read_pack_traces(
        &self,
        entry: &SegmentEntry,
        tenant_id: &str,
        namespace: &str,
    ) -> Result<Vec<Trace>, CompactError> {
        let pack_file = entry
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::TracePack)
            .ok_or_else(|| CompactError::MissingRecordPack {
                segment_id: entry.segment_id.clone(),
            })?;
        let bytes = self
            .store
            .get(&Path::from(pack_file.path.as_str()))
            .await?
            .bytes()
            .await?;
        let pack = decode_trace_pack(&bytes)?;

        pack.trace_ids()?
            .into_iter()
            .map(|trace_id| {
                let spans = pack.read_trace(&trace_id)?.unwrap_or_default();
                Ok(Trace {
                    tenant_id: tenant_id.to_string(),
                    namespace: namespace.to_string(),
                    trace_id,
                    spans,
                })
            })
            .collect()
    }

    async fn delete_files(&self, paths: &[String]) -> Result<(), CompactError> {
        for path in paths {
            match self.store.delete(&Path::from(path.as_str())).await {
                Ok(()) => {}
                Err(slatedb::object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }

        Ok(())
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

        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let (groups, _) = Compactor::new(store, BASE_PATH)
            .collect_pending(&db, &HashMap::new())
            .await
            .expect("traces should collect");

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

        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let (groups, _) = Compactor::new(store, BASE_PATH)
            .collect_pending(&db, &HashMap::new())
            .await
            .expect("traces should collect");

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
        assert_eq!(entry.files.len(), 5);

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
    async fn a_second_compaction_without_new_spans_writes_nothing() {
        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let db = TraceLakeDb::open_in_memory("/test-watermark")
            .await
            .expect("db should open");
        db.put_spans(&[scale_span(0, 0), scale_span(1, 0)])
            .await
            .expect("spans should write");

        let compactor = Compactor::new(Arc::clone(&store), BASE_PATH);
        let first = compactor
            .compact(&db)
            .await
            .expect("compaction should succeed");
        assert_eq!(first.len(), 1);

        let manifest = read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        let watermark = manifest.watermark.expect("watermark is set");
        assert!(watermark.contains("trace-001"));
        assert!(watermark.ends_with("span-000"));

        assert!(
            db.scan_all_spans().await.expect("scan").is_empty(),
            "compacted hot keys are dropped"
        );

        let second = compactor
            .compact(&db)
            .await
            .expect("second compaction should succeed");
        assert!(second.is_empty(), "nothing new to compact");

        let manifest = read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        assert_eq!(manifest.watermark, Some(watermark));
        assert_eq!(manifest.segments.len(), 1);
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
