# Expression tiers — typed embedded-language blocks as values

**Goal.** A tier declaration can declare an **expression tier**: its `@<name> { … }` blocks are
*expressions* — verbatim foreign-language text with `${…}` holes — and each block evaluates to a
typed value by desugaring to a call of the declared handler function. This turns the tier
name-space into the language's embedded-DSL surface: a package (pure Noeta — no native code
needed) ships `@sql`, `@json`, `@yaml`, `@html`, and a consumer writes

```noeta
q = @sql { select * from users where id = ${user_id} and age > ${min_age} }
```

getting back a **parsed, checked, typed** value (`Query`) — holes are real expressions, checked
in the enclosing scope against the handler's declared hole type, and never string-spliced.

Builds directly on text-tiers S1/S2 (generic verbatim capture, `text:` declarations) and
tier-providers T2–T4 (`@tier` declarations, open name set). The LiveView templating system
(server-hmr arc's consumer) is the flagship downstream client: `@html` blocks whose holes become
reactive computeds — designed in this arc's discussion, built as its own follow-on.

## Why this shape

- **Lexical capture is the one thing only the compiler can do.** A hole must close over locals
  (`${user_id}`) and type-check in scope. Everything else — parsing the foreign text, deciding
  what holes mean, caching, rendering — is ordinary library code in the handler. So core grows
  exactly one generic construct, and every DSL is a package.
- **Holes are string interpolation's `${…}`, verbatim.** Same trigger, same `\$` escape for a
  literal `${`, same nested-brace scan (`find_hole_end`), same sub-parse machinery
  (`parse_hole`: span-shifted re-lex, full expression grammar, diagnostics against the real
  file). Expression tiers inherit interpolation's contract *and* its known limitation (an
  unescaped `}` inside a string literal inside a hole ends the hole early; write `\}` — the
  string-escape passthrough yields `}`).
- **The desugar makes typing free.** `@sql { … }` rewrites (post-activation, pre-check) to
  `handler([statics…], [fn() => hole1, …])` — a plain call. Bidirectional checking then lands
  hole-type errors on the hole expressions' real spans; the block's type is the handler's return
  type; cross-package resolution rides the linker's qualified names. The checker, both backends,
  REPL, LSP, and DAP see an ordinary call — no new runtime form, no new Op.

## Declaration surface

```noeta
@tier(sql, text: "sql", expr: Query)
fn parse_query(statics: List<string>, holes: List<() -> SqlValue>): Query { … }
```

- `expr: <Type>` marks the tier as an expression tier and names the block-value type. It must
  textually match the handler's declared return type (they are adjacent lines; the redundancy is
  the declaration documenting itself). Mismatch, or `expr:` alongside `config:`, is **E0051**.
- `text: "<lang>"` is optional on an expression tier (recommended — it drives editor injection);
  omitted, the body is still captured verbatim (the lexer's declaration scan keys on either
  `text:` or `expr:`) and the lang defaults to none.
- The decorated fn is the **handler**, not a runner. Its signature must be exactly
  `fn(statics: List<string>, holes: List<() -> U>): T` for the declared `T` and any hole type
  `U` (E0051 otherwise, with a tier-kind-specific message). `U` is typically a union or a
  wrapper the package defines; heterogeneous holes check against it like any expression.
- An expression tier has **no runner semantics**: `noeta <tier>` dispatch rejects it, its blocks
  never activate/strip via `[targets.*.tiers]`, and adjacency (`@doc`-style) does not apply.

## Use-site semantics

- `@name { body }` in **expression position** parses to `Expr::TierExpr { tier, statics, holes }`:
  the raw body splits at unescaped `${` (statics count = holes count + 1, empty strings where
  holes touch); text segments unescape `\{ \} \\` (per text-tiers S1) plus `\$`; hole text
  sub-parses via `parse_hole` with absolute spans.
- Desugar (in `activate_tiers`, the one seam CLI/LSP/MCP all consume): `TierExpr` →
  `Call(handler, [ [static, …], [Closure(fn() => hole), …] ])`. The handler is referenced by its
  (possibly link-qualified) name, so cross-package use works exactly like tier-runner dispatch.
  Holes become zero-param closures — **lazily evaluated, at the handler's discretion**, which is
  what lets `@html` wrap holes in computeds and a future `@sql` skip evaluating an unused
  fragment.
