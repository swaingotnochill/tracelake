//! `query_records()` and `get_record()`: typed filters and projections over
//! user-defined tables, merging the hot store with compacted record segments.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use slatedb::object_store::path::Path;
use std::collections::{BTreeMap, BTreeSet};
use tracelake_compact::{RecordPackReader, read_record_manifest, read_schema};
use tracelake_core::{
    BloomFilter, FieldDef, FieldType, Record, RecordError, RecordStats, SegmentEntry,
    SegmentFileKind, TableSchema, typed_value,
};

use crate::trace_fetch::{QueryError, TraceLakeReader, tombstone_ids};

/// Default result cap when the caller does not set one.
pub const DEFAULT_RECORD_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<RecordFilter>,
    /// Fields returned per record. The primary key is always included.
    #[serde(default)]
    pub projection: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    DEFAULT_RECORD_LIMIT
}

impl Default for RecordQuery {
    fn default() -> Self {
        Self {
            filter: None,
            projection: Vec::new(),
            limit: DEFAULT_RECORD_LIMIT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordFilter {
    And(Vec<RecordFilter>),
    Or(Vec<RecordFilter>),
    Eq { field: String, value: Value },
    Gt { field: String, value: Value },
    Ge { field: String, value: Value },
    Lt { field: String, value: Value },
    Le { field: String, value: Value },
}

/// Query results plus the segment counts that show whether pruning worked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordQueryResult {
    pub records: Vec<Record>,
    pub scanned_segments: u64,
    pub pruned_segments: u64,
}

impl RecordFilter {
    pub fn and(filters: Vec<RecordFilter>) -> Self {
        RecordFilter::And(filters)
    }

    pub fn eq(field: impl Into<String>, value: Value) -> Self {
        RecordFilter::Eq {
            field: field.into(),
            value,
        }
    }

    pub fn ge(field: impl Into<String>, value: Value) -> Self {
        RecordFilter::Ge {
            field: field.into(),
            value,
        }
    }

    pub fn gt(field: impl Into<String>, value: Value) -> Self {
        RecordFilter::Gt {
            field: field.into(),
            value,
        }
    }

    pub fn lt(field: impl Into<String>, value: Value) -> Self {
        RecordFilter::Lt {
            field: field.into(),
            value,
        }
    }

    pub fn le(field: impl Into<String>, value: Value) -> Self {
        RecordFilter::Le {
            field: field.into(),
            value,
        }
    }

    fn fields(&self, out: &mut BTreeSet<String>) {
        match self {
            RecordFilter::And(filters) | RecordFilter::Or(filters) => {
                for filter in filters {
                    filter.fields(out);
                }
            }
            RecordFilter::Eq { field, .. }
            | RecordFilter::Gt { field, .. }
            | RecordFilter::Ge { field, .. }
            | RecordFilter::Lt { field, .. }
            | RecordFilter::Le { field, .. } => {
                out.insert(field.clone());
            }
        }
    }
}

impl RecordQuery {
    /// Rejects filters and projections that name undeclared fields or use a
    /// value of the wrong declared type, once, before any segment is read.
    pub fn validate(&self, schema: &TableSchema) -> Result<(), QueryError> {
        let mut fields = BTreeSet::new();

        for name in &self.projection {
            require_field(schema, name)?;
        }
        if let Some(filter) = &self.filter {
            filter.fields(&mut fields);
            validate_filter(filter, schema)?;
        }

        Ok(())
    }
}

impl TraceLakeReader {
    /// Fetches one record by primary key. The bloom filter of each segment
    /// skips packs that cannot hold it, and a hot copy overrides compacted
    /// copies.
    pub async fn get_record(
        &self,
        tenant_id: &str,
        namespace: &str,
        table: &str,
        record_id: &str,
    ) -> Result<Option<Record>, QueryError> {
        let schema = self.load_schema(tenant_id, namespace, table).await?;
        let fields: Vec<&str> = schema
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();

        let manifest =
            read_record_manifest(&self.store, &self.base_path, tenant_id, namespace, table).await?;
        let deleted = tombstone_ids(
            manifest.as_ref(),
            self.db
                .scan_record_tombstones(tenant_id, namespace, table)
                .await?,
        );
        if deleted.contains(record_id) {
            return Ok(None);
        }

        let mut found = None;
        if let Some(manifest) = manifest {
            for entry in &manifest.segments {
                if !self.segment_may_hold(entry, record_id).await? {
                    continue;
                }

                if let Some(record) = self
                    .read_record_pack(entry, &schema, record_id, &fields)
                    .await?
                {
                    found = Some(record);
                }
            }
        }

        if let Some(mut hot) = self
            .db
            .get_record(tenant_id, namespace, &schema, record_id)
            .await?
        {
            schema.fill_defaults(&mut hot);
            found = Some(hot);
        }

        Ok(found)
    }

    /// Filters, projects, sorts by primary key, and caps one table.
    pub async fn query_records(
        &self,
        tenant_id: &str,
        namespace: &str,
        table: &str,
        query: &RecordQuery,
    ) -> Result<RecordQueryResult, QueryError> {
        let schema = self.load_schema(tenant_id, namespace, table).await?;
        query.validate(&schema)?;

        let mut needed: BTreeSet<String> = query.projection.iter().cloned().collect();
        if let Some(filter) = &query.filter {
            filter.fields(&mut needed);
        }
        needed.insert(schema.primary_key.clone());
        let needed_refs: Vec<&str> = needed.iter().map(String::as_str).collect();

        let mut by_id: BTreeMap<String, Record> = BTreeMap::new();
        let mut scanned_segments = 0;
        let mut pruned_segments = 0;

        let manifest =
            read_record_manifest(&self.store, &self.base_path, tenant_id, namespace, table).await?;
        let deleted = tombstone_ids(
            manifest.as_ref(),
            self.db
                .scan_record_tombstones(tenant_id, namespace, table)
                .await?,
        );

        if let Some(manifest) = manifest {
            let packs = RecordPackReader::new(std::sync::Arc::clone(&self.store));

            for entry in &manifest.segments {
                if let (Some(stats), Some(filter)) = (&entry.record_stats, &query.filter)
                    && !record_stats_may_match(stats, &schema, filter)
                {
                    pruned_segments += 1;
                    continue;
                }

                scanned_segments += 1;
                for record in self
                    .read_segment_pack(&packs, entry, &schema, &needed_refs)
                    .await?
                {
                    let record_id = schema.record_id(&record)?;
                    if deleted.contains(&record_id) {
                        continue;
                    }
                    by_id.insert(record_id, record);
                }
            }
        }

        for mut record in self
            .db
            .get_table_records(tenant_id, namespace, table)
            .await?
        {
            schema.fill_defaults(&mut record);
            let record_id = schema.record_id(&record)?;
            if deleted.contains(&record_id) {
                continue;
            }
            by_id.insert(record_id, record);
        }

        let record_ids: Vec<&String> = by_id.keys().collect();
        let mut records = Vec::new();

        for record_id in record_ids {
            let Some(record) = by_id.get(record_id) else {
                continue;
            };
            if let Some(filter) = &query.filter
                && !record_matches(record, &schema, filter)?
            {
                continue;
            }

            records.push(project(record, &schema, &query.projection));
            if records.len() == query.limit {
                break;
            }
        }

        Ok(RecordQueryResult {
            records,
            scanned_segments,
            pruned_segments,
        })
    }

    pub(crate) async fn load_schema(
        &self,
        tenant_id: &str,
        namespace: &str,
        table: &str,
    ) -> Result<TableSchema, QueryError> {
        read_schema(&self.store, &self.base_path, tenant_id, namespace, table)
            .await?
            .ok_or_else(|| QueryError::SchemaMissing {
                table: table.to_string(),
            })
    }

    async fn segment_may_hold(
        &self,
        entry: &SegmentEntry,
        record_id: &str,
    ) -> Result<bool, QueryError> {
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

        Ok(bloom.contains(record_id.as_bytes()))
    }

    async fn read_record_pack(
        &self,
        entry: &SegmentEntry,
        schema: &TableSchema,
        record_id: &str,
        fields: &[&str],
    ) -> Result<Option<Record>, QueryError> {
        let pack_path = record_pack_path(entry)?;
        let bytes = self
            .store
            .get(&Path::from(pack_path))
            .await?
            .bytes()
            .await?;

        Ok(tracelake_core::decode_record_pack(&bytes)?
            .read_record_with(schema, record_id, fields)?)
    }

    async fn read_segment_pack(
        &self,
        packs: &RecordPackReader,
        entry: &SegmentEntry,
        schema: &TableSchema,
        fields: &[&str],
    ) -> Result<Vec<Record>, QueryError> {
        Ok(packs
            .read_pack(record_pack_path(entry)?, schema, fields)
            .await?)
    }
}

fn record_pack_path(entry: &SegmentEntry) -> Result<&str, QueryError> {
    entry
        .files
        .iter()
        .find(|file| file.kind == SegmentFileKind::RecordPack)
        .map(|file| file.path.as_str())
        .ok_or_else(|| QueryError::MissingRecordPack {
            segment_id: entry.segment_id.clone(),
        })
}

fn require_field<'a>(schema: &'a TableSchema, name: &str) -> Result<&'a FieldDef, QueryError> {
    schema.field(name).ok_or_else(|| QueryError::InvalidQuery {
        reason: format!("field '{name}' is not declared in table '{}'", schema.table),
    })
}

