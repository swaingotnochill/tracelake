# TraceLake Architecture

## 1. Database Decisions

### Decision 1: Rust as the language

**Why**: Performance-critical paths (serialization, compaction, query execution) need
tight control over memory and CPU. No GC pauses. Same ecosystem as SlateDB, DataFusion,
Tantivy — all Rust. No FFI overhead.

**Rejected**: Go (GC pauses, worse numerical perf), C++ (safety, build complexity),
Java (GC, memory overhead).

### Decision 2: SlateDB for the ingest/WAL layer

**Why**: SlateDB is an embeddable, object-storage-first LSM-tree. It handles:
- WAL durability (writes to object store, not local disk)
- MemTable buffering (hot data in memory before flush)
- SSTable compaction (merges and rewrites on object store)
- Bloom filters (fast negative lookups)
- Compression (Zstd, Snappy, LZ4)
- Block caching (Foyer: memory + NVMe tiers)
- Segmented compaction (per-key-prefix lifecycle management)

We do NOT build our own WAL, our own memtable, or our own SSTable format.
SlateDB gives us all of this. We focus on what makes TraceLake special: the trace-native
format on top.

**How we use it**: Spans are written as key-value pairs. The key encodes
`tenant\0project\0day\0shard\0trace_id\0span_id`. SlateDB handles batching, WAL flush,
and SSTable creation. A separate compactor reads SlateDB SSTables and builds trace segments.

**Rejected**: RocksDB (disk-bound, not object-store-first), Custom WAL (reinventing SlateDB),
Write directly to Parquet (no real-time reads, no WAL).

**Caveat**: SlateDB is a KV store, not a trace store. It does not understand trace_id
grouping or columnar analytics. We build those on top. This is intentional — SlateDB
handles the hard distributed systems problems (durability, compaction, caching) so we
focus on trace-specific optimization.

### Decision 3: SlateDB Segmented Compaction (RFC-0024)

**Why**: SlateDB's segment-oriented compaction lets us define key prefixes as segments.
Each segment has its own L0, compaction schedule, and lifecycle. We encode keys so that
each `(tenant, project, day, shard)` is a segment. This gives us:

