# Gap analysis: TraceLake against SmithDB and Brainstore

Written at the alpha milestone on 2026-10-31. Sources are the vendor
architecture posts listed at the end.

## What each system is

SmithDB is LangChain's data layer for LangSmith. It is an object-storage-backed
LSM with a Vortex columnar format, Apache DataFusion for analytics, a small
Postgres metastore for segment metadata, and stateless ingestion, query, and
compaction services. It reports sub-100 ms trace loads and sub-second full-text
search.

Brainstore is Braintrust's engine. Every write appends to a write-ahead log on
object storage, and continuous compaction turns segments into a row store, a
Tantivy inverted index, a column store, vector indexes, and bloom filters. It
reports under 50 ms hot and under 500 ms cold full-text search, and it treats
semi-structured fields as first class instead of shredding JSON into a fixed
schema.

## Where TraceLake stands

| Capability | TraceLake today | SmithDB | Brainstore |
|------------|-----------------|---------|------------|
| Object storage as truth | Yes, through `object_store` | Yes | Yes |
| Hot write buffer | SlateDB LSM | LSM | Object-store WAL plus memory |
| Trace-local row store | Trace pack keyed by trace_id, fetched by range | Yes | Row store per segment |
| Typed record tables | Yes, with schema evolution | Yes, columnar | Semi-structured first |
| Filter pushdown | Bloom, per-field stats | Segment metadata | Segment metadata |
| Full-text search | Tantivy per segment, hot fallback | Inverted index | Tantivy inverted index |
| Analytics | DataFusion over Parquet columns (this milestone) | DataFusion over Vortex | Column store |
| Metadata store | JSON manifests in object storage | Postgres | Metadata store |
| Deletes and retention | Not implemented | Expected | Expected |
| Vector search | Not implemented, out of scope | Not stated | Yes |
| Auth and tenancy controls | None, keys are namespaced | Enterprise controls | Per-customer partitions |
| Stateless services | Library plus one process | Yes | Yes |
| OTLP and SDK ingestion | Not implemented | LangSmith clients | Braintrust clients |
| Payload preview | Not implemented, full spans only | Not stated | Full spans |
| Compression | SlateDB internal, raw JSON in packs | Vortex encoding | Tantivy and column encodings |

Trace fetch is range-based as of the performance review: a lookup reads the
header, the directory, and one trace block, so its cost follows trace size
rather than segment size. See [performance.md](../performance.md).

## Gaps that matter for alpha

1. **Compaction repeats work.** The compactor scans the whole hot store on every
   run, and hot data is not removed after compaction. Segments therefore
   duplicate records, and queries deduplicate them. This is correct but grows
   storage and wastes compaction time. Alpha needs a per-namespace watermark and
   a drop step for compacted hot keys.
2. **No deletes or retention.** There is no way to remove a trace. A regulated
   deployment cannot use the alpha until this exists.
3. **Analytics are incomplete.** Aggregation over Parquet columns is landing in
   this milestone. SmithDB and Brainstore already serve percentiles and grouped
   counts.
4. **One server, one process.** The architecture is stateless by construction,
   but the server still owns an in-process hot store. Running two servers against
   one object store is untested.
5. **No OTLP or SDK.** Every client must speak the JSON API. This blocks the
   common adoption path.
6. **Text search over hot data is expensive.** It builds an index per query.
   SmithDB indexes at L0 and Brainstore indexes the WAL, so neither pays that
   cost.

## Gaps that can wait

- Vector search. Out of scope per `AGENTS.md`, and not needed for trace
  debugging.
- A Postgres metastore. JSON manifests are enough while one compactor owns a
  namespace. SmithDB's metastore buys transactional handoff between stateless
  services.
- Multi-region replication. Object storage already handles durability.
- Compression tuning. Packs store JSON. Zstd over pack blocks and Parquet
  encoding will close most of the size gap.
- Payload preview so list queries do not deserialize multi-megabyte fields.

## What to do next, in order

1. Compaction watermark and hot-key drop, so segments stop duplicating records.
2. Deletes and retention with tombstones that survive compaction.
3. Aggregation coverage for span percentiles by model and status.
4. OTLP ingestion and a small client SDK.
5. Query timeouts and per-tenant limits, then authentication.

## Sources

- [Introducing SmithDB](https://www.langchain.com/blog/introducing-smithdb)
- [Full text search in SmithDB](https://www.langchain.com/blog/full-text-search-in-smithdb-designing-an-inverted-index-for-object-storage)
- [SmithDB entry on Database of Databases](https://dbdb.io/db/smithdb)
- [Brainstore architecture](https://www.braintrust.dev/blog/brainstore-architecture)
- [Brainstore announcement](https://www.braintrust.dev/blog/brainstore)
