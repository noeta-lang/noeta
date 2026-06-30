# P-LAZY — lazy real-disk reads behind the `fs.open` handle

Status: **DONE** (2026-06-30, two commits on `main`). Source: deferred backlog "Lazy real-disk reads
behind the `fs.open` handle (M2.5 snapshots the file at open; surface is final)". The gate (a real
large-file workload) was waived by the user — done now while the M2.5 handle design is fresh.

## What shipped

A read handle no longer snapshots the whole file at open. The host decides delivery via a neutral
`ReadSource` (returned by the new `Host::fs_open_read`): the deterministic `SandboxHost` always hands
over a whole-file `Snapshot` (so the differential is byte-identical to before), while `RealHost` hands
over a `Lazy(id)` stream the handle pulls from a line at a time via `Host::fs_read_more` (a
`tokio::io::BufReader` per id, kept in a `RealHost` registry, dropped at EOF). The handle keeps all
cursor/line/character logic and stays `Clone + PartialEq + Eq` — only an integer id crosses the seam,
so `lang-value`'s `Payload::FileHandle` was untouched (no miri needed). `read_line`/`read` gained a
`&mut dyn Host` parameter; both backends thread `self.host` in (the `recv`/`handle` value is
independent of `self`, so the borrows don't conflict).

- **Commit 1/2** `96d64be` — the seam: `ReadSource`, the two `Host` methods, the lazy-capable
  `FileHandle` (refill loop + `fill_more`), both backends threaded. RealHost still eager. No behavior
  change anywhere.
- **Commit 2/2** — RealHost streams: registry + `fs_open_read`/`fs_read_more`, a host test, this bench.

## Benchmark (validates the gain)

`crates/lang-runtime/benches/lazy_fs.rs` — time-to-first-line on an ~8 MB / 200k-line file, the old
whole-file snapshot vs the new lazy stream (fresh `RealHost` per iteration in both arms, so the delta
is the read strategy alone):

| arm | time-to-first-line |
|---|---|
| `snapshot` (read whole file, take line 1) | **~1.576 ms** |
| `lazy` (`fs_open_read` + one `read_line`) | **~50.9 µs** |

≈ **31× faster** to the first line, and peak memory is one buffered chunk instead of the whole file —
the point of the change. The gap widens with file size (snapshot is O(file), lazy is O(first line)).

---

## Original plan (for context)

## The cost

`fs.open` (M2.5) snapshots the **entire file** into the handle at open time. For a file larger than
available buffer / RAM, that's wasteful or impossible — the workload reads the file incrementally but
pays a whole-file read (and allocation) up front. The *surface* (the cursor `FileHandle` API) is
already final; only the backing strategy changes.

## The fix

Back the `RealHost` file handle with a lazy reader (seek + read on demand) instead of an in-memory
snapshot, so `read_line`/`read`/cursor advance touch only the bytes consumed. The `SandboxHost`
(deterministic, differential) keeps its in-memory `Vfs` — files there are small test fixtures, and
the in-memory model is what makes the differential deterministic. So this is a **`RealHost`-only,
CLI-only** change; the differential is untouched (it runs the sandbox host).

## Why last / demand-driven

Only matters for files too large to buffer whole — no current workload hits it. The API is final, so
there's no design debt accruing by waiting. Park until a real workload needs it.

## Benchmark (validates the gain)
A `RealHost` micro-bench (or a CLI-driven measurement) reading the first K lines of a large temp
file: snapshot reads all N bytes, lazy reads ~K lines' worth. Validate by peak memory / time-to-first
-line, not differential output. Record numbers here when implemented.

## Verification
Differential unchanged (sandbox host unaffected). `RealHost` path tested via the `lang-runtime`
crate's host tests. Workspace/clippy/fmt clean. Branch `types-inferred-static`.
