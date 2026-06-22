# Slice M2.4 — Streaming + directory-hierarchy filesystem

Status: todo

> **Boundary note (post-M2.3):** *flat* real-disk fs (`read`/`write`/`exists`/`remove`/`list`, async on the runtime, paths relative to cwd) **already landed in M2.3** — it was the concrete async IO the tokio runtime needed to drive, and it brought the fallible `Host` fs methods with it. So M2.4 is now scoped to the genuinely new surface it always implied: **streaming handles** and a **directory/path hierarchy** model. The cluster's "real-disk/streaming filesystem" goal is split M2.3 (flat real disk) + M2.4 (streaming + directories).

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** M2.1 (the `Host` boundary) **and** M2.3 (the async runtime + flat real disk). **Determinism posture:** `use std.{fs}` programs stay differential-covered on the **sandbox** in-memory VFS (the sandbox impl of one shared fs model); **real disk** is `RealHost`-only, async, integration-tested outside the differential.

## Goal
Add **streaming** IO (chunked read/write over a handle) and a real **directory/path hierarchy** to the fs surface, richening the flat namespace M2.3 shipped — while the in-memory VFS remains the sandbox implementation of the *same* fs interface so the oracle still covers `fs`.

## Why both impls share one model
M1.10.3 shipped `fs` as a flat in-memory `BTreeMap<path, content>` VFS, explicitly noting *"real-disk/streaming IO is deferred to M2."* Real disk has directories, real paths, metadata, and large files that should not be slurped whole. To keep `use std.{fs}` programs identical across sandbox (conformance) and real disk (CLI), both must implement **one richer fs model**: a path/directory hierarchy and a streaming surface. The in-memory VFS becomes the deterministic sandbox impl of that model; `RealHost`'s disk impl is the real one. The differential covers the sandbox path; real-disk fidelity is integration-tested.

## Scope
- In:
  - Richen the fs model behind the `Host` trait to a real **path/directory hierarchy** (not a flat map); update `SandboxHost`'s VFS to that model so it still resolves deterministically, and make `RealHost`'s `fs_list` honor a path argument (M2.3 lists cwd only).
  - **Streaming surface:** `fs.open(path, mode)` → a handle; chunked `read`/`write` (and `lines()`/iterator form) so large files stream rather than load whole. Works over both the sandbox VFS and real disk.
  - Extend the **`E0021 IoError`** family for the new failure modes, keeping codes append-only.
- Already done in M2.3 (not re-done here): flat `RealHost` real-disk `read`/`write`/`exists`/`remove`/`list` async on the runtime; the fallible `Host` fs methods.
- Out: file watching/notify; symlink/permission/metadata APIs beyond what streaming needs; network filesystems; the bundled HTTP/WS server (§9.5, later M2); async *surface* (`await` over a stream — later M2 pass).

## Checklist (vertical slice)
- [ ] Grammar / AST: none (stdlib surface).
- [ ] Checker rule: signatures for the new fs functions + the stream-handle type accepted by the gradual checker.
- [ ] Bytecode: none — `fs.open`/handle methods lower to `Op::CallMethod`.
- [ ] VM op: `call_fs` gains `open`/streaming dispatch through `self.host`; tree-walker mirrors. The handle is a value type both backends construct identically (so it renders/compares identically in the sandbox differential).
- [ ] Conformance cases: `std/fs_stream.lang` (open → chunked read/write round-trip, line iteration, sorted directory `list`) + negatives `std/fs_stream_not_found.lang` (E0021) over the **sandbox** VFS — differential-covered. A separate CLI integration test exercises real disk in a temp dir, outside the differential.
- [ ] Snapshots: rendered diagnostics for the IO error variants where useful.

## Definition of done
- `use std.{fs}` with directories + streaming works identically on the sandbox VFS (conformance, `--differential` 0 skipped / zero divergence) and on real disk (`lang run`, integration test).
- Large-file streaming does not load the whole file; the handle API is the same over both hosts.
- New IO error variants have negative conformance cases with stable, append-only codes.
- `lang-runtime` stays `unsafe`-free; fmt/clippy clean.

## Notes / traps
- The streaming **handle is a value type** — both backends must construct/render/compare it identically, or the sandbox differential breaks. Keep its observable surface minimal.
- Directory `list` over the sandbox VFS must stay **sorted**; real-disk `list` must be sorted too for any output that flows into a conformance-style check.
- Real-disk tests belong to the CLI/integration layer, never the differential corpus — a `skipped` would violate the "0 skipped" guarantee. The sandbox case is the conformance case; the real case is a separate integration test.
- This closes M2 cluster 1. The next cluster (persistent runtime + bundled HTTP/WS server + the async/await surface + signals) is a separate planning pass.
