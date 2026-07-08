# `noeta fmt` — a canonical source formatter

**Status: ARC COMPLETE (F0–F7), branch `fmt`, worktree `.claude/worktrees/fmt` (local, unpushed).**
The `noeta fmt` canonical formatter ships: a lean, reusable `noeta-fmt` crate (lex+trivia → parse →
AST → `Doc` → text) behind a **safety gate** (re-parse + AST-equal-modulo-spans, else abort
untouched), with **comment reattachment**, opt-in **width-driven wrapping** (`[fmt] wrap`, default
off, so the existing corpus needs no reflow), and configurable **match-arm alignment**. Front-ends:
`noeta fmt` (files/dirs, `--check`, `--stdin`, atomic writes) and the LSP `textDocument/formatting`
provider. Property-tested over the ~530-file `.noe` corpus: **safe + idempotent + comment-complete
under both `wrap` policies**. `fmt` was one of the two subcommands `docs/The-CLI.md` listed as
*intentionally absent* (the other is `check`). The parallel `noeta mcp` arc wraps this engine rather
than reimplementing it — `noeta-fmt` is a reusable library, as required.

**Follow-ons landed (branch `fmt-followups`, off `main`):**
- ✅ **Uniform trailing commas** — parser accepts a trailing comma in every comma list (added
  `.allow_trailing()` to the 11 that lacked it); formatter emits one on wrapped sequences via a new
  `Doc::if_break` combinator. Conformance + corpus green under both wrap policies.
- ✅ **On-type formatting (narrow)** — `noeta_fmt::format_stmt_at` reformats the single top-level
  statement containing an offset (safety-checked by splice+reparse); LSP advertises
  `documentOnTypeFormattingProvider` (trigger `}`), quiet on mid-typed/unparseable buffers.
- ✅ **Optional import sorting** — `[fmt] sort_imports` (default off): alphabetizes comment-free `use`
  runs + names; safety gate made import-order-invariant. Also fixed a pre-existing bug (single import
  `use A.B.C` was printing as `use A.B.{C}`).
- ✅ **Extension wiring** — VS Code ext turns on format-on-save + format-on-type by default for `.noe`
  (verified the live LSP advertises both providers); ext 0.3.0→0.4.0.
- ✅ **Range ("Format Selection") formatting** (branch `fmt-range`) — `noeta_fmt::format_range`
  reformats the top-level statements overlapping a byte range (partial selection expanded to whole
  statements), one safety-checked edit per changed statement; LSP advertises
  `documentRangeFormattingProvider` (Format Selection works with no extension change).

*Still deferred (optional): width-wrapping of long binary/method chains and `A|B|C` unions;
`noeta fmt --diff`; `// fmt: off`; broader config.*

## The one idea that shapes everything: canonical reformat, not whitespace touch-up