fn validate_filter(filter: &RecordFilter, schema: &TableSchema) -> Result<(), QueryError> {
    match filter {
        RecordFilter::And(filters) | RecordFilter::Or(filters) => {
            for filter in filters {
                validate_filter(filter, schema)?;
            }
        }
        RecordFilter::Eq { field, value }
        | RecordFilter::Gt { field, value }
        | RecordFilter::Ge { field, value }
        | RecordFilter::Lt { field, value }
        | RecordFilter::Le { field, value } => {
            let def = require_field(schema, field)?;
            typed_value(def, value)?;
        }
    }

    Ok(())
}

/// Whether one record's values can satisfy the filter. Absent and null fields
/// never satisfy a comparison.
fn record_matches(
    record: &Record,
    schema: &TableSchema,
    filter: &RecordFilter,
) -> Result<bool, RecordError> {
    Ok(match filter {
        RecordFilter::And(filters) => {
            for filter in filters {
                if !record_matches(record, schema, filter)? {
                    return Ok(false);
                }
            }
            true
        }
        RecordFilter::Or(filters) => {
            for filter in filters {
                if record_matches(record, schema, filter)? {
                    return Ok(true);
                }
            }
            false
        }
        RecordFilter::Eq { field, value } => compare(record, schema, field, value, |order| {
            order == std::cmp::Ordering::Equal
        })?,
        RecordFilter::Gt { field, value } => compare(record, schema, field, value, |order| {
            order == std::cmp::Ordering::Greater
        })?,
        RecordFilter::Ge { field, value } => compare(record, schema, field, value, |order| {
            order != std::cmp::Ordering::Less
        })?,
        RecordFilter::Lt { field, value } => compare(record, schema, field, value, |order| {
            order == std::cmp::Ordering::Less
        })?,
        RecordFilter::Le { field, value } => compare(record, schema, field, value, |order| {
            order != std::cmp::Ordering::Greater
        })?,
    })
}

