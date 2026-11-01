# TraceLake

A purpose-built database for LLM agent traces. Stores trace data in object storage (S3/GCS),
uses NVMe/memory as hot cache, and beats generic databases on trace-native workloads.

## What It Does

- **Ingest** spans from any LLM app over HTTP.
- **Fetch** a full trace tree in a single read — not a distributed row reconstruction.
- **Search** prompts, outputs, tool errors with a Tantivy full-text index.
- **Aggregate** latency, cost, tokens, scores with columnar analytics.
- **Define tables** with your own typed schema, including inspect-ai eval samples.

One database. Three endpoints. No ClickHouse bolt-on. No Elasticsearch bolt-on.

## API

Three endpoints for traces, plus a small table API for user-defined schemas and eval
records. No index creation, no separate analytics system.

### Ingest

```rust
async fn ingest_spans(request: IngestRequest) -> Result<IngestResponse>;

struct IngestRequest {
    spans: Vec<Span>,        // batch of spans, can belong to multiple traces
    idempotency_keys: bool,  // use span_id as idempotency key (default: true)
}

struct IngestResponse {
    accepted: usize,
    rejected: usize,
}
```

### Query

```rust
// Fetch a single trace by ID with all its spans.
async fn get_trace(trace_id: &str) -> Result<Option<Trace>>;

// Search traces with filters. Call get_trace() to hydrate full payloads.
async fn search_traces(request: SearchRequest) -> Result<SearchResponse>;

// Run an aggregation query over columnar data.
async fn aggregate(request: AggregateRequest) -> Result<AggregateResponse>;
```

### Search Filters

All optional, all AND-combined. Use exactly one of `trace_ids` or `text_query` or the
filter set. `limit` is required (max 1000).

```rust
struct SearchRequest {
    trace_ids: Option<Vec<String>>,
    text_query: Option<String>,          // "hallucination AND tool_error"
    trace_id_prefix: Option<String>,
    span_types: Option<Vec<SpanType>>,   // LLM, Tool, Agent, Chain, Eval, System, Retriever
    statuses: Option<Vec<SpanStatus>>,   // Ok, Error, Timeout
    models: Option<Vec<String>>,
    tags: Option<Vec<String>>,
    min_latency_ms: Option<f64>,
    max_latency_ms: Option<f64>,
    min_cost_usd: Option<f64>,
    max_cost_usd: Option<f64>,
    min_score: Option<f64>,
    min_start_time: Option<DateTime<Utc>>,
    max_start_time: Option<DateTime<Utc>>,
    limit: usize,
    offset: usize,
    order_by: SearchOrderBy,             // Time, Latency, Cost, Score, Relevance
    order_dir: OrderDir,                 // Asc, Desc
    include_spans: bool,                 // default: true
    include_attributes: bool,            // default: false
}
```

### Aggregation

```rust
struct AggregateRequest {
    metric: AggregateMetric,             // Latency, Cost, Tokens, Count
    aggregation: AggFunction,            // P50, P95, P99, Avg, Sum, Min, Max, Count
    group_by: Vec<GroupBy>,              // Model, SpanType, Status, Hour, Day
    filters: SearchFilters,               // same filter set as search_traces
    time_range: TimeRange,                // required
}
```

### Data Types

```rust
struct Span {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    span_type: SpanType,
    name: String,
    start_time: DateTime<Utc>,
    end_time: Option<DateTime<Utc>>,
    status: SpanStatus,
    attributes: serde_json::Value,
    input: Option<serde_json::Value>,
    output: Option<serde_json::Value>,
    scores: Vec<Score>,
    tokens: Option<TokenUsage>,
    cost_usd: Option<f64>,
    model: Option<String>,
    provider: Option<String>,
    error: Option<String>,
    tags: Vec<String>,
}

struct Trace {
    trace_id: String,
    spans: Vec<Span>,
}

enum SpanType { LLM, Tool, Retriever, Agent, Chain, Eval, System }
enum SpanStatus { Ok, Error, Timeout }
struct Score { name: String, value: f64 }
struct TokenUsage { prompt: u32, completion: u32, total: u32 }
```

