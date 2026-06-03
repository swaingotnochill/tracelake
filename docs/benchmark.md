# TraceLake Benchmarks

Benchmarks are the product contract. TraceLake must beat SmithDB and Brainstore on
agent-trace workloads, not just on generic database microbenchmarks.

Sources checked on 2026-06-03:
- SmithDB launch and performance post: https://www.langchain.com/blog/introducing-smithdb
- Brainstore benchmark post: https://www.braintrust.dev/blog/brainstore-benchmarks
- Brainstore architecture post: https://www.braintrust.dev/blog/brainstore-architecture
- Brainstore platform architecture docs: https://www.braintrust.dev/docs/platform/architecture
- Braintrust faster experiments post: https://www.braintrust.dev/blog/faster-experiments

## 1. Competitor Benchmarks

| Workload | SmithDB Published Number | Brainstore Published Number | Notes |
|----------|--------------------------|-----------------------------|-------|
| Trace tree load | P50 92ms / P99 595ms | Not publicly reported in same shape | SmithDB's closest match to full trace inspection. |
| Single run load | P50 71ms / P99 358ms | Span load 346ms on 3.9M production traces | Treat as point load of one run/span payload. |
| Runs filtering | P50 82ms / P99 434ms | Not publicly reported in same shape | Filter by status, model, metadata, feedback, tags, and time. |
| Trace ingestion latency | P50 630ms / P99 1.47s | Write latency 6,984ms on 3.9M production traces | Measure durable ACK and visible read-after-write separately. |
| Full-text search | P50 400ms / P99 870ms | 401ms on 3.9M production traces | Search prompts, outputs, errors, tool traces, and names. |
| Threads filtering | P50 131ms / P95 268ms | Not publicly reported in same shape | Long-running conversation/thread reconstruction. |
| Realtime + historical merge | Not published as a latency table | Brainstore describes merging WAL realtime data with object-storage columnstore | Must be tested because correctness and freshness depend on it. |
| Large payload handling | SmithDB notes large fields are separated from core fields | Brainstore describes typical spans around 50KB and typical traces around 10MB | Lists must avoid loading full payloads unless requested. |

## 2. TraceLake Target Benchmarks

Targets are intentionally specific to workload shape. Report P50, P95, P99, max,
throughput, CPU, memory, object-store operations, bytes read/written, cache hit rate,
and result correctness for every run.

| Workload | Dataset | Target |
|----------|---------|--------|
| Trace tree load by `trace_id`, hot cache | Production-shaped traces | P50 < 75ms / P99 < 500ms |
| Trace tree load by `trace_id`, cold object-store read | Production-shaped traces | P50 < 150ms / P99 < 750ms |
| Single run/span load | Production-shaped traces | P50 < 60ms / P99 < 300ms |
| Runs filtering | Status/model/metadata/feedback/tags/time | P50 < 65ms / P99 < 350ms |
| Recent failed traces, limit 100 | Latest 24h summaries | P50 < 75ms / P99 < 400ms |
| Full-text search, top 50 | Prompt/output/error/tool text | P50 < 300ms / P99 < 750ms |
| Threads filtering | Multi-trace conversations | P50 < 100ms / P95 < 220ms |
| Hydrate 100 matching traces | After filter or full-text search | P50 < 250ms / P99 < 1s |
| p95 latency by model/tool | 24h columnar aggregation | P50 < 100ms / P99 < 500ms |
| Cost/tokens by model/provider | 24h columnar aggregation | P50 < 100ms / P99 < 500ms |
| Fresh read-after-write visibility | WAL/hot data merged with compacted data | Visible < 1s, no compaction wait |
| Trace ingestion latency | Durable ACK after WAL/object-store commit | P50 < 500ms / P99 < 1.2s |
| Sustained ingest throughput | Production-shaped spans | P0 > 100K spans/s, stretch > 500K spans/s |
| Compaction throughput | Production-shaped spans | Keeps up with sustained ingest with < 15m lag |
| Storage amplification | Logical TB after compression + indexes | Track row store, column store, text index, bloom, stats, manifest separately |

## 3. Benchmark Workloads

### W1: Trace Tree Load

Input: random `trace_id` sampled from the loaded corpus.

Checks:
- Returns every span for the trace.
- Preserves parent/child relationships.
- Sorts spans by `start_time`.
- Includes full payload only when requested.
- Measures warm cache and cold cache separately.

Debug counters:
- Manifest lookups.
- Bloom checks.
- Object-store GET count.
- Bytes read from `.tracepack`.
- Deserialization time.
- Cache hit/miss.

### W2: Single Run/Span Load

Input: random `span_id` sampled from traces with varied payload sizes.

Checks:
- Returns the exact span payload.
- Avoids hydrating unrelated spans when a single span is requested.

Debug counters:
- Span index lookup latency.
- Trace-pack block read size.
- Payload materialization time.

### W3: Runs Filtering

