//! Ingests an inspect-ai eval log and runs real TraceLake queries.
//!
//!     cargo run -p tracelake-server --example inspect_ai_demo
//!
//! The default log is `examples/inspect-ai-evals/evals.json`, produced by
//! `inspect eval` and `inspect log dump` in that folder.

use serde_json::Value;
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;
use std::error::Error;
use std::fs;
use std::sync::Arc;
use tracelake_compact::{RecordCompactor, put_schema};
use tracelake_core::{FieldType, TableSchema};
use tracelake_ingest::{EvalLog, TraceLakeDb, eval_sample_schema, ingest_eval_log};
use tracelake_query::{RecordFilter, RecordQuery, RecordQueryResult, TraceLakeReader};

const TENANT: &str = "demo";
const NAMESPACE: &str = "evals";
const BASE_PATH: &str = "tracelake/segments";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/inspect-ai-evals/evals.json".to_string());
    let log: EvalLog = serde_json::from_slice(&fs::read(&path)?)?;

    let db = TraceLakeDb::open_in_memory("/inspect-ai-demo").await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let schema = put_schema(&store, BASE_PATH, TENANT, NAMESPACE, &eval_sample_schema()).await?;

    println!(
        "eval {} on {}: {} samples, schema {} v{}",
        log.eval.eval_id,
        log.eval.model,
        log.samples.len(),
        schema.table,
        schema.version
    );

    let compactor = RecordCompactor::new(Arc::clone(&store), BASE_PATH);
    let split = log.samples.len() / 2;

    // Two batches with separate hot stores, which is what the compactor sees
    // once earlier hot data has been dropped. The batches get disjoint score
    // ranges, so the score filter has a segment it can prune.
    let mut first = log.clone();
    first.samples = log.samples[..split].to_vec();
    ingest_eval_log(&db, TENANT, NAMESPACE, &first).await?;
    compactor.compact(&db).await?;

    let second_db = TraceLakeDb::open_in_memory("/inspect-ai-demo-second").await?;
    let mut second = log.clone();
    second.samples = log.samples[split..].to_vec();
    ingest_eval_log(&second_db, TENANT, NAMESPACE, &second).await?;
    compactor.compact(&second_db).await?;

    let reader = TraceLakeReader::new(second_db, Arc::clone(&store), BASE_PATH);

    let all = reader
        .query_records(TENANT, NAMESPACE, &schema.table, &every_sample())
        .await?;
    print_rows("all samples across two segments", &all);

    let passing = reader
        .query_records(TENANT, NAMESPACE, &schema.table, &scored_at_least(0.5))
        .await?;
    print_rows("score >= 0.5", &passing);

    let sample = reader
        .get_record(TENANT, NAMESPACE, &schema.table, "arith-1@1")
        .await?;
    if let Some(sample) = sample {
        println!(
            "\npoint read arith-1@1: score={:?} status={:?} trace_id={:?}",
            sample.get("score"),
            sample.get("status"),
            sample.get("trace_id"),
        );
    }

    let evolved = evolve(&schema)?;
    let registered = put_schema(&store, BASE_PATH, TENANT, NAMESPACE, &evolved).await?;
    let after = reader
        .query_records(
            TENANT,
            NAMESPACE,
            &registered.table,
            &RecordQuery {
                filter: Some(RecordFilter::eq("sample_id", Value::from("arith-1@1"))),
                ..RecordQuery::default()
            },
        )
        .await?;
    let grader = after
        .records
        .first()
        .and_then(|record| record.get("grader"))
        .cloned()
        .unwrap_or(Value::Null);
    println!(
        "\nevolved {} to v{}: the older segment still reads, grader={grader}",
        registered.table, registered.version
    );

    Ok(())
}

fn every_sample() -> RecordQuery {
    RecordQuery {
        projection: vec![
            "sample_id".to_string(),
            "task".to_string(),
            "score".to_string(),
            "total_tokens".to_string(),
        ],
        limit: 50,
        ..RecordQuery::default()
    }
}

fn scored_at_least(score: f64) -> RecordQuery {
    RecordQuery {
        filter: Some(RecordFilter::ge("score", Value::from(score))),
        projection: vec![
            "sample_id".to_string(),
            "task".to_string(),
            "score".to_string(),
            "total_tokens".to_string(),
        ],
        limit: 50,
    }
}

fn evolve(schema: &TableSchema) -> Result<TableSchema, Box<dyn Error>> {
    let mut builder = TableSchema::builder(schema.table.clone());
    for field in &schema.fields {
        builder = match (&field.default, field.required) {
            (Some(default), _) => builder.required_field_with_default(
                field.name.clone(),
                field.field_type,
                default.clone(),
            ),
            (None, true) => builder.required_field(field.name.clone(), field.field_type),
            (None, false) => builder.field(field.name.clone(), field.field_type),
        };
    }

    Ok(builder
        .field("grader", FieldType::String)
        .primary_key(schema.primary_key.clone())
        .build()?)
}

fn print_rows(title: &str, result: &RecordQueryResult) {
    println!("\n{title}");
    println!(
        "{:<12} {:<16} {:>6} {:>8}",
        "sample_id", "task", "score", "tokens"
    );

    for record in &result.records {
        let text = |name: &str| {
            record
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string()
        };
        let score = record
            .get("score")
            .and_then(Value::as_f64)
            .map(|score| format!("{score:.2}"))
            .unwrap_or_else(|| "-".to_string());
        let tokens = record
            .get("total_tokens")
            .and_then(Value::as_i64)
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<12} {:<16} {:>6} {:>8}",
            text("sample_id"),
            text("task"),
            score,
            tokens
        );
    }

    println!(
        "{} rows, segments scanned {}, pruned {}",
        result.records.len(),
        result.scanned_segments,
        result.pruned_segments
    );
}
