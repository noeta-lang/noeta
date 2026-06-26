# Object-model redesign — Slice 6: dev-tier blocks (`@tier` primitive, `test` first)

**Status: design, not started.** The dev-tier-blocks slice of the object-model arc (slices 1–5 done).
This plan supersedes the dev-tier section of `plans/object-model-redesign/README.md` where the two
disagree — that section predates two design sessions (the `@dev`→`@tier` rename, and the
2026-06-26 *profiles + provider-map* discussion captured here).

The goal: **co-located developer-tooling content** (`test`/`bench`/`doc`/`debug`) that the build
strips from production by construction, declared on **one primitive** so built-in and third-party
tiers are identical. Ship **`test` end-to-end first**; the rest land as declarations, not new
language features.

---

## The model (settled)

There are **two orthogonal axes**, and conflating them was the old design's flaw:

| | A **tier** | A **profile** (your "target") |
|---|---|---|
| Is a | *kind of co-located content* | *build configuration* |
| Property of | the **source** | the **build invocation** |
| Examples | `test`, `bench`, `doc`, `debug` | `dev`, `debug`, `production` |
| Declared by | a `@tier` directive (in a library/the prelude) | the package manifest (later) |
| Carries | a **name + content-kind** | a **provider-map of which tiers are live** + codegen knobs |

A tier says *what* a block is. A profile says *which* tiers are live in this build and *whose*
implementation provides each. This is the Cargo-profile / MSBuild-configuration model: a profile
selects conditional-compilation content + codegen; tiers are the conditional content it selects.

### Tier = `(name, content-kind)` — nothing more

Because the profile owns inclusion, the `@tier` primitive **drops the "inclusion-rule"** the README
sketched. A tier is just a name and a **content-kind**, which tells the compiler how to read a
block's body and what "active" *does*:

| content-kind | block body is | when the tier is **active** | when **inactive** |
|---|---|---|---|
| **code** | parsed + typechecked items (`fn`s) — or statements in statement position | lowered + compiled in as extra roots | **filtered out before lowering** (never reaches IR/bytecode) |
| **text** | captured **verbatim** (markdown/prose), not parsed | surfaced in the manifest for its extraction tool (doc generator); **never in the runtime binary** | not surfaced |

Built-ins: `test` (code), `bench` (code), `doc` (text), `debug` (code, used in *statement*
position as inline conditional code). Stripping is **not a DCE pass** (the codebase has none) — an
inactive tier's blocks are simply not lowered, so they cost nothing by construction. (Real DCE, for
*unused reachable* code, stays a separate future concern.)

### Profile = a provider-map, selecting + activating + resolving every tier

Inclusion is a **map, not a list** (settled 2026-06-26): the key is the local `@<tier>` name written
in source, the value is the **provider package** (or a table for per-tier options). This single map
does what the README split across an opt-in map *and* an inclusion list:

```toml
# (later — lands with the package manifest, see Sequencing)
[profiles.dev.tiers]
test  = "std"                              # provider = stdlib's @test
bench = "criterion-lang"                   # picks one provider among those offering @bench
doc   = "std"
debug = "std"
microbench = { package = "other-pkg" }     # alias: a 2nd provider under a distinct directive name

[profiles.production.tiers]
doc = "std"                                # only doc surfaced; all code tiers stripped
```

The map value buys, all at once:
- **Activation** — a tier in the map is live for that profile; absent ⇒ stripped/not-surfaced. (So
  test/doc are *not forced* — a minimalist profile opts into nothing.)
- **Provenance** — you know exactly whose `@tier` declaration (content-kind, block API, runner)
  governs each tier.
- **Conflict resolution by construction** — opting a tier in *requires* naming its provider, so the
  README's "ambiguous `@bench`, provided by X and Y" **error case cannot arise**; the choice is
  always explicit. Stronger than erroring after the fact.
- **Aliasing** — key = the `@<name>` you write, value = provider, so two providers can be used at
  once under distinct keys.
- **Room for options** — a bare string `bench = "criterion-lang"` is shorthand for
  `bench = { package = "criterion-lang" }`; the table form carries profile-level defaults
  (`{ package = "criterion-lang", samples = 100 }`), the home for the README's `@bench(samples:100)`.

**Text tiers go through the same map** (corrected 2026-06-26): `doc` is *not* "always on." A user must
be able to choose whether docs are active and **which package's `@doc` directive/generator** provides
them — so `doc` is selected in the provider-map exactly like a code tier. content-kind only changes
what activation *does* (surface-for-extraction vs compile-in), never whether the profile governs it.

