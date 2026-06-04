# AGENTS.md — TraceLake

## CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them — don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it — don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

---

TraceLake is an open-source database purpose-built for LLM agent traces.
It stores trace data in object storage (S3/GCS), uses NVMe/memory as hot cache,
and beats generic databases (ClickHouse, Postgres, Elasticsearch) on trace-native workloads.

---

## 1. WHAT WE BUILD AND WHY

### The Problem

LLM apps produce traces (spans). Each trace is a tree of spans — LLM calls, tool calls,
retriever calls, agent decisions, eval scores. Teams store these in ClickHouse, Postgres,
or Elasticsearch. None of these are built for traces:

- **ClickHouse**: Great at analytics. Terrible at fetching one full trace (50-500ms).
  No native text search. Expensive replicated disks.
- **Postgres**: Slow JSON handling (50-200ms per trace). No columnar analytics.
- **Elasticsearch**: Expensive to operate. Not trace-native. Slow on joins/hydration.

The UI constantly needs "give me this entire trace tree." That's a point lookup with
tree reconstruction — the opposite of what a column store is good at.

### What TraceLake Does Differently

The storage unit is a **trace pack** (all spans for a trace_id grouped together), not a span row.
Object storage is the source of truth. Compute is stateless. NVMe is disposable cache.

This means:
- Fetching a full trace = one read, not a distributed row reconstruction.
- Text search over prompts/outputs = native inverted index, not an external bolt-on.
- Analytics = columnar scan over pre-built columns, not JSON extraction at query time.
- Storage cost = S3 ($0.023/GB), not replicated EBS ($0.08-0.16/GB).

### Benchmarks We Must Beat

Benchmark requirements, competitor numbers, target latencies, data shapes, and harness
design live in [benchmark.md](benchmark.md).

---

## 2. ARCHITECTURE

Database architecture details live in [architecture.md](architecture.md).

## 3. PROJECT STRUCTURE

```
tracelake/
  Cargo.toml                    # workspace root
  AGENTS.md                     # project guide
  architecture.md               # database architecture

  tracelake-core/               # data types, key encoding, serialization
    src/
      span.rs                   # Span, Trace, Score, TokenUsage types
      key.rs                    # key encoding/decoding for SlateDB
      trace_pack.rs             # trace pack format (write + read)
      segment.rs                # segment manifest format

  tracelake-ingest/             # SlateDB integration, ingest endpoint
    src/
      db.rs                     # SlateDB setup, config, segment extractor
      ingest.rs                 # ingest logic (validate, batch, write to SlateDB)

  tracelake-compact/            # compaction pipeline
    src/
      compactor.rs              # reads SlateDB SSTs, builds trace segments
      trace_pack_writer.rs      # builds .tracepack
      column_writer.rs          # builds .columns (Parquet)
      text_indexer.rs           # builds .text (Tantivy)
      bloom_writer.rs           # builds .bloom
      stats_writer.rs           # builds .stats
      manifest_writer.rs        # builds .manifest

  tracelake-query/              # query engine
    src/
      planner.rs                # query planner (routes to correct engine)
      trace_fetch.rs            # get_trace() implementation
      search.rs                 # search_traces() implementation
      aggregate.rs              # aggregate() implementation (DataFusion)
      merge.rs                  # merge hot (SlateDB) + compacted results

  tracelake-server/             # HTTP/gRPC server
    src/
      main.rs                   # server entry point
      ingest_handler.rs         # ingest endpoint
      query_handler.rs          # search + aggregate endpoints

  tracelake-bench/              # benchmark suite
    src/
      data_gen.rs               # synthetic trace data generator
      bench_ingest.rs           # ingest throughput benchmark
      bench_fetch.rs            # trace fetch latency benchmark
      bench_search.rs           # search latency benchmark
      bench_aggregate.rs        # aggregation latency benchmark
      bench_hydrate.rs          # hydration benchmark
```

---

## 4. STEP-BY-STEP BUILD ORDER

Each step produces working, testable code. No step depends on a future step.

### Step 1: tracelake-core — Data Types and Key Encoding

Build the Span struct, key encoding/decoding, and unit tests.
No external dependencies beyond serde. This is pure data transformation.

Verify: key encode → key decode round-trips correctly for all span shapes.
Benchmark: key encoding speed (target: >10M keys/s per thread).

### Step 2: tracelake-ingest — SlateDB Integration

Open a SlateDB instance with InMemory object store.
Write spans using the key encoding from Step 1.
Read them back using scan_prefix().

Verify: write 100K spans, read them back grouped by trace_id.
Benchmark: SlateDB put() throughput with our key pattern.

### Step 3: tracelake-compact — Trace Pack Writer

Read spans from SlateDB (sorted by key, which means grouped by trace_id).
Write them into a trace pack binary format.
Read the trace pack back and verify correctness.

