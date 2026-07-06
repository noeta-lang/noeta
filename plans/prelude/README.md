# Prelude-redesign arc — shrink the always-global prelude, fix the builtin-shadowing divergence

**Status: ARC COMPLETE — every slice, including the EX closing track.** EX.1 `f0ea860` (member
access fully explicit: `self.field` only, bare names never fields, targeted E0005 hint; 72 corpus
files migrated by a span-driven tool; generic `self.value` T-erasure fixed), EX.2a `ab4bb33`
(associated-vs-instance DERIVED from the body via new `Stmt::mentions` — zero runtime cost; E0047
wrong-way calls both directions; associated handles `ctor = Stack.new`; struct methods finally
registered in the checker method table), EX.2b `55815e3` + `7e2e0bd` (BOUND handles `f = c.bump` —
receiver captured, reference-semantics pinned, GC-traversed child, ships over Wire; binding an
associated fn through a value = static E0047). The handle surface is complete: unbound instance,
unbound associated, bound, built-in.
Branch `worktree-prelude-redesign` (worktree off main). Commits: P0 `88494b7` (selective import),
P1.1 `f5e5663` (eager methods + len), MH.1 `2181ad0` (user-type handles), MH.2 `890b3c4` (dispatch
refactor, perf-gated) + `28e0cdd` (built-in handles), P1.2 `337b277` (free fns removed), P2a
`691cccf` (std.reactive + virtual modules), P2b `4692b62` (std.task — named `task` not `async`:
keyword; + import-bound globals now ship into real isolates via new Wire variants), P2c `0aedcd4`
(std.id + bytecode Builtin::NextId), P3 `ebc5b5f` (**E0046 ReservedName — the divergence is
closed**; bare `none` in match-pattern position stays legal, it is the Option constructor pattern),
P1.3 `f78fb66` (collection `count`→`len` complete; iterators keep `count`). The deferred.md row is
struck. Method handles: `plans/prelude/method-handles.md`.

## Why this exists

A user binding that collides with a prelude builtin name **diverges between the two backends**:

```noeta
sum = 5          // tree-walker: E0006 (sum is an immutable prelude binding)
                 // VM:          runs fine (local shadows the builtin namespace)
```

The tree-walker declares all 17 prelude builtins as immutable global bindings
(`noeta-eval/src/lib.rs:1153`), so `sum = …` is a reassignment of an immutable → **E0006**.
The VM resolves builtins as a separate namespace checked *after* locals/globals
(`noeta-compiler/src/lib.rs:1170`), so a user binding shadows it and runs. The static checker
catches neither. It's **latent** (no corpus program binds a prelude name, so the differential's
`0 skipped` / agreement gate still holds) but it breaks the "both backends agree by construction"
invariant the whole differential rests on.

**Root-cause fix strategy (decided with user):** the reason common names collide at all is that the
always-global prelude is too big (17 names, incl. `len`/`map`/`sum`/`filter`). *Shrink the prelude*
so almost nothing common is a reserved global, **then reject binding the small remainder statically**
so both backends agree before runtime. Reject-shadowing (not allow) — consistent with the
immutable-rebind rule (E0006) and the inferred-static philosophy.

## Settled design

**Final home for each of the 17 prelude names:**

| Name(s) | Today | New home |
|---|---|---|
| `Ok`, `Err`, `some`, `panic`, `assert` | prelude | **stay always-global prelude** (5 names, un-shadowable) |
| `len` | free fn | **`.len()`** method on collections (rename existing collection `.count()`→`.len()`; iterators keep `.count()`) — D2 SETTLED |
| `map`, `filter`, `sum` | free fn (+ lazy iter method) | **eager list methods** `xs.map(f)`/`.filter(f)`/`.sum()` — D1 SETTLED |
| `signal`, `computed`, `effect` | prelude | **`use std.reactive.{…}`** |
| `sleep`, `all`, `race`, `map_bounded` | prelude | **`use std.task.{…}`** (planned as `std.async`; renamed at P2b — `async` is a keyword) |
| `next_id` | prelude | **`use std.id.{next_id}`** |

