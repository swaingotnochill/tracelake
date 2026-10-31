# Alpha readiness

This is the alpha checklist for TraceLake as a trace database. It states what a
user can do today, what is required before calling the release an alpha, and what
is explicitly out of scope.

## What works

| Capability | How to use it | Evidence |
|------------|---------------|----------|
| Ingest spans | `POST /v1/spans`, `put_spans` | `db.rs` tests, server e2e |
| Fetch a full trace | `POST /v1/traces/...`, `get_trace` | `trace_fetch.rs` tests, bloom-skip proof |
| Structural search | `TraceSearch` with status, model, time | `search.rs` tests, stats-prune proof |
| Full-text search | `TraceSearch.text_query` | `text_indexer.rs`, `search.rs` tests |
| Aggregate spans | `POST /v1/spans/aggregate` | `aggregate.rs`, server e2e |
| Define a table | `PUT /v1/tables/{t}/{ns}/{table}/schema` | schema registry tests |
| Evolve a table | register a compatible shape | `evolve` property tests, server e2e |
| Write and query records | `POST .../records`, `GET .../records/{id}`, `POST .../query` | `record_query` tests |
| Aggregate records | `POST .../aggregate` | `aggregate.rs`, server e2e |
| Ingest inspect-ai evals | `POST /v1/evals` | `inspect_ai.rs`, example log |
| Durable local storage | `TRACELAKE_DATA_DIR` | restart test |
| Compaction | `POST /v1/compact` | segment tests, server e2e |

## Alpha exit criteria

| Criterion | State | Note |
|-----------|-------|------|
| Every write is durable before the client sees 200 | Yes | SlateDB WAL; restart test |
| A trace fetch reads one pack and skips absent traces with bloom filters | Yes | Bloom-negative test |
| A search can skip a segment without reading it | Yes | Deleted-pack proofs |
| Analytics do not deserialize rows | Yes | Parquet columns and DataFusion |
| Schemas can change without rewriting history | Yes | Pack field descriptors and defaults |
| Tenant isolation at the key and path level | Yes | Isolation tests |
| A single process can restart without data loss | Yes | Restart test |
| Request size is bounded | Yes | 32 MiB body limit |
| Compaction does not duplicate records | No | Hot keys are not dropped |
| Deletes and retention exist | No | Alpha blocker for regulated data |
| Authentication exists | No | Loopback bind only |
| Query timeouts and limits exist | No | Broad queries can run long |

## Required before the alpha tag

1. Compaction watermark and hot-key drop, so a second compaction does not copy
   the same records into a new segment.
2. A delete path with tombstones that survive compaction and retention windows.
3. A query timeout and a per-tenant concurrency cap.
4. S3 and GCS wiring in the server. Only the local file-system store is wired
   today, although every store path goes through `object_store`.
5. A dependency audit job in CI.

Items 1 and 2 are the ones that change the storage format, so they should land
before any external alpha user writes data.

## Operational notes

- The server writes the hot store and segments under `TRACELAKE_DATA_DIR`.
  Back up that directory or, on object storage, the bucket prefix.
- One server process owns a data directory. Two processes against one directory
  are untested.
- Compaction is triggered by `POST /v1/compact`; there is no background scheduler
  yet.
- Bind to loopback or a private network. There is no authentication.

## Out of scope for alpha

- Vector search and embedding indexes.
- Multi-region replication.
- A SQL surface. Queries are typed structs and JSON.
- Authentication, RBAC, and audit logs.
