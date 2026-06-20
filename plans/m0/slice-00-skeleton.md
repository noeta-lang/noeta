# Slice 0 — Skeleton + diagnostics spine + hairline end-to-end + harness

Status: done

## Goal
A hairline-thin but complete pipeline — source → tokens → AST → tree-walk → stdout — runs `echo "hello"`, and the test harness exists.

## Scope
- In: workspace bootstrap; `lang-span`; `lang-diagnostics` (seed a few `E0xxx` + the single `ariadne` renderer); `lang-ast` (minimal nodes + `SyntaxKind` enum); lexer/parser/eval just enough for string literals + `echo`; the `Backend`/`RunResult` seam; `lang-conformance` runner with `// expect:` header parsing, `--json`, `--file`, `--stage`; `lang-cli run` + `lang test`; first token + AST `insta` snapshots; `hello.lang` conformance case.
- Out: every language feature beyond `echo "literal"` (later slices).

## Checklist (vertical slice)
- [x] Grammar / AST: string literal, `echo` statement, program node — each carries a `Span`.
- [x] Checker rule: n/a (no checker in M0).
- [x] Bytecode: n/a (tree-walker only).
- [x] Eval op: `TreeWalkBackend` evaluates `echo`, returns `RunResult { stdout, exit_code, diagnostics }`.
- [x] Conformance cases: `tests/conformance/hello.lang` + `tests/conformance/lexer/unterminated_string.lang` (negative case).
- [x] Snapshots: token stream (`lang-lexer`) + AST (`lang-parser`).

## Outcome
Workspace bootstrapped (9 crates, strict DAG, `unsafe` forbidden workspace-wide). The
hairline pipeline runs `echo "hello"` end to end. Harness lands with `// expect:` header
parsing, JSON/`--file`/`--stage` modes, the `Backend`/`RunResult` differential seam, and a
`cargo test` corpus gate. 25 tests green; fmt/clippy clean; zero warnings.

Notes for later slices:
- The parser is hand-written (recursive descent) behind `parse()`, not `chumsky` — a
  deliberate, reversible choice recorded in `crates/lang-parser/README.md` and `ARCHITECTURE.md`.
- Conformance `error CODE at L:C` positions are **absolute in the file**, so header
  comment lines shift line numbers.

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
