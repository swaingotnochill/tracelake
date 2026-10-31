# Alpha readiness

This is the alpha checklist for TraceLake as a trace database. It states what a
user can do today, what is required before calling the release an alpha, and what
is explicitly out of scope.

## What works

| Capability | How to use it | Evidence |
|------------|---------------|----------|
| Ingest spans | `POST /v1/spans`, `put_spans` | `db.rs` tests, server e2e |
| Fetch a full trace | `POST /v1/traces/...`, `get_trace` | `trace_fetch.rs` tests, bloom-skip proof, range-read fetch |
| Structural search | `TraceSearch` with status, model, time | `search.rs` tests, stats-prune proof |
| Full-text search | `TraceSearch.text_query` | `text_indexer.rs`, `search.rs` tests |
| Aggregate spans | `POST /v1/spans/aggregate` | `aggregate.rs`, server e2e |
| Define a table | `PUT /v1/tables/{t}/{ns}/{table}/schema` | schema registry tests |
| Evolve a table | register a compatible shape | `evolve` property tests, server e2e |
| Write and query records | `POST .../records`, `GET .../records/{id}`, `POST .../query` | `record_query` tests |
| Aggregate records | `POST .../aggregate` | `aggregate.rs`, server e2e |
| Ingest inspect-ai evals | `POST /v1/evals` | `inspect_ai.rs`, example log |
| Delete a trace or record | `DELETE /v1/traces/...`, `DELETE .../records/{id}` | delete e2e test |
| Retention | `POST /v1/retention` with a cutoff | retention test |
| Durable local storage | `TRACELAKE_DATA_DIR` | restart test |
| Compaction | `POST /v1/compact` | segment tests, server e2e |

## Alpha exit criteria

| Criterion | State | Note |
|-----------|-------|------|
| Every write is durable before the client sees 200 | Yes | SlateDB WAL; restart test |
| A trace fetch reads one pack and skips absent traces with bloom filters | Yes | Bloom-negative test. Segments are read as three ranges, so cost follows trace size, measured at 0.14 to 0.17 ms per trace |
| A search can skip a segment without reading it | Yes | Deleted-pack proofs |
| Analytics do not deserialize rows | Yes | Parquet columns and DataFusion |
| Schemas can change without rewriting history | Yes | Pack field descriptors and defaults |
| Tenant isolation at the key and path level | Yes | Isolation tests |
| A single process can restart without data loss | Yes | Restart test |
| Request size is bounded | Yes | 32 MiB body limit |
| Compaction does not duplicate records | Yes | Watermark plus hot-key deletion; repeat-compaction test |
| Deletes and retention exist | Yes | Tombstones mask compacted copies; delete and retention tests |
| Authentication exists | No | Loopback bind only |
| Query timeouts and limits exist | No | Broad queries can run long |

## Required before the alpha tag

1. A query timeout and a per-tenant concurrency cap.
2. S3 and GCS wiring in the server. Only the local file-system store is wired
   today, although every store path goes through `object_store`.
3. A dependency audit job in CI.
4. Segment rewrite so deleted rows stop consuming space. Deletes are logically
   correct today; tombstones accumulate in the manifest and old segments keep
   the deleted bytes.

## Operational notes

- The server writes the hot store and segments under `TRACELAKE_DATA_DIR`.
  Back up that directory or, on object storage, the bucket prefix.
- One server process owns a data directory. Two processes against one directory
  are untested.
- Compaction is triggered by `POST /v1/compact`; there is no background scheduler
  yet. It compacts keys above each namespace watermark, publishes the segment,
  then deletes those hot keys. A crash between publish and delete leaves
  duplicates that the watermark skips on the next run.
- A span or record that arrives with a key below the watermark stays in the hot
  store and is served from there. It is not copied into a segment. This keeps
  late data readable while hot data is retained, and is a limitation once hot
  retention exists.
- Deletes write a tombstone under `v1/d/`, remove the hot copy, and mask
  compacted copies. Compaction folds tombstones into the manifest. Retention
  deletes traces whose latest span starts before the cutoff; it does not rewrite
  old segments, so deleted bytes stay on disk until a segment rewrite lands.
- Bind to loopback or a private network. There is no authentication.

## Out of scope for alpha

- Vector search and embedding indexes.
- Multi-region replication.
- A SQL surface. Queries are typed structs and JSON.
- Authentication, RBAC, and audit logs.
