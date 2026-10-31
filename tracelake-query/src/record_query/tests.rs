use super::*;
use serde_json::json;
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;
use std::sync::Arc;
use tracelake_compact::{RecordCompactor, put_schema, read_record_manifest};
use tracelake_core::{FieldType, SegmentFileKind};
use tracelake_ingest::TraceLakeDb;

const BASE_PATH: &str = "tracelake/segments";

fn schema() -> TableSchema {
    TableSchema::builder("eval_samples")
        .field("sample_id", FieldType::String)
        .required_field("task", FieldType::String)
        .field("score", FieldType::F64)
        .field("completed_at", FieldType::Timestamp)
        .field("notes", FieldType::Json)
        .primary_key("sample_id")
        .build()
        .expect("schema should build")
}

fn record(id: &str, task: &str, score: f64) -> Record {
    Record::new()
        .set("sample_id", json!(id))
        .set("task", json!(task))
        .set("score", json!(score))
        .set("completed_at", json!(1_000 + id.len() as i64))
        .set(
            "notes",
            json!({ "text": "a long payload the projection can skip" }),
        )
}

async fn compact(store: &Arc<dyn ObjectStore>, records: &[Record]) {
    let schema = schema();
    let db = TraceLakeDb::open_in_memory("/record-query-compact")
        .await
        .expect("db should open");
    put_schema(store, BASE_PATH, "tenant-1", "default", &schema)
        .await
        .expect("schema should register");
    db.put_records("tenant-1", "default", &schema, records)
        .await
        .expect("records should write");
    RecordCompactor::new(Arc::clone(store), BASE_PATH)
        .compact(&db)
        .await
        .expect("compaction should succeed");
}

async fn reader_with_hot(store: &Arc<dyn ObjectStore>, hot: &[Record]) -> TraceLakeReader {
    put_schema(store, BASE_PATH, "tenant-1", "default", &schema())
        .await
        .expect("schema should register");
    let db = TraceLakeDb::open_in_memory("/record-query-hot")
        .await
        .expect("db should open");
    if !hot.is_empty() {
        db.put_records("tenant-1", "default", &schema(), hot)
            .await
            .expect("hot records should write");
    }
    TraceLakeReader::new(db, Arc::clone(store), BASE_PATH)
}

fn empty_store() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

#[tokio::test]
async fn a_query_against_an_unregistered_table_is_rejected() {
    let reader = reader_with_hot(&empty_store(), &[]).await;

    let error = reader
        .query_records("tenant-1", "default", "missing", &RecordQuery::default())
        .await
        .expect_err("missing schema must fail");

    assert!(matches!(error, QueryError::SchemaMissing { .. }));
}

#[tokio::test]
async fn invalid_filters_and_projections_are_rejected_before_any_read() {
    let store = empty_store();
    put_schema(&store, BASE_PATH, "tenant-1", "default", &schema())
        .await
        .expect("schema should register");
    let reader = reader_with_hot(&store, &[]).await;

    let undeclared = RecordQuery {
        filter: Some(RecordFilter::eq("nope", json!("x"))),
        ..RecordQuery::default()
    };
    let error = reader
        .query_records("tenant-1", "default", "eval_samples", &undeclared)
        .await
        .expect_err("undeclared field must fail");
    assert!(matches!(error, QueryError::InvalidQuery { .. }));

    let mistyped = RecordQuery {
        filter: Some(RecordFilter::eq("score", json!("high"))),
        ..RecordQuery::default()
    };
    let error = reader
        .query_records("tenant-1", "default", "eval_samples", &mistyped)
        .await
        .expect_err("wrong filter type must fail");
    assert!(matches!(error, QueryError::Record(_)));

    let bad_projection = RecordQuery {
        projection: vec!["nope".to_string()],
        ..RecordQuery::default()
    };
    let error = reader
        .query_records("tenant-1", "default", "eval_samples", &bad_projection)
        .await
        .expect_err("undeclared projection must fail");
    assert!(matches!(error, QueryError::InvalidQuery { .. }));
}

