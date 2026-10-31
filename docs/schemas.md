# Custom table schemas

TraceLake stores spans as a fixed shape. Everything else, including eval results, is a
user-defined table. A table has a typed schema, a primary key, and one schema object per
tenant and namespace.

## Declare a schema

```rust
use tracelake_core::{FieldType, TableSchema};

let schema = TableSchema::builder("inspect_ai_samples")
    .field("sample_id", FieldType::String)
    .required_field("task", FieldType::String)
    .field("score", FieldType::F64)
    .field("completed_at", FieldType::Timestamp)
    .primary_key("sample_id")
    .build()?;
```

`build` checks the shape once. `validate_shape` applies the same rules when a stored
schema is loaded, so a hand-edited schema object is rejected before it reaches a query.

| Type | JSON value | Stored as |
|------|------------|-----------|
| `string` | string | length-prefixed UTF-8 |
| `i64` | integer | 8 bytes, little endian |
| `f64` | number | 8 bytes, IEEE 754 bits |
| `bool` | boolean | 1 byte |
| `json` | any object, array, or scalar except null | length-prefixed JSON |
| `timestamp` | integer | 8 bytes, nanoseconds since the Unix epoch |

The primary key must be a `string` field. Its value is the record's identity: the
storage key suffix and the pack directory entry both come from it. A record carries no
separate id field, so the two cannot disagree.

## Register and evolve the schema

```rust
let registered = put_schema(&store, base_path, tenant_id, namespace, &schema).await?;
```

The registry owns the version. The first registration writes version 1, an identical
shape is a no-op, and a compatible change writes the next version. Segments record their
own field list, so older data keeps its meaning after the schema moves on.

| Change | Allowed | How old records read |
|--------|---------|----------------------|
| Add an optional field | yes | field absent |
| Add a required field with a default | yes | the default |
| Add a required field without a default | no | earlier records cannot satisfy it |
| Drop a field | yes | field absent from the current schema |
| Change a field's type | no | the pack would decode as the wrong type |
| Change the primary key | no | record identity would change |

Adding a required field needs `required_field_with_default`:

```rust
let schema = TableSchema::builder("inspect_ai_samples")
    .field("sample_id", FieldType::String)
    .required_field_with_default("grader", FieldType::String, json!("exact_match"))
    .primary_key("sample_id")
    .build()?;
```

A type change needs a new table name and a rewrite of the records. Type changes are
refused because a pack written as `string` cannot be read as `i64` without guessing.

## Write records

```rust
let record = Record::new()
    .set("sample_id", json!("17@1"))
    .set("task", json!("gsm8k"))
    .set("score", json!(0.75));

db.put_records(tenant_id, namespace, &schema, &[record]).await?;
```

Validation happens at this boundary: unknown fields, missing required fields, and values
of the wrong type are rejected before any write. Reads trust the schema.

## Query records

```rust
let query = RecordQuery {
    filter: Some(RecordFilter::and(vec![
        RecordFilter::eq("task", json!("gsm8k")),
        RecordFilter::ge("score", json!(0.5)),
    ])),
    projection: vec!["task".into(), "score".into()],
    limit: 100,
};

let result = reader
    .query_records(tenant_id, namespace, "inspect_ai_samples", &query)
    .await?;
```

`and`, `or`, `eq`, `gt`, `ge`, `lt`, and `le` are the operators. A comparison matches only
when the field is present and not null. A filter or projection that names an undeclared
field, or uses a value of the wrong type, fails before any segment is read.

The result carries the projection plus the primary key, sorted by primary key, capped at
`limit`. `scanned_segments` and `pruned_segments` report what the planner did.

`get_record(tenant, namespace, table, record_id)` is the point lookup. It checks each
segment's bloom filter, then reads only the one pack that can hold the id, then prefers a
hot copy if one exists.

## Storage layout

```text
SlateDB hot store
  v1/r/t/{tenant}/ns/{namespace}/tb/{table}/r/{record_id}

Object store
  {base_path}/{tenant}/{namespace}/tables/{table}/schema.json
  {base_path}/{tenant}/{namespace}/tables/{table}/manifest.json
  {base_path}/{tenant}/{namespace}/tables/{table}/segment-000001.recordpack
  {base_path}/{tenant}/{namespace}/tables/{table}/segment-000001.bloom
  {base_path}/{tenant}/{namespace}/tables/{table}/segment-000001.stats
```

Record keys live under their own root, `v1/r/`, so a span scan and a record scan cannot
read each other's values.

A record pack is `[magic TRCREC01][field descriptors][record count u32][directory offset
u64][record blocks][directory offsets][directory entries][crc32]`. Each descriptor holds
a field name and its type. A record block is a presence bitmap over the pack's fields
followed by the present values in that order. A reader maps fields by name, skips pack
fields the current schema dropped, and fills declared defaults for fields the pack did
not hold. A field whose stored type differs from the declared type is reported only when
that field is read.

