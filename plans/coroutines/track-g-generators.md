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
- **G.1b — straight-line desugar (executable generators, no control flow across `yield`). ✅ DONE**
  (2026-06-30). The desugar runs in **IR lowering** (`lower_generator` in `lang-ir/lower.rs`), as an
  AST→AST transform producing ordinary AST that the existing lowering paths turn into IR — only the
  final wrap is a dedicated `Rvalue::MakeGen`/`Op::MakeGen` (→ `Value::iter_gen`, both backends). The
  body becomes `mut $state = 0`, one `mut <local> = none` per **top-level** local (conservatively
  hoisted to a captured cell), then `return make_gen(($resume) => { <if-chain over $state> })`. The
  if-chain splits the body at each top-level `yield`: state *k* runs segment *k*, advances `$state`,
  and `return some(<yielded>)`; the final segment `return none`. Hoisted `mut` locals become captured
  cells, so a value computed before a `yield` survives into the next segment, and the original
  `let x = …` inside the step reassigns the outer cell (the language's bare-assignment rule). The
  desugar applies only at named-`fn`/method bodies (`generator: bool` on `lower_func`), never to a
  closure or the generated step closure, so it runs exactly once. `$state`/`$resume`/`$step` use `$`
  (lexer-forbidden in source) → collision-free.
  - **Checker:** the G.1a "not yet executable" gate is gone; a `yield` nested in control flow
    (`if`/`for`/`while`) is now E0039 "not yet supported (Track G.2)" (`first_nested_yield`).
  - **Prerequisite bug fix (pre-existing, committed separately):** the drop-insertion pass
    (`lang-ir-passes/drops.rs`) was treating a closure's **upvalue** as a droppable frame-local and
    inserting a spurious last-use `DropVar`, which cleared the shared cell — so a stateful closure
    that outlived its defining frame lost its captured state on a read-only call (`100,0,0` instead
    of `100,200,0`). Fixed by threading the **enclosing-locals** chain through the pass and excluding
    upvalues from the droppable/owned sets. Generators rest entirely on this pattern.
  - Conformance: `generators/straight_line.lang` (drained generator, locals across yields, params,
    composed with `take`/`map`/`filter`/`enumerate`/streaming `for`), `yield_in_control_flow.lang`
    (the G.2 gate), and `closures/returned_stateful_closure.lang` (the drop-fix regression guard).
    347 conformance / differential 337 / 0-skipped / leaks 0 both / clippy+fmt+workspace clean.
- **G.2 — control flow across `yield`. ✅ DONE** (2026-06-30). The straight-line if-chain became a
  **CFG flattener** (`Flattener` in `lang-ir/lower.rs`): the body lowers to a graph of states, and the
  step closure is a `while true` dispatch loop over `$state` where a state either **returns** (a
  `yield` → `some`, or exhaustion → `none`) or **jumps** (`$state = j; continue`). `yield` becomes a
  `Yield(e, next)` terminator; `if`-with-yield/escaping-ctrl becomes a `Branch`; `while`-with-yield
  becomes a head/​body/​after loop with a back-edge; `break`/`continue` become `Goto` to the enclosing
  flattened loop's after/​head. A construct with **no yield and no escaping `break`/`continue`** is
  emitted **verbatim** (a `match` — its arms are expressions, so it never carries yield/ctrl; a
  self-contained `for`/​`while`/​`if`), running whole within one state. This unlocks infinite
  generators (`while true { yield … }` + `take`), bounded `while`, conditional yields (`if`/`else`
  straddling the suspend), early `break`, and `continue`. Every flattened-level local (now including
  those inside flattened `if`/`while`) is hoisted to a cell, and a `mut x = …`/`let x = …` is rewritten
  to a bare assignment so it reassigns the cell instead of re-shadowing it (a latent G.1b gap for
  `mut` locals, fixed here).
  - **Checker:** the G.1b gate narrowed — `yield` inside `if`/`while` now runs; only a `yield` inside a
    `for` is still E0039 ("a later slice adds `for`", via `first_yield_under_for`). `for`-across-yield
    needs the iterator cursor as machine state; deferred.
  - Conformance: `generators/control_flow.lang` (infinite + `take`, bounded `while`, conditional
    yield, `break`, `continue`, wholesale `for`), `yield_in_for.lang` (the remaining gate). 348
    conformance / differential 339 / 0-skipped / leaks 0 both / clippy+fmt+workspace clean.
- **G.4 — `for` across `yield`. ✅ DONE** (2026-07-01, `85a6d7f`). The G.2 gate is lifted: a
  `for <pat> in src { … yield … }` lowers to the iterator protocol in the flattener (`lower_one`'s new
  `For`-with-yield arm). A hoisted cell holds the iterator — `src.iter()` for a collection, or `src`
  directly when it is already an `Iterator<T>` (reusing the checker's `for_stream_sites` set) — and the
  loop becomes a flattened `while` over `.next()`: `head` fetches `$next = $cursor.next()` and
  `Branch`es on `match $next { some(_) => true, _ => false }`; the body binds the loop variable(s) from
  `$next ?? none` (the `none` arm is unreachable) and routes `break`/`continue` to the after/head
  states. Because the cursor is a cell, the source position survives every `yield`. Unlocks `for` over
  collections/ranges/iterator-sources, nested loops, `break`/`continue` across the suspend, tuple
  destructuring, and composition with the lazy adapters. The checker's `first_yield_under_for` gate and
  helper are removed. Conformance `generators/for_across_yield.lang` (replaces `yield_in_for.lang`);
  348 conformance / differential 339 / 0-skipped / leaks 0 both / clippy+fmt+workspace clean.
- **G.3 — coloring + liveness. ✅ DONE** (2026-07-01, coloring `2afc84a`, liveness `6df8606`).
  - **Coloring** was already enforced: the checker resets its yield context (`current_yield`) at every
    closure boundary, so a `yield` inside a closure — including one passed to a builtin like `map` —
    hits the "not inside a generator" path and reports E0039. Locked in by
    `generators/yield_in_closure.lang`. (No code change; the reset landed back in G.1a.)
  - **Liveness** replaces the flattener's hoist-everything: a local becomes a persistent captured cell
    only when it is live across a suspend/jump (referenced in >1 state, via `ref_block_count` over the
    conservative total `block_mentions`/`stmt_mentions`). A fresh declaration (`mut x`/for-var) used in
    a single suspend-free segment stays a block-local, re-bound each entry. Behavior-preserving by
    construction: only *declaring* names are eligible (a block-local and a cell shadow an outer
    identically); a bare-`x =`-first name (may reassign an outer), a destructure/tuple target, and the
    synthetic `for` cursor/next cells stay on the always-hoist path. Both backends consume the same IR
    → invisible to `RunResult`. Locked in by `generators/liveness.lang` (cross-yield accumulators vs
    single-segment temporaries). 350 conformance / differential 0-skipped / leaks 0 both /
    clippy+fmt+workspace clean.

**Track G is COMPLETE** (G.1a → G.1b → G.2 → G.4 → G.3). Generators (`yield`) are fully executable
with control flow across the suspend, `for`-across-yield, coloring, and liveness-minimized cells. Next
in the coroutines plan: **Track A** (async/await) — a later milestone (deterministic injected executor
already decided in the parent doc).

## Verification (every sub-slice)

`cargo run -q -p lang-conformance` (+ `--differential` 0-skipped / agree, `--check-leaks` 0 both);
`cargo test --workspace`, clippy `--all-targets`, fmt; **miri when `lang-value` is touched** (G.1a adds
an `IterState` variant, so miri runs). New conformance per slice (error cases in G.1a; a drained +
adapter-composed generator in G.1b; infinite-generator-`take`, `while`, conditional yield, early
`break` in G.2; the coloring error in G.3). Diagnostic budget: **E0039** (the first Track-G code).
