use crate::bloom_writer::build_record_bloom;
use crate::compactor::CompactError;
use crate::manifest_writer::publish_manifest;
use crate::record_stats_writer::build_record_stats;
use crate::schema_store::{put_schema, read_schema};
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::{
    Record, SegmentEntry, SegmentFile, SegmentFileKind, SegmentManifest, TableSchema,
    encode_record_pack,
};
use tracelake_ingest::TraceLakeDb;

/// Every hot record for one (tenant, namespace, table), with the schema they
/// must be read back under.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordGroup {
    pub tenant_id: String,
    pub namespace: String,
    pub table: String,
    pub schema: TableSchema,
    pub records: Vec<Record>,
}

/// Groups hot records by table and loads each table's registered schema.
pub async fn collect_record_groups(
    db: &TraceLakeDb,
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
) -> Result<Vec<RecordGroup>, CompactError> {
    let mut groups: Vec<RecordGroup> = Vec::new();

    for stored in db.scan_all_records().await? {
        let same_group = groups.last().is_some_and(|group| {
            group.tenant_id == stored.tenant_id
                && group.namespace == stored.namespace
                && group.table == stored.table
        });

        if !same_group {
            let schema = read_schema(
                store,
                base_path,
                &stored.tenant_id,
                &stored.namespace,
                &stored.table,
            )
            .await?
            .ok_or_else(|| CompactError::SchemaNotFound {
                table: stored.table.clone(),
            })?;

            groups.push(RecordGroup {
                tenant_id: stored.tenant_id.clone(),
                namespace: stored.namespace.clone(),
                table: stored.table.clone(),
                schema,
                records: Vec::new(),
            });
        }

        if let Some(group) = groups.last_mut() {
            group.records.push(stored.record);
        }
    }

    Ok(groups)
}

/// Writes one immutable record segment per table: a typed pack, a bloom filter
/// over record ids, and per-field stats, then publishes the table manifest.
pub struct RecordCompactor {
    store: Arc<dyn ObjectStore>,
    base_path: String,
}

impl RecordCompactor {
    pub fn new(store: Arc<dyn ObjectStore>, base_path: impl Into<String>) -> Self {
        Self {
            store,
            base_path: base_path.into(),
        }
    }

    pub async fn compact(&self, db: &TraceLakeDb) -> Result<Vec<SegmentEntry>, CompactError> {
        let groups = collect_record_groups(db, &self.store, &self.base_path).await?;

        let mut written = Vec::with_capacity(groups.len());
        for group in &groups {
            written.push(self.compact_group(group).await?);
        }

        Ok(written)
    }

    async fn compact_group(&self, group: &RecordGroup) -> Result<SegmentEntry, CompactError> {
        put_schema(
            &self.store,
            &self.base_path,
            &group.tenant_id,
            &group.namespace,
            &group.schema,
        )
        .await?;

        let mut manifest = crate::manifest_writer::read_record_manifest(
            &self.store,
            &self.base_path,
            &group.tenant_id,
            &group.namespace,
            &group.table,
        )
        .await?
        .unwrap_or_else(|| {
            SegmentManifest::new_table(&group.tenant_id, &group.namespace, &group.table)
        });

        let segment_id = tracelake_core::segment_id(manifest.next_segment_number());
        let pack_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::RecordPack,
                encode_record_pack(&group.schema, &group.records)?,
            )
            .await?;
        let bloom_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Bloom,
                build_record_bloom(&group.schema, &group.records)?,
            )
            .await?;

        let record_stats = build_record_stats(&group.schema, &group.records)?;
        let stats_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Stats,
                serde_json::to_vec(&record_stats)?,
            )
            .await?;

        let entry = SegmentEntry {
            segment_id: segment_id.clone(),
            trace_count: 0,
            span_count: 0,
            files: vec![pack_file, bloom_file, stats_file],
            stats: tracelake_core::SegmentStats::default(),
            record_stats: Some(record_stats),
        };

        manifest.segments.push(entry.clone());
        publish_manifest(&self.store, &self.base_path, &manifest).await?;

        Ok(entry)
    }

    async fn put_file(
        &self,
        group: &RecordGroup,
        segment_id: &str,
        kind: SegmentFileKind,
        bytes: Vec<u8>,
    ) -> Result<SegmentFile, CompactError> {
        let path = format!(
            "{}/{}/{}/tables/{}/{segment_id}.{}",
            self.base_path,
            group.tenant_id,
            group.namespace,
            group.table,
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
