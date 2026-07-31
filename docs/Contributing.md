# Contributing & Developer Guide

This page is the developer's entry point to *building* the language — the compiler, not programs in it. It complements the in-repo `AGENTS.md` (the exhaustive conventions reference) and `ARCHITECTURE.md` (the technical overview); this page orients you and points at them.

Noeta is a pre-1.0 implementation — public, with tagged releases — built primarily through agentic engineering, so the discipline below is written to be followed by humans and AI agents alike.

## Orientation

| To understand… | Read… |
|---|---|
| What the language is | [Home](Home) and the [Language Tour](Language-Tour) |
| How the implementation is structured | [Architecture & Pipeline](Architecture-and-Pipeline), and `ARCHITECTURE.md` in the repo |
| The full conventions & agent workflow | `AGENTS.md` in the repo |
| The individual subsystems | the [Concepts & design](Home#concepts--design) pages |
| What to work on next | `plans/roadmap.md` (the frontier) and `plans/backlog.md` (every open item) |

## Build & run

```sh
cargo build                                  # build the workspace + the `noeta` binary
cargo test                                   # unit + snapshot + conformance + property tests
cargo run -p noeta-cli -- run file.noe       # run a program (or use ./target/debug/noeta)
cargo run -p noeta-conformance -- --differential   # the differential oracle (dev harness)
cargo run -p noeta-conformance -- --file hello.noe # one conformance fixture — `--file` matches by
                                                   # path suffix (e.g. `--file gc/class_self_cycle.noe`)
```

You need a recent stable Rust (1.95+).

## The compilation pipeline

Source flows through sharp, single-purpose crates: lexer → parser → checker → IR → RC passes → two backends. Each stage is a separate crate with explicit input/output types and no hidden shared mutable state, so a change is local to one crate and verifiable by that crate's snapshots. The full diagram and crate map are on [Architecture & Pipeline](Architecture-and-Pipeline). Every crate carries a `README.md` as its primary documentation — start there when working in one.

## The differential oracle is the spine

The language is implemented twice — a reference interpreter (`noeta-eval`) and a bytecode VM (`noeta-vm`) — and the conformance harness asserts their observable output is byte-for-byte identical on every corpus program, with a hard **`0 skipped`** gate. This is the backbone of the test strategy. Two rules follow:

1. **A feature lands in both backends** (or is explicitly oracle-exempt). Shared semantics go in `noeta-stdlib` so the two agree by construction rather than by luck.
2. **The differential must stay green** — `--differential` at 0 skipped, backends agree — after every change.

## The new-feature template

A language feature is added as a **vertical slice**, in this order:

1. **Grammar / AST** — token(s) in `noeta-lexer`, node(s) in `noeta-ast`, production in `noeta-parser` (keep surface sugar as its own AST node).
2. **Checker rule** — typing/inference in `noeta-check` (+ a new `Type` form in `noeta-types` if needed); add a negative conformance case for any new static-error class.
3. **Lowering** — AST → IR in `noeta-ir` (+ `noeta-ir-passes` if the feature introduces owned heap values needing drops/reuse).
4. **Both backends** — evaluation in `noeta-eval` **and** bytecode in `noeta-compiler`/`noeta-vm`; keep shared semantics in `noeta-stdlib`.
5. **Conformance cases** — `tests/conformance/**.noe` with `// expect:` headers, including error cases; must run `--differential` at 0 skipped.
6. **Snapshot update** — review `insta` snapshots deliberately; never blind-accept.

> **The iron rule: every feature or fix lands with a conformance corpus entry.** Prefer vertical-slice tasks ("implement `~` end to end") over diffuse refactors — a slice's done-condition is "its conformance cases pass."

## Testing architecture

- **Per-stage snapshots** (`insta`) — tokens, AST, and rendered diagnostics are pinned at each boundary.
- **Conformance corpus** (`tests/conformance/**.noe`) — executable end-to-end behavior with `// expect:` headers, run through both backends and asserted identical.
- **Property tests** (`proptest`) — invariants like parse→print→parse round-trips, and the static-≤-dynamic last-use property for the RC passes.
- **The leak oracle** — heap residency must be 0 at clean exit, both backends, whole corpus.
- **The refcount-anomaly oracle** — during cycle collection, every unreachable object's refcount must equal its in-edges from the garbage set (unreachable garbage can only reference itself), so a *skipped* retain or release is caught even when teardown's backup sweep would have absorbed the orphan. Runs inside the leak and JIT oracles.
- **The JIT oracle** (`--jit-differential`, `jit` feature) — every corpus program through the interpreter and the forced-Tier-1 JIT: byte-identical `RunResult`, zero residency, zero anomalies. This is the gate for native-code refcount contracts that miri cannot see.
- **miri** — covers the quarantined `unsafe` that can execute under it: `noeta-value`, `noeta-gc`, the `noeta-db` newtype, a `noeta-stdlib` reinterpret, and the test-only `noeta-alloc-probe`. The compiler and the default-feature interpreter paths are `unsafe`-free.
- **What miri cannot see — the JIT oracle covers it.** The Tier-1 seam's `unsafe` (`noeta-jit`, the VM's `jit`-feature helpers) executes as generated native code, which miri can't run; the JIT oracle above is that code's gate for memory and refcount correctness.
- **The hot-reload end-to-end suites** (`scripts/hot-e2e.sh`) — `hot_serve`, `hot_live`, `parallel_hot`, `live_serve`, `graceful_drain`, driven through the shipped binary against a real listening socket: edit a running handler and assert the new body serves, the signal state survives, the swap reaches every worker, and a live client is told to reload. They stay `#[ignore]`d because they bind ports and spawn processes, so nothing runs them implicitly; the script is the one place that does, called by both the `jit` CI job and the merge tier of `scripts/gate.sh`. Everything else about a hot swap is checked on the compile side — this is the only gate that watches one land in a server that is actually serving.
- **Coverage** — measured with `cargo-llvm-cov` (never tarpaulin, which can't see the subprocess-driven CLI tests). `cargo llvm-cov --workspace --summary-only`.
- **Benchmarks** — `cargo bench -p noeta-vm` runs the `criterion` benches over the VM hot paths; a VM-touching change should check for no regression.

## Before you're done

`scripts/gate.sh` runs the CI workflow's jobs locally, in the same split, and prints a per-step PASS/FAIL summary — a step that cannot run is reported SKIP, never PASS, and a failure never stops the remaining steps. Run the tier that matches the moment:

```sh
scripts/gate.sh --quick   # fmt + both clippy splits                        (1m20s warm)
scripts/gate.sh           # + the suite & oracles, doc samples, JIT gates,
                          #   and the real-socket hot-reload e2e suites     (~15 min warm, 35 cold)
scripts/gate.sh --full    # + wasm portability, miri, editor tooling        (before a release tag)
```

- Zero compiler warnings (`cargo build` produces no `warning:` lines).
- The gate is green at the merge tier, and `--differential` is at 0 skipped.
- New functionality has tests; architectural changes update the docs.

## Conventions (summary — full list in `AGENTS.md`)

- Conventional-commit titles; work on a branch/worktree to avoid conflicts with parallel agents.
- American English in code, comments, and docs.
- No hard line-wrap in Markdown.
- Prefer enums/constants over magic strings; keep `unsafe` quarantined and justified.
- Each crate keeps its `README.md` current; keep `README.md`/`AGENTS.md`/`ARCHITECTURE.md`/these docs aligned when architecture or features change.

## The docs themselves

The docs live in the repo under `docs/` — versioned together with the code — as flat pages with `_Sidebar.md` for navigation, published to docs.noeta.dev. When you change a feature, update the relevant reference page here alongside the code — the same "lands with its docs" discipline as the conformance rule.
