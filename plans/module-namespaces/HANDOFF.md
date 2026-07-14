# Handoff: navigable module namespaces + fix the check/run divergence

## Goal (decided)

Make `use std.http` bind **`http` as a single navigable namespace handle**, so
`http.client.get(...)` and `http.server.serve(...)` both work. Generalize the
mechanism to **all extension roots**, not just `http` — any root whose modules
share a dotted prefix becomes a navigable group. As a direct consequence, close
a soundness bug: `noeta check` currently accepts references that `noeta run`
rejects.

Two settled design points (from the user):
1. `use std.http` binds **only** `http` (a single handle you dot into). It does
   NOT also inject `client`/`server` as bare names into scope.
2. **Generalize to all roots** — it's the same machinery; `http` is just the only
   current instance of a multi-submodule namespace.

## The two problems

### Problem A — no group namespace (the ergonomics gap)
Registry modules are **flat leaves**. `http.client` and `http.server` are two
*whole* module names; `http` is only a shared string prefix. The split exists
purely for **DCE / ring separation** (reqwest+TLS ride on `http.client` behind
`ring-http-client`; `http.server` has `ring: None`), so a server-only program
sheds reqwest. That internal concern leaked into import syntax:
- `use std.http.client` binds the **leaf** `client` → you must write the
  context-free `client.get(...)` (see `tests/conformance/std/http_sync.noe`).
- There is no `http` namespace node, so `http.client.get(...)` is unwritable.

### Problem B — check/run divergence (the soundness bug)
`use std.{http}` finds no module `http`. Because the *same* `use` syntax also
imports user types from sibling modules (`use App.Models.User`), an unresolved
name silently falls through to an **opaque type binding**
(`crates/noeta-compiler/src/lib.rs:1024` — `self.types.insert(name, TypeInfo::Opaque)`).
So a typo'd std module is indistinguishable from a legit cross-module type
import: `check` passes, only `run` errors.

Reproduction (`badhttp.noe`):
```noeta
use std.{http}
r = http.get("https://svc.test/echo")
echo "status"
```
- `noeta check badhttp.noe` → exit **0**, "0 errors"
- `noeta run   badhttp.noe` → exit **1**, `[E0005] cannot find `http` in this scope` at line 2
- `noeta build --native badhttp.noe` → writes a binary that is **dead on arrival**
  (same E0005 at runtime)

The chosen fix (group namespaces + checker strictness) **subsumes** this bug:
`use std.http` becomes valid, and `http.get` (no `get` member on the group) plus
`use std.bogus` become clean **compile-time** errors.

## How resolution works today (code map)

- **Module lookup is flat:** `crates/noeta-native/src/registry.rs:825` `find_module`
  splits off the root on the first `.`, then matches the *entire* remaining
  string against `ExtModule.name`. `find_module("std.http")` → `None`;
  `find_module("std.http.client")` → the client module.
- **Module identities / rings:** `crates/noeta-stdlib/src/registry.rs`
  - `http.client` at `:3158`, `ring: Some("ring-http-client")` at `:3167`
  - `http.server` at `:3174`, `ring: None` at `:3182`
  - other rings: `datetime` → `ring-datetime` (`datetime.rs:611`); p2p modules →
    `ring-p2p` (`registry.rs:3232`, `:3242`).
- **`use` resolution in the compiler:** `crates/noeta-compiler/src/lib.rs`
  - `qualified_module(path, name)` `:97` = `path.join(".") + "." + name`
  - `is_native_module(...)` `:104` — true iff root is an extension root AND
    `find_module(qualified)` is Some
  - `selective_import_module(...)` `:115` — a member import like `use std.math.sqrt`
  - `Stmt::Use` handling `:1015-1026` — the opaque fallthrough at `:1024`
  - module value const: `Const::NativeModule(qualified_module(...))` emitted at `:1963`
  - member-fn call const: `Const::ModuleFn { module, func }` at `:1977`, lowered at
    `Rvalue::ModuleFn` `:3376`
  - E0005 is a **runtime** miss for an unknown name in `main`
    (`:1743`, `:1975` note the fall-through-to-runtime path)
- **Path-qualified module find (already exists, useful):**
  `registry.rs:853` `find_module_qualified(&["std","http","client"])`;
  `registry.rs:848` `is_extension_root(root)`.
- **Checker:** `crates/noeta-check/src/stdlib.rs:35` `is_stdlib_module` (via
  `reg.find_module`). The checker mirrors the compiler's opaque leniency — this is
  where the strictness must be added.
- **Extension TYPES** (must remain reachable through a group too, e.g.
  `use std.http.Response`, `use std.id.Uuid`): `ExtType` carries a `namespace`
  field (`registry.rs:488`) and `qualified()` = `namespace.name` (`:550`).

## CRITICAL constraint — do not break ring DCE

`--native` per-program ring stripping is driven by `aot_ring_features`
(`crates/noeta-cli/src/lib.rs:3501`), which walks the compiled bytecode's
`Const::NativeModule(name)` / `Const::ModuleFn { module, func }` and maps each to a
ring via `noeta_stdlib::registry::ring_of` (keyed on the **concrete** module
identity, e.g. `std.http.client`). Unit test at `lib.rs:5509`
(`aot_ring_features_selects_http_client_but_not_server`).