Verify: write 10K spans (100 traces × 100 spans), read each trace back.
Benchmark: trace pack write speed, trace pack read speed by trace_id.

### Step 4: tracelake-compact — Full Segment Builder

Extend Step 3 to also write .bloom, .stats, .manifest.
(Tantivy text index and Parquet columns come in later steps.)

Verify: build a segment, read it back, verify bloom filter catches non-existent traces.
Benchmark: compaction throughput (spans/second).

### Step 5: tracelake-query — Trace Fetch

Implement get_trace(): check SlateDB hot buffer first, then compacted segments.
This is the merge path described in [architecture.md](architecture.md).

Verify: write a trace, fetch it before compaction (hot), fetch it after compaction (cold).
Benchmark: trace fetch latency (warm and cold).

### Step 6: tracelake-query — Search

Implement search_traces() with status/model/time filters.
Use bloom filters and stats for pruning.

Verify: write 1000 traces with various statuses, search for failed traces, verify correct.
Benchmark: search latency with 10K/100K/1M traces in segments.

### Step 7: tracelake-compact — Tantivy Text Index

Build Tantivy index during compaction. Store on object store.
Query Tantivy index during search.

Verify: write traces with "hallucination" in output, text search finds them.
Benchmark: text search latency, index build time.

### Step 8: tracelake-query — Aggregation via DataFusion

Write .columns (Parquet) during compaction.
Register custom TableProvider in DataFusion.
Implement aggregate().

Verify: write traces with various latencies, compute p95 by model, verify correct.
Benchmark: aggregation latency over 1M/10M/100M spans.

### Step 9: tracelake-server — HTTP API

Wire up ingest, search, aggregate behind HTTP endpoints.
Run end-to-end: ingest → compact → query.

### Step 10: tracelake-bench — Full Benchmark Suite

Implement the benchmark suite described in [benchmark.md](benchmark.md). Do not claim
performance wins until the benchmark harness proves correctness, discloses dataset shape,
and separates warm-cache, cold-cache, ingest, search, hydration, and freshness results.

---

## 4.1 IMPLEMENTATION PLAN ASSESSMENT

The initial plan is directionally good:
- Object storage as source of truth matches SmithDB, Brainstore, and turbopuffer-style
  architecture.
- Trace-local row storage is the right core bet for trace tree loads and hydration.
- Separate columnar, text, bloom, stats, and manifest files match the actual workload mix.
- Hot + compacted merge is required; both SmithDB and Brainstore optimize for realtime
  reads while background compaction catches up.
- Rust, DataFusion, Tantivy, Parquet/Arrow, and stateless services are a reasonable stack.

Changes to make before implementation:
- Treat SmithDB/Brainstore as the first-class benchmark competitors, not only generic
  ClickHouse/Langfuse/Elasticsearch systems.
- Validate trace fetch and hydration on realistic large payloads early. A tiny synthetic
  span benchmark can hide the cost of deserializing prompts, outputs, and tool payloads.
- Add thread filtering and tree-aware filters to the benchmark suite. SmithDB explicitly
  reports threads filtering, and agent observability workloads increasingly need it.
- Measure read-after-write freshness as a benchmark. The design is incomplete if recent
  WAL/hot writes are fast to ingest but slow or inconsistent to query.
- Do not make the MVP depend on reaching 1B spans first. Prove correctness and latency at
  3.9M production-shaped traces, then scale to 50M traces / 1B spans.
- Revisit the storage format choice after Step 3. FlatBuffers may be good for fixed fields,
  but huge semi-structured JSON payloads need a streaming/truncated preview path so UI
  lists do not fully deserialize multi-MB objects.

---

## 5. CODING RULES

### DRY

- One definition of the span schema. Used for ingest, storage, and query.
- One key encoding function. Used for write and read.
- One segment manifest format. Used by compactor and query planner.
- If you write the same logic twice, extract it into a shared function immediately.

### YAGNI

- Do NOT build: user authentication, RBAC, multi-region replication,
  custom retention policies, vector search, SQL parser, admin UI,
  alerting integrations, export to CSV/JSON, GraphQL API, WebSocket streams.
- Do NOT over-engineer: The query planner is a match statement, not an optimizer
  with cost models. The segment format is a flat binary file, not a self-describing
  container with version negotiation. The API is three endpoints, not a query language.
- Build what's needed for Steps 1-10. Nothing more.
- When in doubt, leave it out. We can always add it later.

### Clean Code

- Functions under 50 lines. If longer, split.
- No magic numbers. Named constants for block sizes, thresholds, limits.
- No implicit behavior. If a function can fail, return Result. If a parameter
  is required, don't make it Option.
