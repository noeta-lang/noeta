# P-LAZY — lazy real-disk reads behind the `fs.open` handle

Status: **planned** (sweep item #4, demand-driven). Source: deferred backlog "Lazy real-disk reads
behind the `fs.open` handle (M2.5 snapshots the file at open; surface is final)".

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
