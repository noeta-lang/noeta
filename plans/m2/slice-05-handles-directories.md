# Slice M2.5 — Cursor file handles + directory hierarchy

Status: **done**

> **Origin:** the heavier remainder split out of M2.4. M2.3 shipped flat real-disk fs; M2.4 shipped the line-oriented streaming *workflows* (`read_lines`/`append`). This slice carries the two pieces that are genuinely new machinery: a **stateful file handle** value type (true lazy streaming) and a **directory/path hierarchy**.

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** M2.3 (real disk) + M2.4 (line surface). **Determinism posture:** the sandbox handle (in-memory cursor) is what the differential checks — both backends must build/advance/render/compare it identically; the real-disk handle is `RealHost`-only, integration-tested outside the differential.

## Goal
Add true streaming via `fs.open(path, mode)` → a **cursor-bearing handle** (read a line/chunk at a time without loading the whole file), and a real **directory/path hierarchy** so `fs.list(dir)` lists a directory and nested paths are first-class.

## Why this is its own slice
A file handle is a **new mutable heap value type** — interior-mutable cursor state, present in both the tree-walker (`Value`) and the VM (`Payload`), with its own construction, method dispatch (`read_line`/`read`/`close`), display, equality, and refcount/GC integration (the same checklist the `Set` type went through in M1.10.2, plus mutation). That is too much to fold into M2.4's behavior-only additions without risking the differential, so it is sliced separately.

## Scope
- In:
  - **Handle value type:** `fs.open(path, mode)` → handle; `handle.read_line()` → `Option<string>` (advances the cursor, `none` at EOF), `handle.read(n)` → chunk, `handle.write(chunk)` (write mode), `handle.close()`. Sandbox handle wraps an in-memory cursor over the VFS content; `RealHost` handle wraps a buffered `tokio` file (block-on at the leaf). Construct/render/compare identically across backends for the sandbox differential.
  - **Directory hierarchy:** richen the VFS + `Host` to a path/directory model; `fs.list(dir)` lists a directory (sorted), nested paths resolve, `fs.mkdir`/`fs.is_dir` as needed. `RealHost.fs_list` honors a path argument (M2.3 lists cwd only).
  - Extend the **`E0021 IoError`** family for the new failure modes (append-only codes).
- Out: file watching/notify; symlinks/permissions/metadata beyond what streaming needs; the async *surface* (`await` over a handle — a later M2 pass); the bundled server (§9.5).

## Checklist (vertical slice)
- [x] Grammar / AST: none (stdlib surface; the handle is a runtime value, not syntax).
- [x] Checker rule: none needed — the gradual checker already accepts method calls on an inferred handle receiver (conformance runs through the checker and passes); a handle has no annotation surface, so no `PRELUDE_TYPES` entry.
- [x] Bytecode: none — `fs.open` + handle methods lower to `Op::CallMethod`.
- [x] VM op: new `Payload::FileHandle(lang_stdlib::FileHandle)` (GC leaf — holds only `String`s) with `with_file_handle`/`with_file_handle_mut` heap accessors, display, JSON, and equality; tree-walker `Value::FileHandle(Rc<RefCell<FileHandle>>)`. `call_fs` dispatches `open`; `call_method`/`Op::CallMethod` dispatch the shared `FileHandleMethod`. The cursor logic itself lives once in `lang_stdlib::FileHandle`, so both backends advance it identically.
- [x] Conformance: `std/fs_handle.lang` (write-mode buffer→close→flush; read-mode `read_line` to EOF; `read(n)` by char; append round-trip) + `std/fs_dirs.lang` (nested paths, `mkdir`, `is_dir`, sorted `list(dir)`) + negative `std/fs_handle_closed.lang` (write after close → E0021), all over the sandbox VFS — differential-covered. Real-disk dir behavior is a `lang-runtime` unit test.
- [x] Snapshots: no new rendered-diagnostic snapshots — the new failures reuse the existing `E0021 IoError` rendering.

## Definition of done
- [x] `fs.open`/handle methods + `fs.list(dir)`/`mkdir`/`is_dir` work identically on the sandbox VFS (conformance, `--differential` 96 matched / 0 skipped / zero divergence) and on real disk (`RealHost`, unit-tested).
- [~] The handle *API* is the same over both hosts; on the sandbox it streams a snapshot. **Deviation from the sketch:** the read handle snapshots the file at open rather than holding a live `tokio` file, so real-disk reads are not yet lazy — see Outcome. This keeps the handle a pure, differential-safe value; lazy real-disk reads are a later internal optimization behind the same surface.
- [x] The new failure modes (closed handle, wrong mode, unknown mode) reuse the stable `E0021` code; the closed-handle case has a negative conformance test.
- [x] `lang-runtime` stays `unsafe`-free; fmt/clippy clean.