**Import model — SELECTIVE BARE IMPORT, generalized to ALL std modules.** The parser already
produces the right AST by path depth (no grammar change):

| Syntax | `path` / `names` | Meaning |
|---|---|---|
| `use std.math` ≡ `use std.{math}` | `["std"]` / `[math]` | import module → qualified `math.sqrt(x)` |
| `use std.math.sqrt` ≡ `use std.math.{sqrt}` | `["std","math"]` / `[sqrt]` | selective → bare `sqrt(x)` |
| `use std.math.{sqrt, cos}` | `["std","math"]` / `[sqrt,cos]` | selective, multiple |
| `use std.{math, json}` | `["std"]` / `[math,json]` | multiple modules |

Braces are **never required for a single name** — purely a grouping delimiter for multiples, at any
depth. Names under `std.` are modules; names under `std.<module>.` are members.

**Execution stays where it is** — gating is name-resolution only. `signal`/`sleep`/etc. touch
`self.reactive` / `self.executor`, which the stdlib registry seam deliberately cannot reach
(`ModuleDispatch` only gets `&mut dyn Host`), so they keep their existing inline `Builtin` execution;
we only stop binding their names into scope unless the module is imported. (This mirrors why
`fs.*_async` already bypasses the registry.) So `std.reactive`/`std.task`/`std.id` are
**non-registry importable modules** whose member names alias existing `Builtin`s.

## Micro-decisions (SETTLED with user)

- **D1 — map/filter/sum → eager list methods.** Add `xs.map(f)`/`xs.filter(f)`/`xs.sum()` directly on
  lists, returning `List`/value, reusing the `Builtin::Map/Filter/Sum` impls. 1:1 migration
  (`map(xs,f)`→`xs.map(f)`). Flips the existing negative test that asserts bare-list `.map` errors.
