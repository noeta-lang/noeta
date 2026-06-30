# Track G — generators (`yield`)

Parent: `plans/coroutines/README.md` (representation + typing decided there). **Status: IN PROGRESS**
(branch `main`, per repo convention). Builds directly on the completed Track I (the lazy `Iterator`
protocol + `Value::Iter`/`IterState`).

## Representation (settled in the parent doc)

A function whose body contains `yield` is a **generator**. It lowers to an **ordinary closure** whose
captured **mutable cells** hold the state (`$state` discriminant + locals live across a `yield`),
wrapped in a new `IterState::Gen { step }` variant of the Track-I `Value::Iter`. No new runtime value
kind, no runtime suspension; the step closure body is the state machine. `Gen.next()` reuses the I.1c
closure-from-`next` applier (the step takes one ignored *resume* arg, forward-compatible with Track A)
and interprets the step's returned `?T` (`some(x)` → element, `none` → end). Generators compose with
every Track-I adapter for free.

## Typing (settled in the parent doc)

Return is plain `Iterator<T>` (no `Generator<T>`, no `gen` keyword — `yield` marks it). With the
declared element type, `yield e` is a checking-mode `e <: T` (E0007). Pure pull → `yield` is a
statement (no send-type). `return e;` in a generator is forbidden; bare `return;` ends iteration.

## Sub-slices (each its own green, in-oracle commit)

- **G.1a — surface + typing + the `Gen` runtime. ✅ DONE** (2026-06-30, no executable generator yet).
  - Lexer `yield`; parser `yield expr` → `Stmt::Yield`; AST node (+ pretty).
  - Generator detection (`body_has_yield`, recursing into `if`/`for`/`while`, not nested callables).
  - Checker: a generator's declared return must be `Iterator<T>` (else **E0039**); each `yield e` is
    checked `e <: T` (E0007, checking-mode via `current_yield`); `yield` outside a generator → E0039;
    `return e;` (value) in a generator → E0039 (bare `return;` allowed); the yield context resets at a
    closure boundary (coloring foundation). `Iterator` is now a writable type annotation
    (`PRELUDE_TYPES`). **Interim execution gate:** a well-formed generator (correct return, body
    otherwise clean) gets a clean E0039 *"not yet executable (Track G.1b)"* — so a generator
    type-checks but cannot run yet, preserving the "lowering is total" invariant (no panic) until the
    desugar lands. (Un-annotated-generator inference also deferred to G.4.)
  - Runtime: `IterState::Gen { step }` in both backends + `Value::iter_gen(step)` constructor +
    the `iter_next` `Gen` arm (call the step closure via the I.1c applier with one resume arg,
    interpret its `?T` via the new lang-value `option_take` helper). Dead until G.1b wires the
    desugar, but validated by a lang-value **miri** unit test (a `Gen` driven by a hand-written step
    stand-in, heap elements, leak-clean).
  - Conformance (`tests/conformance/generators/`): error cases only — yield-outside-generator,
    yield-type-mismatch, return-value-in-generator, wrong-return-type, and the not-yet-executable gate
    (pins that the typing accepts a well-formed generator). All fail at check time, never lowering.
  - Gates: conformance 345, differential 336/0-skipped/agree, leaks 0 both, clippy/fmt/workspace
    clean, miri clean. **E0039 is the first Track-G diagnostic code.**
- **G.1b — straight-line desugar (executable generators, no control flow across `yield`).** The
  AST→AST (post-check) transform for a body that is a flat statement sequence: split at each top-level
  `yield` into states, conservatively hoist every local to a `$`-cell, emit the dispatch-loop step
  closure + `__make_gen`. A `yield` inside any control-flow construct → E0039 "not yet supported
  (Track G.2)". Conformance: a finite straight-line generator drained by `collect()`/`for`, composed
  with an adapter (`gen().map(f)`).
- **G.2 — control flow across `yield`.** Extend the transform to `while`/`loop`/`if`/`match` and
  `break`/`continue` straddling a `yield` (the dispatch-loop state assignment). This is the
  high-value slice — infinite generators (`loop { yield … }`), `while`, conditional yields.
- **G.3 — liveness (optimization) + coloring.** Replace the conservative hoist-everything with real
  liveness (only locals live across a `yield` become cells); enforce the coloring rule (`yield` inside
  a closure passed to a builtin → E0039).

## Verification (every sub-slice)

`cargo run -q -p lang-conformance` (+ `--differential` 0-skipped / agree, `--check-leaks` 0 both);
`cargo test --workspace`, clippy `--all-targets`, fmt; **miri when `lang-value` is touched** (G.1a adds
an `IterState` variant, so miri runs). New conformance per slice (error cases in G.1a; a drained +
adapter-composed generator in G.1b; infinite-generator-`take`, `while`, conditional yield, early
`break` in G.2; the coloring error in G.3). Diagnostic budget: **E0039** (the first Track-G code).
