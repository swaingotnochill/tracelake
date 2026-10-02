# rocq-of-rust Assessment for TraceLake

**Date:** 2026-10-02 · **Method:** all data pulled live from the GitHub API, raw.githubusercontent.com, crates.io API, and formal.land on 2026-10-02 (User-Agent `tracelake-research`). No web search. Unknowns marked **[inconclusive]**.
**Context:** companion to `formal-verification-brief.md` (recommended stack: proptest → fuzz+Miri → Kani). Our workspace: Rust 1.96, edition 2024, no `unsafe`, deps slatedb 0.13 / axum 0.8 / tokio / serde / chrono / crc32fast.

## 1. TL;DR verdict

**Not recommended for TraceLake.** rocq-of-rust is the most credible Rust→Rocq translation effort in existence — actively developed, funded by the Ethereum and Aleph Zero foundations, and used in real client work (revm, Sui Move) by its vendor, Formal Land. But it does not fit us: (a) proofs live in **separate Rocq files** in a parallel proof tree — the Rust source stays untouched, so the "verification code residing with the Rust code" requirement is satisfied only in the loose same-repo sense, unlike Kani/proptest which are inline Rust; (b) it requires a pinned **nightly-2025-12-07 rustc-dev driver build** plus a **Rocq 9.0 opam toolchain** — a heavy second toolchain our team doesn't know; (c) our correctness-critical code leans on constructs it doesn't cover: `HashMap` and `DefaultHasher` live in `std` (only `core`+`alloc` are translated), and `crc32fast`/`serde`/`chrono` are foreign crates that would each need manual vendoring-and-translating, as Formal Land itself does via git submodules; (d) realistic proof effort for a Rocq novice is **weeks-to-months per theorem**, not days — their own Sui type-safety proof is 1,646 lines and still contains `Admitted` lemmas. Verdict per surface: *feasible in principle for pure byte-shuffling in `key.rs`/`trace_pack.rs` (with expert help), not worth it relative to the Kani path.* Keep the existing recommendation.

## 2. How it works

**Mechanics — THIR-based, three stages.** It is a rustc driver (source: `lib/src/thir_expression.rs`, `thir_pattern.rs`, `thir_ty.rs`; builds with the `rustc-dev` component). README workflow:

1. **Translation:** `cargo rocq-of-rust` in any Rust project generates a `Crate.v` — an *untyped, monadic* deep embedding. All values have type `Value.t`; all control flow and allocation go through a monad `M`. For `fn add_one(x: u32) -> u32 { x + 1 }` (from README):

```coq
Definition add_one (ε : list Value.t) (τ : list Ty.t) (α : list Value.t) : M :=
  match ε, τ, α with
  | [], [], [ x ] =>
    ltac:(M.monadic
      (let x := M.alloc (| Ty.path "u32", x |) in
      M.call_closure (|
        Ty.path "u32",
        BinOp.Wrap.add,
        [ M.read (| x |); Value.Integer IntegerKind.U32 1 ]
      |)))
  | _, _, _ => M.impossible "wrong number of arguments"
  end.
```

Two lines of Rust → 25 lines of monadic Rocq (the checked-in generated file `RocqOfRust/examples/default/examples/custom/add_one.v` is exactly 25 lines, and its `IsFunction` instance proof is `Admitted` in the generated output). `docs/proofs/proof.md` states the generated translation is "something around five times larger than the original Rust code."

2. **Linking:** type-checking and trait inference over the untyped tree using native Rocq typeclasses (`docs/proofs/link-plugin.md`, `links.md`).

3. **Simulation + proof:** the human writes an idiomatic Rocq *simulation* (~1× the Rust size) and proves the translated code evaluates to it. Statement shape (`docs/proofs/proof.md`): `Run.pure (f (φ x1) ... (φ xn)) (inl (φ (simu_f x1 ... xn)))` — `φ` injects typed values into `Value.t`; `inl` means "never panics". Modern form (`RocqOfRust/core/simulate/option.v`):

```coq
Lemma ok_or_eq {T E : Set} `{Link T} `{Link E}
    (self : Self T) (err : E) (stack : Stack.t) :
  {{ SimulateM.eval_f (Impl_Option.run_ok_or self err) stack 🌲
     (Output.Success (ok_or self err), stack) }}.
Proof.
  destruct self; s.
Qed.
```