#[tokio::test]
async fn queries_read_hot_records_with_filters_and_projection() {
    let reader = reader_with_hot(
        &empty_store(),
        &[
            record("sample-a", "gsm8k", 0.9),
            record("sample-b", "gsm8k", 0.2),
            record("sample-c", "math", 0.7),
        ],
    )
    .await;

    let query = RecordQuery {
        filter: Some(RecordFilter::and(vec![
            RecordFilter::eq("task", json!("gsm8k")),
            RecordFilter::ge("score", json!(0.5)),
        ])),
        projection: vec!["score".to_string()],
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &query)
        .await
        .expect("query should succeed");

    assert_eq!(result.records.len(), 1);
    let found = &result.records[0];
    assert_eq!(found.get("sample_id"), Some(&json!("sample-a")));
    assert_eq!(found.get("score"), Some(&json!(0.9)));
    assert_eq!(
        found.fields.keys().cloned().collect::<Vec<_>>(),
        vec!["sample_id".to_string(), "score".to_string()]
    );
}

#[tokio::test]
async fn queries_merge_compacted_and_hot_records_with_hot_winning() {
    let store = empty_store();
    compact(&store, &[record("sample-a", "gsm8k", 0.1)]).await;

    let db = TraceLakeDb::open_in_memory("/record-query-override")
        .await
        .expect("db should open");
    let updated = record("sample-a", "gsm8k", 0.95);
    db.put_records(
        "tenant-1",
        "default",
        &schema(),
        std::slice::from_ref(&updated),
    )
    .await
    .expect("hot record should write");
    let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

    let query = RecordQuery {
        filter: Some(RecordFilter::ge("score", json!(0.9))),
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &query)
        .await
        .expect("query should succeed");

    assert_eq!(result.records, vec![updated]);
}

#[tokio::test]
async fn stats_pruning_skips_segments_the_filter_cannot_match() {
    let store = empty_store();
    let low: Vec<Record> = (0..4)
        .map(|index| record(&format!("low-{index}"), "gsm8k", 0.1))
        .collect();
    compact(&store, &low).await;
    let high: Vec<Record> = (0..4)
        .map(|index| record(&format!("high-{index}"), "math", 0.9))
        .collect();
    compact(&store, &high).await;

    let manifest = read_record_manifest(&store, BASE_PATH, "tenant-1", "default", "eval_samples")
        .await
        .expect("manifest should read")
        .expect("manifest should exist");
    assert_eq!(manifest.segments.len(), 2);

    let pruned_pack = manifest.segments[0]
        .files
        .iter()
        .find(|file| file.kind == SegmentFileKind::RecordPack)
        .expect("pack should exist")
        .path
        .clone();
    store
        .delete(&Path::from(pruned_pack.as_str()))
        .await
        .expect("pack should delete");

    let reader = reader_with_hot(&store, &[]).await;
    let query = RecordQuery {
        filter: Some(RecordFilter::ge("score", json!(0.5))),
        limit: 10,
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &query)
        .await
        .expect("pruned query must not read the deleted pack");

    assert_eq!(result.scanned_segments, 1);
    assert_eq!(result.pruned_segments, 1);
    assert_eq!(result.records.len(), 4);
}

#[tokio::test]
async fn string_distinct_stats_prune_by_exact_value() {
    let store = empty_store();
    compact(&store, &[record("a", "gsm8k", 0.5)]).await;

    let reader = reader_with_hot(&store, &[]).await;
    let query = RecordQuery {
        filter: Some(RecordFilter::eq("task", json!("math"))),
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &query)
        .await
        .expect("query should succeed");

    assert_eq!(result.pruned_segments, 1);
    assert_eq!(result.scanned_segments, 0);
    assert!(result.records.is_empty());
}

#[tokio::test]
async fn get_record_reads_cold_records_and_prefers_hot_copies() {
    let store = empty_store();
    compact(&store, &[record("sample-a", "gsm8k", 0.1)]).await;

    let reader = reader_with_hot(&store, &[]).await;
    let cold = reader
        .get_record("tenant-1", "default", "eval_samples", "sample-a")
        .await
        .expect("lookup should succeed")
        .expect("record should exist");
    assert_eq!(cold.get("score"), Some(&json!(0.1)));

    let absent = reader
        .get_record("tenant-1", "default", "eval_samples", "absent")
        .await
        .expect("absent lookup should succeed");
    assert_eq!(absent, None);

    let db = TraceLakeDb::open_in_memory("/record-get-override")
        .await
        .expect("db should open");
    let updated = record("sample-a", "gsm8k", 0.99);
    db.put_records(
        "tenant-1",
        "default",
        &schema(),
        std::slice::from_ref(&updated),
    )
    .await
    .expect("hot record should write");
    let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

    let hot = reader
        .get_record("tenant-1", "default", "eval_samples", "sample-a")
        .await
        .expect("lookup should succeed")
        .expect("record should exist");
    assert_eq!(hot, updated);
}

