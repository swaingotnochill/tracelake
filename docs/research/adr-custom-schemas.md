# Decision: user-defined table schemas and inspect-ai eval storage

Date: 2026-10-31.

## Problem

The database stored one fixed shape, the span. Eval runs from inspect-ai produce a
different shape. A sample carries a task, a model, an epoch, a score per scorer, token
usage, timings, and a large input and output payload. Teams also want their own tables,
not just evals.

Storing those rows in `Span.attributes` would have kept the code small and made the
queries wrong. Every filter would extract from JSON at query time, which is the cost this
database exists to avoid. The trace path already had the right machinery, hot and cold
merge, trace packs, bloom filters, stats, manifests, and a single publish point. The task
was to give that machinery a schema-driven row without building a second database beside
it.

## What done means

A user registers a typed schema, writes records, compacts, and queries typed fields with
segment pruning and field projection. An inspect-ai eval log travels the same path with
no eval-specific code in storage or query. Evidence is a passing suite plus one HTTP test
that registers a schema, writes records, queries, compacts, queries again, and ingests an
eval log.

## The shape

- `TableSchema` holds a version, a table name, a primary key, and typed fields.
  `validate_shape` is the only rule set for the shape, and both construction and loading
  from the object store call it.
- `Record` holds only fields. Identity comes from the primary-key value through
  `TableSchema::record_id`, so a record cannot carry an id that disagrees with its key
  field.
- Records are validated at `put_records` and again inside `encode_record_pack`. Reads
  trust the schema.
- Hot records live at `v1/r/t/{tenant}/ns/{namespace}/tb/{table}/r/{record_id}` as JSON.
  Span keys start with `v1/t/`, so the two key families cannot read each other's values.
- Compaction writes one `TRCREC01` pack per table segment, a bloom filter over record ids,
  per-field stats, and the table manifest. The pack carries its own field descriptors, so
  a pack written under an older shape stays readable after the schema evolves.
- `RecordQuery` carries one filter tree, a projection, and a limit. `RecordQuery::validate`
  checks fields and value types once before any segment read. Segment pruning compares the
  filter against per-field min and max and against up to 64 distinct string values.
- The inspect-ai adapter is an ingest factory. It emits ordinary records for the ordinary
  table `inspect_ai_samples`.

## Principles that changed a decision

- **Model the domain.** Eval rows became a table with a declared schema instead of loose
  attributes, and segment bounds became data in the manifest instead of extra branches in
  the search path.
- **Type system discipline.** Removing `Record.record_id` deleted a state that could
  disagree with the primary key. Per-pack field descriptors make a type mismatch an error
  only when the mismatched field is read, instead of a whole-segment failure.
- **Boundary discipline.** Validation sits at the two write boundaries and in
  `RecordQuery::validate`. The query and merge code trusts the schema.
- **Fix root causes.** The pack reader dropped the 4-byte length prefix on strings and
  JSON. The fix went into `read_bytes`, the one reader, and a round-trip test now pins it.
  The duplicated record identity was fixed by removing the second source, not by checking
  that the two agree.
- **Sequence verifiable units.** Core, ingest, compaction, query, and server landed as
  separate units, each with a green test run before the next started.
- **Laziness protocol.** No second storage format, no Parquet files, no new manifest
  type, no query planner. The record path reuses the existing manifest, bloom filter,
  checksum helper, pack pattern, and merge rules.
- **Prove it works.** The evidence is the HTTP end-to-end test and the full suite, not a
  clean compile.
- **Explain the number.** The benchmark reports run count, range, dataset shape, and the
  limiter instead of a single speedup.

## Design space

Two design seats ran against the same usage contract and grounding document. Both seats
failed on the first launch because the pstack model config named providers this host does
not have (`anthropic/*`, `opencode/*`), and the only configured provider is `openrouter`.
After the config was pointed at DeepSeek, both seats completed and converged on the same
shape: one typed row block with a presence bitmap, a table manifest holding per-field
stats, record keys under `v1/r/`, and inspect-ai as a pure ingest factory. The agreement
removed the need to graft one design onto the other.

Adopted from the first seat: identity derived from the primary key, stored schemas
re-validated on load, and an optional score because errored samples have none.

Adopted from the second seat: filter narrowing as a single gate, and one record body used
by both hot and cold paths instead of a second codec.

## Rejected alternatives

- **Attributes only, no schema.** No typed filters, and every query pays JSON extraction
  per row. This is the design the product exists to replace.
- **Column chunks, one file per field.** Needs a second physical layout and a second codec,
  and the eval table has 17 columns, not thousands. Column files return with the Parquet
  aggregation step.
- **Derive one schema per eval, with a column per scorer.** Scorer names change per eval,
  and a registered schema is immutable once segments exist, so the table would churn and
  old packs would stop decoding. Scorer selection stays a caller decision through the
  `scorer` and `score` columns.
- **Parquet and DataFusion now.** The project plan holds aggregation for a later step, and
  record filtering does not need a query engine.
