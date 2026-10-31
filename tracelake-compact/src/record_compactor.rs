use crate::bloom_writer::build_record_bloom;
use crate::compactor::CompactError;
use crate::manifest_writer::{publish_manifest, read_record_manifest};
use crate::record_stats_writer::build_record_stats;
use crate::schema_store::{put_schema, read_schema};
use slatedb::object_store::{ObjectStore, path::Path};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracelake_core::{
    Record, SegmentEntry, SegmentFile, SegmentFileKind, SegmentManifest, TableSchema, Tombstone,
    TombstoneKey, decode_record_key, decode_tombstone_key, encode_record_key, encode_record_pack,
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

/// Groups hot records by table, skipping keys at or below each table's
/// watermark, and returns the last kept key per group.
pub async fn collect_record_groups(
    db: &TraceLakeDb,
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    deleted: &HashMap<(String, String, String), HashSet<String>>,
) -> Result<Vec<(RecordGroup, String)>, CompactError> {
    let mut groups: Vec<(RecordGroup, String)> = Vec::new();
    let mut known: HashMap<(String, String, String), Option<String>> = HashMap::new();

    for stored in db.scan_all_records().await? {
        let location = (
            stored.tenant_id.clone(),
            stored.namespace.clone(),
            stored.table.clone(),
        );

        let record_id = decode_record_key(stored.key.as_bytes())?.record_id;
        if deleted
            .get(&location)
            .is_some_and(|ids| ids.contains(&record_id))
        {
            continue;
        }

        if !known.contains_key(&location) {
            let manifest = read_record_manifest(
                store,
                base_path,
                &stored.tenant_id,
                &stored.namespace,
                &stored.table,
            )
            .await?;
            known.insert(
                location.clone(),
                manifest.and_then(|manifest| manifest.watermark),
            );
        }

        if let Some(watermark) = known.get(&location).and_then(Option::as_deref)
            && stored.key.as_str() <= watermark
        {
            continue;
        }

        let same_group = groups.last().is_some_and(|(group, _)| {
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

            groups.push((
                RecordGroup {
                    tenant_id: stored.tenant_id.clone(),
                    namespace: stored.namespace.clone(),
                    table: stored.table.clone(),
                    schema,
                    records: Vec::new(),
                },
                stored.key.clone(),
            ));
        }

        if let Some((group, last)) = groups.last_mut() {
            group.records.push(stored.record);
            *last = stored.key;
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
        let tombstones = self.collect_tombstones(db).await?;
        let deleted = deleted_ids(&tombstones);
        let groups = collect_record_groups(db, &self.store, &self.base_path, &deleted).await?;

        let mut written = Vec::with_capacity(groups.len());
        let mut handled: HashSet<(String, String, String)> = HashSet::new();

        for (group, watermark) in &groups {
            let location = (
                group.tenant_id.clone(),
                group.namespace.clone(),
                group.table.clone(),
            );
            let table_tombstones = tombstones
                .get(&location)
                .map(Vec::as_slice)
                .unwrap_or_default();
            written.push(
                self.compact_group(db, group, watermark, table_tombstones)
                    .await?,
            );
            handled.insert(location);
        }

        for (location, table_tombstones) in &tombstones {
            if handled.contains(location) {
                continue;
            }
            self.persist_tombstones(db, location, table_tombstones)
                .await?;
        }

        Ok(written)
    }

    async fn collect_tombstones(&self, db: &TraceLakeDb) -> Result<TableTombstones, CompactError> {
        let mut tombstones: TableTombstones = HashMap::new();

        for (key, tombstone) in db.scan_all_tombstones().await? {
            if let TombstoneKey::Record {
                tenant_id,
                namespace,
                table,
                ..
            } = decode_tombstone_key(&key)?
            {
                tombstones
                    .entry((tenant_id, namespace, table))
                    .or_default()
                    .push((key, tombstone));
            }
        }

        Ok(tombstones)
    }

    async fn persist_tombstones(
        &self,
        db: &TraceLakeDb,
        location: &(String, String, String),
        tombstones: &[(Vec<u8>, Tombstone)],
    ) -> Result<(), CompactError> {
        let (tenant_id, namespace, table) = location;
        let mut manifest =
            read_record_manifest(&self.store, &self.base_path, tenant_id, namespace, table)
                .await?
                .unwrap_or_else(|| SegmentManifest::new_table(tenant_id, namespace, table));

        manifest.apply_tombstones(&tombstone_values(tombstones));
        publish_manifest(&self.store, &self.base_path, &manifest).await?;

        db.delete_keys(&tombstone_keys(tombstones)).await?;
        Ok(())
    }

    async fn compact_group(
        &self,
        db: &TraceLakeDb,
        group: &RecordGroup,
        watermark: &str,
        tombstones: &[(Vec<u8>, Tombstone)],
    ) -> Result<SegmentEntry, CompactError> {
        let schema = put_schema(
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
                encode_record_pack(&schema, &group.records)?,
            )
            .await?;
        let bloom_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Bloom,
                build_record_bloom(&schema, &group.records)?,
            )
            .await?;

        let record_stats = build_record_stats(&schema, &group.records)?;
        let stats_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Stats,
                serde_json::to_vec(&record_stats)?,
            )
            .await?;

        let columns_file = self
            .put_file(
                group,
                &segment_id,
                SegmentFileKind::Columns,
                crate::column_writer::encode_parquet(&crate::column_writer::build_record_batch(
                    &schema,
                    &group.records,
                )?)?,
            )
            .await?;

        let entry = SegmentEntry {
            segment_id: segment_id.clone(),
            trace_count: 0,
            span_count: 0,
            files: vec![pack_file, bloom_file, stats_file, columns_file],
            stats: tracelake_core::SegmentStats::default(),
            record_stats: Some(record_stats),
        };

        manifest.segments.push(entry.clone());
        manifest.watermark = Some(watermark.to_string());
        manifest.apply_tombstones(&tombstone_values(tombstones));
        publish_manifest(&self.store, &self.base_path, &manifest).await?;

        let keys = group
            .records
            .iter()
            .map(|record| {
                schema
                    .record_id(record)
                    .map_err(CompactError::Record)
                    .and_then(|record_id| {
                        encode_record_key(
                            &group.tenant_id,
                            &group.namespace,
                            &group.table,
                            &record_id,
                        )
                        .map_err(CompactError::Key)
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        db.delete_keys(&keys).await?;
        db.delete_keys(&tombstone_keys(tombstones)).await?;

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

/// Hot record tombstones grouped by table, with storage keys.
type TableTombstones = HashMap<(String, String, String), Vec<(Vec<u8>, Tombstone)>>;

fn tombstone_values(tombstones: &[(Vec<u8>, Tombstone)]) -> Vec<Tombstone> {
    tombstones
        .iter()
        .map(|(_, tombstone)| tombstone.clone())
        .collect()
}

fn tombstone_keys(tombstones: &[(Vec<u8>, Tombstone)]) -> Vec<Vec<u8>> {
    tombstones.iter().map(|(key, _)| key.clone()).collect()
}

fn deleted_ids(tombstones: &TableTombstones) -> HashMap<(String, String, String), HashSet<String>> {
    tombstones
        .iter()
        .map(|(location, values)| {
            (
                location.clone(),
                values
                    .iter()
                    .map(|(_, tombstone)| tombstone.id.clone())
                    .collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tracelake_core::{FieldType, Record, TableSchema};

    const BASE_PATH: &str = "tracelake/segments";

    #[tokio::test]
    async fn a_second_record_compaction_without_new_records_writes_nothing() {
        let store: Arc<dyn ObjectStore> = Arc::new(slatedb::object_store::memory::InMemory::new());
        let schema = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .primary_key("id")
            .build()
            .expect("schema builds");
        crate::schema_store::put_schema(&store, BASE_PATH, "tenant-1", "default", &schema)
            .await
            .expect("schema registers");

        let db = TraceLakeDb::open_in_memory("/record-watermark")
            .await
            .expect("db should open");
        let records: Vec<Record> = (0..2)
            .map(|index| Record::new().set("id", json!(format!("record-{index}"))))
            .collect();
        db.put_records("tenant-1", "default", &schema, &records)
            .await
            .expect("records should write");

        let compactor = RecordCompactor::new(Arc::clone(&store), BASE_PATH);
        let first = compactor
            .compact(&db)
            .await
            .expect("compaction should succeed");
        assert_eq!(first.len(), 1);

        let manifest = read_record_manifest(&store, BASE_PATH, "tenant-1", "default", "notes")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        let watermark = manifest.watermark.expect("watermark is set");
        assert!(watermark.ends_with("record-1"));

        assert!(
            db.scan_all_records().await.expect("scan").is_empty(),
            "compacted hot keys are dropped"
        );

        let second = compactor
            .compact(&db)
            .await
            .expect("second compaction should succeed");
        assert!(second.is_empty(), "nothing new to compact");
        assert_eq!(manifest.segments.len(), 1);
    }
}
