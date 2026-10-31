//! `aggregate_records()` and `aggregate_spans()`: grouped metrics over the
//! Parquet column files that compaction writes beside each segment.

use datafusion::arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, LargeStringArray, StringArray,
    StringViewArray,
};
use datafusion::arrow::datatypes::DataType;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use slatedb::object_store::path::Path;
use std::collections::{BTreeMap, HashSet};
use tempfile::TempDir;
use tracelake_compact::read_record_manifest;
use tracelake_core::{FieldType, SegmentEntry, SegmentFileKind, TableSchema};

use crate::trace_fetch::{QueryError, TraceLakeReader};

pub const DEFAULT_AGGREGATE_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateRequest {
    #[serde(default)]
    pub group_by: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<AggregateMetric>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    DEFAULT_AGGREGATE_LIMIT
}

impl Default for AggregateRequest {
    fn default() -> Self {
        Self {
            group_by: Vec::new(),
            metrics: vec![AggregateMetric::Count],
            limit: DEFAULT_AGGREGATE_LIMIT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateMetric {
    Count,
    Sum { field: String },
    Avg { field: String },
    Min { field: String },
    Max { field: String },
    P50 { field: String },
    P95 { field: String },
    P99 { field: String },
}

impl AggregateMetric {
    fn field(&self) -> Option<&str> {
        match self {
            AggregateMetric::Count => None,
            AggregateMetric::Sum { field }
            | AggregateMetric::Avg { field }
            | AggregateMetric::Min { field }
            | AggregateMetric::Max { field }
            | AggregateMetric::P50 { field }
            | AggregateMetric::P95 { field }
            | AggregateMetric::P99 { field } => Some(field),
        }
    }

    fn alias(&self) -> String {
        match self {
            AggregateMetric::Count => "count".to_string(),
            AggregateMetric::Sum { field } => format!("sum_{field}"),
            AggregateMetric::Avg { field } => format!("avg_{field}"),
            AggregateMetric::Min { field } => format!("min_{field}"),
            AggregateMetric::Max { field } => format!("max_{field}"),
            AggregateMetric::P50 { field } => format!("p50_{field}"),
            AggregateMetric::P95 { field } => format!("p95_{field}"),
            AggregateMetric::P99 { field } => format!("p99_{field}"),
        }
    }

    fn expression(&self) -> String {
        let alias = self.alias();
        match self {
            AggregateMetric::Count => format!("count(*) AS {alias}"),
            AggregateMetric::Sum { field } => {
                format!("sum({}) AS {alias}", quote(field))
            }
            AggregateMetric::Avg { field } => {
                format!("avg({}) AS {alias}", quote(field))
            }
            AggregateMetric::Min { field } => {
                format!("min({}) AS {alias}", quote(field))
            }
            AggregateMetric::Max { field } => {
                format!("max({}) AS {alias}", quote(field))
            }
            AggregateMetric::P50 { field } => {
                format!("approx_percentile_cont({}, 0.5) AS {alias}", quote(field))
            }
            AggregateMetric::P95 { field } => {
                format!("approx_percentile_cont({}, 0.95) AS {alias}", quote(field))
            }
            AggregateMetric::P99 { field } => {
                format!("approx_percentile_cont({}, 0.99) AS {alias}", quote(field))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateResult {
    pub rows: Vec<BTreeMap<String, Value>>,
    pub scanned_segments: u64,
    pub pruned_segments: u64,
}

impl TraceLakeReader {
    pub async fn aggregate_records(
        &self,
        tenant_id: &str,
        namespace: &str,
        table: &str,
        request: &AggregateRequest,
    ) -> Result<AggregateResult, QueryError> {
        let schema = self.load_schema(tenant_id, namespace, table).await?;
        let columns = record_columns(&schema);
        validate_request(request, &columns)?;

        let manifest =
            read_record_manifest(&self.store, &self.base_path, tenant_id, namespace, table).await?;
        let files = manifest
            .as_ref()
            .map(|manifest| column_paths(&manifest.segments))
            .unwrap_or_default();

        self.aggregate_files(files, request, &columns).await
    }

    pub async fn aggregate_spans(
        &self,
        tenant_id: &str,
        namespace: &str,
        request: &AggregateRequest,
    ) -> Result<AggregateResult, QueryError> {
        let columns = span_columns();
        validate_request(request, &columns)?;

        let manifest =
            tracelake_compact::read_manifest(&self.store, &self.base_path, tenant_id, namespace)
                .await?;
        let files = manifest
            .as_ref()
            .map(|manifest| column_paths(&manifest.segments))
            .unwrap_or_default();

        self.aggregate_files(files, request, &columns).await
    }

    async fn aggregate_files(
        &self,
        files: Vec<String>,
        request: &AggregateRequest,
        columns: &BTreeMap<String, FieldType>,
    ) -> Result<AggregateResult, QueryError> {
        let needed = needed_columns(request, columns)?;
        if files.is_empty() {
            return Ok(AggregateResult {
                rows: Vec::new(),
                scanned_segments: 0,
                pruned_segments: 0,
            });
        }

        let dir = TempDir::new().map_err(aggregate_error)?;
        let context = SessionContext::new();
        let mut unions: Vec<String> = Vec::with_capacity(files.len());

        for (index, file) in files.iter().enumerate() {
            let bytes = self
                .store
                .get(&Path::from(file.as_str()))
                .await?
                .bytes()
                .await?;
            let path = dir.path().join(format!("{index}.parquet"));
            std::fs::write(&path, bytes).map_err(aggregate_error)?;

            let available = parquet_columns(&path)?;
            let table = format!("segment_{index}");
            context
                .register_parquet(
                    &table,
                    path.to_string_lossy().to_string(),
                    ParquetReadOptions::default(),
                )
                .await
                .map_err(aggregate_error)?;

            unions.push(select_list(&needed, &available, &table));
        }

        let sql = build_sql(request, &unions);
        let batches = context
            .sql(&sql)
            .await
            .map_err(aggregate_error)?
            .collect()
            .await
            .map_err(aggregate_error)?;

        Ok(AggregateResult {
            rows: rows_from_batches(&batches, request),
            scanned_segments: files.len() as u64,
            pruned_segments: 0,
        })
    }
}

fn aggregate_error(error: impl std::fmt::Display) -> QueryError {
    QueryError::Aggregate {
        reason: error.to_string(),
    }
}

fn column_paths(segments: &[SegmentEntry]) -> Vec<String> {
    segments
        .iter()
        .flat_map(|entry| entry.files.iter())
        .filter(|file| file.kind == SegmentFileKind::Columns)
        .map(|file| file.path.clone())
        .collect()
}

fn validate_request(
    request: &AggregateRequest,
    columns: &BTreeMap<String, FieldType>,
) -> Result<(), QueryError> {
    if request.metrics.is_empty() {
        return Err(aggregate_error("at least one metric is required"));
    }

    for field in &request.group_by {
        require_field(columns, field)?;
    }
    for metric in &request.metrics {
        let Some(field) = metric.field() else {
            continue;
        };
        let field_type = require_field(columns, field)?;
        let numeric = matches!(
            field_type,
            FieldType::I64 | FieldType::F64 | FieldType::Timestamp
        );
        let min_max = matches!(
            metric,
            AggregateMetric::Min { .. } | AggregateMetric::Max { .. }
        );
        if !numeric && !min_max {
            return Err(aggregate_error(format!(
                "metric {} needs a numeric field",
                metric.alias()
            )));
        }
    }

    Ok(())
}

fn require_field(
    columns: &BTreeMap<String, FieldType>,
    field: &str,
) -> Result<FieldType, QueryError> {
    columns
        .get(field)
        .copied()
        .ok_or_else(|| aggregate_error(format!("'{field}' has no column file representation")))
}

fn needed_columns(
    request: &AggregateRequest,
    columns: &BTreeMap<String, FieldType>,
) -> Result<Vec<(String, FieldType)>, QueryError> {
    let mut needed: Vec<(String, FieldType)> = Vec::new();

    for field in &request.group_by {
        let field_type = require_field(columns, field)?;
        if !needed.iter().any(|(name, _)| name == field) {
            needed.push((field.clone(), field_type));
        }
    }
    for metric in &request.metrics {
        let Some(field) = metric.field() else {
            continue;
        };
        if needed.iter().any(|(name, _)| name == field) {
            continue;
        }
        needed.push((field.to_string(), require_field(columns, field)?));
    }

    Ok(needed)
}

fn select_list(needed: &[(String, FieldType)], available: &HashSet<String>, table: &str) -> String {
    let columns: Vec<String> = needed
        .iter()
        .map(|(field, field_type)| {
            if available.contains(field) {
                quote(field)
            } else {
                format!(
                    "CAST(NULL AS {}) AS {}",
                    sql_type(*field_type),
                    quote(field)
                )
            }
        })
        .collect();

    format!("SELECT {} FROM {table}", columns.join(", "))
}

fn build_sql(request: &AggregateRequest, unions: &[String]) -> String {
    let group: Vec<String> = request.group_by.iter().map(|field| quote(field)).collect();
    let metrics: Vec<String> = request
        .metrics
        .iter()
        .map(AggregateMetric::expression)
        .collect();
    let mut select = group.clone();
    select.extend(metrics);

    let mut sql = format!("SELECT {} FROM (", select.join(", "));
    sql.push_str(&unions.join(" UNION ALL "));
    sql.push(')');

    if !group.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group.join(", ")));
        sql.push_str(&format!(" ORDER BY {}", group.join(", ")));
    }
    sql.push_str(&format!(" LIMIT {}", request.limit.max(1)));

    sql
}

fn quote(field: &str) -> String {
    format!("\"{}\"", field.replace('"', "\"\""))
}

fn sql_type(field_type: FieldType) -> &'static str {
    match field_type {
        FieldType::String | FieldType::Json => "VARCHAR",
        FieldType::I64 | FieldType::Timestamp => "BIGINT",
        FieldType::F64 => "DOUBLE",
        FieldType::Bool => "BOOLEAN",
    }
}

fn parquet_columns(path: &std::path::Path) -> Result<HashSet<String>, QueryError> {
    let file = std::fs::File::open(path).map_err(aggregate_error)?;
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(aggregate_error)?;

    Ok(builder
        .schema()
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect())
}

fn rows_from_batches(
    batches: &[RecordBatch],
    request: &AggregateRequest,
) -> Vec<BTreeMap<String, Value>> {
    let mut rows = Vec::new();

    for batch in batches {
        for index in 0..batch.num_rows() {
            let mut row = BTreeMap::new();
            for field in &request.group_by {
                if let Some(column) = batch.column_by_name(field) {
                    row.insert(field.clone(), value_at(column, index));
                }
            }
            for metric in &request.metrics {
                if let Some(column) = batch.column_by_name(&metric.alias()) {
                    row.insert(metric.alias(), value_at(column, index));
                }
            }
            rows.push(row);
        }
    }

    rows
}

fn value_at(column: &ArrayRef, index: usize) -> Value {
    if column.is_null(index) {
        return Value::Null;
    }

    match column.data_type() {
        DataType::Utf8 => column
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|array| Value::from(array.value(index)))
            .unwrap_or(Value::Null),
        DataType::Utf8View => column
            .as_any()
            .downcast_ref::<StringViewArray>()
            .map(|array| Value::from(array.value(index)))
            .unwrap_or(Value::Null),
        DataType::LargeUtf8 => column
            .as_any()
            .downcast_ref::<LargeStringArray>()
            .map(|array| Value::from(array.value(index)))
            .unwrap_or(Value::Null),
        DataType::Int64 => column
            .as_any()
            .downcast_ref::<Int64Array>()
            .map(|array| Value::from(array.value(index)))
            .unwrap_or(Value::Null),
        DataType::Float64 => column
            .as_any()
            .downcast_ref::<Float64Array>()
            .map(|array| Value::from(array.value(index)))
            .unwrap_or(Value::Null),
        DataType::Boolean => column
            .as_any()
            .downcast_ref::<BooleanArray>()
            .map(|array| Value::from(array.value(index)))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn record_columns(schema: &TableSchema) -> BTreeMap<String, FieldType> {
    schema
        .fields
        .iter()
        .filter(|field| field.field_type != FieldType::Json)
        .map(|field| (field.name.clone(), field.field_type))
        .collect()
}

fn span_columns() -> BTreeMap<String, FieldType> {
    [
        ("trace_id", FieldType::String),
        ("span_id", FieldType::String),
        ("span_type", FieldType::String),
        ("name", FieldType::String),
        ("status", FieldType::String),
        ("model", FieldType::String),
        ("provider", FieldType::String),
        ("start_time_ns", FieldType::Timestamp),
        ("end_time_ns", FieldType::Timestamp),
        ("latency_ms", FieldType::I64),
        ("cost_usd", FieldType::F64),
        ("tokens_total", FieldType::I64),
        ("error", FieldType::String),
    ]
    .into_iter()
    .map(|(name, field_type)| (name.to_string(), field_type))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns() -> BTreeMap<String, FieldType> {
        [
            ("model", FieldType::String),
            ("latency_ms", FieldType::I64),
            ("cost_usd", FieldType::F64),
        ]
        .into_iter()
        .map(|(name, field_type)| (name.to_string(), field_type))
        .collect()
    }

    fn request() -> AggregateRequest {
        AggregateRequest {
            group_by: vec!["model".to_string()],
            metrics: vec![
                AggregateMetric::Count,
                AggregateMetric::P95 {
                    field: "latency_ms".to_string(),
                },
            ],
            limit: 10,
        }
    }

    #[test]
    fn the_request_round_trips_through_json() {
        let request = request();
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: AggregateRequest = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn metrics_accept_the_wire_shape() {
        let decoded: AggregateRequest = serde_json::from_value(json!({
            "group_by": ["model"],
            "metrics": ["count", { "p95": { "field": "latency_ms" } }]
        }))
        .unwrap();

        assert_eq!(decoded.metrics.len(), 2);
        assert_eq!(decoded.metrics[0], AggregateMetric::Count);
    }

    #[test]
    fn sql_groups_orders_and_limits() {
        let sql = build_sql(&request(), &["SELECT 1".to_string()]);

        assert!(sql.contains("GROUP BY \"model\""));
        assert!(sql.contains("ORDER BY \"model\""));
        assert!(sql.contains("LIMIT 10"));
        assert!(sql.contains("approx_percentile_cont(\"latency_ms\", 0.95) AS p95_latency_ms"));
    }

    #[test]
    fn a_missing_column_selects_a_typed_null() {
        let available: HashSet<String> = ["model".to_string()].into_iter().collect();
        let needed = needed_columns(&request(), &columns()).unwrap();
        let select = select_list(&needed, &available, "segment_0");

        assert!(select.contains("\"model\""));
        assert!(select.contains("CAST(NULL AS BIGINT) AS \"latency_ms\""));
        assert!(!select.contains("CAST(NULL AS BIGINT) AS \"cost_usd\""));
    }

    #[test]
    fn invalid_requests_are_rejected() {
        let columns = columns();

        let mut missing = request();
        missing.group_by = vec!["nope".to_string()];
        assert!(validate_request(&missing, &columns).is_err());

        let mut non_numeric = request();
        non_numeric.metrics = vec![AggregateMetric::Avg {
            field: "model".to_string(),
        }];
        assert!(validate_request(&non_numeric, &columns).is_err());

        let mut empty = request();
        empty.metrics.clear();
        assert!(validate_request(&empty, &columns).is_err());

        assert!(validate_request(&request(), &columns).is_ok());
    }

    #[test]
    fn needed_columns_are_deduplicated() {
        let needed = needed_columns(&request(), &columns()).unwrap();
        assert_eq!(
            needed
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["model", "latency_ms"]
        );
    }
}