## Outcome (done)

Shipped in **two commits** (the slice was split for reviewability, the heavy value-type change kept separate from the additive directory work):

- **M2.5a — directory hierarchy** (`84144de`). The `Vfs` gained an explicit `dirs: BTreeSet` (empty dirs leave a trace; a path's parents stay implicit under its files) plus `mkdir`/`is_dir`/`list_dir`. Three **additive** `Host` methods (`fs_list_dir`/`fs_mkdir`/`fs_is_dir`) left the flat `fs_list` untouched; `RealHost` maps them onto `tokio::fs` `read_dir`/`create_dir_all` and `Path::is_dir`. Both backends' `call_fs` gained `fs.mkdir`/`fs.is_dir` and let `fs.list` take an optional directory argument. Sandbox differential-covered (`std/fs_dirs.lang`); real disk unit-tested.
- **M2.5b — cursor file handle** (this commit). The project's first mutable heap value type beyond field assignment. The whole state machine — read snapshot + byte cursor, write/append buffer, mode/closed guards — lives once in **`lang_stdlib::FileHandle`** (new `handle` module), so the two backends are byte-identical by construction. The VM stores it as `Payload::FileHandle(FileHandle)` (a GC **leaf** — it owns only `String`s, so it joins `Str`/`Int` in the no-children `free`/`children` arms) reached via new `with_file_handle`/`with_file_handle_mut` heap accessors; the tree-walker wraps it in `Rc<RefCell<FileHandle>>`. `fs.open(path, mode)` builds the handle (read mode snapshots via `host.fs_read` — a missing file is the same `E0021` as `fs.read`); `read_line`/`read` return `some`/`none`, `write` buffers, `close` hands the backend a `Flush` instruction it routes through `host.fs_write`/`fs_append`. Dispatch goes through the shared `FileHandleMethod` enum (exhaustive `match` in both backends — the same static guard as `SetMethod`).

**Key design decisions (deviations from the sketch, recorded):**
- **`lang-value` now depends on `lang-stdlib`.** Storing `lang_stdlib::FileHandle` directly is what makes the cursor logic *shared*, not re-derived — the whole differential-by-construction point of the slice. lang-stdlib has no internal deps, so no cycle. This mirrors, but inverts, the `NativeModule(String)` choice (which avoided the dep by stringly-typing); here the shared *behavior* is worth the edge.
- **Read handles snapshot, they do not stream lazily.** The sketch imagined a buffered `tokio` file inside the handle; that would put a non-`Clone`, non-deterministic OS resource inside a *value* (which must display/compare/clone identically across backends). Instead the handle is pure data and persistence routes through `self.host` at method time. Real-disk reads therefore load the whole file at open — correct and identical in behavior, just not yet lazy. Lazy real-disk streaming is a pure-internal optimization behind this exact surface, deferred.
- **`close` flushes; an unclosed write handle does not persist.** The must-close-to-flush contract is deterministic and identical on both backends.

**Verification:** conformance **102 passed / 0 failed**; `lang test --differential` **96 matched / 0 skipped / 100% / backends agree**; `cargo test --workspace` **329 passed / 0 failed** (incl. 6 new `lang_stdlib::handle` unit tests + the `lang-runtime` real-disk dir test); fmt/clippy `--all-targets` clean; `lang-value` miri green; `lang-runtime` stays `unsafe`-free. **This closes M2 cluster 1.**

## Notes / traps
- The handle **is a value** — both backends must construct/advance/render/compare it identically, or the sandbox differential breaks. Keep its observable surface minimal and its cursor logic shared where possible.
- A handle holds mutable state across method calls (the cursor), unlike every prior value type — get the refcount/GC and the `RefCell`-style interior mutability right, and proptest the read-to-EOF invariant.
- Directory `list(dir)` over the sandbox VFS must stay **sorted**; real-disk `list(dir)` too for any output flowing into a conformance-style check.
- This closes M2 cluster 1. The next cluster (persistent runtime + bundled HTTP/WS server + the async/await surface + signals) is a separate planning pass.