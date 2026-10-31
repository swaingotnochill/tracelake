//! TraceLake benchmark suite. Run with `cargo run --release -p tracelake-bench`.
//!
//! Every phase prints the dataset shape, the elapsed time, and a correctness
//! count, so a timing cannot pass for work that did not happen.

mod data_gen;

use data_gen::CorpusConfig;
use slatedb::object_store::ObjectStore;
use slatedb::object_store::memory::InMemory;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracelake_compact::Compactor;
use tracelake_core::{Span, SpanStatus};
use tracelake_ingest::TraceLakeDb;
use tracelake_query::{AggregateMetric, AggregateRequest, TraceLakeReader, TraceSearch};

const BASE_PATH: &str = "tracelake/segments";
const TENANT: &str = "tenant-bench";
const NAMESPACE: &str = "default";
const FETCH_SAMPLE: usize = 100;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args();
    let corpus = data_gen::traces(config);
    let spans: Vec<Span> = corpus.iter().flatten().cloned().collect();
    let input_bytes: usize = spans
        .iter()
        .map(|span| {
            serde_json::to_vec(span)
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        })
        .sum();

    println!(
        "dataset: {} traces x {} spans = {} spans, payload {} B per field, \
         ~{} MB serialized, in-memory object store",
        config.trace_count,
        config.spans_per_trace,
        config.span_count(),
        config.payload_bytes,
        input_bytes / 1_000_000
    );

    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let db = TraceLakeDb::open_in_memory("/bench-ingest").await?;

    let started = Instant::now();
    db.put_spans(&spans).await?;
    let ingest = started.elapsed();
    println!(
        "ingest: {:?} ({:.0} spans/s)",
        ingest,
        rate(config.span_count(), ingest)
    );

    let hot = reader(&db, &store);
    let fetch_hot = time_fetch(&hot, config.trace_count, config.spans_per_trace).await?;
    let failed = failed_search();
    let search_hot = time_search(&hot, &failed, "search_hot").await?;

    let started = Instant::now();
    let compacted = Compactor::new(Arc::clone(&store), BASE_PATH)
        .compact(&db)
        .await?;
    let compact = started.elapsed();
    println!(
        "compact: {:?} ({:.0} spans/s, {} segments)",
        compact,
        rate(config.span_count(), compact),
        compacted.len()
    );

    let cold_db = TraceLakeDb::open_in_memory("/bench-cold").await?;
    let cold = reader(&cold_db, &store);

    let first = Instant::now();
    let trace = cold
        .get_trace(TENANT, NAMESPACE, "trace-0000000")
        .await?
        .expect("compacted trace exists");
    println!(
        "fetch_cold_first: {:?} ({} spans)",
        first.elapsed(),
        trace.spans.len()
    );

    let fetch_cold = time_fetch(&cold, config.trace_count, config.spans_per_trace).await?;
    let search_cold = time_search(&cold, &failed, "search_cold").await?;
    let text_search = time_search(
        &cold,
        &TraceSearch {
            text_query: Some("timeout".to_string()),
            ..TraceSearch::default()
        },
        "text_search_cold",
    )
    .await?;

    println!("fetch_hot: {:.0} us/trace", micros(fetch_hot, FETCH_SAMPLE));
    println!(
        "fetch_cold_warm: {:.0} us/trace",
        micros(fetch_cold, FETCH_SAMPLE)
    );
    println!(
        "search_hot (status=error): {:?}, {} traces",
        search_hot.0, search_hot.1
    );
    println!(
        "search_cold (status=error): {:?}, {} traces",
        search_cold.0, search_cold.1
    );
    println!(
        "text_search_cold (\"timeout\"): {:?}, {} traces",
        text_search.0, text_search.1
    );

    let started = Instant::now();
    let aggregate = cold
        .aggregate_spans(
            TENANT,
            NAMESPACE,
            &AggregateRequest {
                group_by: vec!["model".to_string()],
                metrics: vec![
                    AggregateMetric::Count,
                    AggregateMetric::P95 {
                        field: "latency_ms".to_string(),
                    },
                    AggregateMetric::Sum {
                        field: "cost_usd".to_string(),
                    },
                ],
                limit: 10,
            },
        )
        .await?;
    let aggregate_elapsed = started.elapsed();
    assert_eq!(aggregate.rows.len(), 2, "one row per model");
    println!(
        "aggregate_spans (group by model, count/p95/sum): {:?}, {} groups",
        aggregate_elapsed,
        aggregate.rows.len()
    );

    Ok(())
}

fn reader(db: &TraceLakeDb, store: &Arc<dyn ObjectStore>) -> TraceLakeReader {
    TraceLakeReader::new(db.clone(), Arc::clone(store), BASE_PATH)
}

async fn time_fetch(
    reader: &TraceLakeReader,
    trace_count: usize,
    spans_per_trace: usize,
) -> Result<Duration, Box<dyn Error>> {
    let sample = trace_count.min(FETCH_SAMPLE);
    let started = Instant::now();

    for trace in 0..sample {
        let trace = reader
            .get_trace(TENANT, NAMESPACE, &format!("trace-{trace:07}"))
            .await?
            .expect("trace exists");
        assert_eq!(trace.spans.len(), spans_per_trace);
    }

    Ok(started.elapsed())
}

async fn time_search(
    reader: &TraceLakeReader,
    search: &TraceSearch,
    label: &str,
) -> Result<(Duration, usize), Box<dyn Error>> {
    let started = Instant::now();
    let summaries = reader.search_traces(TENANT, NAMESPACE, search).await?;
    assert!(!summaries.is_empty(), "{label} must match the corpus");
    Ok((started.elapsed(), summaries.len()))
}

fn micros(elapsed: Duration, count: usize) -> f64 {
    elapsed.as_micros() as f64 / count.max(1) as f64
}

fn rate(count: usize, elapsed: Duration) -> f64 {
    count as f64 / elapsed.as_secs_f64()
}

fn parse_args() -> CorpusConfig {
    let mut config = CorpusConfig::default();
    let args: Vec<String> = std::env::args().skip(1).collect();

    for pair in args.chunks(2) {
        let Some(value) = pair.get(1).and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        match pair[0].as_str() {
            "--traces" => config.trace_count = value,
            "--spans" => config.spans_per_trace = value,
            "--payload" => config.payload_bytes = value,
            _ => {}
        }
    }

    config
}

fn failed_search() -> TraceSearch {
    TraceSearch {
        status: Some(SpanStatus::Error),
        ..TraceSearch::default()
    }
}