**Therefore:** the new `http` namespace must be a *compile-time resolution
convenience only*. `http.client.get(...)` MUST still lower to a constant carrying
the concrete leaf identity `std.http.client` (NOT `std.http`). If the bytecode
ever records `std.http`, `ring_of("std.http")` → `None` → reqwest is dropped and
the binary breaks. The group handle resolves to the leaf submodule at the call
site; the emitted `Const` is unchanged from today.

Verified current DCE numbers (must remain unchanged after this work):
| program | rings | `--native` size |
|---|---|---|
| core-only | none | ~11.1 MB |
| `client.get` | ring-http-client | ~17.2 MB |
| `p2p.identity` | ring-p2p | ~43.4 MB |

## Design / algorithm

Resolution for `use root.a.b … z` (last segment = the bound name; braced
`use root.{x}` binds `x`), with `P` = full dotted qualified path:

1. `find_module(P)` is Some → **concrete module** (existing behavior). Bind a
   module value (`Const::NativeModule(P)`).
2. Else if `P` is a strict **namespace prefix** of ≥1 registered module (some
   module name equals `P + "." + <rest>`) → bind a **namespace group** value.
3. Else if `P` names a member function (selective import) → existing behavior.
4. Else if `P` names an extension **type** → existing type import.
5. Else:
   - If `root` **is an extension root** (`is_extension_root`) → **compile error
     E0005** (this is the strictness that closes the divergence — a known,
     fully-enumerable root cannot have unknown members).
   - If `root` is **not** an extension root → keep the opaque fallthrough (user
     packages are resolved later by the linker; must not regress
     `use App.Models.User`).

Member access on a namespace value (`http.client.get`):
- `http` → namespace value.
- `.client` → resolve `http.client`; it's a module → module value
  (`Const::NativeModule("std.http.client")`).
- `.get` → existing module-function dispatch (`call_native_module`).
- Also support `http.Response` → the extension type whose qualified identity is
  `std.http.Response`.

Backward compatibility (hard requirement): `use std.http.client` binding the leaf
`client`, then `client.get(...)`, MUST still work — all conformance tests and docs
use this form.

## Suggested slicing (commit per green slice; worktree; do not push)

1. **Resolver + compiler:** namespace-group binding + chained member access
   (`http` → `.client` → `.get`), emitting the concrete leaf `Const`. Green test:
   `use std.http` then `http.client.get(...)` runs on BOTH backends and produces
   the same output as `use std.http.client` + `client.get(...)`. Confirm the
   `aot_ring_features` unit test + a `--native` size check still hold.
2. **Checker strictness:** under an extension root, unknown module/member/type →
   `E0005` at check time. Regression test that `check` and `run` **agree** on
   `badhttp.noe` (both non-zero) and that `http.get` (bad member on a valid group)
   is a compile error, while `http.client.get` checks clean. Ensure
   `use App.Models.User` (non-extension root) still passes.
3. **Tooling:** LSP/hover/completion + fmt learn the namespace level (completion
   after `http.` offers submodules `client`/`server` and any group-level types;
   hover on `http` describes the group). fmt must round-trip both import forms.
4. **Docs + conformance:** add group-form examples; keep leaf-form examples
   working. Update `docs/` where `std.http.client` usage is shown.

## Guardrails / standing constraints

- Work in an **isolated git worktree** under `.claude/worktrees/`; never the shared
  root. **Commit per green slice. Do NOT push without authorization.**
- Prefer the architecturally sound fix over a band-aid (this whole arc is that
  choice over a check-only strictness patch).
- Do NOT narrow/defer scope without explicit confirmation from the user.
- Keep the **differential oracle** green (VM vs AOT parity) throughout.
- Re-verify the three `--native` sizes above are unchanged (ring DCE intact) and
  that a `--exe`/`--native` build of a grouped-import program runs correctly.

## Quick repro assets

Programs used during investigation (recreate as needed):
```noeta
// core.noe — no rings
mut total = 0
for i in 0..1000 { total = total + i }
echo "sum ${total}"

// good today: use std.http.client ; client.get(...)
use std.http.client
r = client.get("https://svc.test/echo")
echo "status ${r.status()}"

// TARGET after this work: use std.http ; http.client.get(...)
```

## Decisions (confirmed by user, 2026-07-14)

1. **Close BOTH opaque holes.** Hole A (extension-root leniency in
   compiler/checker, `noeta-compiler/src/lib.rs:1024`) AND Hole B (linker
   `Resolution::NoModule` swallow, `noeta-loader/src/lib.rs:591`). Hole B is
   gated so an isolated single-file `noeta check` stays lenient (a sibling
   `use App.Other` is only an error when the link is *complete* / whole-project);
   in a complete link an unresolvable user module is a real diagnostic.
2. **Error code = E0019 `UnresolvedImport`** for `use`-statement resolution
   failures (both extension-root and user-module). E0005 `UnknownName` remains
   the bare-name-in-expression miss. `check` and `run`/`link` both emit E0019 for
   a bad `use`.
3. **Did-you-mean covers the linker's full module pool** — stdlib/extension
   modules, group submodules, member functions, AND user/package module names
   gathered from the linked pool. Genuinely helpful, not stdlib-only.