fn compare(
    record: &Record,
    schema: &TableSchema,
    field: &str,
    value: &Value,
    accept: impl Fn(std::cmp::Ordering) -> bool,
) -> Result<bool, RecordError> {
    let def = schema
        .field(field)
        .ok_or_else(|| RecordError::UnknownField {
            name: field.to_string(),
        })?;
    let Some(stored) = record.get(field) else {
        return Ok(false);
    };
    if stored.is_null() {
        return Ok(false);
    }

    let stored = typed_value(def, stored)?;
    let filter = typed_value(def, value)?;

    Ok(stored.compare(&filter).is_some_and(accept))
}

/// Prune soundness: false only when no record in the segment can satisfy the
/// filter. Missing bounds never prune.
fn record_stats_may_match(
    stats: &RecordStats,
    schema: &TableSchema,
    filter: &RecordFilter,
) -> bool {
    match filter {
        RecordFilter::And(filters) => filters
            .iter()
            .all(|filter| record_stats_may_match(stats, schema, filter)),
        RecordFilter::Or(filters) => filters
            .iter()
            .any(|filter| record_stats_may_match(stats, schema, filter)),
        RecordFilter::Eq { field, value } => {
            let Some((range, field_type)) = stats_range(stats, schema, field) else {
                return true;
            };
            if field_type == FieldType::String
                && let Some(distinct) = &range.distinct
                && let Some(value) = value.as_str()
                && !distinct.iter().any(|existing| existing == value)
            {
                return false;
            }
            range_allows(range, field_type, value, Op::Eq)
        }
        RecordFilter::Gt { field, value } => range_may_match(stats, schema, field, value, Op::Gt),
        RecordFilter::Ge { field, value } => range_may_match(stats, schema, field, value, Op::Ge),
        RecordFilter::Lt { field, value } => range_may_match(stats, schema, field, value, Op::Lt),
        RecordFilter::Le { field, value } => range_may_match(stats, schema, field, value, Op::Le),
    }
}

