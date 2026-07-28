# noeta-span

Source spans and source-file bookkeeping.

- **Takes in:** nothing (leaf crate)
- **Emits:** `Span`, `SourceId`, `Source` (byte-offset ↔ line:col), `LineCol`, `SourceMap` — the shared vocabulary for pointing at source — plus `PackageMap`/`PackageOrigin`, the per-`SourceId` record of which **package** each source of a merged program came from.

`PackageMap` lives here because it is the same shape as `SourceMap`: linking merges every package's modules into one program, so a package boundary survives only as a side-table keyed by the `SourceId` every span already carries. An unrecorded source is *unknown*, never "the root package" — the checker's orphan rule (E0070) stands down on unknown provenance rather than judging a single-file or synthetic program as though it were a resolved dependency graph.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