**Upstream of the map:** a package must be in `[dependencies]` for its provider to be *selectable*.
So: dependencies declare what's **available** → the profile's tier-map picks **which available
provider** per tier and **activates** it. Availability is global; selection+activation is per-profile
(which is what lets a profile swap providers — constrained to API-compatible ones, since source
`@test { }` blocks are written against one provider's block API). Repetition across profiles
(`test = "std"` in dev *and* ci) is solved by a **base/default profile** the others inherit-and-
override — keeping the "one place to see what's live in this build" property.

### Deliberately **not** in a profile (settled)

- **Runtime env vars / secrets / deployment config.** Hard line: *compile-time constants* baked into
  the artifact may eventually be profile-scoped, but *runtime environment* (DB URLs, secrets) is a
  deployment concern that must not live in a committed build manifest. **Env is deferred entirely**
  from the first cut — it's the seam where build systems turn to mud; it gets its own deliberate
  design, and the safe shape when it lands is `API_BASE = env("API_BASE")` (pull at build), never
  hardcoded values.
- **Platform/arch triple.** That is the *other*, conventional meaning of "target"
  (`wasm32`/`x86_64`), an independent axis (you build the `dev` profile *for* wasm). → drives the
  rename below.

### Naming (settled in discussion)

- **`profile`, not `target`**, for the build configuration — `target` is reserved for the platform
  triple to avoid the collision the moment cross-compilation appears.
- The **`debug` tier** (the `@debug { }` inline-code blocks) vs a **`debug` profile** (which includes
  the debug tier + unoptimized codegen + checks) share a word but mean different axes; keep the
  profile carrying more than the one tier so the overlap reads as intentional, or pick distinct
  names at manifest-design time.

---

## What lands in **slice 6** vs later

The package manifest does not exist yet (the language has `use std.{…}` but no project-manifest
file). So the **profiles/TOML provider-map surface is designed here but implemented with the package
system** (it was always "forward-looking, not a blocker"). The key architectural lever:

> **A profile resolves to nothing the compiler can't get from a plain `{tier → (content-kind,
> provider)}` set.** The front-end's tier filter + the manifest-discovery surface consume that
> *resolved active-tier set*; they don't care whether it came from a TOML profile, a `--profile`
> flag, or a hardcoded default.

So **slice 6 ships the in-language primitive against an active-tier-set interface**, with the CLI
command supplying the set until profiles land:

**In slice 6:**
1. `@tier` **declaration** primitive — `(name, content-kind)`; built-in `test`/`doc` declared in the
   prelude (and `bench`/`debug` as they land). Validation: a block against an undeclared/inactive
   tier is a compile error ("unknown tier `tset`" — a typo doesn't silently vanish).
2. **Block parsing** — `@<tier> { … }` standalone, in **declaration position** (top-level) and
   **statement position** (the `@debug { stmts }` case); the directive grammar extended from
   *annotates-a-declaration* to *carries-a-body*. Optional args (`@test(skip)`,
   `@bench(samples: 100)`).
3. **Annotation parsing** — `@<tier> fn …` (a code tier on a single declaration), the base form the
   block is grouping sugar for. Text tiers (`@doc`) stay block-only.
4. **The tier filter** — inactive code-tier blocks dropped before lowering; active ones lowered as
   extra roots. The **active-tier-set is the input interface** (a `Set<TierName>`), supplied by the
   CLI command (see open question on the runner).
5. **Manifest discovery** — active tiers' blocks enumerated in the shared reflection manifest (the
   same artifact behind `attributes_of`/`roles_of`), so a runner finds them exactly as it enumerates
   attributes.
6. **`test` end-to-end** — the test runner: discover `@test` `fn`s via the manifest, run each, an
   assertion failure/panic = a failed test, report pass/fail. **Same-namespace private access** (an
   in-source `@test {}` sees the module's privates; a separate test-tier *file* sees only `pub`).

**Deferred to the package-system milestone (designed here):**
- The **profiles/TOML provider-map** surface that *produces* the active-tier set + provider
  resolution; profile inheritance; `lang init` pre-filling built-ins.
- `bench` (measure), `doc` (text extraction) runners, and **third-party tiers** — all *declarations*
  + manifest-reading runners, not new language features.
- `@debug { }` inline-statement form (a code tier in statement position) — small, can fold into
  slice 6 or follow; flagged as its own step.
- **Env / compile-time constants** in profiles (deferred entirely, see above).

---

## Surface grammar (slice 6)

```
// Declaration: a library/the prelude declares a tier (name + content-kind).
@tier test : code
@tier doc  : text

// Use — block form (grouping), declaration position:
@test {
    fn adds() { assert(add(1, 2) == 3); }
    fn subs() { assert(sub(3, 1) == 2); }
}

// Use — annotation form (code tiers), the base form:
@test fn multiplies() { assert(mul(2, 3) == 6); }

// Use — block with per-block args:
@test(skip) { fn flaky() { … } }
@bench(samples: 100) { fn hot_path() { … } }