#[derive(Clone, Copy)]
enum Op {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
}

fn stats_range<'a>(
    stats: &'a RecordStats,
    schema: &TableSchema,
    field: &str,
) -> Option<(&'a tracelake_core::FieldRange, FieldType)> {
    Some((stats.fields.get(field)?, schema.field(field)?.field_type))
}

fn range_may_match(
    stats: &RecordStats,
    schema: &TableSchema,
    field: &str,
    value: &Value,
    op: Op,
) -> bool {
    let Some((range, field_type)) = stats_range(stats, schema, field) else {
        return true;
    };

    range_allows(range, field_type, value, op)
}

/// Whether some value inside a segment's `[min, max]` can satisfy `op`. A
/// missing bound leaves that side unconstrained.
fn range_allows(
    range: &tracelake_core::FieldRange,
    field_type: FieldType,
    value: &Value,
    op: Op,
) -> bool {
    match field_type {
        FieldType::String => match value.as_str() {
            Some(value) => allows(
                op,
                range.string_min.as_deref(),
                range.string_max.as_deref(),
                value,
            ),
            None => true,
        },
        FieldType::I64 | FieldType::Timestamp => match value.as_i64() {
            Some(value) => allows(op, range.i64_min, range.i64_max, value),
            None => true,
        },
        FieldType::F64 => match value.as_f64() {
            Some(value) => allows(op, range.f64_min, range.f64_max, value),
            None => true,
        },
        FieldType::Bool | FieldType::Json => true,
    }
}

fn allows<T: PartialOrd>(op: Op, min: Option<T>, max: Option<T>, value: T) -> bool {
    match op {
        Op::Eq => min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max),
        Op::Gt => max.is_none_or(|max| max > value),
        Op::Ge => max.is_none_or(|max| max >= value),
        Op::Lt => min.is_none_or(|min| min < value),
        Op::Le => min.is_none_or(|min| min <= value),
    }
}

fn project(record: &Record, schema: &TableSchema, projection: &[String]) -> Record {
    if projection.is_empty() {
        return record.clone();
    }

    let mut kept = Record::new();
    for name in projection {
        if let Some(value) = record.get(name) {
            kept = kept.set(name.clone(), value.clone());
        }
    }
    if let Some(value) = record.get(&schema.primary_key) {
        kept = kept.set(schema.primary_key.clone(), value.clone());
    }

    kept
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod verification;

#[cfg(kani)]
mod verify {
    use super::*;

    /// A pruned `ge` bound proves no value in the segment reaches it.
    #[kani::proof]
    fn ge_prune_means_the_segment_max_is_below_the_bound() {
        let min: i64 = kani::any();
        let max: i64 = kani::any();
        let value: i64 = kani::any();
        kani::assume(min <= max);

        if !allows(Op::Ge, Some(min), Some(max), value) {
            assert!(max < value);
        }
    }

    /// A pruned `le` bound proves no value in the segment is at or below it.
    #[kani::proof]
    fn le_prune_means_the_segment_min_is_above_the_bound() {
        let min: i64 = kani::any();
        let max: i64 = kani::any();
        let value: i64 = kani::any();
        kani::assume(min <= max);

        if !allows(Op::Le, Some(min), Some(max), value) {
            assert!(min > value);
        }
    }

    /// A pruned equality bound proves the value lies outside the segment range.
    #[kani::proof]
    fn eq_prune_means_the_value_is_outside_the_range() {
        let min: i64 = kani::any();
        let max: i64 = kani::any();
        let value: i64 = kani::any();
        kani::assume(min <= max);

        if !allows(Op::Eq, Some(min), Some(max), value) {
            assert!(value < min || value > max);
        }
    }

    /// Pruning never rejects a value that sits inside the segment range.
    #[kani::proof]
    fn equality_inside_the_range_is_never_pruned() {
        let min: i64 = kani::any();
        let max: i64 = kani::any();
        let value: i64 = kani::any();
        kani::assume(min <= max);
        kani::assume(value >= min && value <= max);

        assert!(allows(Op::Eq, Some(min), Some(max), value));
    }
}
