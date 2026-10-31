# TraceLake Verification Protocol

This document defines what "verified" means in TraceLake, what is covered,
how to run the verification suite, and the rules any contributor — human or
AI agent — must follow when changing verified code.

Companion research: [formal-verification-brief.md](research/formal-verification-brief.md)
(tool selection), [rocq-of-rust-assessment.md](research/rocq-of-rust-assessment.md)
(rejected alternative), [fv-implementation-report.md](research/fv-implementation-report.md)
(first results).

## 1. The three tiers

Every operation we own sits in exactly one tier. A claim of "verified" must
always name the tier.

| Tier | Meaning | Tools | Trust level |
|------|---------|-------|-------------|
| **PROVEN** | Holds for **all** inputs within documented bounds (loop unwinds). No panic, no UB, functional law. | Kani (`#[cfg(kani)] mod verify` in the source file) | Highest — counterexample-free by construction |
| **PROPERTY** | Holds for hundreds of randomized, shrunk cases; regressions are pinned via committed seed files. | proptest (`tests/prop_*.rs`, `src/search/verification.rs`) | High — bugs in practice |
| **INTEGRATION** | Behavior checked end-to-end on concrete scenarios. | `#[tokio::test]` suites + router-level HTTP tests | Normal test trust |

**Statistical properties** (e.g. bloom FPR ≤ 2× target) are PROPERTY tier
with explicitly generous margins; they cannot be proven, only bounded.

## 2. Trusted boundaries (NOT verified, by design)

SlateDB, tokio, axum, `object_store`, serde/serde_json internals, crc32fast,
and chrono internals are **assumptions**. No proof crosses into them. The
async orchestration (`get_trace`/`search_traces` flows, compactor I/O,
server handlers) is INTEGRATION tier: its pure decision logic is extracted
and verified (see manifest), its I/O choreography is tested.

If SlateDB returns a span `scan_prefix` did not store, no proof we write
catches that. That is the boundary, stated plainly.

## 3. Coverage manifest

| Function | Tier | Properties | Where |
|----------|------|------------|-------|
| `key::encode_span_key` / `decode_span_key` | PROPERTY (full keys) + PROVEN (timestamp codec core) | full-key round-trip, ordering, prefix laws on random keys; the extracted `encode_timestamp_bytes`/`parse_timestamp_digits` codec is proven for ALL non-negative i64 (round-trip, ordering, fold totality) — `format!`/std-parse/String plumbing stays out of the solver by design | `key.rs::verify` (Kani), `tests/prop_key.rs` |
| `key::*_prefix` | PROPERTY | prefix isolation between traces; nested prefix structure | `tests/prop_key.rs` |
| `key::encode_span_storage_key` | PROPERTY | composes with decode to span identity | `tests/prop_key.rs` |
| `bloom::BloomFilter` insert/contains | PROVEN (bit core) + PROPERTY | `bit_position` (the one expression shared by insert/contains) proven in-bounds for all double hashes and probe counts — no-false-negative is structural (same function both sides) + statistical (proptest incl. round-trip) | `bloom.rs::verify`, `tests/prop_bloom.rs` |
| `bloom::BloomFilter::decode` | PROVEN + PROPERTY | totality on arbitrary bytes; accepted filters answer contains safely | same |
| bloom false-positive rate | statistical | absent-key hit rate < 0.05 at 0.01 target (deterministic hasher) | `tests/prop_bloom.rs` |
| `trace_pack::encode/decode_trace_pack` | PROVEN (decoder totality) + PROPERTY (round-trip) | restamped single-byte corruptions never panic through checksum, header, and directory walk (serde-free hand-built pack); exact round-trip and read_trace robustness on random corpora are proptest-tier — serde_json span payloads stay out of the solver by design | `trace_pack.rs::verify`, `tests/prop_trace_pack.rs` |
| `TracePack::read_trace` / `trace_ids` | PROVEN + PROPERTY | only requested block decoded; sorted ids | same |
| `segment::*` manifest types | PROPERTY | JSON round-trip equality; `next_segment_number` law; `segment_id` formatting (exhaustive 0..=999_999) | `tests/prop_segment.rs` |
| `query::merge_spans` | PROVEN + PROPERTY | L1 empty→empty; L2 sorted output; L3 last-layer-wins; L4 no-loss/no-spurious; L5 idempotence. Uses a deterministic hasher (`BuildHasherDefault<DefaultHasher>`) — OS-seeded `RandomState` is unprovable under Kani and made merges unreproducible | `merge.rs::verify`, `tests/prop_merge.rs` |
| `query::stats_may_overlap` | PROPERTY | prune soundness (never skips a possibly-matching segment) | `src/search/verification.rs` — Kani blocked: chrono `DateTime` has no symbolic construction; would need an i64-ns core extraction |
| `query::span_matches` | PROPERTY | per-filter exactness; unset filters always match; completeness | same |
| `query::summarize_trace` | INTEGRATION (+ helpers PROPERTY) | exercised via search suites | `search.rs` tests |
| `compact::group_spans` | PROPERTY | partition law: groups/traces ordered by first appearance, every span exactly once | `tests/prop_group.rs` |
| compact writers (bloom/stats/manifest/pack) | INTEGRATION | byte-exact segment round-trips | `compactor.rs` tests |
| ingest `TraceLakeDb` | INTEGRATION | write/read/isolation/100K | `db.rs` tests |
| `schema::TableSchema::validate` / `record_id` / `fingerprint` / `validate_shape` | PROPERTY | generated shapes: a record built from its schema validates, a dropped required field, an undeclared field, and a wrong-typed value are each reported; fingerprint is stable for one shape and changes with the shape; a loaded shape without its primary key is refused | `tests/prop_schema.rs` |
| `record_pack::encode/decode_record_pack` | PROVEN (decoder totality) + PROPERTY (round-trip, projection, foreign schema) | any single ASCII corruption with the checksum restamped either errors or walks the directory without panicking; exact round-trip and projection on random schemas and records; a pack written under another schema is refused; arbitrary bytes never panic the reader | `record_pack.rs::verify`, `tests/prop_record_pack.rs` |
| `query::record_matches` | PROPERTY | `and` and `or` follow their laws; `ge` equals `eq` or `gt`; `le` equals `eq` or `lt`; absent and null fields never match a comparison | `src/record_query/verification.rs` |
| `query::record_stats_may_match` | PROPERTY | prune soundness: if a segment is pruned, no record it held satisfies the filter | same |
| `query::RecordQuery::validate` | INTEGRATION | undeclared fields, wrong-typed filter values, and undeclared projections fail before any segment read | `record_query/tests.rs` |
| `compact::build_record_stats` | INTEGRATION | exact i64, f64, and string bounds, distinct tracking with its cap, null and absent values leave a field unbounded | `record_stats_writer.rs` tests |
| `compact::schema_store` | INTEGRATION | register once, identical re-registration is a no-op, a different shape conflicts, a stored shape is re-validated on load, isolation by tenant, namespace, and table | `schema_store.rs` tests |
| `query::query_records` / `get_record` | INTEGRATION | hot and cold merge with hot winning, stats prune proven by deleting the pruned pack, bloom-negative lookup, projection and limit, server e2e for custom tables and eval logs | `record_query/tests.rs`, server tests |
| `get_trace` / `search_traces` / server | INTEGRATION | hot/cold merge, bloom skip proof, stats prune proof, e2e HTTP | `trace_fetch.rs`, `search.rs`, server tests |