- Evaluation is per-encounter: each evaluation of the block expression calls the handler with a
  fresh statics list and fresh closures. Handlers that parse their statics should memoize (the
  statics are constant per site); a compile-time-interned statics table is a follow-on
  optimization, not v1.
- Misuse diagnostics (**E0052 InvalidTierExpression**): a block of a *non*-expression tier in
  expression position ("`@test` is not an expression tier"); an expression tier's block in
  statement position ("an expression-tier block is a value — assign or return it"), raised in
  activation where statement blocks are resolved.

## What stays core vs. what packages own

Core (this arc): body capture (already shipped by text-tiers S1), the expression-position parse
+ hole splitting, `expr:` declaration checking, the desugar, E0051/E0052. Generic — nothing
HTML- or SQL-shaped.

Packages: the handler (parse statics, interpret holes, build the typed result), the value types
(`Query`, `Json`, `Html`), any runtime (LiveView mount/patch loop), editor injection grammar for
their lang id. A pure-Noeta package can ship all of this today via cross-package `use`; a native
package can, once the tier-providers `ExtTier` port merges, declare the tier from Rust with a
native handler.

## Status

✅ **E1–E5 COMPLETE** (branch `expr-tiers`, rebased onto main after text-tiers merged, `4bf10d87`→`52ec2091`). `@tier(name, text: "lang", expr: T)` declares an expression tier; `@name { … }` blocks parse to `Expr::TierExpr`, type as the handler call they desugar to, and lower through the shared `noeta_ast::desugar::tier_expr_call` constructor (Try/Await architecture — node survives parse for fmt, checker types it, IR rewrites it). E0051 (handler signature / `expr:` return match / `config:` exclusion), E0052 (statement-position + non-expr-tier-as-value). 605/605 conformance both backends; `examples/sql_tier.noe` end-to-end. Reconciled with text-tiers S3 (dropped a redundant `doc_span` field in favor of S3's span-slicing; fmt renders `@tier(…)` with the `expr:` key). E6/E7 remain (gated).

## Slices

- **E1 — parser**: `@ident { … }` primary expression → `Expr::TierExpr`; body splitting
  (statics/holes, escapes); lexer's `declared_text_tiers` scan additionally catches `expr:`-only
  declarations. Tests: shapes, spans, escapes, zero-hole, adjacent holes.
- **E2 — checker**: `TierDecl.expr` + `DeclaredTier.expr`; E0051 rules (handler signature,
  `expr:`/return match, `config:` exclusion); E0052 kind/position misuse; registry surface for
  the parser-facing "is expr tier" question is *not* needed (position decides the parse; the
  checker validates kind).
- **E3 — desugar + conformance**: rewrite in `activate_tiers`; e2e `@greet`-style pure-Noeta
  tier through run/check/REPL on both backends; cross-fn and cross-module handler resolution.
- **E4 — cross-file capture + dispatch guard + polish**: consumer files lex with the workspace's
  declared text/expr tier names (shared infrastructure with text-tiers S3 — coordinate; whoever
  lands second rebases); `noeta <tier>` rejects expression tiers; fmt idempotence over expr
  blocks (expected free — fmt re-emits raw source — but gate it).
- **E5 — example + docs**: `examples/` DSL (e.g. `@json` in userland), docs page section, the
  `@sql`-shaped conformance story.
- **E6 (gated on tier-providers merge) — ExtTier port**: `expr`/`text` fields on `ExtTier`,
  native handlers; std dogfood (`@json` in `std.json`).
- **E7 (gated on text-tiers S4/S5) — editor injection for holes**: tree-sitter/TextMate lex
  holes inside expr-tier bodies as Noeta injections within the foreign-language injection.

## Non-goals (v1)

- Generic handlers (`fn(…, holes: List<() -> T>): Template<T>`) — concrete types only.
- Compile-time handler evaluation (const-checking `@sql` syntax at build) — runtime parse with
  memoization; revisit with const-eval.
- Statics interning / per-site caching in core — handler-side memoization suffices to start.
- The `@html` LiveView package itself — separate arc on server-hmr's substrate.