- Per-segment parallel compaction (compacting tenant A's Monday doesn't block tenant B's Tuesday)
- Natural retention (drop old day segments without touching recent data)
- Per-tenant isolation (one noisy tenant doesn't affect compaction of others)
- Backpressure scoped to segments, not global

**Implementation**: A `PrefixExtractor` that returns the segment prefix from each key.
SlateDB handles the rest — routing writes to segments, flushing per-segment L0 SSTs,
compacting within segments.

### Decision 4: Separate WAL Object Store (RFC-0009)

**Why**: WAL writes go to object storage every 100ms (flush interval). On S3 Standard,
each PUT has ~50-100ms latency. For high-throughput ingest, this is the bottleneck.

SlateDB supports a separate, faster object store just for WAL. We can use:
- S3 Express One Zone (~10ms PUT latency, higher cost but tiny data volumes)
- Local MinIO (sub-ms latency, NVMe-backed)
- Memory-backed object store for testing

The main data SSTs still go to cheap S3 Standard. WAL is tiny (uncompressed batches)
and short-lived (compacted away quickly), so the extra cost is negligible.

**For MVP**: Use a single object store (MinIO for local dev, S3 for prod). Optimize later
if profiling shows WAL latency is the bottleneck.

### Decision 5: Trace Packs — the storage unit is the trace, not the span

**Why**: The #1 query in a trace UI is "show me this entire trace." In ClickHouse, this
requires scanning multiple tables, filtering by trace_id, and reconstructing the tree.
In TraceLake, all spans for a trace_id are stored contiguously in a single trace pack.
Fetching a trace = reading one contiguous block from one file.

A trace pack is a binary file containing:
```
[header: segment metadata]
[trace directory: trace_id → offset (sorted for binary search)]
[trace 1: spans ordered by start_time]
[trace 2: spans ordered by start_time]
...
[footer: offsets, checksum]
```

This is the core architectural bet. If traces were always one span, this would be
pointless. But real traces are trees of 5-50 spans, and fetching the whole tree is the
dominant query. Trace packs turn an N-read operation into a 1-read operation.

**Rejected**: Span-per-row (ClickHouse approach — fast aggregates, slow trace fetch),
Span-per-column-family (RocksDB approach — still N reads for trace fetch).

### Decision 6: Separate column store for analytics

**Why**: Trace packs are row-oriented (good for fetch, bad for analytics).
For "p95 latency by model over 24h," we need columnar storage.

After building trace packs, the compactor also writes a `.columns` file in Arrow/Parquet
format containing: start_time, end_time, latency_ms, cost_usd, tokens, status, model,
provider, scores. This gives us predicate pushdown (skip segments outside time range)
and vectorized execution (scan only the columns we need).

**Rejected**: Using ClickHouse for analytics (adds a whole system to operate),
Querying trace packs with column extraction (slow — must deserialize full rows).

### Decision 7: Tantivy for full-text search

**Why**: Users search for "traces where the output mentions hallucination" or
"tool errors containing timeout." This needs an inverted index.

Tantivy is a Rust-native full-text search engine. It's fast, embeddable, and
segment-based (matches our immutable-segment model). During compaction, we build
a Tantivy index over input, output, name, and error fields and store it alongside
the trace segment.

**Rejected**: Elasticsearch (separate system to operate, network overhead, Java),
Meilisearch (separate system, not embeddable), Custom inverted index (reinventing Tantivy).

**Caveat**: Tantivy expects a local directory. We store index files on object storage
and cache them locally (via SlateDB's Foyer cache). Cold queries that miss the cache
will need to fetch index files from S3 — this adds ~100ms latency. Warm queries will
be <5ms.

### Decision 8: DataFusion for aggregation queries

**Why**: Apache DataFusion is a Rust-native query engine built on Apache Arrow.
It handles SQL parsing, query planning, and vectorized execution. We register a
custom `TableProvider` that reads our `.columns` files, and DataFusion handles the rest.

**Rejected**: DuckDB (C++, different ecosystem, not embeddable as a library in the same way),
Custom aggregation engine (reinventing a query planner for no reason).

**Caveat**: DataFusion is optimized for Parquet files. If our column format is Parquet,
we get predicate pushdown and column pruning for free. If we need a custom format for
performance reasons later, we write a custom `TableProvider`.

### Decision 9: Object storage as source of truth

**Why**: S3/GCS provide 11 9s durability, unlimited scale, and $0.023/GB/month pricing.
No local disk management, no replication, no volume provisioning. Compute can be
stateless — any node can serve any query from object storage.

This is the turbopuffer architecture: NVMe is cache, S3 is truth. It works because:
- Object storage GET latency is ~50-100ms (cold) — acceptable if we cache aggressively
- Object storage throughput is massive — compaction never becomes I/O bound
- Storage cost is 5-10x cheaper than EBS

**Rejected**: Local disk as truth (requires replication, volume management, backup),
Memory as truth (expensive, limited capacity, not durable).

### Decision 10: FlatBuffers for serialization

**Why**: FlatBuffers provide zero-copy deserialization (access fields directly from the
byte buffer without parsing). This matters because trace packs can be large (1-10MB)
and deserialization latency directly affects trace fetch time.

SlateDB already uses FlatBuffers internally. We use the same ecosystem.

**Rejected**: Protocol Buffers (requires allocation during deserialization),
JSON (slow, large, no schema enforcement), Bincode (no zero-copy, no schema evolution).

**Note**: We may use `serde_json::Value` for the `attributes` field (arbitrary JSON).
FlatBuffers is for the fixed-schema span fields. JSON is for the flexible attributes.
This is the standard pattern — structured fields in the fast path, flexible fields
stored as opaque blobs.

### Decision 11: Compression — Zstd

**Why**: Zstd provides the best compression ratio at reasonable speed.
Level 3 gives ~10-15x compression on trace data with <1ms decompression per MB.
Higher levels give diminishing returns and slower compression during compaction.

**Rejected**: Snappy (3-5x compression, same I/O cost), LZ4 (similar to Snappy),
Gzip (slow decompression), No compression (storage cost explodes).

---

## 2. Storage Layout

Every decision below exists because of the query it serves. If you can't connect
a storage decision to a query, it shouldn't exist.

```
s3://bucket/
  {tenant}/{project}/
    segments/
      day=2026-06-03/shard=000/
        segment-000001.manifest   # offsets, checksums, span count, schema
        segment-000001.tracepack  # row store: spans grouped by trace_id
        segment-000001.columns    # columnar: Arrow/Parquet for analytics
        segment-000001.text       # Tantivy inverted index
        segment-000001.bloom      # bloom: trace_ids, span_ids, tags
        segment-000001.stats      # min/max: time, latency, cost, score
```

### Trace Pack (.tracepack) — Serves: get_trace(), search include_spans

Binary file. Spans grouped by trace_id, sorted by start_time within each trace.
Trace directory at the front for O(log N) trace lookup by ID.

### Columns (.columns) — Serves: aggregate()

Parquet file. One row per span. Columns: start_time, end_time, latency_ms,
cost_usd, token counts, status, model, provider, score fields.
Predicate pushdown on time range, model, status.

### Text (.text) — Serves: search_traces(text_query)

Tantivy index segments. Indexed fields: input, output, name, error.
Returns trace_ids + relevance scores. Caller then fetches trace packs.

### Bloom (.bloom) — Serves: get_trace(), search_traces(trace_ids)

Bloom filter over all trace_ids, span_ids, tags, user_ids in the segment.
Used to avoid reading trace packs from S3 when the trace doesn't exist.

### Stats (.stats) — Serves: search_traces(time_range), aggregate()

Min/max values for time, latency, cost, score. Used to skip entire segments
when the query's filter range doesn't overlap.

### Manifest (.manifest) — Serves: everything (routing)

JSON file. Lists all segment files, their sizes, checksums, time ranges,
and stats. The query planner reads manifests to decide which segments to query.

---

## 3. Data Flow

### Write Path

```
SDK sends spans
    → Ingest API validates + batches
    → SlateDB put() with key = tenant\0project\0day\0shard\0trace_id\0span_id
    → SlateDB writes to WAL on object store (every ~100ms)
    → SlateDB buffers in MemTable
    → SlateDB flushes to L0 SSTables on object store
    → ACK to SDK

Note: The SDK gets an ACK when SlateDB confirms durability.
The trace segment files are built later by the compactor.
Real-time queries merge MemTable + L0 SSTs (via SlateDB) + compacted segments.
```

### Compaction Path

```
SlateDB L0 SSTables exist for a segment
    → Compactor detects segment has enough L0 SSTs (or age threshold)
    → Reads all L0 SSTs from SlateDB
    → Groups spans by trace_id (trace locality sort)
    → Writes .tracepack (row store, grouped by trace)
    → Writes .columns (Parquet columnar)
    → Writes .text (Tantivy index)
    → Writes .bloom (bloom filter)
    → Writes .stats (min/max ranges)
    → Writes .manifest (metadata)
    → Atomically publishes manifest
    → Tells SlateDB to drop compacted SSTs (or lets SlateDB's own GC handle it)
```

### Read Path

```
Query arrives
    → Query planner analyzes the query
    → Selects which segments to query (via manifest + stats pruning)
    → For each relevant segment:
        → Check bloom → skip if negative
        → Check stats → skip if time range doesn't overlap
    → Merge results from:
        SlateDB hot buffer (MemTable + L0 SSTs)  → most recent data
        Compacted trace segments                  → historical data
    → For text queries: query Tantivy index first, get trace_ids, then fetch
    → For aggregate queries: scan .columns via DataFusion
    → For trace fetch: read .tracepack at trace_id offset
    → Return results
```

---

## 4. Key Encoding In SlateDB

All data in SlateDB is keyed by bytes. We encode spans as:

```
Primary key (span data):
  {tenant_id}\x00{project_id}\x00{day}\x00{shard:03}\x00{trace_id}\x00{span_id}\x00
  Example: "org_abc\x00proj_def\x002026-06-03\x00000\x00trace_123\x00span_456\x00"

Trace index (for prefix scan — all spans in a trace):
  Same prefix up to trace_id, no span_id suffix.
  Scan prefix "org_abc\x00proj_def\x002026-06-03\x00000\x00trace_123\x00"
  → returns all spans for that trace in that day/shard.
```

Segment prefix (what SlateDB's PrefixExtractor returns):
```
  {tenant_id}\x00{project_id}\x00{day}\x00{shard:03}\x00
  Example: "org_abc\x00proj_def\x002026-06-03\x00000\x00"
```

This means:
- Each segment is one (tenant, project, day, shard) combination.
- Within a segment, spans are sorted by trace_id then span_id.
- A prefix scan on trace_id returns all spans for that trace.
- SlateDB's segment-oriented compaction (RFC-0024) handles per-segment lifecycle.

---

## 5. Read Path: Merging Hot + Compacted

This is the hardest part architecturally. Queries must see both:
- **Hot data**: Spans in SlateDB's MemTable and L0 SSTs (not yet compacted)
- **Compacted data**: Spans in trace segment files (already compacted)

Merge strategy:

```
fn execute_query(query) -> Result {
    // Step 1: Get candidate trace_ids from compacted segments
    let compacted_trace_ids = search_compacted_segments(query);

    // Step 2: Get candidate trace_ids from SlateDB hot buffer
    let hot_spans = scan_slatedb_for_recent_spans(query.time_range);
    let hot_trace_ids = hot_spans.map(|s| s.trace_id).unique();

    // Step 3: Merge (hot data takes precedence — it's newer)
    let all_trace_ids = merge_trace_ids(compacted_trace_ids, hot_trace_ids);

    // Step 4: For trace_ids found in compacted segments, read from .tracepack
    // For trace_ids only in hot buffer, read from SlateDB directly
    // For trace_ids in both, merge spans (hot spans are newer, compacted spans are older)
}
```

The key insight: we NEVER rewrite spans that are in SlateDB's hot buffer.
The compactor only processes SlateDB SSTables, which are already flushed.
MemTable data is always read directly from SlateDB.

---

## 6. SlateDB Integration

### We use (don't rebuild):

| SlateDB Feature | How We Use It |
|----------------|---------------|
| `put()` / `put_with_options()` | Write spans as KV pairs |
| `scan()` / `scan_prefix()` | Read all spans for a trace_id prefix |
| `get()` | Point lookup for bloom filter checks |
| WAL on object store | Durability for ingest |
| MemTable | Hot buffer for recent spans |
| L0 SSTables | Intermediate storage before our compaction |
| Segmented compaction (RFC-0024) | Per (tenant, project, day, shard) lifecycle |
| Bloom filters | Skip GETs for non-existent traces |
| Foyer cache (memory + disk) | Cache hot segment data |
| Zstd compression | Reduce S3 storage and GET costs |
| Separate WAL store (RFC-0009) | Faster WAL for high-throughput ingest |
| `PrefixExtractor` | Route keys to segments |
| Delete / tombstones | GDPR deletion support |

### We build on top (SlateDB doesn't do this):

| Component | Why SlateDB Doesn't Do It |
|-----------|--------------------------|
| Trace pack format | SlateDB stores KV pairs, not grouped trace structures |
| Column store | SlateDB is row-oriented (LSM), not columnar |
| Text index | SlateDB has no inverted index |
| Aggregation engine | SlateDB has no query planner or SQL |
| Trace query planner | SlateDB doesn't understand trace shape |

---

## 7. External Dependencies

Keep this list minimal. Every dependency is a maintenance burden.

| Crate | Purpose | Required? |
|-------|---------|-----------|
| `slatedb` | WAL, SSTables, compaction, caching, bloom | Yes |
| `tantivy` | Full-text search | Yes |
| `datafusion` | Aggregation query engine | Yes |
| `arrow` | In-memory columnar format | Yes |
| `parquet` | Columnar storage format | Yes |
| `object_store` | S3/GCS abstraction (re-exported by SlateDB) | Yes |
| `flatbuffers` | Zero-copy serialization | Yes |
| `serde` / `serde_json` | JSON attributes | Yes |
| `axum` | HTTP server | Yes |
| `tonic` | gRPC server | Yes |
| `tokio` | Async runtime | Yes |
| `thiserror` | Error types | Yes |
| `tracing` | Structured logging | Yes |
| `criterion` | Benchmarking | Dev only |

No other dependencies without justification.