#[tokio::test]
async fn limits_and_ordering_are_deterministic() {
    let records: Vec<Record> = (0..5)
        .map(|index| record(&format!("sample-{index}"), "gsm8k", 0.5))
        .collect();
    let reader = reader_with_hot(&empty_store(), &records).await;

    let query = RecordQuery {
        limit: 3,
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &query)
        .await
        .expect("query should succeed");

    let ids: Vec<&serde_json::Value> = result
        .records
        .iter()
        .map(|r| r.get("sample_id").unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![&json!("sample-0"), &json!("sample-1"), &json!("sample-2")]
    );
}

#[tokio::test]
async fn or_filters_match_any_branch_and_absent_fields_never_match() {
    let reader = reader_with_hot(
        &empty_store(),
        &[
            record("a", "gsm8k", 0.9),
            record("b", "math", 0.9),
            Record::new()
                .set("sample_id", json!("c"))
                .set("task", json!("gsm8k")),
        ],
    )
    .await;

    let query = RecordQuery {
        filter: Some(RecordFilter::Or(vec![
            RecordFilter::eq("task", json!("math")),
            RecordFilter::eq("sample_id", json!("a")),
        ])),
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &query)
        .await
        .expect("query should succeed");

    let mut ids: Vec<&str> = result
        .records
        .iter()
        .filter_map(|record| record.get("sample_id").and_then(|value| value.as_str()))
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "b"]);

    let missing = RecordQuery {
        filter: Some(RecordFilter::eq("score", json!(0.9))),
        ..RecordQuery::default()
    };
    let result = reader
        .query_records("tenant-1", "default", "eval_samples", &missing)
        .await
        .expect("query should succeed");
    assert_eq!(result.records.len(), 2);
}

fn bench_schema() -> TableSchema {
    TableSchema::builder("bench_samples")
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
        .expect("bench schema builds")
}

fn bench_record(segment: usize, index: usize) -> Record {
    let payload = "the model explains the refund policy for order 8837. ".repeat(20);
    Record::new()
        .set("sample_id", json!(format!("seg{segment}-rec{index:05}")))
        .set("eval_id", json!("eval-bench"))
        .set("task", json!(if index.is_multiple_of(2) { "gsm8k" } else { "math" }))
        .set("model", json!("openai/gpt-4o"))
        .set("epoch", json!(1))
        .set(
            "status",
            json!(if index.is_multiple_of(97) {
                "error"
            } else {
                "success"
            }),
        )
        .set("scorer", json!("accuracy"))
        .set(
            "score",
            json!(segment as f64 / 10.0 + index as f64 / 10_000.0),
        )
        .set("total_tokens", json!(15 + index as i64 % 500))
        .set("total_time_s", json!(1.5))
        .set(
            "completed_at",
            json!(1_790_856_000_000_000_000i64 + index as i64),
        )
        .set("trace_id", json!(format!("trace-{segment}-{index}")))
        .set("input", json!({ "prompt": payload }))
        .set("target", json!("81"))
        .set("output", json!({ "completion": payload }))
        .set("scores", json!({ "accuracy": { "value": 1.0 } }))
}

