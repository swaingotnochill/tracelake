use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use serde_json::Value;
use std::sync::Arc;
use tracelake_core::{FieldType, Record, Span, TableSchema};

use crate::compactor::CompactError;

/// Columns written for a table: every declared field except JSON, which has no
/// fixed Arrow type and stays in the record pack.
pub fn record_schema(schema: &TableSchema) -> Arc<Schema> {
    let fields: Vec<Field> = schema
        .fields
        .iter()
        .filter_map(|field| {
            let data_type = match field.field_type {
                FieldType::Json => return None,
                FieldType::String => DataType::Utf8,
                FieldType::I64 | FieldType::Timestamp => DataType::Int64,
                FieldType::F64 => DataType::Float64,
                FieldType::Bool => DataType::Boolean,
            };
            Some(Field::new(&field.name, data_type, true))
        })
        .collect();

    Arc::new(Schema::new(fields))
}

pub fn build_record_batch(
    schema: &TableSchema,
    records: &[Record],
) -> Result<RecordBatch, CompactError> {
    let arrow_schema = record_schema(schema);
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(arrow_schema.fields().len());

    for field in schema
        .fields
        .iter()
        .filter(|field| field.field_type != FieldType::Json)
    {
        let values: Vec<Option<Value>> = records
            .iter()
            .map(|record| record.get(&field.name).cloned())
            .collect();

        let column: ArrayRef = match field.field_type {
            FieldType::String => Arc::new(StringArray::from(
                values
                    .iter()
                    .map(|value| value.as_ref().and_then(Value::as_str))
                    .collect::<Vec<_>>(),
            )),
            FieldType::I64 | FieldType::Timestamp => Arc::new(Int64Array::from(
                values
                    .iter()
                    .map(|value| value.as_ref().and_then(Value::as_i64))
                    .collect::<Vec<_>>(),
            )),
            FieldType::F64 => Arc::new(Float64Array::from(
                values
                    .iter()
                    .map(|value| value.as_ref().and_then(Value::as_f64))
                    .collect::<Vec<_>>(),
            )),
            FieldType::Bool => Arc::new(BooleanArray::from(
                values
                    .iter()
                    .map(|value| value.as_ref().and_then(Value::as_bool))
                    .collect::<Vec<_>>(),
            )),
            FieldType::Json => continue,
        };

        columns.push(column);
    }

    Ok(RecordBatch::try_new(arrow_schema, columns)?)
}

pub fn span_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("span_type", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("model", DataType::Utf8, true),
        Field::new("provider", DataType::Utf8, true),
        Field::new("start_time_ns", DataType::Int64, true),
        Field::new("end_time_ns", DataType::Int64, true),
        Field::new("latency_ms", DataType::Int64, true),
        Field::new("cost_usd", DataType::Float64, true),
        Field::new("tokens_total", DataType::Int64, true),
        Field::new("error", DataType::Utf8, true),
    ]))
}

pub fn build_span_batch(spans: &[Span]) -> Result<RecordBatch, CompactError> {
    let text = |value: Option<&str>| value.map(str::to_string);
    let start_ns: Vec<Option<i64>> = spans
        .iter()
        .map(|span| span.start_time.timestamp_nanos_opt())
        .collect();
    let end_ns: Vec<Option<i64>> = spans
        .iter()
        .map(|span| span.end_time.and_then(|end| end.timestamp_nanos_opt()))
        .collect();
    let latency_ms: Vec<Option<i64>> = spans
        .iter()
        .map(|span| {
            span.end_time
                .map(|end| (end - span.start_time).num_milliseconds().max(0))
        })
        .collect();

    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| Some(span.trace_id.clone()))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| Some(span.span_id.clone()))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| Some(span_type(&span.span_type)))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| Some(span.name.clone()))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| Some(status(&span.status)))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| text(span.model.as_deref()))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| text(span.provider.as_deref()))
                .collect::<Vec<_>>(),
        )),
        Arc::new(Int64Array::from(start_ns)),
        Arc::new(Int64Array::from(end_ns)),
        Arc::new(Int64Array::from(latency_ms)),
        Arc::new(Float64Array::from(
            spans.iter().map(|span| span.cost_usd).collect::<Vec<_>>(),
        )),
        Arc::new(Int64Array::from(
            spans
                .iter()
                .map(|span| span.tokens.as_ref().map(|tokens| i64::from(tokens.total)))
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            spans
                .iter()
                .map(|span| text(span.error.as_deref()))
                .collect::<Vec<_>>(),
        )),
    ];

    Ok(RecordBatch::try_new(span_schema(), columns)?)
}

