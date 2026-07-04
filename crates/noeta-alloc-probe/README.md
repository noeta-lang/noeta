# noeta-alloc-probe

A test-only heap-residency probe.

- **Takes in:** nothing (a standalone global-allocator wrapper).
- **Emits:** `TrackingAlloc` (a `#[global_allocator]`-ready pass-through allocator) and `peak_during`, which reports the peak live heap a bracketed region touched.

`TrackingAlloc` wraps the system allocator and maintains two process-wide counters — currently-live bytes and their all-time high-water mark. Register it as a test binary's global allocator and bracket a region with `peak_during` to learn the peak heap a piece of work used, a number the time benchmarks cannot see (e.g. the packed-value memory-density wins: a flat `Vec<u64>` buffer vs. N boxed objects).

It is deliberately a standalone crate, depended on only as a `dev-dependency`, so the `unsafe` it needs to implement `GlobalAlloc` stays out of the production runtime crates and the workspace `unsafe_code = "forbid"` quarantine is relaxed in exactly one extra, test-only place. It is `miri`-exempt by nature (it *is* the allocator).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
