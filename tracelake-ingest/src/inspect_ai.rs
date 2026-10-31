use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use tracelake_core::{FieldType, Record, RecordError, TableSchema};

use crate::TraceLakeDb;
use crate::ingest::IngestError;

pub const EVAL_SAMPLE_TABLE: &str = "inspect_ai_samples";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EvalLog {
    #[serde(default)]
    pub eval: EvalSpec,
    #[serde(default)]
    pub samples: Vec<EvalSample>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EvalSpec {
    #[serde(default)]
    pub eval_id: String,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub task_display_name: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalSample {
    pub id: Value,
    #[serde(default = "default_epoch")]
    pub epoch: i64,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub target: Value,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub scores: BTreeMap<String, EvalScore>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub model_usage: BTreeMap<String, ModelUsage>,
    #[serde(default)]
    pub total_time: Option<f64>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalScore {
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
}

fn default_epoch() -> i64 {
    1
}

pub fn eval_sample_schema() -> TableSchema {
    TableSchema::builder(EVAL_SAMPLE_TABLE)
        .field("sample_id", FieldType::String)
        .required_field("eval_id", FieldType::String)
        .required_field("task", FieldType::String)
        .required_field("model", FieldType::String)
        .required_field("epoch", FieldType::I64)
        .required_field("status", FieldType::String)
        .field("error", FieldType::String)
        .field("scorer", FieldType::String)
        .field("score", FieldType::F64)
        .field("total_tokens", FieldType::I64)
        .field("total_time_s", FieldType::F64)
        .field("completed_at", FieldType::Timestamp)
        .field("trace_id", FieldType::String)
        .field("input", FieldType::Json)
        .field("target", FieldType::Json)
        .field("output", FieldType::Json)
        .field("scores", FieldType::Json)
        .primary_key("sample_id")
        .build()
        .expect("the inspect-ai sample schema is valid")
}

/// Flattens one eval log into sample records. `scorer` picks which entry of
/// `scores` becomes the typed `score` column; the first scorer by name is used
/// when it is unset.
pub fn sample_records(log: &EvalLog, scorer: Option<&str>) -> Result<Vec<Record>, RecordError> {
    log.samples
        .iter()
        .map(|sample| sample_record(log, sample, scorer))
        .collect()
}

pub async fn ingest_eval_log(
    db: &TraceLakeDb,
    tenant_id: &str,
    namespace: &str,
    log: &EvalLog,
) -> Result<usize, IngestError> {
    let schema = eval_sample_schema();
    let records = sample_records(log, None)?;

    db.put_records(tenant_id, namespace, &schema, &records)
        .await?;

    Ok(records.len())
}

fn sample_record(
    log: &EvalLog,
    sample: &EvalSample,
    scorer: Option<&str>,
) -> Result<Record, RecordError> {
    let sample_id = sample_key(&sample.id, sample.epoch)?;
    let (scorer_name, score) = score_column(sample, scorer);

    let mut record = Record::new()
        .set("sample_id", Value::from(sample_id))
        .set("eval_id", Value::from(log.eval.eval_id.clone()))
        .set("task", Value::from(log.eval.task.clone()))
        .set("model", Value::from(log.eval.model.clone()))
        .set("epoch", Value::from(sample.epoch))
        .set(
            "status",
            Value::from(if sample.error.is_some() {
                "error"
            } else {
                "success"
            }),
        );

    if let Some(error) = &sample.error {
        record = record.set("error", Value::from(error.clone()));
    }
    if let Some(name) = scorer_name {
        record = record.set("scorer", Value::from(name));
    }
    if let Some(score) = score {
        record = record.set("score", Value::from(score));
    }

    let total_tokens: i64 = sample
        .model_usage
        .values()
        .map(|usage| usage.total_tokens)
        .sum();
    if total_tokens != 0 {
        record = record.set("total_tokens", Value::from(total_tokens));
    }
    if let Some(total_time) = sample.total_time {
        record = record.set("total_time_s", Value::from(total_time));
    }
    if let Some(completed_at) = sample.completed_at.as_deref().and_then(timestamp_nanos) {
        record = record.set("completed_at", Value::from(completed_at));
    }
    if let Some(trace_id) = sample.metadata.get("trace_id").and_then(Value::as_str) {
        record = record.set("trace_id", Value::from(trace_id));
    }

    for (name, value) in [
        ("input", &sample.input),
        ("target", &sample.target),
        ("output", &sample.output),
    ] {
        if !value.is_null() {
            record = record.set(name, value.clone());
        }
    }

    if !sample.scores.is_empty() {
        record = record.set("scores", scores_json(&sample.scores));
    }

    Ok(record)
}

fn scores_json(scores: &BTreeMap<String, EvalScore>) -> Value {
    let mut map = serde_json::Map::new();

    for (name, score) in scores {
        let mut entry = serde_json::Map::new();
        entry.insert("value".to_string(), score.value.clone());
        if let Some(answer) = &score.answer {
            entry.insert("answer".to_string(), Value::from(answer.clone()));
        }
        if let Some(explanation) = &score.explanation {
            entry.insert("explanation".to_string(), Value::from(explanation.clone()));
        }
        map.insert(name.clone(), Value::Object(entry));
    }

    Value::Object(map)
}

fn sample_key(id: &Value, epoch: i64) -> Result<String, RecordError> {
    let id = match id {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => {
            return Err(RecordError::TypeMismatch {
                name: "id".to_string(),
                expected: "string or number",
            });
        }
    };

    Ok(format!("{id}@{epoch}"))
}

fn score_column(sample: &EvalSample, scorer: Option<&str>) -> (Option<String>, Option<f64>) {
    let selected = match scorer {
        Some(name) => sample.scores.get_key_value(name),
        None => sample.scores.iter().next(),
    };

    match selected {
        Some((name, score)) => (Some(name.clone()), score_number(&score.value)),
        None => (None, None),
    }
}

/// inspect-ai stores a pass/fail as "C", "I", or "P" and a graded score as a
/// number or a numeric string. The typed column needs one number.
fn score_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => match text.as_str() {
            "C" | "correct" => Some(1.0),
            "I" | "incorrect" => Some(0.0),
            "P" | "partial" => Some(0.5),
            other => other.parse().ok(),
        },
        _ => None,
    }
}

fn timestamp_nanos(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|time| time.timestamp_nanos_opt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example_log() -> EvalLog {
        serde_json::from_value(json!({
            "eval": {
                "eval_id": "eval-abc",
                "task": "gsm8k",
                "task_display_name": "GSM8K",
                "model": "openai/gpt-4o",
                "created": "2026-10-01T12:00:00+00:00",
                "status": "success"
            },
            "samples": [
                {
                    "id": "17",
                    "epoch": 1,
                    "input": "What is 2 + 2?",
                    "target": "4",
                    "output": { "model": "openai/gpt-4o", "completion": "4" },
                    "scores": {
                        "accuracy": { "value": "C", "answer": "4", "explanation": "matches" },
                        "partial": { "value": 0.5 }
                    },
                    "metadata": { "trace_id": "trace-17" },
                    "model_usage": { "openai/gpt-4o": { "input_tokens": 12, "output_tokens": 3, "total_tokens": 15 } },
                    "total_time": 1.5,
                    "started_at": "2026-10-01T12:00:00+00:00",
                    "completed_at": "2026-10-01T12:00:02+00:00",
                    "error": null
                },
                {
                    "id": "18",
                    "epoch": 2,
                    "input": "What is 3 + 3?",
                    "target": "6",
                    "output": null,
                    "scores": {},
                    "metadata": {},
                    "model_usage": {},
                    "error": "model timeout"
                }
            ]
        }))
        .expect("fixture should parse")
    }

    #[test]
    fn fixture_parses_and_flattens_to_records() {
        let log = example_log();
        let schema = eval_sample_schema();
        let records = sample_records(&log, None).expect("records build");

        assert_eq!(records.len(), 2);
        schema
            .validate_all(&records)
            .expect("records match the schema");

        assert_eq!(records[0].get("sample_id"), Some(&json!("17@1")));
        assert_eq!(records[0].get("eval_id"), Some(&json!("eval-abc")));
        assert_eq!(records[0].get("task"), Some(&json!("gsm8k")));
        assert_eq!(records[0].get("model"), Some(&json!("openai/gpt-4o")));
        assert_eq!(records[0].get("epoch"), Some(&json!(1)));
        assert_eq!(records[0].get("status"), Some(&json!("success")));
        assert_eq!(records[0].get("scorer"), Some(&json!("accuracy")));
        assert_eq!(records[0].get("score"), Some(&json!(1.0)));
        assert_eq!(records[0].get("total_tokens"), Some(&json!(15)));
        assert_eq!(records[0].get("total_time_s"), Some(&json!(1.5)));
        assert_eq!(records[0].get("trace_id"), Some(&json!("trace-17")));
        assert_eq!(records[0].get("input"), Some(&json!("What is 2 + 2?")));
        assert_eq!(records[0].get("target"), Some(&json!("4")));
        let completed_at = DateTime::parse_from_rfc3339("2026-10-01T12:00:02+00:00")
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
        assert_eq!(records[0].get("completed_at"), Some(&json!(completed_at)));
        assert_eq!(
            records[0]
                .get("scores")
                .and_then(|value| value.get("partial")),
            Some(&json!({ "value": 0.5 }))
        );

        assert_eq!(records[1].get("sample_id"), Some(&json!("18@2")));
        assert_eq!(records[1].get("status"), Some(&json!("error")));
        assert_eq!(records[1].get("error"), Some(&json!("model timeout")));
        assert!(records[1].get("score").is_none());
        assert!(records[1].get("output").is_none());
    }

    #[test]
    fn a_named_scorer_selects_the_score_column() {
        let log = example_log();
        let records = sample_records(&log, Some("partial")).expect("records build");

        assert_eq!(records[0].get("scorer"), Some(&json!("partial")));
        assert_eq!(records[0].get("score"), Some(&json!(0.5)));
        assert!(records[1].get("score").is_none());
    }

    #[test]
    fn scorer_sentinels_and_numeric_strings_become_numbers() {
        let mut log = example_log();
        log.samples[0].scores.get_mut("accuracy").unwrap().value = json!("I");
        let records = sample_records(&log, Some("accuracy")).unwrap();
        assert_eq!(records[0].get("score"), Some(&json!(0.0)));

        log.samples[0].scores.get_mut("accuracy").unwrap().value = json!("P");
        let records = sample_records(&log, Some("accuracy")).unwrap();
        assert_eq!(records[0].get("score"), Some(&json!(0.5)));

        log.samples[0].scores.get_mut("accuracy").unwrap().value = json!("0.25");
        let records = sample_records(&log, Some("accuracy")).unwrap();
        assert_eq!(records[0].get("score"), Some(&json!(0.25)));

        log.samples[0].scores.get_mut("accuracy").unwrap().value = json!("not a score");
        let records = sample_records(&log, Some("accuracy")).unwrap();
        assert!(records[0].get("score").is_none());
    }

    #[test]
    fn non_scalar_sample_ids_are_rejected() {
        let mut log = example_log();
        log.samples[0].id = json!({ "nested": true });

        assert!(matches!(
            sample_records(&log, None),
            Err(RecordError::TypeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn eval_log_ingests_into_the_hot_store() {
        let db = TraceLakeDb::open_in_memory("/test-eval-ingest")
            .await
            .expect("db should open");
        let log = example_log();

        let written = ingest_eval_log(&db, "tenant-1", "evals", &log)
            .await
            .expect("ingest should succeed");
        assert_eq!(written, 2);

        let stored = db
            .get_table_records("tenant-1", "evals", EVAL_SAMPLE_TABLE)
            .await
            .expect("records should read back");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].get("sample_id"), Some(&json!("17@1")));
    }
}