The proof is one line (`s` is their custom simulation tactic) — *after* the simulation and tactic infrastructure exists. Notably, the adjacent `map_eq` lemma (for `Option::map`) is **`Admitted`** in their own std work: even one-method std lemmas are unfinished.

**Where theorems and proofs live — separate Rocq artifacts, not inline.** `docs/proofs/proof.md` prescribes the layout: `source_file.v` (generated translation) + `simulations/source_file.v` (hand-written spec) + `proofs/source_file.v` (proof). All are Rocq files in a Rocq project (their revm/Sui proofs live under `RocqOfRust/revm/…`, `RocqOfRust/move_sui/proofs/…` inside the rocq-of-rust repo). **There is no in-Rust annotation mechanism** — no `#[kani::proof]`- or `#[ensures]`-style attributes; Rust source is never modified. A user project *can* keep the generated `Crate.v` plus `simulations/`/`proofs/` directories inside the Rust workspace's git repo (the tool writes `Crate.v` into the cargo project), and check proofs in CI — but it is a second language and build system sitting next to the Rust, not "with the Rust code" in the way proptest/Kani harnesses are.

## 3. Live status data

| Item | Value (2026-10-02) | Source |
|---|---|---|
| Stars / forks | 1,168 / 43 | api.github.com/repos/formal-land/rocq-of-rust |
| Created / last push | 2022-12-28 / 2026-10-01 (commits 2026-09-30) | repo API; commits API |
| Open issues | 62 | repo API |
| Default branch | `main` | repo API |
| GitHub releases | **0** (none ever) | releases API |
| crates.io | **not published** (neither `rocq-of-rust` nor `coq-of-rust`) — install from git: `cargo install --path lib/` | crates.io API; docs/builds.md |
| Version | opam `0.1` | `RocqOfRust/rocq-of-rust.opam` |
| Contributors | 16; `clarus` (Formal Land founder G. Claret) 1,095 of ~3,500 commits (~2/3) — vendor-dominated | contributors API |
| Rust pin | `nightly-2025-12-07` + `rustc-dev`, `llvm-tools-preview` | `rust-toolchain` file |
| Rocq pin | `rocq-core >= 9.0 < 9.1`, plus `rocq-stdlib`, `rocq-smpl`, `coq-coqutil`, `coq-hammer` (opam); CI runs in `rocq/rocq-prover:9.0` docker | `rocq-of-rust.opam`; `.github/workflows/rust.yml` |
| License | Dual: Rocq proofs/libs MIT (`RocqOfRust/LICENSE`); **Rust transpiler AGPL-3.0** (`lib/LICENSE`) | README |
| Funding / production use | Ethereum Foundation (revm, CompPoly), Aleph Zero; Sui (Mysten) type-checker; formal.land lists Zcash Orchard (different tool `garden`), smoo.th, Tezos | README; formal.land homepage + `/reports/2026-02-15_revm-formal-specification.pdf` |
| Repo size / proof corpus | 4,397 tree entries; 1,936 `.v` files; `core` 663 files, `alloc` 181, `revm` 612, `examples` 1,157 | git/trees API |
| Recent commits | revm precompiles (sha256, identity, selfdestruct) — vendor client work | commits API |

Activity is genuinely strong (near-daily pushes). But there are no releases, no crates.io package, and every substantial verification project in-tree (revm, Sui, Solana, alloy, move) was done *by Formal Land staff as paid engagements*. I found **no evidence of a third party independently using rocq-of-rust in production** — adoption outside the vendor **[inconclusive, but no positive signal found]**.

## 4. Fit analysis per TraceLake module

Key coverage facts: only `core` + `alloc` are translated (checked: no `std` tree; `HashMap` and `DefaultHasher` are `std`-only → untranslated). `String`/`Vec` exist but are enormous (`core/option.rs` 2,822 lines → `core/option.v` 7,334 lines — 2.6×). Foreign crates are handled by *manually vendoring them as git submodules and translating their source* (`.gitmodules`: rust-lang/rust, bytes, uint, alloy, revm, solana ×3, move-sui) — there is **no automatic translate-crates.io-deps mode**. `RocqOfRust/blacklist.txt` lists files that fail or take >10 min even in core (`future/async_drop.v`, `iter/adapters/flatten.v`, `net/ip_addr.v`, `num/int_sqrt.v`). Async is unsupported (listed as future work in `docs/intro.md`; async std file blacklisted).

