# Slice M2.5 — Cursor file handles + directory hierarchy

Status: todo

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
- [ ] Grammar / AST: none (stdlib surface; the handle is a runtime value, not syntax).
- [ ] Checker rule: a handle type the gradual checker accepts as a method receiver.
- [ ] Bytecode: none — `fs.open` + handle methods lower to `Op::CallMethod`.
- [ ] VM op: a new `Payload::FileHandle` (and tree-walker `Value::FileHandle`) with retain/release/GC, display, and equality; `call_fs`/`call_method` dispatch `open` + handle methods. Both backends' in-memory cursor behaves identically.
- [ ] Conformance: `std/fs_handle.lang` (open → read_line loop to EOF → close; write-mode round-trip) + `std/fs_dirs.lang` (nested paths, sorted `fs.list(dir)`) + negatives (E0021), all over the sandbox VFS — differential-covered. Real-disk handle/dir behavior is a separate CLI integration test.
- [ ] Snapshots: rendered diagnostics for the new IO error variants.

## Definition of done
- `fs.open`/handle methods + `fs.list(dir)` work identically on the sandbox VFS (conformance, `--differential` 0 skipped / zero divergence) and on real disk (`lang run`, integration test).
- Large-file streaming does not load the whole file; the handle API is the same over both hosts.
- New IO error variants have negative conformance cases with stable, append-only codes.
- `lang-runtime` stays `unsafe`-free; fmt/clippy clean.

## Notes / traps
- The handle **is a value** — both backends must construct/advance/render/compare it identically, or the sandbox differential breaks. Keep its observable surface minimal and its cursor logic shared where possible.
- A handle holds mutable state across method calls (the cursor), unlike every prior value type — get the refcount/GC and the `RefCell`-style interior mutability right, and proptest the read-to-EOF invariant.
- Directory `list(dir)` over the sandbox VFS must stay **sorted**; real-disk `list(dir)` too for any output flowing into a conformance-style check.
- This closes M2 cluster 1. The next cluster (persistent runtime + bundled HTTP/WS server + the async/await surface + signals) is a separate planning pass.