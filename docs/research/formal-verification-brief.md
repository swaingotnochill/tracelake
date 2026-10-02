# Formal Verification Tooling for TraceLake — Decision Brief

**Date:** 2026-10-02 · **Repo state:** Rust 1.96, edition 2024, no `unsafe`, deps: slatedb 0.13, axum 0.8, tokio, serde, chrono, crc32fast.
**Method:** all maintenance data pulled live from crates.io API and GitHub API/repos on 2026-10-02 (see Sources). No web search; conclusions marked **[inconclusive]** where live data could not answer a question.

## Executive summary

**Recommendation: adopt proptest (Phase 0) + cargo-fuzz + Miri (Phase 1) + Kani (Phase 2). Defer Loom until background threads exist. Do not adopt Prusti or Creusot now.**

TraceLake's correctness-critical surface is small, pure, and byte-oriented: `tracelake-core` (key encode/decode, bloom filter, trace pack binary parser, manifest types) and two pure helpers in `tracelake-query` (`stats_may_overlap`, `span_matches`). None of it touches async. That is exactly the shape property-based testing, fuzzing, and bounded model checking are good at. Async/integration layers (SlateDB `db.rs`, `trace_fetch.rs`, axum server) are **out of reach** for all proof tools — they stay covered by the existing `tokio::test` integration suite.

Cheapest wins first: property tests and fuzz targets need ~3–5 days total, run in normal CI, and attack the highest-risk code (arbitrary-input parsers). Kani proofs cost more (compile-time ~2×, separate CI job) but upgrade round-trip properties from "tested on random samples" to "checked exhaustively within bounds". Prusti/Creusot are research-grade, nightly-pinned, and high-effort for the postconditions we actually care about — reassess only if merge/search logic grows subtle invariants.

---

## 1. Comparison table

Data collected 2026-10-02. "Latest release" = most recent stable distribution point.

| Tool | What it proves / finds | Reaches async / foreign deps? | Where verification code lives | Latest version + release date (live) | Maturity verdict |
|---|---|---|---|---|---|
| **proptest** | Finds counterexamples to stated properties (random + shrinking) | Yes for plain `#[tokio::test]` properties, but best on pure fns; foreign deps treated as opaque | `#[cfg(test)] mod verification` inline, or `tests/` dir | 1.11.0 (2026-03-24); repo pushed 2026-09-27 | **Mature, active.** Adopt. |
| **cargo-fuzz / libFuzzer** | Finds panics/UB/hangs on arbitrary input (coverage-guided) | Target fns only; keep targets on pure parsers | `fuzz/` crate, `#![no_main]` targets, `cfg(fuzzing)` | cargo-fuzz 0.13.2 (2026-06-09), libfuzzer-sys 0.4.13 (2026-06-04); repo pushed 2026-07-20 | **Mature, active.** Adopt for parsers. |
| **Miri** | Finds UB (via interpreter): OOB, leaks, races w/ flags — catches future `unsafe` mistakes | Runs whole test binaries; async works only if runtime avoids unsupported syscalls — keep on `tracelake-core` tests | Existing `#[cfg(test)]` tests, run via `cargo +nightly miri test -p tracelake-core` | Rustup component; repo rust-lang/miri pushed 2026-10-02, no GitHub "latest release" (ships per-toolchain) | **Mature, always current with nightly.** Adopt (cheap). |
| **Kani** | *Proves* absence of panics/UB + functional properties within bounded loops (CBMC, bit-precise) | No — harnesses on pure, extracted logic; tokio/SlateDB dep graph compiled but not practically verifiable | `#[cfg(kani)] mod verify` + `#[kani::proof]` in same files | kani 0.68.0 (2026-09-16); repo pushed 2026-10-02; main pins nightly-2026-09-25 | **Production-grade** (AWS s2n-quic runs it in CI). Adopt after Phase 1. |
| **Loom** | Exhaustively permutes concurrent executions of a test (C11 model) | Designed for concurrency; no async runtime support — tests must be sync over `loom` primitives | `#[cfg(loom)]` tests, `RUSTFLAGS="--cfg loom"` | 0.7.2 (crate 2024-04-23; GitHub release 2025-08-12); repo pushed 2026-02-20 | **Mature but slow-moving.** Defer — no shared-memory concurrency in TraceLake today. |
| **Prusti** | *Proves* contracts (pre/postconditions, invariants) via Viper; deductive, not bounded | No async; contracts on pure fns | `#[requires]/#[ensures]` annotations inline (source invasive) | GitHub prereleases only, last 2024-03-26 (dated builds); repo moved to `prusti/prusti`, pushed 2026-10-02; supported rustc version **[inconclusive]** from live data | **Research-grade; release channel stale.** Do not adopt now. |
| **Creusot** | *Proves* functional correctness via Why3/OCaml; strongest specs, ghost state | No async; pure fns; requires nightly + opam build | `#[requires]/#[ensures]/#[invariant]` (pearlite) inline | v0.13.0 (2026-07-30); repo moved to `creusot-rs/creusot`, pushed 2026-10-02; pins nightly-2026-08-03 | **Active research tool.** Too heavy for current needs. |
| **quickcheck** | Property testing, simpler than proptest (no shrinking strategies, coarser control) | Same as proptest | `#[cfg(test)]` | 1.1.0 (crate updated 2026-02-10); repo pushed 2026-04-03 | Mature but **redundant with proptest** — pick one. |
| **afl.rs** | Coverage-guided fuzzing via AFL++ (persistent mode, corpus dir) | Same limits as cargo-fuzz | `#[afl]` targets in separate crate | 0.18.2 (2026-05-11); repo pushed 2026-08-10 | Mature, active. **Redundant** with cargo-fuzz for our targets. |
| **honggfuzz** | Coverage-guided fuzzing via honggfuzz (good for long-running, parallel fuzzing) | Same limits | `#[fuzz]` targets | 0.5.62 (2026-08-04); repo pushed 2026-08-04 | Mature, very active. Optional second engine; skip initially. |