pub fn encode_parquet(batch: &RecordBatch) -> Result<Vec<u8>, CompactError> {
    let mut buffer = Vec::new();
    {
        let properties = WriterProperties::builder().build();
        let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), Some(properties))?;
        writer.write(batch)?;
        writer.close()?;
    }

    Ok(buffer)
}

fn span_type(span_type: &tracelake_core::SpanType) -> String {
    match span_type {
        tracelake_core::SpanType::Custom(name) => name.clone(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn status(status: &tracelake_core::SpanStatus) -> String {
    format!("{status:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array as _;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use serde_json::json;
    use std::fs::File;
    use std::io::Write;
    use tracelake_core::{SpanStatus, SpanType};

    fn schema() -> TableSchema {
        TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("score", FieldType::F64)
            .field("at", FieldType::Timestamp)
            .field("passed", FieldType::Bool)
            .field("body", FieldType::Json)
            .primary_key("id")
            .build()
            .unwrap()
    }

    fn record(id: &str, score: f64) -> Record {
        Record::new()
            .set("id", json!(id))
            .set("score", json!(score))
            .set("at", json!(1_790_856_000_000_000_000i64))
            .set("passed", json!(score >= 0.5))
            .set("body", json!({ "text": "not a column" }))
    }

    fn read_back(bytes: &[u8]) -> Vec<RecordBatch> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("columns.parquet");
        File::create(&path).unwrap().write_all(bytes).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
        builder
            .build()
            .unwrap()
            .map(|batch| batch.unwrap())
            .collect()
    }

    #[test]
    fn records_round_trip_through_a_column_file() {
        let schema = schema();
        let records = vec![record("a", 0.25), record("b", 0.75)];
        let batch = build_record_batch(&schema, &records).unwrap();
        let bytes = encode_parquet(&batch).unwrap();
        let batches = read_back(&bytes);

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 2);
        assert_eq!(
            batches[0]
                .schema()
                .fields()
                .iter()
                .map(|field| field.name().as_str())
                .collect::<Vec<_>>(),
            vec!["id", "score", "at", "passed"]
        );

        let scores = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(scores.value(0), 0.25);
        assert_eq!(scores.value(1), 0.75);
    }

    #[test]
    fn missing_values_become_nulls() {
        let schema = schema();
        let records = vec![Record::new().set("id", json!("a"))];
        let bytes = encode_parquet(&build_record_batch(&schema, &records).unwrap()).unwrap();
        let batches = read_back(&bytes);

        let scores = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!(scores.is_null(0));
    }

    #[test]
    fn spans_round_trip_with_latency_and_tokens() {
        let start = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 10, 1, 12, 0, 0)
            .single()
            .expect("start is valid");
        let spans = vec![Span {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            span_id: "span-1".to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: "completion".to_string(),
            start_time: start,
            end_time: Some(start + chrono::Duration::milliseconds(250)),
            status: SpanStatus::Ok,
            attributes: Value::Null,
            input: None,
            output: None,
            tokens: Some(tracelake_core::TokenUsage {
                prompt: 10,
                completion: 5,
                total: 15,
            }),
            cost_usd: Some(0.01),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }];

        let bytes = encode_parquet(&build_span_batch(&spans).unwrap()).unwrap();
        let batches = read_back(&bytes);

        assert_eq!(batches[0].num_rows(), 1);
        let latency = batches[0]
            .column(9)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(latency.value(0), 250);
        let tokens = batches[0]
            .column(11)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(tokens.value(0), 15);
    }
}
