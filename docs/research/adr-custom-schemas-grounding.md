# Grounding: custom schemas for TraceLake, eval storage via inspect-ai

Read-only context for a design sketch. Do not write source code in the repo; write your
sketch to the output path you were given.

## What TraceLake is

An open-source database for LLM agent traces. Object storage is the source of truth
(S3/GCS), a SlateDB LSM is the hot ingest/WAL layer, and compaction writes immutable
segments to the object store. Queries merge hot + compacted.

Crates (all Rust 2024, edition 2024, `thiserror` for errors, no `unwrap` outside tests,
every `pub fn`/`pub struct` documented, functions under 50 lines):

- `tracelake-core`: data types, key encoding, pack formats, segment manifest. Pure.
- `tracelake-ingest`: SlateDB wrapper (`TraceLakeDb`), ingest validation.
- `tracelake-compact`: reads hot spans, writes `.tracepack` + `.bloom` + `.stats`,
  publishes `manifest.json` per (tenant, namespace).
- `tracelake-query`: `get_trace`, `search_traces`, hot/cold merge, stats pruning.
- `tracelake-server`: axum HTTP API.

## Existing shapes that the design must interop with

Span key (canonical, lexicographic order = query order):

```
v1/t/{tenant}/ns/{namespace}/tr/{trace_id}/sp/{20-digit nanosecond start}/{span_id}
```

`encode_span_key`, `decode_span_key`, `tenant_prefix`, `namespace_prefix`,
`trace_prefix` live in `tracelake-core/src/key.rs`. Tenant and namespace segments
reject empty values and `/`.

Hot store API (`TraceLakeDb`, `tracelake-ingest/src/db.rs`): `put_span`, `put_spans`,
`get_trace_spans`, `get_namespace_spans`, `scan_all_spans`. `scan_all_spans` currently
scans the whole keyspace with an empty prefix.

Trace pack (`tracelake-core/src/trace_pack.rs`): `[magic TRCPAK01][trace_count u32]
[span_count u64][directory_offset u64][trace blocks][directory: u64 entry offsets then
entries][crc32 footer]`. Directory entries are `{trace_id u16 len + bytes, span_count
u32, block_offset u64, block_len u64}` sorted by trace_id, binary-searched on read. Span
payloads are length-prefixed JSON. `decode_trace_pack` validates magic, checksum, and
directory bounds, then `read_trace` decodes one block only.

Manifest (`tracelake-core/src/segment.rs`): `SegmentManifest { version, tenant_id,
namespace, segments: Vec<SegmentEntry> }`, `SegmentEntry { segment_id: "segment-000001",
trace_count, span_count, files: Vec<SegmentFile>, stats: SegmentStats }`,
`SegmentFile { kind, path, size_bytes, crc32 }`, `SegmentFileKind { TracePack, Bloom,
Stats }`, `SegmentStats { min/max of start_time, end_time, latency_ms, cost_usd, score
}`. Published with one PUT at `{base_path}/{tenant}/{namespace}/manifest.json`.

Segment files: `{base_path}/{tenant}/{namespace}/{segment_id}.{ext}`.

Query: `TraceLakeReader { db, store, packs, base_path }`; `get_trace` checks each
segment's bloom file, reads the pack on a possible hit, then merges with hot spans
(hot wins on `span_id` conflict, newer segments before older); `search_traces` prunes
segments with `stats_may_overlap` on `start_time`, then reads whole packs, dedups by
`span_id`, and summarizes per trace.

Verification protocol (`docs/verification.md`): pure functions in `tracelake-core` and
the pure query helpers get a proptest property, and a Kani harness where the solver can
reach it. Never weaken a property to pass. Bug fixes land with the failing test first.

Constraints from `AGENTS.md`: YAGNI (no auth, no SQL parser, no admin UI, no vector
search), one definition of each schema/key/manifest format, async only around I/O, CPU
work synchronous, no magic numbers, errors carry context.

## The task

Two deliverables:

1. Users define their own schemas. A schema is a named table with typed fields, a
   primary key, and per-tenant/namespace registration. Values are validated against the
   schema at the ingest boundary and stored in a typed format, not as opaque JSON.
2. Evals from inspect-ai are stored and queried through that mechanism, efficiently:
   filter and project on declared fields without deserializing every field, and skip
   whole segments using per-field stats.

## Fixed usage contract (viewer-facing API sketch; derive your types from it)

```rust
// Schema definition and registration
let schema = TableSchema::new("inspect_ai_samples")
    .field("sample_id", FieldType::String)
    .field("eval_id", FieldType::String)
    .field("epoch", FieldType::I64)
    .field("task", FieldType::String)
    .field("model", FieldType::String)
    .field("score", FieldType::F64)          // optional; use required_field instead
    .primary_key("sample_id");
let schema = schema.build()?;                // validation, one failure mode per error variant

// Registration (object store is the source of truth)
schema_registry.put(tenant, namespace, &schema).await?;

// Ingest: validation at the boundary, typed storage underneath
db.put_records(tenant, namespace, &schema, &records).await?;

// Query: typed filters + projection
let query = RecordQuery {
    filter: Some(RecordFilter::And(vec![
        RecordFilter::Eq { field: "task".into(), value: json!("gsm8k") },
        RecordFilter::Ge { field: "score".into(), value: json!(0.5) },
    ])),
    projection: vec!["sample_id".into(), "score".into()],
    limit: 100,
    ..RecordQuery::default()
};
let page = reader.query_records(tenant, namespace, "inspect_ai_samples", &query).await?;
```

## Grading rubric (the picker scores against these)

1. Type sketches and signatures only, plus the flow. No implementation.
2. One unambiguous storage and validation path: no second way to write or read a record.
3. Per-field stats and segment pruning are part of the shape, not an afterthought.
4. Schema correctness is enforced by types or by one validation point, not scattered
   checks.
5. The eval adapter fits the same mechanism as any user-defined table, with no special
   case in the storage or query layer.
6. Fits the existing crate boundaries and the existing verification protocol; names the
   proptest and Kani candidates.
7. Smallest public surface that hides the most complexity. Explicitly names what it
   does not do (schema evolution, columnar Parquet, etc.).

## Output

A design sketch: data structures first (field-by-field), then signatures and data flow,
then the module map naming which crate owns each piece, then the storage layout, then
the validation points, then the rejected alternatives with one line each on why the
alternatives lose. Shapes only. No code bodies.