### Custom tables

Define a typed schema, register it, and query it with typed filters and projection.

```rust
let schema = TableSchema::builder("inspect_ai_samples")
    .field("sample_id", FieldType::String)
    .required_field("task", FieldType::String)
    .field("score", FieldType::F64)
    .primary_key("sample_id")
    .build()?;

put_schema(&store, base_path, tenant_id, namespace, &schema).await?;
db.put_records(tenant_id, namespace, &schema, &records).await?;

let result = reader
    .query_records(tenant_id, namespace, "inspect_ai_samples", &RecordQuery {
        filter: Some(RecordFilter::ge("score", json!(0.5))),
        projection: vec!["task".into(), "score".into()],
        ..RecordQuery::default()
    })
    .await?;
```

Schemas evolve. Adding an optional field, adding a required field with a default, and
dropping a field are allowed; older segments keep their meaning because each pack records
its own field list. Changing a field's type migrates stored packs and columns to
the new type when every value converts.

Aggregation runs over the Parquet column files that compaction writes:

```rust
let result = reader
    .aggregate_records(tenant_id, namespace, "inspect_ai_samples", &AggregateRequest {
        group_by: vec!["task".into()],
        metrics: vec![
            AggregateMetric::Count,
            AggregateMetric::Avg { field: "score".into() },
            AggregateMetric::P95 { field: "total_tokens".into() },
        ],
        ..AggregateRequest::default()
    })
    .await?;
```

`POST /v1/spans/aggregate` does the same for span columns, for example p95
latency grouped by model.

`POST /v1/evals` registers the eval schema, flattens an inspect-ai log, and writes one row
per sample. See [docs/schemas.md](docs/schemas.md) for field types, storage layout, query
semantics, pruning rules, and measured costs.

A runnable example, a real inspect-ai eval against GLM-4.7 plus the database queries on
its log, is in [examples/inspect-ai-evals](examples/inspect-ai-evals). Run
`examples/inspect-ai-evals/run.sh` to regenerate the log and the query output.

## Documentation

A static documentation site lives in `site/`. Open `site/index.html`, or serve it:

```bash
python3 site/build.py
python3 -m http.server -d site 8080
```

It covers the HTTP API, custom schemas, architecture, operations, security,
performance, verification, and research notes. The site is generated from the
repository Markdown, so the pages and the source cannot drift.

## Architecture

Database decisions, storage layout, key encoding, read/write paths, and every
technical choice with the reasoning.

See [architecture.md](docs/architecture.md).

## Development Guide

Build order, coding rules, profiling tools, dependencies.

See [AGENTS.md](AGENTS.md).

## Quick Start

The runnable application is the `tracelake-server` package. Its binary is also named `tracelake-server`. The root `tracelake` package is a workspace placeholder, so name the server package explicitly.

### Run the server

```bash
cargo run -p tracelake-server
```

The server listens on `http://127.0.0.1:8080`. It creates `./tracelake-data` and keeps the SlateDB hot store under `hot/` and compacted segments under `segments/`. On startup it prints the address, the selected store, the read limits, and the main endpoints.

To change the address or data directory:

```bash
TRACELAKE_ADDR=0.0.0.0:8080 \
TRACELAKE_DATA_DIR=/var/lib/tracelake \
cargo run -p tracelake-server
```

The default bind address is loopback because the alpha has no authentication. Keep it on loopback or a private network, or put an authenticating proxy in front of it.

### Configuration

| Variable | Default | Purpose |
|----------|---------|---------|
| `TRACELAKE_ADDR` | `127.0.0.1:8080` | Bind address |
| `TRACELAKE_STORE` | `local` | `local`, `s3`, or `gcs` |
| `TRACELAKE_DATA_DIR` | `./tracelake-data` | Local store root |
| `TRACELAKE_BUCKET` | unset | Bucket for S3 or GCS |
| `TRACELAKE_REGION` | provider default | S3 region |
| `TRACELAKE_ENDPOINT` | unset | S3-compatible endpoint such as MinIO |
| `TRACELAKE_PREFIX` | `tracelake` | Prefix for remote hot and segment paths |
| `TRACELAKE_QUERY_TIMEOUT_MS` | `30000` | Read timeout |
| `TRACELAKE_TENANT_CONCURRENCY` | `16` | Concurrent reads per tenant |

