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
| Read limits | timeout and per-tenant cap | limiter unit tests, 429 e2e test |
| Dependency advisories | `audit` CI workflow | `.github/workflows/audit.yml` |
| Select an object store | `TRACELAKE_STORE=local`, `s3`, or `gcs` | `store.rs` tests |
| Durable storage | `TRACELAKE_DATA_DIR`, or an S3 or GCS bucket | restart test, store selection tests |
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
| Dependency advisories are checked in CI | Yes | `audit` workflow on changes and weekly |
| Object storage is selectable at runtime | Yes | `store.rs` selection and configuration tests. S3 and GCS are compile-tested and configuration-tested; no live bucket was exercised here |
| Request size is bounded | Yes | 32 MiB body limit |
| A read cannot run forever | Yes | 30 s query timeout, 504 on expiry |
| One tenant cannot starve others | Yes | 16 concurrent reads per tenant, 429 above it |
| Compaction does not duplicate records | Yes | Watermark plus hot-key deletion; repeat-compaction test |
| Deletes and retention exist | Yes | Tombstones mask compacted copies; delete and retention tests |
| Deleted bytes are reclaimed | Yes | Compaction rewrites affected segments, drops deleted rows, removes empty segments, and deletes old files; the test asserts smaller files and an absent trace in the new pack |
| Authentication exists | No | Loopback bind only |
| Request-rate limiting exists | No | Concurrency and timeouts exist, a per-second rate cap does not |

## Required before the alpha tag

1. A live S3 or GCS run in CI or a staging environment. The store selection is
   tested and the code paths compile, but no bucket credentials are available
   here.

## Operational notes

- Read limits are set by `TRACELAKE_QUERY_TIMEOUT_MS` (default 30000) and
  `TRACELAKE_TENANT_CONCURRENCY` (default 16). The cap is per process and per
  tenant name. A request over the cap gets 429. A request past the timeout gets
  504. The timeout wraps the database call; the permit is released when the call
  returns, times out, or is cancelled.
- `TRACELAKE_STORE` selects `local` (default), `s3`, or `gcs`.
  `TRACELAKE_BUCKET` names the bucket. `TRACELAKE_REGION` and
  `TRACELAKE_ENDPOINT` configure S3 or an S3-compatible server such as MinIO.
  `TRACELAKE_PREFIX` scopes remote paths and defaults to `tracelake`.
  Credentials come from the provider chain (`AWS_*` or `GOOGLE_*`); the server
  never reads keys from its own settings.
- For a local store, the server writes the hot store and segments under
  `TRACELAKE_DATA_DIR`. Back up that directory, or the bucket prefix on object
  storage.
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
  compacted copies. Compaction folds tombstones into the manifest, rewrites any
  segment whose bloom names a tombstoned id, drops the deleted rows, removes a
  segment that becomes empty, and deletes the old files. Retention deletes
  traces whose latest span starts before the cutoff.
- Bind to loopback or a private network. There is no authentication.

## Out of scope for alpha

- Vector search and embedding indexes.
- Multi-region replication.
- A SQL surface. Queries are typed structs and JSON.
- Authentication, RBAC, and audit logs.
