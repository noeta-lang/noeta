# Slice 9 — §14 demo + REPL + proptest

Status: done

## Goal
The complete syntax-doc §14 order program runs end-to-end; property tests are in place; the REPL is hardened. This closes M0.

## Scope
- In: the full §14 `App.Orders` program (`docs/resources/02-syntax.md` §14) as the canonical acceptance artifact + conformance case; the prelude pieces it needs (`next_id` deterministic, `sum`, `User` stub); `proptest` properties; REPL polish (multiline, error recovery, value printing).
- Out: anything past M0 scope.

## Checklist (vertical slice)
- [x] Grammar / AST: none anticipated, but the demo surfaced **named call arguments**
  (`NegativePrice(index: i)`) — added to the call-args parser (label parsed, binds
  positionally in M0; M1 validates/reorders against declarations).
- [x] Eval op: the demo's full feature set composes (validate/place/match/label/total).
- [x] Conformance cases: `examples/orders.lang` + its byte-identical corpus mirror
  `tests/conformance/demo/orders.lang` (with `// expect:` assertions).
- [x] proptest: pipeline totality (no panics) + determinism over token-soup/arbitrary text,
  valid-program parse+eval cleanliness, and evaluator-no-panic over the whole corpus.
- [x] Snapshots: AST for the full demo (via `include_str!`, so it can't drift from the file).

## Notes / traps
- `next_id()` must be deterministic (seeded counter) so the demo's output is stable.
- The parse→print→parse property requires a stable AST pretty-printer (already used for snapshots).

## Definition of done (also the M0 DoD)
- `lang run examples/orders.lang` produces the expected output (exit 0); same program green under `lang test`.
- REPL + file runner work; `proptest` properties run.
- Full `cargo test` + `lang test` green (human and `--json`); `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` clean; zero `unsafe`.
- Every M0 surface feature has a passing conformance case (incl. negative/error cases). M0 marked done in `roadmap.md`.

## Outcome (done) — M0 complete
The §14 program runs end-to-end (`lang run examples/orders.lang`, exit 0) printing the
placed-order label, the re-placed order, the empty-order error, and the negative-price
error — all deterministic (seeded `next_id`). It is mirrored byte-for-byte as the corpus
case `demo/orders.lang`; a guard test asserts the two never diverge.

**Named call arguments** were the one missing piece the demo needed (`OrderError`'s
`NegativePrice(index: i)`): the call-args parser now accepts an optional `name:` label and
binds positionally (an M0 "parse the intent" simplification; M1 validates/reorders).

**REPL hardening (`lang-eval::Session`):** a persistent interpreter whose scope and id
counter survive across entries (so `x = 5;` then `echo x;` works), with value-printing for a
trailing bare expression (`1 + 2` → `3`), multiline continuation when an entry parses only
to an end-of-input error (a `fn { ... }` typed across lines), and per-entry error recovery
that never wedges the session. `TreeWalkBackend::run` is untouched (the differential oracle
keeps its fresh-interpreter-per-program contract).

**proptest** (`crates/lang-eval/tests/properties.rs`): the S-expr pretty-printer is not
re-parsable source, so a literal parse→print→parse round-trip is an M2-formatter concern;
its *intent* (parse-then-print is a stable function) is captured by determinism properties.
Properties: pipeline totality on token-soup and arbitrary text (no panics), result + AST
determinism, valid integer-arithmetic programs parse and evaluate cleanly, and a no-panic +
determinism sweep over every corpus file.

**Final gates:** 19 test binaries green; `lang test` 22/22 (human + `--json`); fmt + clippy
clean; `unsafe_code = "forbid"` workspace-wide (zero `unsafe`). M0 is done.
