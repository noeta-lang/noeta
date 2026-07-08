# Phase 0 — Namespaced module system ("root module resolution")

*Parent: [`README.md`](README.md). No network, no ABI freeze — language + checker + compiler + eval +
registry. Foundation for cross-package resolution (Phase 2) and precise DCE. Design source:
`plans/aot/dce.md` carry-over §1 (user-decided 2026-07-08).*

## The problem

Module identity is a **bare name** assumed to live under `std`:

- `noeta-stdlib/registry.rs` — `find_module(name: &str)` matches `m.name == name` flat across all
  extensions; the `Extension` trait owns only `fn name()`, no namespace **root**.
- Hard-coded `std` root in **five** places:
  - `noeta-compiler/lib.rs:92` `is_native_module` → `path == ["std"]`
  - `noeta-compiler/lib.rs:100` `selective_import_module` → `path[0] == "std"`
  - `noeta-compiler/lib.rs:1809` `Const::NativeModule(imported.name)` — **bare name**
  - `noeta-check/lib.rs:1551` `is_std = path == ["std"]`; `self.modules: HashSet<String>` (insert
    1559, `contains` 4531)
  - `noeta-eval/lib.rs:1503` `is_std = path == ["std"]`; `Value::NativeModule(imported.name)` (1510),
    dispatch on bare name (1995)

Consequences: a third-party `guzzle.http` would **collide** with `std.http` (both look up as `http`);
`use std.http.client` **fails** (`E0005: module 'http' has no function 'client'` — the last segment is
read as a function); whole-module `use std.{http}` keeps DCE conservative (client & server in one
module).

## Slices

Each commits green (`cargo test --workspace`, differential + conformance, fmt/clippy). Only `std` is
registered throughout Phase 0, so slices 0.1–0.3 are **faithful refactors** — differential holds by
construction; 0.4 migrates surface + adds conformance.

### 0.1 — `Extension` owns a namespace root; registry resolves by qualified path

- Add `fn root(&self) -> &'static str` to the `Extension` trait (`noeta-native/registry.rs`);
  `StdExtension::root()` returns `"std"`.
- A module's **qualified path** = `<root>.<module.name>` (later: nested, `std.http.client`). Add
  `find_module_qualified(path: &[str])` (root-aware) alongside the existing bare `find_module`; keep
  bare `find_module` as a thin shim during migration.
- Internal only — no language-surface change, no NativeModule shape change yet. Differential
  byte-identical.

### 0.2 — Generalize the hard-coded `std` recognition off checker / compiler / eval ✅ DONE

- Replace every `path == ["std"]` / `path[0] == "std"` module-import recognition with **"`path[0]`
  is a registered extension root"** (`registry::is_extension_root`). Six sites: compiler
  `is_native_module` + `selective_import_module`; checker `is_std` + `selective`; eval `is_std` +
  `selective_module`.
- The stored module payload stays the **bare name** here — changing it to the qualified path couples
  tightly to nested resolution (a nested module's value must remember *which* module it is), so that
  moves to 0.3 with the split that first produces nested modules. Bare-name payloads are unambiguous
  while only `std` is registered.
- Faithful refactor: only `std` registered ⇒ every program resolves identically. Differential green.

### 0.3 — Qualified module payload + nested-path binding + the `std.http` split

*Landed together: the split produces the first real nested modules (`std.http.client`/`.server`);
nested resolution is what consumes them — neither is testable alone.*

- **Qualified payload + dispatch.** `Const::NativeModule` / `Value::NativeModule` (and the `Wire`
  twin, `isolate.rs`) carry the module's qualified path; `call_native_module` and the router
  (`find_function`/`dispatch`, VM `methods.rs:385`) resolve it via `find_module_qualified`. Checker
  `self.modules: HashSet<String>` → a **`bound → full-path` map**.
- **Nested-path resolution, binding the last segment.** `use std.http.client` joins the segments
  after the root, looks up the nested module `std.http.client`, and binds the **last segment**
  (`client`) as the local module value — calls read `client.get(...)`. Distinguish from the selective
  *function* import (`use std.math.sqrt`) by whether the tail names a module vs a function in the
  registry. Rejected (per dce.md): qualified call-site form `http.client.get(...)` — needs namespace
  *values*; last-segment binding is the chosen model.
- **The `std.http` split:**

- Move `serve` (ctx) + `response` (builder) out of `http` into **`std.http.server`**; `get`/`post`/
  …/`_async` stay in **`std.http.client`**. `http` stops being a module. `Response`/`Request` extern
  **types** stay top-level (already registered independently — no move).
- Migrate call sites: `http.get` → `client.get`, `http.serve` → `server.serve` (~21 files + 3 docs
  per dce.md's count; verify against the tree).
- **DCE payoff:** `module_ring("std.http.client")` → `ring-http-client`, `std.http.server` → its own;
  a whole-module `use std.http.server` now sheds reqwest precisely (no more CallMethod conservatism).
  The `ring-http-server` gate + per-`Extension` ring declaration land in **Phase 1** (manifest-driven
  selection) — this slice just makes the module signal precise.
- Conformance: split-module client + server cases; differential green; DCE footprint check on an
  http-server program.

## Phase 0 gate

`use std.http.client` binds and dispatches; a nested + a split-module import each have conformance
cases; the synthetic second-root collision test passes; the full corpus is differential-identical
(only `std` registered, so behavior is unchanged for every existing program); DCE on a
`use std.http.server` program sheds reqwest. fmt/clippy clean; no `unsafe` touched.
