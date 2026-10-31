# HTTP API

TraceLake serves traces, user-defined tables, and eval logs over one HTTP API.
The default address is `127.0.0.1:8080`. There is no authentication; keep the
server on loopback or a private network.

All request and response bodies are JSON. Errors use the shape
`{"error": "message"}`.

## Limits

| Limit | Default | Configuration |
|-------|---------|---------------|
| Request body | 32 MiB | fixed |
| Query timeout | 30 s | `TRACELAKE_QUERY_TIMEOUT_MS` |
| Concurrent reads per tenant | 16 | `TRACELAKE_TENANT_CONCURRENCY` |

A read over the concurrency cap returns 429. A read past the timeout returns 504.
Both apply to fetch, search, record query, and both aggregate endpoints. Writes
are not throttled.

## Ingest spans

```
POST /v1/spans
```

```json
{
  "spans": [
    {
      "tenant_id": "acme",
      "namespace": "prod",
      "trace_id": "trace-1",
      "span_id": "span-a",
      "parent_span_id": null,
      "span_type": "llm",
      "name": "completion",
      "start_time": "2026-10-01T12:00:00Z",
      "end_time": "2026-10-01T12:00:01Z",
      "status": "ok",
      "attributes": {"env": "prod"},
      "input": {"prompt": "hello"},
      "output": {"text": "world"},
      "tokens": {"prompt": 10, "completion": 20, "total": 30},
      "cost_usd": 0.001,
      "model": "gpt-4o",
      "provider": "openai",
      "error": null,
      "tags": ["support"],
      "scores": [{"name": "quality", "value": 0.8}]
    }
  ]
}
```

Response: `{"written": 1}`.

`span_type` is one of `llm`, `tool`, `retriever`, `agent`, `chain`, `eval`,
`system`, or `{"custom": "name"}`. `status` is `ok`, `error`, or `timeout`.
`tenant_id`, `namespace`, `trace_id`, and `span_id` cannot be empty or contain
`/`.

## Fetch a trace

```
GET /v1/traces/{tenant_id}/{namespace}/{trace_id}
```

Response: a `Trace` object with `tenant_id`, `namespace`, `trace_id`, and
`spans` sorted by start time. Returns 404 when the trace does not exist or has
been deleted.

## Search traces

```
POST /v1/traces/search
```

```json
{
  "tenant_id": "acme",
  "namespace": "prod",
  "time_start": "2026-10-01T00:00:00Z",
  "time_end": "2026-10-02T00:00:00Z",
  "status": "error",
  "model": "gpt-4o",
  "text_query": "hallucination",
  "limit": 100
}
```

Every filter is optional. `time_start` and `time_end` bound span start time. A
trace matches when at least one span satisfies every filter. `text_query`
searches span name, input, output, and error through the Tantivy index, and is
combined with the structural filters.

Response: an array of summaries.

```json
[
  {
    "trace_id": "trace-1",
    "span_count": 5,
    "matched_spans": 1,
    "first_match_time": "2026-10-01T12:00:00Z",
    "last_match_time": "2026-10-01T12:00:00Z",
    "models": ["gpt-4o"],
    "error": "model timeout"
  }
]
```

Results are ordered by latest matching span, newest first, and capped at
`limit`.

## Aggregate spans

```
POST /v1/spans/aggregate
```

```json
{
  "tenant_id": "acme",
  "namespace": "prod",
  "group_by": ["model"],
  "metrics": [
    "count",
    {"p95": {"field": "latency_ms"}},
    {"sum": {"field": "cost_usd"}}
  ],
  "limit": 100
}
```

Available metrics are `count`, `sum`, `avg`, `min`, `max`, `p50`, `p95`, and
`p99`. Numeric metrics need a numeric field. Aggregation reads the Parquet
column files written by compaction; it does not decode span payloads.

Response:

```json
{
  "rows": [
    {"model": "gpt-4o", "count": 120, "p95_latency_ms": 812.0, "sum_cost_usd": 1.42}
  ],
  "scanned_segments": 4,
  "pruned_segments": 0
}
```

## Tables

A table is a typed schema with a primary key. One schema is registered per
tenant, namespace, and table. A compatible change becomes the next version.
Field names use letters, digits, `_`, `.`, or `-`, up to 128 bytes.

### Register a schema

```
PUT /v1/tables/{tenant_id}/{namespace}/{table}/schema
```

```json
{
  "version": 1,
  "table": "findings",
  "primary_key": "finding_id",
  "fields": [
    {"name": "finding_id", "field_type": "string", "required": true},
    {"name": "severity", "field_type": "string", "required": true},
    {"name": "confidence", "field_type": "f64", "required": false},
    {"name": "source", "field_type": "string", "required": true, "default": "manual"}
  ]
}
```

`field_type` is `string`, `i64`, `f64`, `bool`, `json`, or `timestamp`.
`timestamp` is an integer count of nanoseconds since the Unix epoch. The version
in the body is ignored; the registry assigns it.

