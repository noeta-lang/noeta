# Slice M2.4 — fs streaming surface (line iteration + append)

Status: **done**

> **Scope split (recorded).** The cluster's "real-disk/streaming filesystem" goal landed in three coherent pieces: M2.3 shipped **flat real-disk fs** (the async IO the runtime drives); **M2.4 (this slice)** ships the **line-oriented streaming workflows** — `fs.read_lines` and `fs.append` — which are differential-safe over the existing read/write model; and **M2.5** carries the heavier remainder: true cursor-based `fs.open` handles (a new *mutable heap value type*) and a directory/path hierarchy. M2.4 deliberately stops short of the mutable handle value type, because adding one is a value-model change (like the `Set` type in M1.10.2) that warrants its own focused slice rather than being rushed in.

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** M2.1 (the `Host` boundary) **and** M2.3 (the async runtime + flat real disk). **Determinism posture:** `use std.{fs}` programs stay differential-covered on the **sandbox** in-memory VFS; the same surface runs on real disk via `RealHost` (`lang run`), integration-tested outside the differential.

## Goal
Cover the dominant streaming *workflows* — process a file line by line, append as you go — over the existing read/write model, identically on both backends, without a new value type.

## Scope
- In:
  - **`fs.read_lines(path)`** → `List<string>`: splits the file on newlines (no trailing empty), so `for line in fs.read_lines(p)` works. Built on the existing `fs_read` (no new `Host` method) — both backends split via Rust's `str::lines`, identical by construction.
  - **`fs.append(path, content)`**: append, creating the file if absent. New `Host::fs_append` (fallible); `SandboxHost` via a new `Vfs::append`; `RealHost` via `tokio` `OpenOptions::append` (async, blocked-on at the leaf).
- Out (→ **M2.5**): `fs.open` cursor handles (true lazy streaming over a mutable handle value type); directory/path hierarchy + path-aware `fs.list`.

## Checklist (vertical slice)
- [x] Grammar / AST: none (stdlib surface).
- [ ] Checker rule: signatures for `read_lines`/`append` (the gradual checker accepts native-module calls as today; richer signatures land with the wider stdlib typing).
- [x] Bytecode: none — both lower to `Op::CallMethod`.
- [x] VM op: `call_fs` gains `append`/`read_lines`; tree-walker mirrors exactly. No new value type (lines are a plain `List<string>`).
- [x] Conformance: `std/fs_lines.lang` (write multi-line → `read_lines` + `for`-iterate → `append` → re-read shows growth) + negative `std/fs_lines_not_found.lang` (`read_lines` of a missing path → E0021), both over the sandbox VFS — differential-covered.
- [ ] Snapshots: none new.

## Definition of done
- `fs.read_lines`/`fs.append` work identically on both backends (sandbox, differential-covered) and on real disk (`RealHost`, exercised by the M2.3 CLI/unit IO path + the `lang-runtime` round-trip test extended with append).
- `lang test --differential` stays at 0 skipped / zero divergence.
- fmt/clippy clean; `lang-runtime` stays `unsafe`-free.

## Outcome (done)

Added `Vfs::append` + `Host::fs_append` (fallible; `SandboxHost` always `Ok`, `RealHost` via `tokio::fs::OpenOptions().append()` with `AsyncWriteExt`, needing the tokio `io-util` feature) and the `append`/`read_lines` arms in both backends' `call_fs`. `read_lines` needs no new `Host` method — it splits `fs_read`'s result with `str::lines`, so the two backends are identical by construction; a missing path is the same `E0021` as `fs.read`.

**Verification:** conformance **99 passed / 0 failed** (`std/fs_lines.lang`, negative `std/fs_lines_not_found.lang`); `lang test --differential` **93 matched / 0 skipped / 100% / backends agree** (up from 91); `cargo test --workspace` **317 passed / 0 failed** (incl. a `Vfs::append` unit test and the `lang-runtime` round-trip test extended to cover real-disk append); fmt/clippy `--all-targets` clean; no `unsafe`.

## Notes / traps
- `read_lines` uses `str::lines`, so `"a\nb\n"` → `["a", "b"]` (no trailing empty) on both backends — do not hand-roll a `split('\n')` that would diverge.
- `append` to a missing path **creates** it (both sandbox and real disk), so it is a safe "open-or-create" log primitive.
- True large-file streaming (a handle that does not load the whole file) is **M2.5** — `read_lines` reads the whole file then splits, which is fine for the common case but is not lazy.