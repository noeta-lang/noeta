# Namespaced types — one qualified-identity model for extern + user types

**Branch:** `extern-type-namespacing` (worktree `.claude/worktrees/extern-type-namespacing`, off `main` @ `7ecc9354`)
**Status:** IN PROGRESS. Design approved (Option A, full scope). Slices commit green one at a time.

## Goal

Today two kinds of named type are handled asymmetrically:

- **Native extern types** (`Uuid`, `Response`, `Span`, `FileHandle`, the reactive/CRDT/cell types — 14
  registry `ExtType`s) live in a **global, prelude-like** name space: referenced by bare name, never
  `use`-imported, and **globally reserved** (E0049) — a user type of the same bare name is rejected
  *even under a distinct `namespace`*. This is why the OTel instrument names `Counter`/`Gauge`/
  `Histogram` can't be native types (12 corpus files declare `class Counter`).
- **User types** are *nominally* namespaced (`namespace App.Data;` + `use App.Data.Counter`) but
  `namespace` is a **runtime no-op**: the linker merges every declaration under its **short name**,
  so two `App.A.User` / `App.B.User` collide and can never coexist.

**This arc unifies both under one model:** every named type — extern or user — has a **qualified
identity** (`std.id.Uuid`, `App.Models.User`) and a **short display name** (`Uuid`, `User`). Types are
brought into a file by `use`, optionally aliased (`use … as …`); nothing is globally reserved;
collisions are per-file local-name clashes resolved by aliasing. Two same-named types from different
namespaces coexist. This is the general "real module scoping" the M0/M1 comments kept deferring, plus
the extern-type importability the brief asked for — done together because they are the same seam.

**Do not touch the metrics arc** (`native-otel-metrics-logs`); it stays on `Instrument`. A *future*
follow-up (not this arc) can adopt idiomatic instrument types once this lands.

## Design decisions (approved)

1. **Option A — import-scoped, truly namespaced** (over Option B ambient-shadowable). Types must be
   `use`-imported to reference by short name; no global reservation; conflation impossible because
   identities are qualified. Cost: corpus migration (adding `use` lines) — explicitly in scope.
2. **One qualified identity end-to-end** (checker **and** runtime), not a checker-only qualification
   bridged to a bare runtime. Removes the translation seam, and is the machinery extern-vs-extern and
   user-vs-user coexistence both require. The frozen `noeta-native` ABI *signature* is unchanged
   (`type_name() -> &'static str`); only its *convention* shifts bare→qualified. No external consumers
   exist yet (third-party native packages are the future PM Phase 3), so establishing the convention
   now is strictly better than migrating later.
3. **Identity vs display split.** Qualified identity drives lookup / equality / dispatch / `is`/`as`.
   The short name drives human-facing output (error messages, `type_of` stringification). Preserves
   the current corpus's observable behavior while identities become qualified underneath.
4. **`as` alias syntax** (over `=>`/`:`): `use std.metrics.Counter as MetricCounter;` and grouped
   `use std.metrics.{Counter as MetricCounter, Gauge};`. `as` is already a lexed keyword (cast
   position only) and unambiguous in `use` position; `=>` is the match-arm separator and `:` the
   type-annotation token, both worse. `UseName` gains `alias: Option<String>`.
5. **Language-level types stay global** — `Iterator`/`Future`/`Sender`/`Receiver` (NOT `ExternValue`s;
   produced by `iter()`/`async`/`.await`/`channel()`) remain ambient and reserved via
   `NATIVE_TYPE_NAMES`. Only the `registry::find_type` half of E0049 is removed.

## Verified mechanics (checked against code)

- Reservation: `noeta-check/src/lib.rs:901` `check_reserved_type_name` (`NATIVE_TYPE_NAMES` OR
  `registry::find_type`). Annotation validity: `lib.rs:2741` `check_type_ref` Named arm (admits
  `find_type(name)` at `:2747`). Method typing: `noeta-check/src/stdlib.rs:256/478` (`Type::Named` +
  `find_type`). Producer sigs: `SigType::Named(TYPE_NAME)` (e.g. `registry.rs:2000`).
- Runtime dispatch by `type_name()` → `find_type`/`dispatch_method`: `noeta-vm/src/methods.rs:703,736`;
  `noeta-eval/src/lib.rs:2892-2900`. `type_name()` constants: `id.rs:27`, `net.rs:14,106`,
  `crypto.rs:101`, `telemetry.rs:46`, `cell.rs:21`, `reactive.rs:43-45`, `synced.rs:48`,
  `crdt.rs:30-32`, inline `"FileHandle"` (`handle.rs:340`, `registry.rs:173`). `ExtType` struct:
  `noeta-native/src/registry.rs:348`.
