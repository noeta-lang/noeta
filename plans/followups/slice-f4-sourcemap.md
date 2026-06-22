# Slice F4 — SourceMap: correct source attribution for cross-module diagnostics

**Status:** **DONE.** Branch `m2-host-io`. Part of the deferred sweep (see the `deferred-sweep` memory); closes the "Diagnostics source attribution" row of `plans/deferred.md`.

**Outcome:** shipped via `SourceId` on `Span` (option B). The parser stamps every span with the file's `SourceId` at its `to_span` boundary (made a `Ctx` method; `binary`/interpolation threaded accordingly); `Span` gained a `source` field with `FIRST`-defaulting constructors, so the lexer, the dozens of synthetic span sites, and all downstream crates compiled unchanged. `Linked` carries a `SourceMap` (`lang-span`); `lang-conformance::errors_of_mapped` and `lang-cli::emit_diagnostics_mapped` resolve each diagnostic through it. No AST visitor — correct by construction. No snapshot churn (the parser pretty-printer never printed the id). Verified: conformance 113/0 (new `tests/conformance/modules/cross_module_error/`, `E0008` at `models.lang:8:12`), differential 107 matched/0 skipped/backends agree, loader + span unit tests lock the stamping, clippy/fmt clean.

## Goal

A check/runtime diagnostic that lands inside a declaration merged in from a sibling module must render against **that module's** source and coordinates, not the entry's. Today every span is a bare `(start, end)` byte range with no source identity, and both render sites (`lang-conformance::errors_of`, `lang-cli::emit_diagnostics`) resolve every diagnostic against the entry source — so a sibling-module `1 / 0` renders at a bogus entry position (the confirmed repro: `main.lang:2:85`, inside a comment).

## Approach (decided with the user, 2026-06-22): SourceId on Span — option B

Reject the global-coordinate + shift-visitor approach (the original deferred-row sketch). Instead **fill the missing abstraction**: a `Span` carries the `SourceId` it belongs to, stamped at parse time. This makes source identity *stored*, not reconstructed positionally — correct by construction, no AST shift-visitor to write or maintain, and the failure mode is a compile error rather than a silently-wrong diagnostic. Spans keep **local** (per-source 0-based) offsets; the `SourceId` disambiguates which source those offsets index.

Costs accepted: `Span` grows 8 → 12 bytes (still `Copy`); the parser stamps `source.id()` at its span-construction boundary (~54 `to_span` sites, mechanical, compiler-guided). No snapshot churn — the parser's pretty-printer prints `@start..end` and will keep doing so (the id is not printed).

## Checklist (vertical slice)

- **lang-span**
  - Add `pub source: SourceId` to `Span`. Keep ergonomic constructors defaulting to `SourceId::FIRST` (`new`, `empty_at`, `From<Range>`) so the dozens of single-source / synthetic call sites compile unchanged and keep today's behavior; add `*_in(source, …)` variants the parser uses to stamp. `merge`/offset helpers **preserve** `self.source` (never reset it to `FIRST`). Add a `Span::SYNTHETIC`-style no-source constructor to DRY the four ad-hoc zero-spans (lang-db ×2, lang-types ×2) — optional nicety, FIRST already behaves identically.
  - Add `SourceMap`: a `Vec<Source>` indexed by `SourceId`, with `source(id) -> &Source`, `line_col(span) -> LineCol` (look up `span.source`, then `source.line_col(span.start)`), and graceful fallback for an out-of-range id (treat as entry).
- **lang-parser**
  - `to_span` (and the interpolation/`empty_at`/`shift` span builders) stamp `ctx.source.id()`. Make `to_span` a `Ctx` method (`ctx.to_span(simple)`); `shift`/interpolation preserve/stamp the source. The whole parse is one source, so the id is constant per parse — entry parses to id 0, each sibling to its own id, automatically.
- **lang-loader**
  - `Linked` gains a `SourceMap` (entry + siblings, the `Source`s it already constructs). No span shifting — parsing already tagged each module's spans. Keep `entry` (still used for the entry name / single-source rendering).
- **lang-conformance** — `errors_of` for the linked path resolves each diagnostic via the `SourceMap` (the single-file path is unchanged: its spans are `FIRST`, matching the lone source).
- **lang-cli** — `emit_diagnostics` for the linked path picks each diagnostic's source from the `SourceMap` (`render(map.source(d.span.source), d)`), since spans are local offsets into their own source.
- **Conformance fixture** — re-add `tests/conformance/modules/cross_module_error/{main.lang,models.lang}`: entry `use`s a sibling whose method body divides by zero; the `// expect: error E0008 at L:C` is the sibling's coordinates (the expectation checks code+line+col, so a correct match proves the right source). (Use whatever the actual divide-by-zero code is.)
- **Unit tests** — lang-loader: linking stamps merged declarations with the sibling's `SourceId`. lang-span: `SourceMap::line_col` resolves a tagged span to the right source.

## Determinism / oracle posture

Not differential-covered: both backends consume the same linked program with identical source-tagged spans, so they agree on `RunResult` regardless — the differential is unaffected (stays at its current count, 0 skipped). The fix is exercised by the hand-written multi-file conformance fixture (load path) + the unit tests. The db/differential path also parses per-source, so its spans are tagged consistently for free.

## Definition of done

The cross-module fixture renders at the sibling's coordinates (green conformance); unit tests lock the stamping; full conformance + differential stay green; fmt/clippy clean; no snapshot churn.