| Module (ours) | Translatable? | Proof feasibility | Effort (Rocq novice) |
|---|---|---|---|
| `core::key` encode/decode (bytes, `format!`, `String`, zero-padded ts) | Mostly — String/format paths painful; produced/checked by the `alloc/string.v` machinery | Round-trip + ordering theorems are the classic use case; sorting/lexicographic reasoning over padded decimals is real proof work | 3–6 weeks to first complete theorem |
| `core::bloom` (integer bit ops, hand-rolled) | Integer ops yes; `DefaultHasher` is `std` → must axiomatize or replace with translated `core::hash::sip` SipHash | No-false-negative needs a hash-collision-free argument — hard; totality of `decode` feasible | 2–4 weeks for totality only |
| `core::trace_pack` (custom binary writer/reader) | Our own code yes; **`crc32fast` is a foreign crate** → vendor-and-translate it (like they did `bytes`) or axiomatize the checksum | Parser totality + round-trip feasible; byte-level `Value.t` reasoning makes every proof long | 4–8 weeks incl. crc32fast handling |
| `core::segment` (serde derives, `chrono::DateTime`) | **No** — serde is a proc-macro foreign crate; chrono is a large foreign crate | Would require translating/axiomatizing both | Not worth attempting |
| `query::merge::merge_spans` (**HashMap**, String keys, closures, `sort_by`) | **Blocked** — `HashMap` untranslated; closures translate but via `FnOnce` typeclass machinery; would need refactor to Vec-based dedup (changing prod code for the prover) or an axiomatized HashMap with its laws as hypotheses | Sortedness/last-layer-wins laws provable *in principle*; HashMap reasoning is the known weak spot (same reason Prusti struggles) | 6–12 weeks realistically |
| `query::search::stats_may_overlap` | Only after refactoring stats to plain `i64` (currently `Option<DateTime<Utc>>` = chrono) | Pure comparison lemma — the single best-fit theorem in our codebase | 2–4 weeks |
| ingest/compact-async/server (SlateDB, tokio, axum) | No (async unsupported; agreed out of scope for any proof tool) | — | — |

Would it even compile our workspace? The cargo plugin must compile the target crate under **its** pinned `nightly-2025-12-07` (guide's own tip: copy their `rust-toolchain` into your project on mismatch). Edition 2024 is fine on a 2025-12 nightly; `tracelake-core`/`tracelake-query` compiling under it is plausible but **[inconclusive — not tested]**; compiling the full workspace incl. slatedb/axum under the pinned nightly is likely unnecessary (translate only core/query crates) but unverified.

## 5. Comparison vs the recommended Kani/proptest path

| Dimension | rocq-of-rust | Kani (Phase 2 of brief) | proptest (Phase 0) |
|---|---|---|---|
| What you get | Unbounded functional correctness for **all** inputs; panic-freedom; executable specs | Bounded exhaustive proofs (explicit unwind), no-panic/no-UB + assertions | Random + shrunk counterexamples |
| Where verification code lives | **Separate `.v` files** (translation/simulations/proofs tree); Rust untouched | **Inline Rust** (`#[cfg(kani)] mod verify`, `#[kani::proof]`) | Inline Rust (`#[cfg(test)]`) |
| Language to learn | Rocq/Coq (tactics, typeclasses, monads, their `M`/`Value.t`/`φ` framework) | Rust + Kani attributes (~1 week) | Rust (days) |
| Toolchain | rustc driver on pinned nightly + `rustc-dev`; Rocq 9.0 + 4 opam packages; docker for CI | `cargo install kani-verifier`; bundled toolchain; ships a GH Action (used by AWS s2n-quic) | cargo dev-dep |
| Time to first useful result (novice) | Weeks (install ~1–2 days; Rocq + framework: weeks; see §6) | 4–6 days total per brief | 3–4 days total per brief |
| Foreign deps / std gaps | Manual vendoring per crate; no `std` (HashMap!); crc32fast/serde/chrono walls | Compiles dep graph; harness leaf logic; crc32fast workaround documented in brief | Opaque, fine |
| Proof maintenance | Simulations must track every Rust change; 5× generated code regen | Harnesses rarely churn | Strategies rarely churn |
| CI cost | Rocq docker + opam + driver build (their own CI needs submodule init + disk checks) | 10–20 min dedicated job | Seconds |
| Maturity for external users | Vendor-funded consultancy vehicle; 0 releases; not on crates.io; no third-party production users found | AWS production use in CI; monthly releases | 200M downloads |
| License risk | Transpiler **AGPL-3.0** (using an unmodified tool is fine; forking/embedding it is not; MIT proofs) | Apache/MIT | MIT/Apache |

**Net:** rocq-of-rust's unique contribution is *unbounded* proofs and last-layer-wins/sortedness laws held for all input sizes — the one thing Kani cannot give (Kani's bounds are explicit and small). Everything else is strictly more cost for the same coverage. Our correctness surface is small and pure; the brief's bounded-exhaustive Kani proofs over it already capture ~all the practical assurance at ~10% of the cost and zero new languages.