- User types: parser `Stmt::Namespace{path}` / `Stmt::Use{path,names}`; linker
  `noeta-loader/src/lib.rs` `link_core`/`resolve` merges by short name, E0019 (pub) / E0020
  (collision); `reroot_program` (`:181`) already rewrites path prefixes for PM deps — the seam full
  qualification extends. Backends key on `TypeDef.name` (eval) / `Shape.name` (vm), both short.
- Value kinds are disjoint: extern values (`Value::Extern`/`Payload::Extern`) vs user objects
  (`Value::Object`/`Payload::Object`) never share a dispatch table — so a native `Counter` and a user
  `Counter` never conflate at runtime regardless of name.

## Namespace assignment (extern types)

`std.id.Uuid`, `std.crypto.Hasher`, `std.http.{Response,Request}`, `std.fs.FileHandle`
(produced by `fs.open`, though registered in the core unit), `std.telemetry.Span`,
`std.cell.Cell`, `std.reactive.{Signal,Computed,Effect}`, `std.synced.SyncedSignal`,
`std.crdt.{GCounter,PnCounter,GSet}`. (Confirm CRDT root `std.crdt` vs `std.synced` in A0.)

---

## Phase A — Extern types: qualified identity, importable, aliased

A complete, shippable milestone on the smaller surface; validates the identity model before the
linker rewrite. Full gate per slice: differential + leak + conformance + diagnostics + fmt + clippy.

- **A0** — `namespace` field on `ExtType` (default `"std"`); real paths on the 14 types; qualified/
  display helpers (`find_type_qualified`, `qualified_name`, `display_name`); `find_type(bare)` kept.
  No behavior change.
- **A1** — qualified runtime identity: `type_name()` → qualified; `find_type`/route caches keyed
  qualified; display split so reflection/errors read short. Both backends + reflection consistent;
  audit isolate `Wire` boundary. Differential green.
- **A2** — checker: `sig_to_type_bound`/`Generic` emit qualified extern `Type::Named`; method typing
  resolves qualified; audit every `Type::Named` match in `noeta-check` (`is_send` `:2324`,
  key-capability `:2772`/`:3581`, reflection).
- **A3** — `as` alias syntax (`UseName.alias`), parser + AST + fmt. Shared with Phase B.
- **A4** — import binding `use std.<ns>.<Type> [as Alias]` → short/alias→qualified map; annotation
  validity gated on import (bare un-imported extern → E0027); drop `find_type` half of E0049.
- **A5** — migrate corpus (~17 `.noe` files gain `use` lines); migrate E0049 tests (`FileHandle`
  case now *allowed*; `Iterator`/`Future` stay E0049).
- **A6** — conformance: user `Counter` alongside an imported native type; two same-named externs via
  aliases; alias rename. Differential agrees.

## Phase B — Real module scoping for user types

Reuses A's identity/display/alias machinery, extended to the linker + user types. The larger, riskier
half; each slice keeps the existing single-namespace corpus green.

- **B0** — linker assigns each user decl a qualified identity from its module namespace and rewrites
  intra-/cross-module type references to qualified form (extend `reroot_program`/`link_core`). E0020 →
  duplicate *qualified* identity; local-name clash stays a per-file check. Existing corpus: types
  become `App.Main.Foo` internally, short display → behavior-preserving.
- **B1** — backends key on qualified user identity (`Shape.name`/`TypeDef.name`, method route tables,
  `is`/`as`, reflection `TypeRepr`) with short display. Differential green.
- **B2** — checker namespace-aware resolution: unadorned name → current-namespace-qualified or
  imported-qualified; `use`/alias → qualified; unify with the extern resolution path from A4.
- **B3** — conformance: two same-named user types from different namespaces coexist via aliases; the
  native-vs-user `Counter` case; E0027 un-imported / E0020 clash cases. Differential agrees.
- **B4** — unify: extern + user types share one identity/resolution/display path; remove residual
  asymmetry; update `docs/resources` + confirm LSP/salsa parity. Final gate.

## Risks / watch-list

- **Differential oracle** is the safety net for every identity change — both backends must key on the
  identical qualified string. Run it every slice.
- **Reflection/`type_of` display** is the one user-visible surface; identity-vs-display split contains
  it, but conformance asserting on type strings may need intentional migration (flag, don't silently
  change).
- **Startup bytecode cache** keys on build identity — qualified names change emitted bytecode; ensure
  the cache key still invalidates correctly (it gates on build-identity, so a recompile is expected).
- **LSP/salsa** cross-package `use` already resolves (PM Phase 2); B must keep in-editor resolution in
  lockstep with the CLI linker.
- **Enum variant paths** (`E.Empty` in match/patterns) reference the type by name — qualification must
  reach these, in both checker and backends.