- **D2 — `len` on collections, `count` on iterators.** One name per operation, no synonyms.
  **Collections** (list/set/map/string): rename the existing `.count()` → **`.len()`** (O(1) size);
  migrate free `len(xs)` → `xs.len()`. **Lazy iterators** keep `.count()` — it's a *consuming*
  terminal (walks the whole iterator), semantically distinct from an O(1) length, so the split is
  honest (Rust's exact rationale). Migration must distinguish receiver kind: collection `.count()` →
  `.len()`, but `xs.iter()....count()` stays.

## Slices

Each slice is one green commit (differential 0-skipped + leak 0 + conformance). Ordering respects
dependencies: enabling feature → move names out → reserve the remainder.

- **P0 — Selective bare import (enabling feature, additive).** Implement `use std.<mod>.<name>`
  binding each name as a bare alias to `<mod>`'s function, generalized to all std modules, across
  checker + both backends. Purely additive: existing `use std.{math}` (qualified) still works;
  now `use std.math.sqrt; sqrt(2.0)` also works. No prelude change yet. Recognize `reactive`/`async`/
  `id` as importable (non-registry) module names so `use std.reactive` populates `self.modules`.
- **P1 — Collection methods (D1/D2).** Add eager list methods `map`/`filter`/`sum` (and resolve
  `len`→`count` per D2) on list (and where sensible set/map/string). Reuse existing free-fn impls.
  Migrate all corpus `map(xs,f)`/`filter`/`sum`/`len(xs)` call sites to method form. Remove
  `Len`/`Map`/`Filter`/`Sum` from the always-on prelude (`Builtin::PRELUDE` + `PRELUDE_NAMES`).
  Flip the `xs.map(f)`-errors negative test.
- **P2a — `std.reactive`.** Register `reactive` as an importable module exporting
  `signal`/`computed`/`effect`. Migrate corpus (add `use std.reactive.{…}`). Remove the three from
  the always-on prelude. Gate their resolution behind import in all three stages.
- **P2b — `std.task`.** Same for `sleep`/`all`/`race`/`map_bounded`. (Planned as `std.async`; renamed — `async` is a keyword, `use std.async.…` does not parse.)
- **P2c — `std.id`.** Same for `next_id`.
- **P3 — Reject-shadowing (closes the divergence).** Prelude is now `Ok`/`Err`/`some`/`panic`/
  `assert` (5). Add **E0046** in `noeta-check`: a binding whose name collides with a remaining
  prelude name is a static error (message clearer than E0006's "immutable"). Both backends reject
  before runtime → divergence closed by construction. Add a guard test binding each of the 5.
- **P4 — Docs + deferred.md.** Strike the divergence row in `plans/deferred.md`; update language/
  stdlib docs (prelude list, import syntax); record the arc outcome.

## Closing track — explicit member access + derive associated/instance (added with user)

Discovered mid-arc (during method handles) that inside a method a bare field **read** still resolves
live off `self` while a bare **write** declares a local — an intended-but-surprising asymmetry from
the 2026-07 object-model decision. **User's actual intent: BOTH reads and writes explicit** — a bare
name inside a method is never a field; `self.field` is required for all member access. This closes
the read/write footgun and, paired with the second slice, makes associated-vs-instance derivable and
unambiguous (which retires MH's instance-only limitation — enabling associated + bound handles).

- **EX.1 — explicit member reads.** Remove the bare-read live-off-`self` fallback: a bare identifier
  in a method body resolves to a local or is E0005 (never a field). `self.field` required for reads
  too (writes already require `self.f = v`). Both backends + checker. Corpus migration: every bare
  field read in a method (`return len(items)`, `items |> map(...)`, `self.words`-mixed files, …) →
  `self.field`. Update the object-model README's "bare field read resolves live off self" note.
- **EX.2 — derive associated/instance + wrong-way is a static error + upgrade handles.** With member
  access fully explicit, a method that references `self` is an **instance** method; one that never
  does is **associated**. Derive this at check time (compile-time only — **zero runtime cost**, per
  user). Then: calling an instance method associated-style (or vice-versa) becomes a **static error**
  (a new diag) instead of today's runtime "no field `n` on unit"; and `Type.method` handles gain the
  unambiguous associated/instance distinction — **upgrade MH to support associated + bound handles**,
  retiring the MH.1 instance-only interim (the `associated` flag becomes real). Migrate
  `builtin_as_value.noe`'s associated cases if wanted.

This is an object-model change (a completed arc) done here because it is now the fastest path and it
completes method handles. Differential-green + leak-0 per slice.

## Follow-on tracks (NOT this arc — captured as backlog)

- **UUID** in `std.id` — needs the deterministic Host seam (like `random`/`time`) since v4=random /
  v7=time-based would break the differential. `next_id` stays the deterministic counter.
- **`std.crypto`** — hashing (SHA-256 etc. = deterministic, the easy part) → bcrypt/password hashing
  (needs the randomness seam for salts + real security-correctness weight). Its own milestone.

## Key files (from the seam map)

- `noeta-builtins/src/lib.rs:43` — `PRELUDE_NAMES` (split into always-on vs moved subsets)
- `noeta-eval/src/lib.rs:1153` (unconditional prelude bind) + `:1535` (`declare_use`) + `:2624`
  (`call_native_module`) + `:2252` (method dispatch) + `:3482-3542` (Len/Map/Filter/Sum arms)
- `noeta-compiler/src/lib.rs:1170` (`resolve`→`Resolved::Prelude`) + `:1353` (`decl`, track imports)
  + `:2759` (`lower_call`)
- `noeta-check/src/lib.rs:1181` (populate `self.modules`) + `:3739`/`:3755` (`synth_call` prelude/
  module resolution) + `noeta-check/src/stdlib.rs:50` (`is_std_module`) + `:191`/`:287` (method
  tables) + `:503` (`prelude_return`)
- `noeta-vm/src/lib.rs:5526` (`run_builtin`) + `:5849`-`6091` (async/reactive arms) + `methods.rs:328`
  (`call_native_module`)
- `noeta-bytecode/src/lib.rs:105` (`Builtin::from_name`)
