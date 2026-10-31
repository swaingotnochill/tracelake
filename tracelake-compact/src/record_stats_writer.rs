use std::collections::BTreeMap;
use tracelake_core::{
    FieldRange, FieldValue, MAX_TRACKED_DISTINCT_VALUES, Record, RecordError, RecordStats,
    TableSchema, typed_value,
};

/// Bounds one field's values across a record batch. Null or absent values are
/// skipped because no comparison filter can match them.
pub fn build_record_stats(
    schema: &TableSchema,
    records: &[Record],
) -> Result<RecordStats, RecordError> {
    let mut stats = RecordStats {
        schema_version: schema.version,
        record_count: records.len() as u64,
        fields: BTreeMap::new(),
    };

    for field in &schema.fields {
        let mut range = FieldRange::new(field.field_type);
        let mut distinct: Option<Vec<String>> = Some(Vec::new());

        for record in records {
            let Some(value) = record.fields.get(&field.name) else {
                continue;
            };
            if value.is_null() {
                continue;
            }

            match typed_value(field, value)? {
                FieldValue::Str(text) => {
                    track_str_min_max(&mut range.string_min, &mut range.string_max, text);
                    distinct = push_distinct(distinct, text);
                }
                FieldValue::I64(number) | FieldValue::Timestamp(number) => {
                    track_min_max(&mut range.i64_min, &mut range.i64_max, number)
                }
                FieldValue::F64(number) => {
                    track_min_max(&mut range.f64_min, &mut range.f64_max, number)
                }
                FieldValue::Bool(_) | FieldValue::Json(_) => {}
            }
        }

        if distinct.as_ref().is_some_and(|values| values.len() <= 1) {
            distinct = None;
        }

        range.distinct = distinct;
        stats.fields.insert(field.name.clone(), range);
    }

    Ok(stats)
}

fn track_str_min_max(min: &mut Option<String>, max: &mut Option<String>, value: &str) {
    if min.as_deref().is_none_or(|current| value < current) {
        *min = Some(value.to_string());
    }
    if max.as_deref().is_none_or(|current| value > current) {
        *max = Some(value.to_string());
    }
}

fn track_min_max<T: PartialOrd + Copy>(min: &mut Option<T>, max: &mut Option<T>, value: T) {
    if min.is_none_or(|current| value < current) {
        *min = Some(value);
    }
    if max.is_none_or(|current| value > current) {
        *max = Some(value);
    }
}

fn push_distinct(distinct: Option<Vec<String>>, value: &str) -> Option<Vec<String>> {
    let mut values = distinct?;
    if values.iter().any(|existing| existing == value) {
        return Some(values);
    }

    values.push(value.to_string());
    (values.len() <= MAX_TRACKED_DISTINCT_VALUES).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tracelake_core::FieldType;

    fn sample_schema() -> TableSchema {
        TableSchema::builder("samples")
            .field("id", FieldType::String)
            .field("task", FieldType::String)
            .field("score", FieldType::F64)
            .field("epoch", FieldType::I64)
            .field("at", FieldType::Timestamp)
            .primary_key("id")
            .build()
            .unwrap()
    }

    fn record(id: &str, task: &str, score: f64, epoch: i64) -> Record {
        Record::new()
            .set("id", json!(id))
            .set("task", json!(task))
            .set("score", json!(score))
            .set("epoch", json!(epoch))
            .set("at", json!(1_000 + epoch))
    }

    #[test]
    fn bounds_and_distinct_values_cover_every_record() {
        let records = vec![
            record("a", "gsm8k", 0.9, 1),
            record("b", "gsm8k", 0.2, 3),
            record("c", "math", 0.5, 2),
        ];

        let stats = build_record_stats(&sample_schema(), &records).unwrap();

        assert_eq!(stats.record_count, 3);
        assert_eq!(stats.schema_version, sample_schema().version);

        let task = &stats.fields["task"];
        assert_eq!(task.string_min.as_deref(), Some("gsm8k"));
        assert_eq!(task.string_max.as_deref(), Some("math"));
        assert_eq!(
            task.distinct.as_ref().unwrap(),
            &vec!["gsm8k".to_string(), "math".to_string()]
        );

        let score = &stats.fields["score"];
        assert_eq!(score.f64_min, Some(0.2));
        assert_eq!(score.f64_max, Some(0.9));

        let epoch = &stats.fields["epoch"];
        assert_eq!(epoch.i64_min, Some(1));
        assert_eq!(epoch.i64_max, Some(3));

        let at = &stats.fields["at"];
        assert_eq!(at.i64_min, Some(1_001));
        assert_eq!(at.i64_max, Some(1_003));
    }

    #[test]
    fn null_and_absent_values_leave_a_field_unbounded() {
        let records = vec![
            Record::new().set("id", json!("a")),
            Record::new()
                .set("id", json!("b"))
                .set("score", serde_json::Value::Null),
        ];

        let stats = build_record_stats(&sample_schema(), &records).unwrap();

        assert_eq!(stats.fields["score"], FieldRange::new(FieldType::F64));
        assert_eq!(stats.fields["task"], FieldRange::new(FieldType::String));
    }

    #[test]
    fn distinct_tracking_stops_at_the_cap() {
        let records: Vec<Record> = (0..=MAX_TRACKED_DISTINCT_VALUES)
            .map(|index| {
                Record::new()
                    .set("id", json!(format!("r{index}")))
                    .set("task", json!(format!("task-{index}")))
            })
            .collect();

        let stats = build_record_stats(&sample_schema(), &records).unwrap();

        assert!(stats.fields["task"].distinct.is_none());
        assert_eq!(stats.fields["task"].string_min.as_deref(), Some("task-0"));
    }

    #[test]
    fn one_distinct_value_is_not_worth_recording() {
        let records = vec![record("a", "gsm8k", 0.5, 1), record("b", "gsm8k", 0.5, 2)];

        let stats = build_record_stats(&sample_schema(), &records).unwrap();

        assert!(stats.fields["task"].distinct.is_none());
    }
}
