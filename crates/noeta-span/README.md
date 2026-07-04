# noeta-span

Source spans and source-file bookkeeping.

- **Takes in:** nothing (leaf crate)
- **Emits:** `Span`, `SourceId`, `Source` (byte-offset ↔ line:col), `LineCol` — the shared vocabulary for pointing at source.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
