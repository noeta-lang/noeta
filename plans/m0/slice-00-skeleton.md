# Slice 0 — Skeleton + diagnostics spine + hairline end-to-end + harness

Status: todo

## Goal
A hairline-thin but complete pipeline — source → tokens → AST → tree-walk → stdout — runs `echo "hello"`, and the test harness exists.

## Scope
- In: workspace bootstrap; `lang-span`; `lang-diagnostics` (seed a few `E0xxx` + the single `ariadne` renderer); `lang-ast` (minimal nodes + `SyntaxKind` enum); lexer/parser/eval just enough for string literals + `echo`; the `Backend`/`RunResult` seam; `lang-conformance` runner with `// expect:` header parsing, `--json`, `--file`, `--stage`; `lang-cli run` + `lang test`; first token + AST `insta` snapshots; `hello.lang` conformance case.
- Out: every language feature beyond `echo "literal"` (later slices).

## Checklist (vertical slice)
- [ ] Grammar / AST: string literal, `echo` statement, program node — each carries a `Span`.
- [ ] Checker rule: n/a (no checker in M0).
- [ ] Bytecode: n/a (tree-walker only).
- [ ] Eval op: `TreeWalkBackend` evaluates `echo`, returns `RunResult { stdout, exit_code, diagnostics }`.
- [ ] Conformance cases: `tests/conformance/hello.lang` (`// expect: stdout "hello"` / `// expect: exit 0`).
- [ ] Snapshots: token stream + AST for `hello.lang`.

## Notes / traps
- Diagnostics centralized from the first error; stable `E0xxx`; rendered in one place.
- Eval returns a structured `RunResult` via `trait Backend`; never writes stdout / calls `process::exit` directly. This is the M1 VM differential seam — get it right now.
- AST nodes are pure data + `Span`; define `SyntaxKind` now even though the rowan green tree is built later.
- Conformance header `error CODE` references resolve against the real `DiagnosticCode` enum (no drift).
- Deterministic output (no wall-clock, seeded RNG, sorted maps in test mode).

## Definition of done
- `lang run examples/hello.lang` prints `hello` (exit 0).
- `lang test` runs `hello.lang` green; `--json`, `--file`, `--stage` all work.
- `cargo build`/`cargo test` green; `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` clean; zero `unsafe`.