- **Schema migration in this change.** Immutable schemas keep packs decodable. Changing a
  shape today means a new table name.

## Verification

| Tier | What | Where |
|------|------|-------|
| PROVEN (pending CI) | record pack decoder and directory walk never panic under any single ASCII corruption with the checksum restamped | `record_pack.rs::verify`. The harness exists and CI runs `cargo kani -p tracelake-core`. A local run exceeded 30 minutes on Apple Silicon CBMC, so this row stays PROPERTY tier until the CI job passes. |
| PROPERTY | pack round trip, projection exactness, foreign schema refusal, arbitrary bytes never panic | `tracelake-core/tests/prop_record_pack.rs` |
| PROPERTY | pruning a segment means no record in it matched, inclusive bounds decompose, boolean laws, absent fields never match | `tracelake-query/src/record_query/verification.rs` |
| INTEGRATION | hot and cold merge, hot wins, bloom negative lookup, stats prune proven by deleting the pruned pack, point lookup | `tracelake-query/src/record_query/tests.rs` |
| INTEGRATION | schema registration, record ingest, query, compaction, eval ingest over HTTP | `tracelake-server/src/lib.rs` |

Commands and results:

```text
cargo test --workspace          # 154 passed, 0 failed, 4 ignored benchmarks
cargo clippy --workspace --all-targets -- -D warnings   # exit 0
cargo fmt --check               # clean
cargo kani -p tracelake-core --harness restamped_corruptions_never_panic_in_decode_or_directory
                                 # exceeded a 30 minute local timeout; CI owns this check
```

## Bugs found and pinned

1. `read_bytes` read the length at the cursor but ended the slice at cursor plus length,
   so every string and JSON field decoded one prefix short and the last field reported a
   truncation. A temporary probe test printed the pack bytes and the cursor positions,
   which showed the cursor moving 2 bytes for a 2-byte string. The fix moved the payload
   start past the prefix. `every_declared_type_round_trips_exactly` and the round-trip
   property now fail if it regresses.
2. `Record` carried both `record_id` and a primary-key field. The two could disagree and
   the pack directory used one while queries used the other. The field is gone.
3. An unreachable non-finite float check was removed after the test could not construct a
   non-finite value, because JSON cannot represent one.
4. Delegated design seats failed with "OAuth access token is invalid" and "Insufficient
   account funds". The cause was model ids for providers this host does not configure.
   `pstack-models.md` now names only `openrouter/deepseek/deepseek-v4.1-flash-1017`.

## Revision: schema evolution

The first version registered one immutable shape per table. That was too strict. The
systems this one is measured against do not work that way. turbopuffer infers attribute
types per namespace and lets a namespace hold sparse attributes. LanceDB applies add,
alter, and drop column operations without rewriting all data. Brainstore keeps indexes
and metadata per segment and treats semi-structured data as a first-class citizen rather
than shredding it into a fixed schema.

The shape now is schema-on-read with compatibility rules:

- A record pack stores its own field names and types. A reader maps by name, skips fields
  the current schema dropped, and fills declared defaults for fields the pack predates.
- `TableSchema::evolve` allows adding optional fields, adding required fields with a
  default, and dropping fields. It refuses type changes and primary-key changes.
- The registry assigns versions. Old segments are never rewritten.
- Per-field stats carry the type they were computed from, so pruning never compares
  bounds across a type change.

The immutable rule cost one recorded bug. Hot records bypassed pack default substitution,
so a record written before a defaulted field existed read back without the default when
the hot copy won the merge. `TableSchema::fill_defaults` now runs at the merge boundary
for hot records, and the end-to-end test writes a record, compacts, evolves, and reads the
default back.

Still refused: changing a field's type. A pack written as `string` cannot be read as
`i64` without guessing, so the rule is a new table name and a rewrite. That is a
deliberate limit, not an oversight.

## Full-text search (Step 7)

Compaction now builds a Tantivy index over span name, input, output, and error and stores
it as the segment `.text` file. `TraceSearch.text_query` runs the index per segment, then
applies the structural filters. Spans in the hot store are indexed per query into a
throwaway index, so recent data is searchable before compaction. The cost of that choice
is documented in `docs/schemas.md`.

## Open questions

- Pruning and hot-store garbage collection. Today the hot store keeps the newest copy of
  every record, so pruning cannot surface a stale version. If SlateDB starts dropping
  compacted hot data, a pruned segment could return an old version while a newer one sits
  in a segment the filter skipped. The fix would be a newest-version guard that reads the
  newest segment bloom before trusting an older row.
- Aggregation. Per-scorer percentiles and counts need column files or a full pack scan.
  The eval UI will want them.
- Nested score fields. `scores` is one JSON column. Filtering on `scores.accuracy`
  requires projecting the column and filtering in the client, or a separate typed column
  chosen at schema time.

## Next step

Aggregation over record packs, so per-scorer counts and percentiles do not require a scan
in the client.
