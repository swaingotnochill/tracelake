# Formal Verification Implementation Report

Outcome of the phased plan in formal-verification-brief.md, stages 0-4.

## Landed

- **Stage 0** — rocq-of-rust assessment committed (`2d40fdd`): PASS.
- **Stage 1** — property suite committed (`c7cc852`): key codec (round-trip,
  ordering, prefix isolation/nesting, totality), bloom (no false negatives,
  membership-exact round-trip, totality, FPR bound), trace pack (exact
  round-trip, prefix rejection, totality), segment manifest (JSON round-trip,
  next-segment law, exhaustive id formatting), merge laws L1-L5, group_spans
  partition law, search helper laws (prune soundness, per-filter exactness,
  completeness). 88 workspace tests green.
- **Bug found and fixed by the suite**: serde_json's default best-effort
  float parsing shifted cost_usd-like values by one ULP across pack
  round-trips (172.0*0.001 -> 0.172). Fixed with the serde_json
  `float_roundtrip` feature; regression test in trace_pack.rs. This was
  silent data corruption in the flagship read path.
- **Stage 2** — Kani harnesses: key.rs (3), bloom.rs (3), trace_pack.rs (2,
  including the checksum-restamped corruption totality), merge.rs (L3
  last-layer-wins, L4 no-loss). search helpers stay PROPERTY tier: chrono
  DateTime has no symbolic construction under Kani (documented in
  docs/verification.md debts).
- **Stage 3** — docs/verification.md (tiers, trusted boundaries, coverage
  manifest, agent protocol) + AGENTS.md verification protocol section.
- **Stage 4** — CI: check (fmt/clippy/tests), kani job (paths-filtered to
  core/query), nightly Miri on core (non-blocking).

## Kani run status — honest note

Local `cargo kani` on this Apple Silicon machine compiles quickly but each
CBMC solve takes minutes (the pack harnesses pull serde_json's serializer
under the solver; unwinds up to 128). Interactive completion of the full
harness set was not practical in one session; three bounded local runs
reached the solving phase without finishing. **The CI kani job is the
authoritative gate.** Expect possible per-harness unwind tuning on the first
CI run; that is normal Kani adoption work and is covered by the protocol in
docs/verification.md (never delete a property to make it pass).

## Remaining gaps

- First green CI kani run (unwind tuning if needed).
- cargo-fuzz targets for the three parsers (phase 1 of the brief).
- stats_may_overlap Kani proof behind an i64-ns core extraction.
- summarize_trace direct property coverage.