#[tokio::test]
#[ignore = "benchmark: 10K records over 10 segments; run with --release --ignored --nocapture"]
async fn bench_record_query_pruning_and_projection() {
    use std::time::{Duration, Instant};

    const SEGMENTS: usize = 10;
    const PER_SEGMENT: usize = 1_000;
    const RUNS: usize = 5;

    let store = empty_store();
    let schema = bench_schema();
    put_schema(&store, BASE_PATH, "tenant-1", "default", &schema)
        .await
        .expect("schema registers");

    let mut bytes_per_record = 0usize;
    for segment in 0..SEGMENTS {
        let records: Vec<Record> = (0..PER_SEGMENT)
            .map(|index| bench_record(segment, index))
            .collect();
        bytes_per_record = serde_json::to_vec(&records[0])
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        let db = TraceLakeDb::open_in_memory(&format!("/bench-record-{segment}"))
            .await
            .expect("db opens");
        db.put_records("tenant-1", "default", &schema, &records)
            .await
            .expect("records write");
        RecordCompactor::new(Arc::clone(&store), BASE_PATH)
            .compact(&db)
            .await
            .expect("compaction succeeds");
    }

    let reader = reader_with_hot(&store, &[]).await;
    let all_fields: Vec<String> = schema
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();

    let full_scan = RecordQuery {
        filter: None,
        projection: all_fields,
        limit: SEGMENTS * PER_SEGMENT,
    };
    let projected_scan = RecordQuery {
        filter: None,
        projection: vec!["sample_id".to_string(), "score".to_string()],
        limit: SEGMENTS * PER_SEGMENT,
    };
    let no_prune = RecordQuery {
        filter: Some(RecordFilter::ge("score", json!(-1.0))),
        projection: vec!["sample_id".to_string(), "score".to_string()],
        limit: SEGMENTS * PER_SEGMENT,
    };
    let pruned = RecordQuery {
        filter: Some(RecordFilter::ge("score", json!(0.95))),
        projection: vec!["sample_id".to_string(), "score".to_string()],
        limit: SEGMENTS * PER_SEGMENT,
    };

    let mut samples: Vec<(&str, Vec<Duration>)> = vec![
        ("full_scan", Vec::new()),
        ("projected_scan", Vec::new()),
        ("filtered_no_prune", Vec::new()),
        ("filtered_pruned", Vec::new()),
        ("point_lookup", Vec::new()),
    ];

    for _ in 0..RUNS {
        for (name, times) in samples.iter_mut() {
            let started = Instant::now();
            match *name {
                "full_scan" => {
                    let result = reader
                        .query_records("tenant-1", "default", "bench_samples", &full_scan)
                        .await
                        .expect("full scan");
                    assert_eq!(result.records.len(), SEGMENTS * PER_SEGMENT);
                }
                "projected_scan" => {
                    let result = reader
                        .query_records("tenant-1", "default", "bench_samples", &projected_scan)
                        .await
                        .expect("projected scan");
                    assert_eq!(result.records.len(), SEGMENTS * PER_SEGMENT);
                    assert_eq!(result.pruned_segments, 0);
                }
                "filtered_no_prune" => {
                    let result = reader
                        .query_records("tenant-1", "default", "bench_samples", &no_prune)
                        .await
                        .expect("unpruned filter");
                    assert_eq!(result.records.len(), SEGMENTS * PER_SEGMENT);
                    assert_eq!(result.pruned_segments, 0);
                }
                "filtered_pruned" => {
                    let result = reader
                        .query_records("tenant-1", "default", "bench_samples", &pruned)
                        .await
                        .expect("pruned filter");
                    assert_eq!(result.pruned_segments, (SEGMENTS - 1) as u64);
                    assert_eq!(result.scanned_segments, 1);
                    // Segment 9 holds scores in [0.9, 0.9999); half clear 0.95.
                    assert_eq!(result.records.len(), PER_SEGMENT / 2);
                }
                _ => {
                    let record = reader
                        .get_record("tenant-1", "default", "bench_samples", "seg9-rec00042")
                        .await
                        .expect("lookup")
                        .expect("record exists");
                    assert_eq!(record.get("score"), Some(&json!(0.9 + 42.0 / 10_000.0)));
                }
            }
            times.push(started.elapsed());
        }
    }

    println!(
        "record query bench: {SEGMENTS} segments x {PER_SEGMENT} records, \
         ~{bytes_per_record} B/record, in-memory object store, release build"
    );
    for (name, times) in &samples {
        let mut sorted = times.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        println!(
            "{name}: median={:?} min={:?} max={:?}",
            median,
            sorted[0],
            sorted[sorted.len() - 1]
        );
    }
}