Inputs:
- `status in [error, timeout]`
- `model in [...]`
- metadata filters such as `tenant`, `environment`, `release`, `thread_id`
- feedback/score filters
- time range filters

Checks:
- Results match a correctness oracle built during data generation.
- Pagination is stable.
- Large payload columns are not read for summary-only results.

Debug counters:
- Segments scanned vs skipped.
- Stats-pruned segments.
- Column chunks read.
- Result materialization time.

### W4: Recent Failed Traces

Input: latest 24h, `status=error`, limit 100, ordered by descending start time.

Checks:
- Returns summaries for the newest failed traces.
- Includes traces still in WAL/hot data.
- Does not wait for compaction.

Debug counters:
- Hot-buffer contribution count.
- Compacted segment contribution count.
- Merge and dedupe time.

### W5: Full-Text Search

Inputs:
- Rare term in output.
- Common term in prompts.
- Error phrase.
- Tool name.
- Boolean query with metadata/time filter.

Checks:
- Top 50 results are correct against the generated inverted-index oracle.
- Relevance ordering is deterministic enough for repeated runs.
- Hydration is measured separately from search.

Debug counters:
- Tantivy/index open time.
- Index files fetched.
- Query execution time.
- Candidate trace count.
- Hydration time.

### W6: Threads Filtering

Input: `thread_id`, time range, status, score, and model filters.

Checks:
- Reconstructs all traces in a thread.
- Handles long-running conversations and late span updates.
- Returns thread summaries without loading every prompt/output blob.

Debug counters:
- Thread index lookups.
- Trace IDs discovered.
- Payload bytes avoided.

### W7: Hydrate 100 Matching Traces

Input: candidate trace IDs from W3 or W5.

Checks:
- Reads 100 complete trace trees.
- Preserves tree structure.
- Batches object-store reads where possible.

Debug counters:
- Parallelism.
- Object-store range GET count.
- Total bytes read.
- Decompression and deserialization time.

### W8: Aggregations

Inputs:
- p95 latency by model/tool over 24h.
- total cost by model/provider over 24h.
- token totals by model/provider over 24h.
- failure rate by release/environment over 24h.

Checks:
- Results match generated aggregate oracle within exact or documented approximate bounds.
- Predicate pushdown and column pruning work.

Debug counters:
- Parquet row groups scanned.
- Row groups skipped.
- Bytes read per column.
- DataFusion planning and execution time.

### W9: Ingest Latency And Throughput

Inputs:
- Concurrent span batches.
- Mixed trace sizes.
- Duplicate span IDs for idempotency checks.
- Open spans followed by late updates.

Checks:
- ACK means durable WAL/object-store commit.
- Duplicate writes are rejected or idempotent.
- Data is query-visible through hot reads before compaction.

Debug counters:
- Batch size.
- WAL object PUT latency.
- MemTable flush latency.
- Backpressure events.
- Rejected/duplicate spans.

### W10: Freshness Merge

Input: fixed query run while writers continuously append spans.

Checks:
- Query sees compacted data plus WAL/hot data.
- No duplicate spans when the same trace exists in both paths.
- Visibility lag stays below target.

Debug counters:
- Hot rows scanned.
- Compacted rows scanned.
- Merge conflicts.
- Visibility lag histogram.

## 4. Benchmark Datasets

### D1: Smoke Dataset

Purpose: fast local debugging.

Shape:
- 10K traces.
- 200K spans.
- 1 day.
- 3 tenants, 10 projects.
- Payloads: 1KB to 50KB.

Use for:
- Correctness tests.
- Debugging query plans.
- CI smoke benchmark.

### D2: External Comparison Dataset

Purpose: compare with Brainstore's public benchmark scale.

Shape:
- 3.9M traces.
- 78M spans if using 20 spans/trace average.
- 7 days.
- Payloads: median 10KB span, p95 50KB span, p99 250KB span.
- Trace sizes: median 200KB, p95 10MB.

Use for:
- Published TraceLake vs SmithDB/Brainstore scorecard.
- Warm and cold read tests.
- Full-text search tests.

### D3: Large Synthetic Dataset

Purpose: scale stress.

Shape:
- 50M traces.
- 1B spans.
- 30 days.
- 100 tenants, 1,000 projects.
- Zipfian tenant/project distribution.
- Mixed hot and cold shards.

Use for:
- Manifest scale.
- Segment pruning.
- Compaction throughput.
- Cache locality.

### D4: Payload Stress Dataset

Purpose: test large semi-structured traces.

Shape:
- 100K traces.
- 5M spans.
- 50KB typical span.
- 10MB typical trace.
- Some traces with 1,000+ spans.

Use for:
- Trace-pack block sizing.
- Payload preview vs full hydration.
- Browser/API-safe response shaping.

### D5: Freshness Dataset

Purpose: test WAL/hot plus compacted merge.

Shape:
- 1M compacted traces.
- Continuous writer appending 10K to 100K spans/s.
- 5% late span updates.
- 1% duplicate span retries.