- Errors carry context. Use `thiserror`. Include what failed and why.
- Tests for every public function. Tests for every edge case (empty traces,
  single-span traces, missing fields, duplicate span_ids).
- No `unwrap()` in non-test code. Use `?` or `context()`.
- Every `pub fn` has a doc comment. Every `pub struct` has a doc comment.
- Async only where needed (I/O). CPU-bound work is synchronous.

---

## 6. WHAT WE DO NOT BUILD

To be explicit about scope boundaries:

- We do NOT build a clickhouse replacement. We build a trace database.
- We do NOT build a vector database. If semantic search is needed, add it later.
- We do NOT build a log storage system. We store agent traces, not generic logs.
- We do NOT build a real-time streaming system. CDC is a future optimization.
- We do NOT build a schema registry. Attributes are opaque JSON.
- We do NOT build SQL. The API is programmatic (search + aggregate structs).
- We do NOT build multi-region replication. Object storage handles durability.
- We do NOT build automatic scaling. That's a deployment concern, not a database concern.

---

## 7. OPEN QUESTIONS — ANSWER DURING PROFILING

These are NOT design decisions to make now. They are measurements to take during
Steps 2 and 3. The answer determines whether we adjust, not whether we build.

1. **SlateDB throughput with our key size**: Our keys are ~200 bytes (long prefixes).
   Does this hurt SlateDB's bloom filter efficiency or SSTable size?
   → Measure in Step 2.

2. **Trace pack optimal size**: How large should each trace pack be?
   Too small = too many S3 objects. Too large = slow cold reads.
   → Measure in Step 3. Initial guess: 64-256 MB.

3. **Tantivy index size on S3**: Tantivy indexes can be large relative to data.
   Does the text index fit in cache alongside trace packs and bloom filters?
   → Measure in Step 7.

4. **Hot/cold merge latency**: How expensive is merging SlateDB hot buffer results
   with compacted segment results? Is it fast enough for real-time queries?
   → Measure in Step 5.

5. **Compaction keeping up with ingest**: Can one compactor node keep up with
   500K spans/s ingest? How many compactors do we need?
   → Measure in Step 4 after generating load in Step 2.

---

## 8. PROFILING, DEBUGGING, AND DEV DEPENDENCIES

### 9.1 Profiling Tools

| Tool | What It Profiles | Install | How to Use |
|------|-----------------|---------|------------|
| `cargo-flamegraph` | CPU time, on-CPU flame graphs | `cargo install flamegraph` | `cargo flamegraph -o flame.svg --bin tracelake-server -- --profile profiling` |
| `samply` | CPU + wall-clock sampling profiler (tokio-aware, modern replacement for pprof) | `cargo install samply` | `samply record -- ./target/profiling/tracelake-server` |
| `tracy` | Frame-level CPU + GPU + memory profiling, real-time viewer | `cargo install tracy` | Build with `tracy` feature, connect Tracy client | 
| `tokio-console` | Async task state, wakers, pending I/O | `cargo install tokio-console` | Set `RUSTFLAGS="--cfg tokio_unstable"` then `tokio-console` |
| `tracing` | Structured event logging (already a dependency) | In-tree | Use `tracing::instrument` on hot paths. Emit span timing for ingest/compaction/query |
| `criterion` | Microbenchmarking with statistical analysis | Dev dependency | `cargo bench -p tracelake-bench` — HTML reports + flamegraph support |

### 9.2 Cargo Profiling Profile

DataFusion's approach: a dedicated Cargo profile that inherits release optimizations
but keeps debug info for external profilers. We do the same.

```toml
# In workspace Cargo.toml
[profile.profiling]
inherits = "release"
debug = true
strip = false   # keep debug info for flamegraphs/samply
```

Build with: `cargo build --profile profiling --bin tracelake-server`

### 9.3 Allocator

DataFusion defaults to `mimalloc` in benchmarks for better allocation performance.
We do the same for tracelake-bench.

```toml
# In tracelake-bench/Cargo.toml
[features]
default = ["mimalloc"]
mimalloc = ["dep:mimalloc"]

[dependencies]
mimalloc = { version = "0.1", optional = true }
```