The segment manifest stores per-field bounds with the type they were computed from, so a
query never prunes on bounds from a different type.

## Stats and pruning

Compaction writes per-field bounds into the table manifest: `i64` bounds for `i64` and
`timestamp` fields, `f64` bounds for `f64`, byte bounds for `string`, and up to 64
distinct values for low-cardinality string fields. A query skips a segment when its
bounds prove no record can match.

The pruning law is checked by property tests: if a segment is pruned, no record it held
matched the filter. A field without bounds never prunes.

## Merge order

Segments are read oldest first, then the hot store. On a primary-key collision the hot
copy wins, then the newer segment. This holds while the hot store keeps the newest copy
of every record, which it does today because compaction does not delete hot data.

## HTTP API

| Method | Path | Body |
|--------|------|------|
| `PUT` | `/v1/tables/{tenant}/{ns}/{table}/schema` | `TableSchema` |
| `GET` | `/v1/tables/{tenant}/{ns}/{table}/schema` | none |
| `POST` | `/v1/tables/{tenant}/{ns}/{table}/records` | `{ "records": [ { field: value } ] }` |
| `GET` | `/v1/tables/{tenant}/{ns}/{table}/records/{record_id}` | none |
| `POST` | `/v1/tables/{tenant}/{ns}/{table}/query` | `RecordQuery` |
| `POST` | `/v1/evals` | `{ "tenant_id", "namespace", "log" }` |
| `POST` | `/v1/compact` | none |

`POST /v1/compact` compacts spans and records in one call and reports both segment lists.

## inspect-ai eval logs

`POST /v1/evals` registers the `inspect_ai_samples` schema, flattens the log, and writes
one row per sample. The adapter accepts the inspect-ai JSON shape: `eval` with `eval_id`,
`task`, `model`, `status`, and `samples` with `id`, `epoch`, `input`, `target`, `output`,
`scores`, `metadata`, `model_usage`, `total_time`, `completed_at`, and `error`.

| Column | Type | Source |
|--------|------|--------|
| `sample_id` | string | `{id}@{epoch}` |
| `eval_id`, `task`, `model` | string | eval header |
| `epoch` | i64 | sample |
| `status` | string | `error` when the sample has an error, else `success` |
| `error` | string | sample error |
| `scorer`, `score` | string, f64 | one scorer: the named one, or the first by name. inspect-ai pass, fail, and partial sentinels (`C`, `I`, `P`) map to 1.0, 0.0, and 0.5 |
| `total_tokens` | i64 | sum of `model_usage.*.total_tokens` |
| `total_time_s` | f64 | `total_time` |
| `completed_at` | timestamp | `completed_at` as nanoseconds |
| `trace_id` | string | `metadata.trace_id` when present |
| `input`, `target`, `output` | json | sample fields when present |
| `scores` | json | full scorer map, including answers and explanations |

The storage and query layers contain no eval-specific code. The adapter emits ordinary
records for an ordinary table.

## Measured cost

Ten segments of 1000 records each, about 2.4 KB per record, in-memory object store, release
build, on a machine with load average 14 on 8 cores. Median of 5 alternating runs:

| Query | Median | Range |
|-------|--------|-------|
| all 17 fields, no filter | 208 ms | 194 to 210 ms |
| 2 fields, no filter | 33 ms | 31 to 33 ms |
| 2 fields, filter matching every segment | 32 ms | 30 to 35 ms |
| 2 fields, filter pruning 9 of 10 segments | 2.7 ms | 2.6 to 3.0 ms |
| point lookup (bloom over 10 segments, one pack read) | 0.67 ms | 0.60 to 1.00 ms |

The limiter is JSON decoding and allocation, not object store I/O, because the store is in
memory. Pruning removes whole pack reads, so it cuts both decoding and I/O. Projection
skips the JSON parse of fields nobody asked for. With S3 as the store, add one GET per
scanned pack and two per pruned segment (manifest and nothing else).

## Text search

Trace search accepts `text_query`. Compaction builds a Tantivy index over every span's
name, input, output, and error and stores it as the segment's `.text` file. Text search
runs the index per segment, then applies the structural filters, so a trace must match
both.

Spans still in the hot store are not indexed yet, so a text query builds a throwaway
index over the hot spans for that request. It returns the same results, and it costs time
proportional to the hot buffer. Compaction moves those spans into an indexed segment.

Each indexed field is clipped at 256 KB (`MAX_INDEXED_TEXT_BYTES`). Prompts larger than
that remain searchable in their first 256 KB.

## Limits

- A field's type cannot change. Register a new table name and write the records again.
- No joins, no aggregation, and no Parquet column files for records yet. Filtering is the
  pushdown; aggregation belongs to the columnar step.
- Every segment read decodes all requested fields of every record in the pack. Selection
  pressure per pack is a later step.
- Text search over the hot store rebuilds an index per query. It is correct, not cheap.