## 6. Effort model (no Coq/Rocq experience)

- **Install:** build the rustc driver (`rustc-dev` on pinned nightly) + opam switch with Rocq 9.0 and 4 packages (their CI needs a docker image and 30+ lines of setup). Budget 1–2 days, macOS ARM friction likely **[inconclusive]**.
- **Learning curve:** Rocq tactics and typeclasses alone is a 2–4 week ramp; on top you must internalize their untyped `Value.t` embedding, the `M` monad, `φ`/`Φ` injections, `Run`/`SimulateM` predicates, and the simulation-refinement methodology (`docs/proofs/proof.md`, `links.md`, `proof_next.md` — three long guides of concepts Kani/proptest have no analogue for).
- **Time to first proof:** a literal `add_one`-style `_eq` lemma: ~1–2 weeks in. A theorem about `merge_spans(layers: &[Vec<Span>]) -> Vec<Span>` (L2 sortedness + L3 last-layer-wins): after rewriting it off `HashMap`, writing `Span`/`Vec`/`String` simulations, and building trait-has-run hypotheses — realistically **1–3 months** for our team. For calibration, Formal Land professionals' Sui `progress` lemma file is **1,646 lines** (`RocqOfRust/move_sui/proofs/move_bytecode_verifier/type_safety.v`, 2 lemmas, 2 still `Admitted`), and their homepage markets a 124-line machine-checked revm theorem as flagship work.
- **Maintenance:** every Rust edit regenerates a 5× translation; simulations and proofs must be reconciled manually (proof.md: "the source Rust code may evolve, and we need a reliable way to know which simulations should be changed" — that mechanism is you).

## 7. Recommendation: **PASS**

Do not adopt rocq-of-rust for TraceLake now; do not pilot. Reasons, in order of weight: (1) proofs cannot live with the Rust code — they are a separate Rocq tree, failing our stated placement requirement while Kani/proptest satisfy it natively; (2) effort is 1–3 orders of magnitude above the Kani path for the same modules, with a new language and a vendor-dominated, release-less tool; (3) concrete coverage walls on our actual code (`HashMap`, `crc32fast`, serde, chrono); (4) no third-party production adoption found.

**Reassess triggers:** (a) a correctness requirement that is inherently unbounded and high-stakes — e.g., a published invariant like "trace-pack decode is total for all byte strings, forever" becoming a contract requirement; (b) budget to engage Formal Land's verification service directly (they sell spec+proof work; that is how every in-tree project was done); (c) Kani unwind bounds becoming the binding constraint on a proof we actually need. If reassessed, also compare **Aeneas** and **Hax** (both listed in their README as alternatives; Aeneas auto-derives the functional form, removing the manual simulation step) before committing.

**If you nevertheless want a spike** (not recommended, 1-week timebox, success = translation pipeline runs, not a proof): `cargo rocq-of-rust` on `tracelake-core` under their toolchain; axiomatize everything foreign (`--axiomatize` flag exists); theorem: `decode_span_key(encode_span_key(t, ns, tr, ts, sid))` returns the original components, stated as a simulation `_eq` lemma over a `SpanKey` simulation. Success criteria: generated `Crate.v` type-checks in Rocq 9.0 and the simulation for `SpanKey` compiles. Abort criteria: pinned nightly fails to build the crate, or >2 days spent before `Crate.v` is produced.