Note: the `kani` crate on crates.io (0.0.1, 2024-05-21) is a **placeholder**, not the verifier. Real Kani installs via `cargo install --locked kani-verifier` + `cargo kani setup` (GitHub releases). Same story for `cargo-miri` (placeholder from 2018; Miri is a rustup component).

---

## 2. Per-tool assessment against THIS codebase

### 2.1 Kani (model-checking/kani) — recommended, Phase 2

**Mechanics.** Bit-precise model checker: compiles the crate plus dependencies with its bundled toolchain (main pins `nightly-2026-09-25`; release 0.68.0 notes an upgrade to `nightly-2026-08-21`), translates to CBMC, and proves reachability properties (no panic, no UB, plus user assertions) for **all inputs within loop bounds**. Loops are unwound to a bound (`--default-unwind N`, or `#[kani::unwind(n)]` per harness); proofs fail with "unwinding assertion" if bounds are too low, so bounds are explicit, not silent.

**Can verify here:**
- `key.rs`: `decode_span_key(encode_span_key(...))` identity; ordering law `ts1 < ts2 ⟹ encode(ts1) < encode(ts2)` (zero-padded width-20 decimal makes lexicographic == numeric — provable); prefix-safety (`trace_prefix(t,ns,A)` never a prefix of `B`'s key when `A ≠ B`, since segments cannot contain `/`); `decode_timestamp` never panics on any byte string.
- `bloom.rs`: `contains` returns true after `insert` for the same filter state (no false negatives — pure integer ops, unwind = `num_hashes ≤ 32`); `decode` is total (never panics) on arbitrary bytes; `decode(encode(f))` preserves membership. `BloomFilter::new`'s float sizing math is *executable* under Kani but the FPR bound itself is a statistics claim — leave that to proptest, not Kani.
- `trace_pack.rs`: bounds safety — `decode_trace_pack`, `read_entry`, `read_block` never panic on arbitrary bytes (checked arithmetic + `slice::get` everywhere — this is exactly what a proof certifies). `read_trace`'s directory binary search is `O(log n)` iterations, cheap to unwind.
- `merge_spans` laws (L1–L4 below) with small symbolic `Span`s — feasible but string-heavy; do it only if merge grows subtler.

**Cannot verify here:** tokio runtime internals, SlateDB's dependency graph (compiles, but proofs over foreign code explode — standard practice is harnesses on leaf logic), `object_store` I/O, axum handlers. Kani also cannot run async fns (`#[kani::proof]` on sync code only).

**Caveat for `crc32fast`:** the checksum call is foreign code inside `decode_trace_pack`. Its slicing loop may need large unwind bounds or make the harness expensive. Mitigation used below: a Phase-2 harness that patches a *valid* checksum onto symbolic bytes before calling deep parsers, sidestepping crc32fast in the hot loop; full-arbitrary-byte decode stays covered by fuzzing. Flagged as the main Kani schedule risk.

**Live maintenance.** kani-0.68.0 released 2026-09-16; repo pushed 2026-10-02 (same day as this brief); 3.5k stars. Ships a GitHub Action (`model-checking/kani-github-action`), used in production.

**Real users.** Verified live: `aws/s2n-quic` CI (`.github/workflows/ci.yml`, `main`) has a dedicated `kani:` job running `model-checking/kani-github-action@v1.1`, and its CI needs-list includes `kani`, `miri`, and `loom` jobs. AWS has publicly credited Kani with finding s2n-quic bugs (header parsing, ack handling); the CI evidence is the load-bearing fact here.

**Bugs-found class:** integer overflow/OOB in parsers, missing bounds checks, off-by-one in bit manipulation — i.e., exactly the class our hand-rolled bloom and trace-pack directory code could harbor.

### 2.2 Prusti (prusti/prusti, moved from viperproject/prusti-dev) — not now

**Mechanics.** Deductive verifier: write `#[requires]`/`#[ensures]` contracts, Prusti generates Viper verification conditions and discharges them with Z3. Sound and unbounded (no loop-unwind limits) — in principle stronger than Kani.

**Can/cannot here.** Would fit `merge_spans` postconditions (sortedness, last-layer-wins) and `stats_may_overlap` soundness. But: contracts are invasive (annotation burden on every helper the verifier walks through, including `Span` operations), float/`HashMap` reasoning is painful (`merge_spans` uses a `HashMap` — RustFix memory-model reasoning over hash maps is a known weak spot), and there is **no async support**. Our best Prusti targets are the same functions Kani covers with far less ceremony.

**Live maintenance.** Repo active (pushed 2026-10-02, moved to `prusti/prusti`, 1.8k stars) **but** the GitHub release channel shows only dated prereleases, the last from **2024-03-26** — over 2.5 years without a tagged release. I could not extract the currently supported rustc version from live data **[inconclusive]**; historically Prusti lags stable substantially. Given we ride Rust 1.96/edition 2024, toolchain-lag is the dominant risk: a Prusti pinned to an older rustc simply cannot compile our workspace.

**Verdict:** reassess if/when merge/search invariants become intricate enough to justify deductive proofs. Not worth the lag risk today.

### 2.3 Creusot (creusot-rs/creusot, moved from xldenis/creusot) — not now

**Mechanics.** Functional-correctness verifier via Why3; write pearlite specs (`#[requires]`, `#[invariant]`, ghost/pearlite logic), proofs discharge in Why3/OCaml toolchain. Strongest spec expressiveness of the three provers (real mathematical functions, termination checking).

**Can/cannot here.** Same scope limits as Prusti: pure functions only, no async. Not on crates.io; installed from source with `opam` (README `INSTALL` path). Pins `nightly-2026-08-03`, so toolchain is recent — but the opam-based install plus Why3 expertise makes it the highest-learning-curve option here.

**Live maintenance.** Genuinely active and releasing: v0.13.0 (2026-07-30), v0.12.0 (2026-06-12), v0.11.0 (2026-04-20); repo pushed 2026-10-02.

**Verdict:** track, don't adopt. Nothing in TraceLake needs mathematical proof beyond bounded checks.

### 2.4 Miri (rust-lang/miri) — adopt Phase 1 (cheap)

**Mechanics.** Interprets MIR; detects UB (OOB reads, invalid values, leaks under `-Zmiri-ignore-leaks=false`, data races with `-Zmiri-many-seeds`-style stress in supported scenarios) while running ordinary test binaries. Ships as a rustup component — always matches a nightly.

**Value here.** We have **zero `unsafe` today**, so Miri finds nothing today — its value is *future-proofing*: AGENTS.md already anticipates "unsafe … only in trace pack serialization". When that day comes, `cargo +nightly miri test -p tracelake-core` turns our existing + new proptest suites into UB detectors for free. Cost: one CI job (nightly-only), minutes of runtime, no code changes.

**Limits:** foreign deps that use unsupported syscalls break under Miri — run it only on `tracelake-core` (pure); skip `tracelake-ingest/server` (SlateDB/tokio use syscalls Miri can't emulate, e.g. threads/IO in places).

### 2.5 Loom (tokio-rs/loom) — defer to Phase 3

**Mechanics.** Explores all interleavings of a *synchronous* concurrent test under the C11 memory model, with state reduction. Tests are written against `loom`'s replacements (`loom::sync::Mutex`, etc.) behind `#[cfg(loom)]`.

**Value here.** TraceLake has **no shared-memory concurrency of its own** today: the compactor is a single async task, SlateDB owns its internals, `TraceLakeReader` is `Clone`-shared handles over immutable config. Loom applies only if we add background compactor threads coordinating with readers (manifest publish protocol, hot-buffer handoff). `Compactor`'s doc comment already warns "concurrent compactors would need manifest coordination" — that is the future Loom target.

**Live maintenance.** 0.7.2 is the latest crate (2024-04-23; GitHub release object 2025-08-12); repo last pushed 2026-02-20. Stable, used by tokio itself, but release cadence is ~yearly. Fine for deferred adoption; note it in Phase 3.

### 2.6 proptest (proptest-rs/proptest) — adopt Phase 0

**Mechanics.** Generates structured random values from `Strategy`s, shrinks counterexamples to minimal cases. Deterministic replay via persisted seeds (`PROPTEST_SEED` / failure files under `proptest-regressions/`). Optional `proptest-derive` gives `#[derive(Arbitrary)]`.

**Value here.** It generalizes the existing example-based tests in `key.rs`, `bloom.rs`, `trace_pack.rs`, `segment.rs`, `merge.rs` from "these five keys" to "arbitrary admissible inputs". This is where most real bugs in encode/decode pairs actually surface (e.g., a generator that produces a segment shape the decoder mis-parses). All concrete properties are enumerated in Phase 0 below.

**Notable:** `DefaultHasher::new()` is unseeded, so `hash_key` is deterministic across runs/processes → the bloom FPR property is *not* statistically flaky for a fixed corpus; it's deterministic. Generous thresholds still recommended because a fixed hash is a fixed unlucky-corpus risk.

**Live maintenance.** 1.11.0 (2026-03-24); repo pushed 2026-09-27; 200M downloads. Safest bet in the list.

### 2.7 quickcheck (burntsushi/quickcheck) — skip (redundant)

1.1.0 (updated 2026-02-10), repo pushed 2026-04-03 — maintained. But proptest subsumes it (better shrinking, `prop_assume!`, derive, determinism controls). Running both adds flakiness surface without new coverage. Picking one property framework is also a DRY decision.

### 2.8 cargo-fuzz (rust-fuzz/cargo-fuzz) — adopt Phase 1

Coverage-guided fuzzing via libFuzzer (`libfuzzer-sys` 0.4.13). Targets live in a `fuzz/` crate with `#![no_main]`; each target is a `fuzz_target!(|data: &[u8]| …)`. Compiles with `-C instrument-coverage`. Our three arbitrary-input parsers (`decode_span_key`, `decode_trace_pack`, `BloomFilter::decode`) are the canonical targets — arbitrary-input parsers are where fuzzing has the highest bug density per minute. Maintenance: 0.13.2 (2026-06-09), repo pushed 2026-07-20.

### 2.9 afl.rs (rust-fuzz/afl.rs) and honggfuzz (rust-fuzz/honggfuzz-rs) — skip initially

Both healthy live (afl 0.18.2, 2026-05-11; honggfuzz 0.5.62, 2026-08-04). They shine for long-running, parallel, multi-machine fuzzing campaigns. Our parser surface is small enough that CI-time libFuzzer smoke runs + a shared corpus cover it. Adopt one later only if a dedicated fuzzing box exists.

---

## 2.5 Applicability map: TraceLake module → tools that reach it

| Module | Pure? | proptest | fuzz | Kani | Loom | Integration tests only |
|---|---|---|---|---|---|---|
| `core::key` encode/decode/prefixes | Yes | ✅ all props | ✅ target | ✅ proofs | — | — |
| `core::bloom` (hand-rolled) | Yes | ✅ | ✅ target | ✅ (integer ops) | — | — |
| `core::trace_pack` writer/reader | Yes | ✅ | ✅ target (crc-restamp) | ✅ (crc workaround) | — | — |
| `core::segment` manifest types | Yes (serde) | ✅ round-trip | low value (serde covers) | ❌ (serde foreign) | — | — |
| `query::merge` merge_spans | Yes | ✅ laws | — | ✅ bounded | — | — |
| `query::search` stats_may_overlap / span_matches / summarize | Yes | ✅ prune soundness | — | possible | — | full search flow |
| `query::trace_fetch` get_trace | Async + I/O | — | — | — | — | ✅ existing tokio::tests |
| `compact::compactor` collect_traces grouping | Pure over scanned spans | ✅ (extract pure fn) | — | possible | — | compact flow |
| `ingest::db` SlateDB wrapper | Async + foreign | — | — | — | — | ✅ |
| `server/*` axum handlers | Async + I/O | — | — | — | — | ✅ end-to-end |
| Future: background compactor threads | Shared memory | — | — | — | ✅ | ✅ |

---

## 3. Recommended stack and phased plan

**Stack:** `proptest` (Phase 0) → `cargo-fuzz` + Miri (Phase 1) → `Kani` (Phase 2) → `loom` (Phase 3, conditional). Everything lives in the existing workspace; no new runtime dependencies except dev-deps (`proptest`, `proptest-derive`) — the fuzz crate is workspace-excluded, Kani harnesses compile only under `cfg(kani)`.

### Phase 0 — Property tests (quick wins)

**Effort: 3–4 days. CI cost: seconds added to `cargo nextest run` (proptest cases are cheap; keep `cases = 256` default). Expected bug yield: moderate — this is where latent encode/decode asymmetries and ordering surprises surface. Risk: low (worst case: tests confirm what example tests already showed).**

Add `proptest = "1.11"` (+ `proptest-derive = "0.8"` if deriving `Arbitrary` for `Span`/`Trace` pays off; hand-rolled strategies are fine otherwise) as a dev-dependency of `tracelake-core` and `tracelake-query`. Place properties in an inline `#[cfg(test)] mod verification` at the bottom of each file (keeps access to private items; mirrors existing style). If a module grows past ~3 properties, move to `tracelake-core/tests/verification_<module>.rs` as an integration test.

Representative strategy sketch (in `key.rs`'s `mod verification`):

```rust
fn segment_strategy() -> impl proptest::strategy::Strategy<Value = String> {
    // admissible segment: non-empty, no '/', printable, 1..=16 chars
    "[A-Za-z0-9_-]{1,16}"
}

proptest! {
    #[test]
    fn prop_key_round_trips(t in segment_strategy(), ns in segment_strategy(),
                            tr in segment_strategy(), sid in segment_strategy(),
                            ts in 0i64..(1i64 << 53)) {
        let key = encode_span_key(&t, &ns, &tr, ts, &sid).unwrap();
        let dec = decode_span_key(&key).unwrap();
        assert_eq!((dec.tenant_id, dec.namespace, dec.trace_id, dec.start_time_ns, dec.span_id),
                   (t, ns, tr, ts, sid));
    }
}
```

**`tracelake-core/src/key.rs` — `mod verification`:**
- **K-RT (round-trip):** for arbitrary valid segments (non-empty, no `/`, alphanumeric+`-_` strategy) and `start_time_ns in 0..=i64::MAX_ns` (`0..2^53` suffices; chrono ns range is the real bound — mirror `encode_span_storage_key`'s `timestamp_nanos_opt` domain): `decode_span_key(&encode_span_key(t,ns,tr,ts,sid))` decodes to the same components.
- **K-ORD (ordering):** same tenant/ns/trace, `ts1 < ts2` ⟹ `encode(..ts1..) < encode(..ts2..)` byte-wise; equal `ts`, different `span_id` ⟹ order follows `span_id` lexicographically (SlateDB correctness depends on this).
- **K-ISO (trace isolation):** for `traceA ≠ traceB` (both `/`-free), `encode(..traceA..)` never `starts_with(trace_prefix(..traceB..))` — the prefix-scan isolation guarantee the whole design leans on.
- **K-PFX:** every encoded key starts with `tenant_prefix`, `namespace_prefix`, `trace_prefix` of its own components, in nested prefix order.
- **K-REJ (decoder robustness):** arbitrary `Vec<u8>` → `decode_span_key` returns `Err` or a well-formed `SpanKey`; never panics. (Document expected behavior: any 20-digit timestamp overflows `i64::MAX` (19 digits) → `parse` fails → `InvalidTimestamp` — proptest will lock that in as intentional.)
- **K-COMPOSE:** `encode_span_storage_key(span)` key decodes back to `span`'s identity fields.

**`tracelake-core/src/bloom.rs`:**
- **B-NFN (no false negatives):** build filter sized for n∈{0,1,8,64,1024}, insert arbitrary byte-string set, `contains(k)` true for every inserted `k`. Also after `decode(encode(f))`.
- **B-FPR:** n=2048 random keys at rate 0.01; absent-key hit rate < 0.05 (2× margin; deterministic hash makes this stable — note in a comment why the margin exists).
- **B-RT:** `decode(encode(f))` membership-identical on inserted set.
- **B-REJ:** arbitrary bytes: `decode` errors on <12 bytes, non-multiple-of-8 tail, `num_bits==0`, word-count mismatch — or succeeds and never panics on `contains`.

**`tracelake-core/src/trace_pack.rs`:**
- **P-RT:** arbitrary admissible `Vec<Trace>` (unique trace_ids ≤ u16 bytes, non-empty spans, small spans via a span strategy) → `decode_trace_pack(encode(...))` then `read_trace(id)` returns exactly the input spans, `trace_count`/`span_count` match, `trace_ids()` sorted.
- **P-NP (never panics):** arbitrary `Vec<u8>` → `decode_trace_pack` returns `Ok` or `Err`, never panics; if `Ok`, `trace_ids()` and `read_trace(&arbitrary_id)` also never panic. This walks the directory binary search and block reader over attacker-shaped bytes.
- **P-TRUNC:** any strict prefix of a valid pack → `Err`.
- **P-EMPTY:** empty input round-trips (already example-tested; keep as property edge).

**`tracelake-core/src/segment.rs`:**
- **M-RT:** arbitrary manifest (strategy over segment entries/stats) → `serde_json` round-trip equality; `None` stats fields absent from JSON and default back.
- **M-NEXT:** `next_segment_number()` = max suffix + 1 across arbitrary id sets, 1 when empty; `segment_id(n)` formatting law `segment_id(next_segment_number()) != any existing id`.

**`tracelake-query/src/merge.rs` — laws:**
- **L1:** `merge_spans(&[]) == []`; `merge_spans(&[vec![], vec![]]) == []`.
- **L2 (sortedness):** output sorted by `(start_time, span_id)`.
- **L3 (precedence):** for arbitrary layers, if span_id `s` appears, the output copy equals the version from the **last** layer containing `s`.
- **L4 (no loss/spurious):** set of output `span_id`s == union of layer `span_id` sets.
- **L5 (idempotence):** `merge_spans(&[a, a]) == merge_spans(&[a])` (precondition: `a` itself has unique span_ids).

**`tracelake-query/src/search.rs` — pure helpers (same file, `mod verification`):**
- **S-PRUNE-SOUND:** `!stats_may_overlap(stats, search)` ⟹ no span with `start_time` within the segment's `[start_time_min, start_time_max]` can satisfy the search's time filter. (Soundness of segment pruning — the property that guarantees the `search_time_range_uses_stats_to_skip_segments` test generalizes.)
- **S-MATCH:** `span_matches` mirrors each filter independently (vary one filter at a time).

**Verification:** each property listed above becomes a `proptest!` block; run `cargo nextest run --workspace`. Gate bug-yield expectation: history of similar codecs says 1–3 real asymmetry bugs is a plausible outcome (e.g., subtle prefix/ordering edge), zero is also fine — Phase 0 mainly *hardens* against future edits.

### Phase 1 — Fuzzing (cargo-fuzz) + Miri on core

**Effort: 1–2 days. CI cost: a fuzz-smoke job ~3–5 min (build fuzz crate once, 30–60s per target with `-max_total_time=30`, shared corpus committed under `fuzz/corpus/`); Miri job ~5–10 min nightly on `tracelake-core`. Expected bug yield: highest per minute of any tool if parser bugs exist; after Phase 0, likely low. Risk: sanitizer-instrumented build breakage; nightly Miri flakiness (pin a known-good nightly date).**

`cargo fuzz init` at repo root (add `fuzz/` to workspace `exclude`). Targets:
- `fuzz_targets/fuzz_decode_span_key`: `decode_span_key(data)`, then re-encode the `Ok` result and assert byte equality (round-trip under fuzzing).
- `fuzz_targets/fuzz_decode_trace_pack`: the important trick — plain arbitrary bytes die at the crc32 check ~always, so the target first **re-stamps a valid checksum**: take `data`, truncate to ≥ `HEADER_LEN+FOOTER_LEN`, recompute `crc32fast::hash(&data[..len-4])`, write it into the footer, then call `decode_trace_pack` + `trace_ids()` + `read_trace(last 8 bytes of data as id)`. This drives the directory binary search and block walker over arbitrary *internally-consistent* input. Also run a second pass with raw bytes (checksum-miss path).
- `fuzz_targets/fuzz_bloom_decode`: `BloomFilter::decode(data)`; on `Ok`, `contains(data)`.

CI: commit seed corpus (one valid pack, bloom, key per target). Workflow: build + `-max_total_time=30` per target per PR (regression via corpus replay), plus an optional scheduled (nightly) 10-min run. Artifacts: crash files committed by devs after minimization (`cargo fuzz fmt`).

Miri: `cargo +nightly miri test -p tracelake-core` in a separate nightly job (with `Zmiri-disable-isolation` not needed; keep default). Value today: near-zero findings (no `unsafe`) — stated honestly; the job is insurance for the day trace-pack serialization goes `unsafe`, as AGENTS.md anticipates.

### Phase 2 — Kani proofs

**Effort: 4–6 days (mostly harness tuning + unwind budgets). CI cost: dedicated job ~10–20 min using `model-checking/kani-github-action@v1` (same action AWS s2n-quic uses at v1.1) or the `ghcr.io/model-checking/kani/kani` docker image; Kani compiles the crate graph itself → roughly doubles build time for the verified crates, isolated in its own job so dev builds are untouched. Expected bug yield: low (post Phase 0/1) but the *output* is certainty: "no input within bounds can panic these parsers". Risk: unwind-bound fighting, especially around `crc32fast` (see harness note), and Kani's bundled-toolchain compile of deps.**

Add `#[cfg(kani)] mod verify` blocks (zero cost to normal builds; `kani` cfg is only set by `cargo kani`):

```rust
#[cfg(kani)]
mod verify {
    use super::*;
    #[kani::proof]
    fn decode_inverts_encode() {
        let ts: i64 = kani::any();              // full symbolic i64
        kani::assume(ts >= 0);
        let a = encode_span_key("t1", "ns", "tr", ts, "s1").unwrap();
        let b = encode_span_key("t1", "ns", "tr", ts.saturating_add(1).max(ts), "s1").unwrap();
        // ordering law on the zero-padded width-20 decimal
        if ts < 99_999_999_999_999_999_999i64.saturating_sub(1) { assert!(a < b); }
        let k = decode_span_key(&a).unwrap();
        assert_eq!(k.start_time_ns, ts);
    }
}
```

Harnesses:
- **key.rs:** (a) `decode(encode(k)) == k` for symbolic `ts` (string ids fixed — symbolic `String`s are possible but blow up `format!` paths; document that choice); (b) ordering law `ts1 < ts2 ⟹ encode(ts1) < encode(ts2)` for two symbolic timestamps; (c) `decode_span_key(kani::any::<&[u8]>()` bounded ≤64B) never panics.
- **bloom.rs:** (a) `contains` after `insert` (unwind 32 for the hash loop; the double-hash loop is the only loop); (b) `decode(encode(f))` then contains (words bounded ≤4 → unwind ~64); (c) `decode` totality on arbitrary ≤ 64B input.
- **trace_pack.rs:** (a) **checksum-restamp harness**: symbolic bytes → patch valid crc32 (avoid deep crc32fast reasoning) → `decode_trace_pack` → if `Ok`, `trace_ids()` + `read_trace` never panic; directory binary search unwinds `⌈log2(trace_count)⌉` — bound `trace_count ≤ 32` → unwind ~6; `read_block`'s span loop unwind = max spans per block bound (e.g. 8); (b) full `P-RT` proof over small symbolic traces (≤2 traces × ≤2 spans) asserting exact span recovery — optional stretch goal.
- **merge.rs:** L1–L4 with 2 layers × ≤3 tiny symbolic spans (strings fixed, `start_time`/status symbolic via simple enums + i64 ns) — stretch goal after parsers.

CI job (`.github/workflows/kani.yml`): `model-checking/kani-github-action@v1` with `command: cargo kani --workspace -p tracelake-core -p tracelake-query --default-unwind 12` (or per-harness `#[kani::unwind]`). Runtime estimate: 10–20 min wall for this harness set (parser proofs are seconds each; crate compilation dominates). Run on PRs touching `tracelake-core`/`tracelake-query` paths only (paths filter).

**Toolchain note:** Kani 0.68.0 bundles its own pinned nightly (release notes: upgraded to nightly-2026-08-21; main pins nightly-2026-09-25) — our edition-2024 code must merely compile under that pinned toolchain, which it should given both are ≥ our 1.96 baseline features. Verify on first install; if the pin lags an edition feature we adopt, proofs block on Kani releases (documented risk, releases are ~monthly).

### Phase 3 — Conditional / future

**Loom (only when we add background compaction threads or a shared manifest cache).** Effort: 2–4 days when the concurrency exists — write `#[cfg(loom)]` tests around the manifest publish/read protocol and hot-buffer handoff, replacing `std::sync` with `loom::sync` behind the cfg. CI: loom tests are combinatorial — keep scopes tiny (2 threads, small state), minutes per test; run in nightly job with `RUSTFLAGS="--cfg loom"`. Maintenance note: 0.7.2 since 2024/2025 — stable but slow-moving; budget for "works, rarely changes".

**Prusti / Creusot reassessment triggers:** (a) `merge_spans`/`search` invariants grow beyond what bounded Kani checks comfortably (e.g., full sort-order proofs over unbounded inputs); (b) Prusti ships tagged releases again and documents rustc ≥ 1.9x support (check release channel + `dev` docs — current channel shows prereleases from 2024-03-26, supported rustc **[inconclusive from live data]**); (c) team bandwidth for deductive-method training (realistically 1–2 weeks learning curve each). Creusot additionally requires opam + Why3 ops burden. Neither clears the bar today.

### Effort / cost / yield summary

| Phase | Calendar effort | CI cost | Expected bug yield | Main risk |
|---|---|---|---|---|
| 0 proptest | 3–4 d | +seconds | 1–3 latent asymmetries (possibly 0) | property flakes (mitigate: margins, seeds in `proptest-regressions/`) |
| 1 fuzz + Miri | 1–2 d | +2 jobs, ~10 min total | parser panics if any exist | sanitizer build friction; nightly pin |
| 2 Kani | 4–6 d | +1 job, 10–20 min | ~0 new bugs, high certainty | unwind tuning; crc32fast; toolchain pin |
| 3 loom | 2–4 d (when concurrency lands) | small | race conditions in manifest protocol | loom's slow release cadence |

---

## 4. Risks and tradeoffs

- **Compile-time burden.** Kani compiles the workspace + deps with its own toolchain: roughly a second full build, plus solver time. Contained by isolating Kani in one CI job and gating on paths. Fuzz targets compile in a separate `-Cinstrument-coverage` profile only in the fuzz job. Dev `cargo build` is untouched (all harnesses behind `cfg(kani)`/`cfg(fuzzing)`/`cfg(test)`).
- **Proptest flakiness.** Statistical properties (B-FPR) can fail on unlucky draws. Mitigations: 2× margins, bounded strategies, committed `proptest-regressions/` seed files make failures reproducible, `PROPTEST_SEED` documented in the module header. Note our bloom hash is *deterministic* (unseeded `DefaultHasher`), so bloom properties are deterministic given a corpus — the residual risk is a fixed unlucky corpus, covered by margin.
- **False confidence.** All tools cover the **pure core** (encode/decode, bloom, pack, manifest, merge laws, prune soundness). They say nothing about: SlateDB write visibility, manifest publish atomicity under concurrency, object-store partial reads, axum handler wiring, auth/idempotency at the server. Integration `tokio::test`s remain the only coverage there — say this out loud in review whenever a "verified" claim is made.
- **Bounded proofs ≠ unbounded proofs.** Kani results hold only within unwind bounds; a harness that passes at unwind 12 says nothing about a 13th directory entry. Keep bounds tight and documented per harness; prefer restructuring (binary search → log loop) over raising bounds.
- **Tool lock-in / churn.** Prusti repo moved orgs (viperproject → prusti) and its release channel is stale; Creusot moved orgs (xldenis → creusot-rs). Kani, Miri, proptest, cargo-fuzz, loom are all under stable orgs with recent pushes. Our exposure is limited: harnesses are ~300 lines of cfg-gated code, deletable in an afternoon.
- **Learning curve.** proptest/cargo-fuzz: days, Rust-team-friendly. Kani: ~1 week to comfortable (unwinding, assumptions, contracts-lite). Prusti/Creusot: weeks, plus verifier-theory background — the real reason they're deferred.
- **Foreign-dependency walls.** No proof tool practically crosses into slatedb/tokio/serde_json internals. Any property needing those (e.g., "SlateDB scan_prefix returns keys in order") must be re-stated as an assumption at our boundary and covered by integration tests instead. crc32fast is the one wall inside our own parsers (checksum harness workaround above).

---

## 5. Sources (all queried 2026-10-02, ~18:40 UTC)

**crates.io API** (`-H "User-Agent: tracelake-research"`):
- https://crates.io/api/v1/crates/proptest — 1.11.0, updated 2026-03-24
- https://crates.io/api/v1/crates/proptest-derive — 0.8.0, updated 2026-02-05
- https://crates.io/api/v1/crates/loom — 0.7.2, updated 2024-04-23
- https://crates.io/api/v1/crates/cargo-fuzz — 0.13.2, updated 2026-06-09
- https://crates.io/api/v1/crates/libfuzzer-sys — 0.4.13, updated 2026-06-04
- https://crates.io/api/v1/crates/afl — 0.18.2, updated 2026-05-11
- https://crates.io/api/v1/crates/quickcheck — 1.1.0, updated 2026-02-10
- https://crates.io/api/v1/crates/honggfuzz — 0.5.62, updated 2026-08-04
- https://crates.io/api/v1/crates/kani — placeholder 0.0.1, 2024-05-21 (not the verifier)
- https://crates.io/api/v1/crates/creusot — not found (source install only)
- https://crates.io/api/v1/crates/prusti — not found (GitHub release install)
- https://crates.io/api/v1/crates/cargo-miri — placeholder 0.0.1, 2018 (Miri is a rustup component)

**GitHub API (repos)** — stars / pushed_at / open_issues:
- https://api.github.com/repos/model-checking/kani — 3504 / 2026-10-02 / 480
- https://api.github.com/repos/viperproject/prusti-dev → 301 → https://api.github.com/repos/prusti/prusti — 1817 / 2026-10-02 / 301
- https://api.github.com/repos/xldenis/creusot → 301 → https://api.github.com/repos/creusot-rs/creusot — 1861 / 2026-10-02 / 103
- https://api.github.com/repos/tokio-rs/loom — 2834 / 2026-02-20 / 129
- https://api.github.com/repos/rust-lang/miri — 6634 / 2026-10-02 / 193
- https://api.github.com/repos/proptest-rs/proptest — 2250 / 2026-09-27 / 151
- https://api.github.com/repos/rust-fuzz/cargo-fuzz — 1904 / 2026-07-20 / 115
- https://api.github.com/repos/rust-fuzz/afl.rs — 1849 / 2026-08-10 / 41
- https://api.github.com/repos/burntsushi/quickcheck — 2803 / 2026-04-03 / 33
- https://api.github.com/repos/rust-fuzz/honggfuzz-rs — 502 / 2026-08-04 / 22

**GitHub API (releases):**
- https://api.github.com/repos/model-checking/kani/releases/latest — kani-0.68.0, published 2026-09-16; notes mention toolchain upgrade to nightly-2026-08-21
- https://api.github.com/repos/prusti/prusti/releases — latest 3 all prereleases, newest 2024-03-26
- https://api.github.com/repos/creusot-rs/creusot/releases — v0.13.0 (2026-07-30), v0.12.0 (2026-06-12), v0.11.0 (2026-04-20)
- https://api.github.com/repos/tokio-rs/loom/releases/latest — v0.7.2, published 2025-08-12
- https://api.github.com/repos/rust-lang/miri/releases/latest — 404 (no GitHub releases; rustup component)

**Raw files:**
- https://raw.githubusercontent.com/model-checking/kani/main/rust-toolchain.toml — channel nightly-2026-09-25
- https://raw.githubusercontent.com/model-checking/kani/main/README.md — "bit-precise model checker"; install `cargo install --locked kani-verifier`
- https://raw.githubusercontent.com/creusot-rs/creusot/master/rust-toolchain — channel nightly-2026-08-03
- https://raw.githubusercontent.com/creusot-rs/creusot/master/README.md — opam/source install path
- https://raw.githubusercontent.com/prusti/prusti/master/README.md — cargo-prusti/prusti-rustc description (no rustc version stated)
- https://raw.githubusercontent.com/prusti/prusti/master/INSTALL.md — no rustc pin extractable **[inconclusive on Prusti's supported rustc]**
- https://raw.githubusercontent.com/aws/s2n-quic/main/.github/workflows/ci.yml — `kani:` job using `model-checking/kani-github-action@v1.1`; `miri` and `loom` jobs in needs-list (AWS production-use evidence)
- https://raw.githubusercontent.com/aws/s2n-tls/main/.github/workflows/ci.yml — no kani match in this file (not pursued further)
- https://raw.githubusercontent.com/tokio-rs/loom/master/README.md — C11 memory-model permutation testing description

**Codebase files read to ground the plan:** `tracelake-core/src/{key,bloom,trace_pack,segment}.rs`, `tracelake-query/src/{merge,trace_fetch,search}.rs`, `tracelake-compact/src/compactor.rs`, `tracelake-ingest/src/db.rs`.