Use for:
- Read-after-write visibility.
- Dedupe correctness.
- Hot/cold merge latency.

## 5. Data Generator

Implement `tracelake-bench/src/data_gen.rs` as a deterministic generator. Every run takes:

```text
--seed
--tenant-count
--project-count
--trace-count
--avg-spans-per-trace
--span-count-distribution
--payload-size-distribution
--time-range-days
--error-rate
--timeout-rate
--late-update-rate
--duplicate-rate
--thread-rate
--output-dir
```

Generated data must include:
- `tenant_id`, `project_id`, `trace_id`, `span_id`, `parent_span_id`, `thread_id`.
- `span_type`: llm, tool, retriever, agent, chain, eval, system.
- `status`: ok, error, timeout.
- `model`, `provider`, `release`, `environment`, tags.
- `start_time`, `end_time`, latency, cost, prompt tokens, completion tokens.
- `input`, `output`, `error`, and `attributes` payloads.
- Scores and feedback fields.
- Known text terms with controlled frequency for full-text search.

The generator must write oracle files:
- `trace_index.parquet`: trace_id -> span_ids, thread_id, time range, status.
- `span_index.parquet`: span_id -> trace_id and fixed fields.
- `filter_oracle.parquet`: expected result IDs for common filters.
- `text_oracle.parquet`: expected result IDs for known text terms.
- `aggregate_oracle.parquet`: expected aggregate values.

The oracle is mandatory. A fast benchmark that returns wrong results is a failed benchmark.

## 6. Benchmark Harness

Use two benchmark layers:

1. Criterion microbenchmarks for stable, isolated code paths.
2. A custom end-to-end load generator for async database workloads.

Criterion benchmarks:
- Key encoding and decoding.
- Span serialization and deserialization.
- Trace-pack write and read by trace ID.
- Bloom filter lookup.
- Stats pruning.
- Payload preview extraction.

End-to-end benchmark binary:

```text
tracelake-bench run \
  --dataset d2 \
  --object-store s3|minio|memory \
  --cache warm|cold \
  --workload trace_load|span_load|runs_filter|recent_failed|text_search|threads|hydrate|aggregate|ingest|freshness \
  --concurrency 1,8,32,128 \
  --duration 10m \
  --sample-rate 1000 \
  --output results/
```

Record results as:
- `results.jsonl`: one row per request with latency, status, bytes, counters, and correctness.
- `summary.json`: P50/P95/P99/max, throughput, errors, cache hit rate, object-store ops.
- `profile.pb`: CPU profile when profiling is enabled.
- `trace.json`: optional tracing export for slow requests.

Use `hdrhistogram` or equivalent for latency histograms. Avoid averages as the primary
metric; tail latency is the benchmark.

## 7. Environment Matrix

Run the same benchmark in these environments:

| Environment | Purpose |
|-------------|---------|
| Memory object store | Correctness and CPU-only baseline. |
| Local MinIO on NVMe | Development object-store behavior without cloud variance. |
| S3 Standard or equivalent | Real cold-read and PUT latency. |
| S3 Express One Zone or equivalent | WAL latency experiment. |

Control variables:
- Region.
- Instance type.
- CPU count.
- RAM.
- NVMe size.
- Cache size.
- Network bandwidth.
- Object-store request rate.
- Segment size.
- Trace-pack block size.
- Compression level.

For cold-cache runs, restart the query process and clear local cache directories. For
warm-cache runs, execute a warmup phase and discard warmup samples.

## 8. Debug Workflow

When a benchmark misses target:

1. Confirm correctness first.
2. Split latency into planning, manifest read, index read, object-store read,
   decompression, deserialization, filtering, merge, and response encoding.
3. Compare cold vs warm cache.
4. Count object-store operations. A trace load should not turn into many tiny GETs.
5. Inspect bytes read. Summary/list queries should not read full prompt/output blobs.
6. Profile CPU with `pprof` or equivalent.
7. Enable request tracing for the slowest 1% of requests.
8. Run the same workload against memory object store. If it is still slow, the issue is CPU
   or format layout, not object storage.
9. Run with one tenant/project/shard. If it gets fast, the issue is routing or pruning.
10. Validate segment sizes and cache hit rate.

Every benchmark failure should produce:
- Reproduction command.
- Dataset seed.
- Slow request trace.
- Query plan.
- Object-store operation summary.
- CPU profile if CPU-bound.

## 9. Reporting

Publish a benchmark report with:
- Source commit.
- Dataset config and seed.
- Environment config.
- Object-store type and region.
- Cache mode.
- Workload mix.
- Correctness result.
- Latency table.
- Throughput table.
- Cost estimate from bytes stored and object-store operations.
- Notes on known bottlenecks.

Do not claim TraceLake beats SmithDB or Brainstore unless:
- The workload shape matches the published competitor workload.
- The dataset scale is disclosed.
- Correctness passes.
- Cold and warm cache behavior are separated.
- Ingest ACK semantics are stated.
- Full-text search latency is separated from hydration latency.