## 4. How to run

```bash
cargo test --workspace                     # everything incl. proptest suites (seconds)
cargo kani -p tracelake-core               # PROVEN tier, core (minutes)
cargo kani -p tracelake-query              # PROVEN tier, merge laws
cargo clippy --workspace --all-targets -- -D warnings
```

CI runs the same: check job on every PR; a Kani job when
`tracelake-core/**` or `tracelake-query/**` changes; Miri (nightly,
non-blocking) on `tracelake-core`.

## 5. Agent / contributor protocol (mandatory)

1. **Changing a pure function in `tracelake-core` or the pure helpers in
   `tracelake-query`** (`merge.rs`, `stats_may_overlap`, `span_matches`,
   `summarize_trace`, `group_spans`): run `cargo test --workspace` AND the
   relevant `cargo kani -p …` before committing. If you cannot run Kani,
   say so in the commit message — do not claim PROVEN tier.
2. **New pure function in those modules**: add a proptest property and, where
   Kani can reach it, a `#[cfg(kani)]` harness in the same file. Update the
   manifest in §3.
3. **Bug fix in verified code**: first add the failing property (proptest or
   Kani assertion) that reproduces the bug, then fix, then commit both
   together. The proptest seed file this generates must be committed.
4. **Reading failures**:
   - proptest prints a shrunk counterexample and writes a seed to
     `*.proptest-regressions`; rerun with `PROPTEST_SEED=<seed>` to
     reproduce deterministically.
   - Kani failures show a trace to the violating input; "unwinding
     assertion" means raise `#[kani::unwind(N)]` or restructure the loop —
     never delete the property to make it pass.
5. **Never widen a margin, bound, or assume to make verification pass**
   without a written justification in the harness comment.
6. Keep harness strings fixed and small; symbolic `String`s explode solver
   time. Symbolic numerics with `kani::assume` bounds are the pattern (see
   `key.rs::verify`).

## 6. Known verification debts

- `stats_may_overlap` Kani proof pending an i64-nanosecond core extraction
  (chrono boundary) — PROPERTY tier until then.
- Solver-cost reality: harnesses over `format!`/std-parse/serde_json/OS-seeded
  hashing are restructured to verification cores (pure byte/int functions) —
  this is deliberate, not a coverage cut. Remaining heavy solves
  (timestamp round-trip/ordering, merge L3/L4, pack corruption) run in the
  CI kani job on x86_64; local Apple-Silicon CBMC is several times slower.
- Fuzzing (cargo-fuzz targets over the three parsers) and Miri CI are
  planned; Miri is insurance — the codebase has no `unsafe` today.
- `summarize_trace` deserves direct PROPERTY-tier coverage once search
  summaries stabilize.
- The record pack Kani harness (`record_pack.rs::verify`) exists and the
  `kani` workflow runs it. A local run exceeded 30 minutes on Apple Silicon, so
  the decoder stays PROPERTY tier until that CI job reports green.
- Record-query pruning is sound while the hot store keeps the newest copy of
  every record. If SlateDB starts dropping compacted hot data, a pruned
  segment could return a stale version. See
  [adr-custom-schemas.md](research/adr-custom-schemas.md).
