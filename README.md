# TraceLake

A purpose-built database for LLM agent traces. Stores trace data in object storage (S3/GCS),
uses NVMe/memory as hot cache, and beats generic databases on trace-native workloads.

## What It Does

- **Ingest** spans from any LLM app (OTLP, HTTP, gRPC).
- **Fetch** a full trace tree in a single read — not a distributed row reconstruction.
- **Search** prompts, outputs, tool errors with full-text search.
- **Aggregate** latency, cost, tokens, scores with columnar analytics.

One database. Three endpoints. No ClickHouse bolt-on. No Elasticsearch bolt-on.

## API

Three endpoints. Push spans, query traces. No admin, no schema management, no index creation.

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

## Architecture

Database decisions, storage layout, key encoding, read/write paths, and every
technical choice with the reasoning.

See [architecture.md](docs/architecture.md).

## Development Guide

Build order, coding rules, profiling tools, dependencies.

See [AGENTS.md](AGENTS.md).

## Quick Start

```bash
# Build
cargo build

# Test
cargo nextest run

# Lint
cargo fmt --check
cargo clippy -- -D warnings

# Benchmark (once tracelake-bench exists)
cargo bench -p tracelake-bench

# Profile (builds with debug info, links against mimalloc)
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
  tracelake-server/         # HTTP/gRPC server
  tracelake-bench/          # benchmark suite
```

## Status

Early development. See AGENTS.md Step 1 for the first task.

## License

TBD