// Inline conditional code (debug tier, statement position) — own step:
@debug { echo "x = ${x}"; }
```

Disambiguation (extends the existing `@name`/`@name(args)` decorator parser, `lang-parser`
~L2253): after `@name` (+ optional `(args)`), a **`{`** opens a *block* (new), a **declaration
keyword** is the existing *annotation*. The block body is a sequence of declarations in declaration
position, statements in statement position; a `text` tier's body is captured verbatim (lexer/parser
support for a raw-text span — the one genuinely new lexing concern).

---

## Implementation sketch (per crate, `test` path)

- **lang-lexer** — no new keyword (`@` + ident is the directive); a **raw-text block** mode for
  `text` tiers (deferred with `doc`). Slice-6 `test` needs none.
- **lang-ast** — a `TierBlock { tier, args, items, span }` (declaration + statement position) and a
  `tier: Option<…>` annotation slot on `FnDecl` (or a wrapping decl). A `@tier` *declaration* node.
- **lang-parser** — extend the decorator grammar to the block form (lookahead on `{`); the
  annotation form reuses the existing leading-decorator path (P2.4 lifted attribute clusters above
  `fn_decl`, the same hook).
- **lang-check** — register declared tiers (prelude built-ins + any `@tier`); validate every
  `@<tier>` use against the active+declared set (new diag, **E0036** — the next free code); typecheck
  active code-tier `fn`s like ordinary fns, with **same-namespace visibility** (the block sees module
  privates — the `current_type`/privacy machinery already distinguishes in-module). A `@test` `fn`
  is checked but is **not** a callable from non-tier code (it's a root, not a symbol).
- **front-end tier filter** — *before* `lang_ir::lower`, drop inactive code-tier blocks; keep active
  ones as additional top-level items. Pure AST→AST filter parameterized by the active set. (No IR/VM
  change — this is why "no DCE pass" is fine.)
- **lang-ast `reflect`** — surface active tiers' blocks in `ReflectionInfo` (a `tiers` index keyed
  like attributes), so both backends + the runner discover them identically.
- **runner (lang-cli)** — a command that compiles with `test` active, enumerates `@test` `fn`s from
  the manifest, invokes each (reusing the call machinery), treats a panic/failed `assert` as a
  failure, prints a report. Needs an **assertion primitive** — verify `assert`/`panic` exist in the
  prelude (the README example assumes `assert`); add if missing (small).

---

## Sub-slicing (each a green gated commit)

- **6a — `@test` block parsing + the strip mechanism. ✅ DONE (`5fc419a`).** `Stmt::TierBlock`; the
  directive grammar's standalone block form (tried before the `@derive` decorator path — backtracks
  cleanly, locked by a snapshot); an inactive block lowers to nothing so both backends strip
  identically (no DCE pass needed — inactive content never reaches the IR); the checker validates the
  tier name against built-ins `{test,bench,doc,debug}` → **E0036 UnknownTier**. The active-set +
  inlining of *active* blocks is deferred to 6b (for 6a every block is inactive). Conformance 286 /
  differential agrees / leak 0. **(Prereq landed first: `f8e6d87` split the conformance harness into
  the dev-only `lang-conformance` binary, freeing the `lang test` verb.)**
- **6b — the test runner.** Discover + run `@test` fns, assertions, pass/fail report; the assertion
  primitive.
- **6c — annotation form `@test fn`.** Grouping-sugar equivalence with the block.
- **6d — same-namespace private access** (in-source vs separate-file visibility).
- **Later:** `@debug` statement form; `bench`; `doc` (text content-kind + verbatim capture +
  extraction); the profiles/TOML provider-map + manifest; third-party tiers.

---

## Verification (every gate green, per commit)

- `cargo build --workspace && cargo test --workspace`; conformance + **differential** (both backends
  agree, 0-skipped — the tier filter runs *before* lowering, so both backends see the same filtered
  program by construction); **leak oracle** residency 0 both backends; clippy + fmt; miri on
  lang-value if any `unsafe` path is touched (none expected — this is front-end + reflection).
- A `tests/conformance/tiers/` case proving: a `@test {}` block runs nothing under a normal run (its
  side effects absent), the program's own output is unchanged (tier stripped), and — once 6b lands —
  the test command discovers + runs it.

---

## Open questions to settle before/during implementation

1. **The user-program test-runner command — SETTLED (2026-06-27).** `lang test`/`--differential`/
   `--check-leaks` today drive the *conformance corpus* (an internal tool that tests **the
   implementation** — two backends agree, no leaks). That has no place in the shipped runtime CLI, so
   it **moves out of `lang-cli` into a dev-only binary** (the conformance *logic* already lives in the
   `lang-conformance` crate as a lib; only the subcommand wiring is in `lang-cli/src/main.rs` — give
   `lang-conformance` its own `[[bin]]` / drive via `cargo test` / an xtask). That frees the verbs:
   **`lang test <FILE>` / `lang bench` become the user-facing tier runners**, `lang doc` the text
   extractor. (Independent cleanup; can land before or with 6a.)
2. **`@tier` declaration syntax.** `@tier name : code` vs `@tier(code) name` vs a prelude-only
   built-in form for slice 6 (defer the user-facing `@tier` decl to the package milestone, since a
   third-party tier can't be *activated* without the manifest anyway). **Recommend:** built-in tiers
   hardcoded in the prelude for slice 6; user `@tier` declarations land with profiles.
3. **`debug`/`bench`/`doc` scope in slice 6** — confirm `test`-only first (per the plan), with the
   others explicitly following.
4. **Labeled blocks** (`@test "arithmetic" { … }`, README "block ergonomics") — in or deferred.