A formatter can be built two ways. A **whitespace touch-up** pass keeps the author's tokens and line
breaks and only fixes indentation and inter-token spacing (early gofmt-ish, most linters' `--fix`). A
**canonical reformatter** throws the original layout away and *re-derives* the entire textual form
from the parsed structure, so the output is a pure function of the AST — the same program always
prints identically no matter how it was written (gofmt, rustfmt, Prettier, black).

We build the **canonical reformatter**. It is the architecturally sound choice for this codebase and
it is *already half-built*:

- **Every AST node already carries a full `Span`** (`noeta-ast/src/lib.rs`: "Every node carries a
  `Span`"). We can map any node back to its exact source bytes — the substrate comment reattachment
  needs.
- **There is precedent**: `noeta-ast/src/pretty.rs` is a stable indented printer (an S-expression
  debug form for snapshots) and its header already names *"the parse→print→parse property test
  (Slice 9)"*. The formatter is the *source-syntax* sibling of that S-expr printer, and it inherits
  the same correctness discipline.
- The language deliberately made **line-end `;` optional** and has **explicit member access**,
  **union types**, **match/if-expr desugaring** — all normalization opportunities a canonical printer
  realizes and a touch-up pass cannot.

The output is defined by one invariant: **`fmt(src)` depends only on `parse(src)`, the file's
comments, and a small set of preserved author-choice trivia (currently: whether each statement was
written with a trailing `;`) — never on the incoming whitespace.** Everything else in this plan
follows from making that true and safe. The formatter is canonical about *layout* (indent, spacing,
blank lines, continuation) while deliberately leaving a few author choices intact (semicolons; see
canon table); it never *forces* a token the author didn't write.

## The one hard problem: comments are thrown away before the AST exists

The lexer **discards every comment**. Line comments are dropped at the `logos` level
(`#[logos(skip(r"//[^\n]*"))]`, `noeta-lexer/src/lib.rs:21`) and never reach the token loop; block
comments are seen as a `BlockCommentOpen` token, scanned to their (nesting-aware) close, and the whole
span is dropped with **no token emitted** (`lib.rs:604`). So by the time we have tokens — let alone an
AST — the comments are gone. A canonical reformatter that prints from the AST would therefore **delete
every comment in the file**, which is catastrophic and non-negotiable to fix.

This is *the* classic hard part of every formatter (rustfmt's comment handling, Prettier's "dangling
comments", gofmt's `ast.CommentMap`). The rest of the printer is mechanical; comments are where
formatters earn their keep. We treat it as a first-class subsystem (slices F1 + F4), not an
afterthought.

### Trivia collection (F1)

Comments live in *trivia* — the source between tokens. We recover them with a dedicated
**trivia-collection pass** so the hot compile path stays byte-for-byte unchanged:

- Add an opt-in lexer mode (`lex_with_trivia`, or a `Lexed.comments: Vec<Comment>` populated only when
  a `collect_trivia` flag is set) that records each comment as `Comment { span, kind: Line | Block,
  text }`. Line comments become a recorded-but-not-emitted token instead of a `logos` skip *only in
  this mode*; block comments are already located in the loop, so we just also push their span. The
  parser's `tokens` stream is **identical** either way — we never change what the parser consumes.
- Rationale for reusing the lexer rather than a standalone scanner: the lexer already knows when it is
  inside a string/interpolation, so `"http://x"` is never mistaken for a `//` comment. A naïve
  regex scanner would get this wrong. (Per the perf-sweep directive: the flag is off on the compile
  path, so zero hot-path cost; a bench asserts no regression.)

The result is a flat, source-ordered `Vec<Comment>` alongside the AST.

**Semicolon presence** is the other author-choice trivia we preserve (the canon keeps `;` where
written). It is *derivable*, not stored on the AST: at print time, for each statement we check the
source bytes between the statement's `span.end` and the next token for a `;`. Threaded through the
same trivia machinery as comments; no AST change required.

### Comment reattachment (F4)

We reattach each comment to the AST node it belongs to, using spans, with the standard three-bucket
model:

- **Leading** — a comment whose span ends before a node's span start, with only whitespace between:
  attaches *above* that node (own-line) or is a same-line **trailing** comment of the *previous* node
  if they share a line.
- **Trailing** — a comment on the same source line as, and after, a node: prints at end-of-line after
  that node.
- **Dangling** — a comment with no adjacent node (empty block `{ /* ... */ }`, comment before a
  closing brace, file-trailing comment): attached to the enclosing container and printed in a
  deterministic slot.

The reattachment walks the AST in source order and greedily assigns each comment to the nearest
following node (leading) unless it shares a line with the preceding node (trailing). A **completeness
property** (F4 gate) asserts *every* comment in the input appears exactly once in the output — the
formatter may never silently eat a comment.

## Architecture

New crate **`noeta-fmt`** (sound: keeps formatting out of the compile DAG and the hot path):

```
noeta-lexer ─┐
noeta-parser ─┼─→ noeta-fmt ─→ noeta-cli (`noeta fmt`)
noeta-ast  ──┘        └──────→ noeta-lsp (`textDocument/formatting`, F7)
```

Pipeline for one file:

```
source ──lex_with_trivia──▶ (tokens, comments)
       ──parse────────────▶ Program (AST, spans)
       ──reattach─────────▶ AST + CommentMap
       ──lower────────────▶ Doc  (Wadler pretty-print IR)
       ──render(width)────▶ formatted String
       ──SAFETY GATE──────▶ re-lex+parse the output, assert AST-equal-modulo-spans, else abort
```

### The printer: a Wadler/Prettier `Doc` algebra (F2), two group-break policies

We do **not** emit strings directly. We lower the AST to a small **`Doc` pretty-printing algebra**
(`text`, `line`/`softline`/`hardline`, `nest`, `group`, `concat`) with a best-fit renderer — the
Wadler-Leijen design that Prettier and every serious formatter uses. This is the "build it right"
lever: the *same* `Doc` tree serves both break policies, so wrapping is a choice of *how groups
decide to break*, never a second printer.

The `wrap` config knob selects the group-break policy for the whole document:

- **`wrap = false` (default) — source-directed.** A `group` breaks iff the source already had a line
  break inside it (respect author intent at statement/block granularity). Normalizes indentation,
  spacing, blank-line runs, continuation indent, and (per config) match-arm arrows, but does **not**
  re-flow a long line the author wrote on one line, nor join lines the author split. This is why the
  existing corpus needs **no reflow** — a file that is already spaced/indented sanely comes out
  essentially unchanged. Predictable, low-surprise, reviewable diffs.
- **`wrap = true` — width-driven.** A `group` breaks iff it does not fit in `line_width` columns (the
  classic Wadler best-fit), ignoring the author's original breaks entirely: long argument lists,
  method / pipeline chains, and long `A | B | C` unions wrap; short broken things join. Fully
  canonical layout. Same `Doc`, same renderer — only the fits-test policy differs.

Both policies are deterministic, so safety + idempotency hold under either. `wrap` is a single
whole-document setting in v1 (not per-construct); finer control is a later follow-on.

### Continuation indentation (pipelines & chains) — a first-class F3 requirement

A statement that spills onto multiple lines must indent its continuation correctly; this is exactly
what the `Doc` `nest` combinator is for, and it must be right under **both** break policies. The
language keeps `|>` as its own left-associative `Expr::Pipeline` node (`noeta-ast/src/lib.rs:687`;
loosest precedence), so `a |> f |> g |> h` is a nested chain. A pipeline chain (and likewise a
`.`-method chain or a long `&&`/`||`/`??` chain) lowers to a single `group(nest(4, [head, line,
"|> " seg, line, "|> " seg, ...]))`. The group **breaks** when the author already broke it
(`wrap=false`) or when it overflows `line_width` (`wrap=true`); either way the `nest(4)` guarantees
every `|>` segment sits on its own line indented one level under the statement start — never left at
the statement's own column, never mis-stepped:

```
// input (author broke the chain)               // output (canon: 4-sp continuation)
let names = users                                let names = users
|> filter(fn(u) => u.active)                         |> filter(fn(u) => u.active)
    |>map(fn(u)=>u.name)                             |> map(fn(u) => u.name)
  |> sort()                                          |> sort()
```

Assignment continuation (`let x =\n    <long expr>`), long argument lists, and match-arm bodies use
the same one-level `nest`. A targeted F3 test pins pipeline/chain continuation indentation across
break shapes.

### Configuration — a minimal `[fmt]` seam in `noeta.toml`

The project already has a manifest, `noeta.toml` (`crates/noeta-cli/src/manifest.rs`), discovered by
walking up from the target and already rejecting unknown top-level keys. The formatter reads an
optional `[fmt]` table from that same file — **no new config file, no new discovery mechanism**:

```toml
[fmt]
wrap             = false       # false (default) = keep author line breaks | true = width-driven wrapping
line_width       = 100         # column budget used only when wrap = true
match_arm_arrows = "compact"   # "compact" (default) | "align"
```

Design intent: introduce the config *seam* now (a `FmtConfig` with defaults, threaded into the
printer) so options are designed-in rather than bolted-on, and ship exactly the knobs v1 needs:

- **`wrap`** (default `false`) — off by default *specifically so the existing corpus needs no
  reflow*: the formatter respects the line breaks already in the code and only normalizes
  indent/spacing/blank-lines/continuation. A team that wants fully-canonical width-driven layout opts
  in with `wrap = true` (+ `line_width`). Both are deterministic → idempotent + safe.
- **`match_arm_arrows`** (default `compact`) — the one purely-aesthetic call the language owner wants
  left to taste: `compact` (single space, edit-stable, forces alignment on no one) or `align`
  (column-aligned `=>` for teams that prefer that readability).

`noeta fmt --stdin` with no discoverable manifest uses these defaults (so piping a snippet from an
editor with no project gives stable, corpus-compatible output). Idempotency/safety hold under every
combination.

### Correctness guarantees (gates on every slice from F3)

1. **Safety (no semantic change).** After printing, re-lex + re-parse the output and assert the new
   AST equals the original **modulo spans** (structural equality on a span-erased view — reuse/extend
   the `pretty.rs` S-expr form with spans stripped as the comparison key). If they differ, the
   formatter **aborts and writes nothing**, returning the file untouched with a diagnostic. A
   formatter that changes meaning is worse than no formatter.
2. **Idempotency.** `fmt(fmt(src)) == fmt(src)`, byte-for-byte, over the whole corpus.
3. **Comment completeness (F4+).** The multiset of comment texts in the output equals that of the
   input.

These run as property tests over the **~3 000-file `.noe` corpus** (`tests/`, `examples/`) — an
enormous ready-made stability bed. Any file that fails safety is a printer bug, surfaced by the gate
rather than shipped.

## Canonical style specification (v1)

Derived from the existing corpus (indent histogram: 2 735 lines @4 / 454 @8 / 74 @12 → **4-space
indent** is overwhelmingly the house style) and the language's own normalization opportunities:

| Aspect | Canon |
|---|---|
| Indent | 4 spaces, never tabs |
| Line width | `line_width` (default 100 cols); bites only when `wrap = true` |
| Wrapping | **configurable** via `[fmt] wrap` — `false` (default; keep author line breaks) or `true` (width-driven reflow of arg lists / chains / unions). See *The printer* + *Configuration* |
| Braces | opening brace on the same line as its `struct`/`class`/`enum`/`fn`/`match`/block header (K&R), as the corpus already does |
| Statements | one per line |
| Trailing `;` | **preserved as written** — the language made line-end `;` optional; the formatter neither adds nor strips them, it keeps each statement's author choice. Presence is tracked as per-statement trivia (see below) |
| Blank lines | collapse runs to max 1 inside a block, max 2 at top level; no leading/trailing blank in a block |
| `match` arms | **configurable** via `[fmt] match_arm_arrows` — `"compact"` (single space around `=>`, gofmt-style; **default**) or `"align"` (column-align `=>` across an arm group). See *Configuration* |
| Continuation | a statement broken across lines (pipelines `\|>`, method chains, long binary/`??` chains) indents its continuation **one level (4 sp)** under the statement start; nested breaks add a level each. See *Continuation indentation* |
| Spacing | one space around binary ops, after `,`/`:`, none inside `(`/`[`, none before `,`/`;` |
| Trailing commas | when `wrap = true`, added on the last element of any list broken onto its own line; left as-is when inline or when `wrap = false` |
| String interpolation | untouched inside `${...}` beyond re-formatting the expression |
| `@doc { }` text tiers | body preserved verbatim (it is free-form prose, not code) |

## CLI surface (F6)

Matches gofmt/rustfmt muscle memory:

```
noeta fmt [PATHS...]        # format files/dirs in place (recurses dirs for *.noe)
noeta fmt --check [PATHS]   # exit 1 + print unified diff if any file would change; write nothing (CI)
noeta fmt --stdin           # read source on stdin, write formatted to stdout (editor "format on save")
noeta fmt -                 # alias for --stdin
```

Rules: never write a file whose input **fails to parse** (emit the parse diagnostics, leave it
untouched, exit non-zero); never write a file that **fails the safety gate**; in-place writes are
atomic (temp + rename) and skipped when the content is already canonical (no needless mtime churn).

## Slices

Each slice is independently green; safety + idempotency + (from F4) comment-completeness are standing
gates re-run every slice.

- **F0 — crate skeleton + spec + config seam + CLI stub. ✅ DONE.** `noeta-fmt` crate created
  (`FmtConfig`/`ArrowStyle`/`FmtError`, `format_source` entry point with the **safety gate** live —
  span-stripped `Pretty` comparison); minimal printer (literals, `echo`, `return`, bare-expr stmts,
  top-level `fn` with untyped params); everything else → `FmtError::Unsupported`. `[fmt]` table wired
  into `manifest.rs` (`resolve_fmt_config`, defaults + validation). `Command::Fmt` in the CLI:
  in-place, `--check` (exit 1 if unformatted), `--stdin`. Corpus harness stands (530 files: 4
  ok+idempotent, 517 unsupported, 9 parse-err; safety held on all). fmt+clippy clean.
- **F1 — trivia collection. ✅ DONE.** `noeta_lexer::lex_with_trivia` → `Lexed.comments`
  (`Comment { span, kind: Line | Block }`); `//` is now a dropped `LineComment` token, block
  comments recorded with their full span. Token stream **provably unchanged** vs. `lex` (property
  test over mixed inputs incl. `//` inside strings); overhead timing shows no hot-path regression
  (comment-heavy 36.6ms vs comment-free 41.3ms). `noeta-fmt` now lexes with trivia (comments threaded
  to the printer, emitted in F4) and **preserves trailing `;` per statement** via
  `trivia::has_trailing_semicolon`. Full workspace + conformance green.
- **F2 — `Doc` algebra. ✅ DONE.** Wadler/Leijen `text|line|softline|hardline|nest|group|concat|join`
  in `doc.rs` + best-fit `render(doc, width)` (Lindig iterative form: work-stack renderer +
  `fits` lookahead that consumes the trailing continuation). Unit tests cover flat/break by width,
  hardline forcing, nest-indents-broken-lines-only, and independent nested groups. Gated
  `#[allow(dead_code)]` until F3 lowers the printer onto it.
- **F3 — AST→Doc printer, source-directed (`wrap = false`). ✅ DONE.** `print.rs` lowers the **whole**
  surface onto `Doc` — every `Stmt`/`Expr`/`TypeRef`/`Pattern`/decl/directive/`@tier` block — and is
  **total** over any parseable program (`FmtError::Unsupported` removed). The safety gate drove out a
  series of real bugs, each now handled: **precedence-minimal parenthesization** (matching the
  parser's Pratt table, so `Sub(Shl(a,b),c)` prints `(a << b) - c`), **restricted-head** parens (a
  struct literal at an `if`/`while`/`for`/`match` head), the **`x.field = v`** binding desugar,
  **list-spread re-sugaring** (`[] ~ ...a ~ [x]` → `[...a, x]`), **string re-escaping** (`\$` guards a
  literal `${}`; raw/template strings round-trip), blank-line preservation (real blank lines only),
  and trailing-whitespace stripping. Pipeline/binary continuation nests one level; `match_arm_arrows`
  `compact`/`align` both verified. Also: the safety-gate comparator was hardened (drop fragile
  quote-tracking). **Corpus: 521/530 ok+idempotent, 0 unsupported, 9 intentional parse-errors; safety
  held on all.** `examples/orders.noe` formats and runs byte-identically. fmt+clippy clean.
- **F4 — comment reattachment. ✅ DONE.** A global source-ordered comment cursor + an
  `interleave_comments` helper places leading (own-line above), trailing (same-line after), and
  dangling (before a close / EOF) comments, interleaved through statement blocks, struct/class bodies,
  enum bodies, and match arms (heterogeneous members unified via `Member`/`EnumMember`, printed in
  source order so author blank lines are preserved too). Block/struct/enum member layout is now
  source-directed (no forced blanks). **Completeness gate added to the corpus harness** (input vs
  output comment multiset) — green over all 521 files, still safe + idempotent. Also fixed a
  gate-invisible fidelity bug: enum-variant/field `;` (whose span *covers* the `;`) is now preserved.
- **F5 — width-driven wrapping (`wrap = true`). ✅ DONE.** A `delimited` helper renders every
  comma sequence (call args, lists, tuples, maps, object literals, params) flat when `wrap = false`
  (byte-stable) and as a width-driven `group` when `wrap = true` — one element per line, indented,
  delimiters on their own lines. Pipeline chains flatten into a single group so `a |> b |> c` breaks
  as one `|>`-per-line unit. **No trailing commas** (verified: call args reject one; universally safe
  without). The corpus harness now runs **both** policies — safety + idempotency + completeness green
  over all 521 files under each; the default stays byte-identical. Golden unit tests pin the wrapped
  layouts. *Deferred refinements (noted): width-wrapping of long binary chains, method chains, and
  `A | B | C` unions (currently source-directed/flat even under `wrap`); trailing commas where the
  grammar allows.*
- **F6 — CLI completion. ✅ DONE.** `noeta fmt` now takes files **or directories** (recurses for
  `.noe`, skipping dot-dirs), writes in place **atomically** (sibling temp + rename), stays silent on
  success (gofmt-style), and `--check` lists each unformatted file and exits non-zero (CI). Per-file
  `noeta.toml` `[fmt]` discovery; parse failures reported and left untouched. 4 `assert_cmd`
  integration tests (stdin, --check, in-place, decline-unparseable) + manual end-to-end verify.
  *(A `--diff` output is a possible follow-on; `--check` currently lists like `gofmt -l`.)*
- **F7 — LSP `textDocument/formatting`. ✅ DONE.** `noeta-lsp` advertises
  `document_formatting_provider` and implements `formatting` over the same `noeta_fmt` engine
  (`DocumentStore::format_document` → a single full-document `TextEdit`; no-op when already canonical;
  no edits on unparseable source). Config parsing was **centralized into `noeta-fmt`**
  (`FmtConfig::from_toml` / `FmtConfig::discover`) so the editor and CLI honor the same `[fmt]` table
  and can never drift; the CLI's `manifest.rs` now delegates to it. 3 store tests; the VS Code /
  VSCodium client picks up "Format Document" / format-on-save automatically from the capability.
  *(Range formatting is an optional follow-on.)*

## Non-goals / deferred (recorded in `plans/deferred.md`)

- **Comment *content* reflow** (rewrapping prose inside `//`/`/* */`) — never; we preserve comment
  text verbatim, even when `wrap = true`.
- **Per-construct / per-region wrap control** — `wrap` is one whole-document setting in v1; finer
  granularity is a later follow-on.
- **Import sorting / `use` grouping**, **format-on-type**, **`// fmt: off` regions**, and a **broader
  config surface** — later follow-ons. v1 ships the `[fmt]` seam with exactly three knobs (`wrap`,
  `line_width`, `match_arm_arrows`); further options are added deliberately, one review at a time, not
  opened up wholesale.
- **`check` subcommand** (the other "intentionally absent" verb) — unrelated arc.

## Decisions

Resolved with the language owner:

1. **Wrapping** — **in v1** (slice F5) as the `[fmt] wrap` knob, **default `false`** (keep author
   line breaks) so the existing corpus needs no reflow; `wrap = true` opts into width-driven layout.
2. **Trailing `;`** — **preserved as written**; the formatter neither adds nor strips (per-statement
   trivia).
3. **`match` `=>` alignment** — **configurable** via `[fmt] match_arm_arrows`, **default `compact`**,
   `align` opt-in — readability of column alignment available without being forced on anyone.
4. **Continuation indentation** — multi-line pipelines/chains nest one 4-space level; a first-class
   F3 requirement with its own test, correct under both `wrap` settings.

No open decisions remain; F0 is ready to start.
