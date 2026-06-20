# Slice 9 — §14 demo + REPL + proptest

Status: todo

## Goal
The complete syntax-doc §14 order program runs end-to-end; property tests are in place; the REPL is hardened. This closes M0.

## Scope
- In: the full §14 `App.Orders` program (`docs/resources/02-syntax.md` §14) as the canonical acceptance artifact + conformance case; the prelude pieces it needs (`next_id` deterministic, `sum`, `User` stub); `proptest` properties; REPL polish (multiline, error recovery, value printing).
- Out: anything past M0 scope.

## Checklist (vertical slice)
- [ ] Grammar / AST: none new (integration of prior slices).
- [ ] Eval op: ensure the demo's full feature set composes (validate/place/match/label/total).
- [ ] Conformance cases: `examples/orders.lang` (or chosen) as a conformance case with expected stdout/exit.
- [ ] proptest: parse→print→parse round-trip stability; evaluator-no-panic over the whole corpus.
- [ ] Snapshots: AST for the full demo.

## Notes / traps
- `next_id()` must be deterministic (seeded counter) so the demo's output is stable.
- The parse→print→parse property requires a stable AST pretty-printer (already used for snapshots).

## Definition of done (also the M0 DoD)
- `lang run examples/orders.lang` produces the expected output (exit 0); same program green under `lang test`.
- REPL + file runner work; `proptest` properties run.
- Full `cargo test` + `lang test` green (human and `--json`); `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` clean; zero `unsafe`.
- Every M0 surface feature has a passing conformance case (incl. negative/error cases). M0 marked done in `roadmap.md`.