In bench binary:
```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

### 9.2 Debugging Tools

| Tool | What It Does | When to Use |
|------|-------------|------------|
| `cargo-expand` | Shows macro expansion | Debugging derive macros, async_trait expansion |
| `cargo-rr` | Record/replay debugging | Reproducing flaky race conditions in compaction or merge |
| `cargo-nextest` | Parallel test runner, better output | Faster CI, better failure output than `cargo test` |
| `insta` | Snapshot testing | Comparing trace pack binary output, segment manifest JSON |
| `miri` | Undefined behavior detection | If we add unsafe code (should be rare — only in trace pack serialization) |

### 9.3 Object Store Testing

Local development needs an S3-compatible store. No real S3 credentials in dev.

| Tool | Purpose | How to Run |
|------|---------|------------|
| MinIO | S3-compatible local object store | `docker run -p 9000:9000 -p 9001:9001 minio/minio server /data --console-address ":9001"` |
| `object_store` InMemory | In-memory object store for unit tests | `Arc::new(InMemory::new())` — no external dependency |

### 9.4 Linting and Formatting

| Tool | Purpose | Config |
|------|---------|--------|
| `rustfmt` | Code formatting | `rustfmt.toml` at workspace root (match SlateDB style) |
| `clippy` | Linting | `clippy.toml` at workspace root |
| `cargo-deny` | License and dependency audit | `deny.toml` at workspace root |

### 9.5 CI Requirements

Every PR must pass:
1. `cargo fmt --check`
2. `cargo clippy -- -D warnings`
3. `cargo nextest run` (all tests)
4. `cargo deny check` (license audit)

### 9.6 How to Profile Each Build Step

| Step | What to Profile | Tool |
|------|----------------|------|
| Step 2 (SlateDB ingest) | Put throughput, WAL flush latency, MemTable growth | `tokio-console` + `tracing` span timing |
| Step 3 (Trace pack) | Serialization speed, deserialization speed, allocation count | `cargo-flamegraph` on bench, `samply` for allocation |
| Step 4 (Segment builder) | Compaction throughput, I/O bytes per span, S3 PUT/GET count | `tracing` event counters + `samply` |
| Step 5 (Trace fetch) | Warm latency, cold latency, cache hit rate, bloom filter false positive rate | `tracing` span timing, `cargo-flamegraph` |
| Step 7 (Tantivy) | Index build time, index size, query latency | `tracing` span timing |
| Step 8 (DataFusion) | Aggregation latency, Parquet scan throughput | `tracing` span timing, DataFusion built-in metrics |

---

## 9. EXTERNAL DEPENDENCIES

### Runtime Dependencies

| Crate | Purpose | Why This One |
|-------|---------|-------------|
| `slatedb` | WAL, SSTables, compaction, caching, bloom | Proven object-storage-first LSM, embeddable |
| `tantivy` | Full-text search | Rust-native, segment-based, embeddable |
| `datafusion` | Aggregation query engine | Rust-native, Arrow ecosystem, extensible |
| `arrow` | In-memory columnar format | Required by DataFusion, zero-copy integration |
| `parquet` | Columnar storage format | Predicate pushdown, compression, DataFusion native |
| `object_store` | S3/GCS abstraction | Re-exported by SlateDB, multi-cloud |
| `flatbuffers` | Zero-copy serialization | Used by SlateDB, zero-copy read |
| `serde` / `serde_json` | JSON attributes | Standard, required for flexible metadata |
| `axum` | HTTP server | Async, tower ecosystem, production-proven |
| `tonic` | gRPC server | Async, prost ecosystem, OTLP ingest |
| `tokio` | Async runtime | Required by all async deps |
| `thiserror` | Error types | Ergonomic error derivation |
| `tracing` | Structured logging | Standard observability, tokio integration |
| `chrono` | Timestamps | UTC datetime, serde support |

### Dev Dependencies

| Crate | Purpose |
|-------|---------|
| `criterion` | Microbenchmarking (HTML reports + flamegraph support) |
| `insta` | Snapshot testing for binary formats |
| `tempfile` | Temp directories for test databases |
| `proptest` | Property-based testing for key encoding, serialization |
| `rstest` | Parameterized tests |
| `mimalloc` | Fast allocator for benchmark binaries (default in bench feature) |

No other dependencies without justification.

## guild workflow

guild coordinates tasks (quest) and persistent knowledge (lore) across sessions and agents.

**BEFORE ANY OTHER ACTION** — before reading files, editing code, or
responding to the user — call the MCP tool `guild_session_start(project="tracelake")`.
It returns the full agent contract, active principles (oath), and the
current top bounty. Follow what it returns.

If `guild_session_start` is not visible in your tool list, run your
host's tool-search for `guild` first — some hosts lazy-load MCP tools.
Do NOT fall back to CLI; the MCP server is available.

### Core rules (full contract is returned by session_start)

- **Never use built-in task tools** (TaskCreate / TaskUpdate / TaskList) —
  they're session-scoped. Use `quest_post` / `quest_accept` / `quest_list` instead.
- **Accept before working on a quest** — `quest_accept(quest_id=...)` prevents
  parallel-agent collisions.
- **Appraise before researching** — `lore_appraise(query=..., all_projects=true)`
  first. If current entries exist, use them.
- **Brief before session end** — when wrapping up or compaction is near,
  call `quest_brief("what was done, what's next, gotchas")` without being asked.

MCP namespace: `mcp__guild__*`. CLI fallback: `guild --help` (last resort only).