S3 and GCS use the provider credential chain (`AWS_*` or `GOOGLE_*`). The server does not read access keys from its own settings. For S3-compatible storage:

```bash
TRACELAKE_STORE=s3 \
TRACELAKE_BUCKET=tracelake \
TRACELAKE_REGION=us-east-1 \
TRACELAKE_ENDPOINT=http://127.0.0.1:9000 \
cargo run -p tracelake-server
```

For MinIO, set `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` in the environment. Run one server process per local data directory. Two processes against one local directory are untested.

### Send a trace

With the server running, ingest one span:

```bash
curl -sS http://127.0.0.1:8080/v1/spans \
  -H 'content-type: application/json' \
  -d '{
    "spans": [{
      "tenant_id": "acme",
      "namespace": "prod",
      "trace_id": "trace-1",
      "span_id": "span-a",
      "parent_span_id": null,
      "span_type": "llm",
      "name": "completion",
      "start_time": "2026-11-01T12:00:00Z",
      "end_time": "2026-11-01T12:00:01Z",
      "status": "ok",
      "attributes": {"env": "local"},
      "input": {"prompt": "hello"},
      "output": {"text": "world"},
      "tokens": {"prompt": 10, "completion": 20, "total": 30},
      "cost_usd": 0.001,
      "model": "gpt-4o",
      "provider": "openai",
      "error": null,
      "tags": ["smoke"],
      "scores": [{"name": "quality", "value": 0.9}]
    }]
  }'
```

The response is `{"written":1}`.

Fetch the trace:

```bash
curl -sS http://127.0.0.1:8080/v1/traces/acme/prod/trace-1
```

New spans are served from the hot store. To publish segments, compact the hot store:

```bash
curl -sS -X POST http://127.0.0.1:8080/v1/compact
```

Search the trace:

```bash
curl -sS http://127.0.0.1:8080/v1/traces/search \
  -H 'content-type: application/json' \
  -d '{"tenant_id":"acme","namespace":"prod","model":"gpt-4o","limit":10}'
```

The full HTTP API, including tables, eval ingestion, retention, and aggregation, is in [docs/api.md](docs/api.md).

### Build and test

```bash
cargo build --workspace
cargo nextest run --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Benchmark and profile

```bash
cargo run --release -p tracelake-bench
cargo build --profile profiling --bin tracelake-server
cargo flamegraph --profile profiling --bin tracelake-server
```

## Project Structure

```
tracelake/
  AGENTS.md                 # coding rules, build steps, profiling, dependencies
  architecture.md           # database architecture, decisions, storage, data flow
  idea.md                   # thesis and competitive analysis
  README.md                 # what it is, API surface, quick start
  Cargo.toml                # workspace root

  tracelake-core/           # data types, key encoding, serialization
  tracelake-ingest/         # SlateDB integration, ingest endpoint
  tracelake-compact/        # compaction pipeline (trace packs, columns, text index)
  tracelake-query/          # query engine (fetch, search, aggregate)
  tracelake-server/         # HTTP server
  tracelake-bench/          # benchmark suite
```

## Status

Steps 1 to 11 of the build order are implemented: ingest, trace fetch, search,
Tantivy text search, Parquet column files with DataFusion aggregation, the HTTP
API, custom table schemas with evolution, inspect-ai eval storage, and the
benchmark suite. Alpha readiness, security posture, and gaps against SmithDB and
Brainstore are recorded in [docs/alpha.md](docs/alpha.md),
[docs/security.md](docs/security.md), and
[docs/research/gap-analysis.md](docs/research/gap-analysis.md). Measured
latency and the range-read fix are in [docs/performance.md](docs/performance.md).

## License

TraceLake is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