Response: `{"table": "findings", "version": 2}`.

Compatible changes are: add an optional field, add a required field with a
default, drop a field, and change a field's type. A primary key change returns
409. An added required field without a default returns 400.

A type change rewrites every stored segment of the table under the new type and
converts the values. Text converts to numbers when it parses, numbers convert to
text, booleans convert to 0 and 1, and integral floats convert to integers. A
value that cannot convert stops the migration before anything is written and
returns 400; the schema and data stay unchanged. A field that changes to or from
`json` accepts any value converted to `json`, and cannot convert from an object
or array to a scalar.

### Read a schema

```
GET /v1/tables/{tenant_id}/{namespace}/{table}/schema
```

Response: the registered schema, or 404.

### Write records

```
POST /v1/tables/{tenant_id}/{namespace}/{table}/records
```

```json
{
  "records": [
    {"finding_id": "f-1", "severity": "high", "confidence": 0.9}
  ]
}
```

Records are validated against the registered schema. Unknown fields, missing
required fields, and wrong types return 400.

Response: `{"written": 1}`.

### Read, delete, and query records

```
GET    /v1/tables/{tenant_id}/{namespace}/{table}/records/{record_id}
DELETE /v1/tables/{tenant_id}/{namespace}/{table}/records/{record_id}
POST   /v1/tables/{tenant_id}/{namespace}/{table}/query
```

Query body:

```json
{
  "filter": {
    "and": [
      {"eq": {"field": "severity", "value": "high"}},
      {"ge": {"field": "confidence", "value": 0.5}}
    ]
  },
  "projection": ["severity", "confidence"],
  "limit": 100
}
```

Filters are `and`, `or`, `eq`, `gt`, `ge`, `lt`, and `le`. A comparison matches
only when the field is present and not null. The primary key is always included
in the result, and rows are sorted by primary key.

Response: `{"records": [...], "scanned_segments": 1, "pruned_segments": 0}`.

Delete returns `{"deleted": true}` or `{"deleted": false}` when only compacted
copies existed. Either way a tombstone is written.

### Aggregate records

```
POST /v1/tables/{tenant_id}/{namespace}/{table}/aggregate
```

Body and response match span aggregation, without a filter. Records with a
deleted tombstone are excluded, including from old column files.

## Evals

```
POST /v1/evals
```

```json
{
  "tenant_id": "acme",
  "namespace": "evals",
  "log": {
    "eval": {"eval_id": "eval-1", "task": "gsm8k", "model": "openai/gpt-4o"},
    "samples": [
      {
        "id": "17",
        "epoch": 1,
        "input": "What is 2 + 2?",
        "target": "4",
        "output": {"completion": "4"},
        "scores": {"accuracy": {"value": "C"}},
        "metadata": {"trace_id": "trace-17"},
        "model_usage": {"openai/gpt-4o": {"total_tokens": 15}},
        "completed_at": "2026-10-01T12:00:02Z"
      }
    ]
  }
}
```

`log` is an inspect-ai eval log in JSON form, as produced by
`inspect log dump`. The adapter writes rows to `inspect_ai_samples`. Pass, fail,
and partial score sentinels (`C`, `I`, `P`) become 1.0, 0.0, and 0.5.

Response: `{"written": 1, "table": "inspect_ai_samples"}`.

## Retention

```
POST /v1/retention
```

```json
{"tenant_id": "acme", "namespace": "prod", "before": "2026-09-01T00:00:00Z"}
```

Deletes every trace whose latest span starts before `before`, and writes
tombstones so compacted copies stay hidden. Response:
`{"deleted_traces": 12}`.

## Compact

```
POST /v1/compact
```

Compacts spans and records that were written since the last run, applies
tombstones, and rewrites segments that still hold deleted rows. Response:

```json
{"segments": ["segment-000003"], "record_segments": ["segment-000004"]}
```

A run with no new data and no deleted rows returns empty lists. A segment that
becomes empty is removed from its manifest.

## Status codes

| Code | Meaning |
|------|---------|
| 200 | Success |
| 400 | Invalid body, identifier, schema change, or query |
| 404 | Trace, record, or schema not found |
| 409 | Incompatible schema change |
| 413 | Request body over 32 MiB |
| 429 | Tenant at its concurrent read cap |
| 500 | Internal error; details go to server stderr |
| 504 | Query exceeded its timeout |

## Example session

```bash
curl -s localhost:8080/v1/spans -H 'content-type: application/json' \
  -d '{"spans":[{"tenant_id":"acme","namespace":"prod","trace_id":"t-1",
       "span_id":"s-1","parent_span_id":null,"span_type":"llm","name":"run",
       "start_time":"2026-10-01T12:00:00Z","end_time":null,"status":"ok",
       "input":null,"output":null,"tokens":null,"cost_usd":null,"model":null,
       "provider":null,"error":null}]}'

curl -s localhost:8080/v1/traces/acme/prod/t-1 | jq '.spans | length'
```