## 8. Risks (if adopted despite the recommendation)

- **Vendor single-point-of-failure:** ~2/3 of commits from the founder; consultancy-driven roadmap (revm/Sui/Solana priorities, not ours); zero tagged releases; API churn risk in generated output.
- **Toolchain lag:** pinned nightly-2025-12-07 rustc driver; any rustc/THIR change breaks it; our Rust 1.96/edition-2024 code must compile under *their* pin, and every workspace toolchain bump re-opens that question.
- **AGPL-3.0 transpiler:** fine to *use* unmodified; any patching/embedding of the driver infects. Legal review if we ever modify it.
- **Proof rot:** simulations track Rust by hand; a database under active development would accumulate proof-update debt on every refactor.
- **Soundness caveats:** generated `Crate.v` itself contains `Admitted` instances (seen in `add_one.v`); even the vendor's std work ships `Admitted` lemmas (`Option::map`). "Verified" claims would need audit of what is actually `Qed` vs `Admitted` per file.
- **Interpreter/eval gaps:** their own `docs/evaluation.md` describes the evaluator as handling only the cases needed by `add_one` — testing translated code on concrete inputs is itself experimental.

## 9. Sources (all queried 2026-10-02)

- https://api.github.com/repos/formal-land/rocq-of-rust — 1,168 stars, pushed 2026-10-01, 62 open issues, `main`, license null (dual per README)
- https://api.github.com/repos/formal-land/rocq-of-rust/releases — 0 releases
- https://api.github.com/repos/formal-land/rocq-of-rust/git/trees/main?recursive=1 — 4,397 entries; 1,936 `.v` files
- https://api.github.com/repos/formal-land/rocq-of-rust/commits?per_page=5 — revm precompile work, 2026-09-30
- https://api.github.com/repos/formal-land/rocq-of-rust/contributors — 16 contributors
- Raw files: `/rust-toolchain` (nightly-2025-12-07), `/Cargo.toml`, `/README.md` (THIR, 3-step workflow, dual license, funding), `/docs/guide.md`, `/docs/builds.md`, `/docs/intro.md`, `/docs/evaluation.md`, `/docs/proofs/proof.md` (simulations/proofs layout, 5× expansion, `{{ env, state | e ⇓ v | state' }}`), `/docs/proofs/links.md` (`add` example), `/RocqOfRust/rocq-of-rust.opam` (Rocq ≥9.0 <9.1, coq-hammer…), `/RocqOfRust/what_we_do.md` (Sui `progress` lemma), `/RocqOfRust/blacklist.txt`, `/.gitmodules` (10 vendored crates incl. rust-lang/rust), `/.github/workflows/rust.yml` (docker rocq/rocq-prover:9.0 + opam + rustc-dev), `/examples/custom/add_one.rs`, `/RocqOfRust/examples/default/examples/custom/add_one.v` (25 lines), `/RocqOfRust/core/simulate/option.v` (248 lines; `ok_or_eq` 1-line proof; `map_eq` Admitted), `/RocqOfRust/core/option.rs` (2,822) vs `/RocqOfRust/core/option.v` (7,334), `/RocqOfRust/move_sui/proofs/move_bytecode_verifier/type_safety.v` (1,646 lines), `/RocqOfRust/revm/revm_interpreter/gas/simulations_legacy/calc.v` (42 lines, commented sketch)
- https://crates.io/api/v1/crates/coq-of-rust and /rocq-of-rust — both "does not exist"
- https://formal.land/ — client reports: revm formal specification (2026-02-15, Ethereum Foundation), Sui Move type-checker, Zcash Orchard, CompPoly, smoo.th, Tezos; "Full formal verification since 2021"
- Local: `/Volumes/Workspace/snroshan/tracelake/docs/research/formal-verification-brief.md` (Kani/proptest recommendation being compared), `tracelake-query/src/merge.rs` (HashMap in `merge_spans`), `tracelake-core/src/{key,bloom,segment}.rs` (DefaultHasher, serde/chrono usage)
