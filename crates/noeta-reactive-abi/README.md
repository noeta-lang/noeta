# noeta-reactive-abi

The **reactive extension contract** — the stable ABI between the reactive engine and a foreign reactive-node extension.

- **Takes in:** `noeta_ext_abi`'s `NativeCtx`/`Retained` re-entry vocabulary and `noeta_reactive::NodeId` handles — the only two things that cross the seam.
- **Emits:** the [`ReactiveSource`] trait (`create_source`/`read_source`/`wake`) and [`ViewSource`]/`ViewSourceExtract`.

`std.reactive` (in `noeta-stdlib`) owns the reactive graph, the flush loop, gate coalescing, and flush telemetry. A foreign source node — today `para.synced`'s `SyncedSignal`, which *is* a node in that same shared graph so a peer merge propagates to `computed`/`effect` exactly like a local `set` — must reach the engine to create its node, subscribe a reader, and wake dependents; it does so through this crate and nothing else. The engine implements `ReactiveSource`, the foreign extension consumes it per-run via `noeta_ext_abi::capability`, and neither the engine's representation nor the consumer's node type ever crosses — only `NodeId` handles and arena `Retained` cells do. The contract runs the other way too: `ViewSourceExtract` is a capability the foreign extension provides so the engine's `view.expose` recognizes the foreign node type without naming it. This is an object-safe trait in its own crate specifically so the engine can evolve behind it rather than exposing free functions over a `pub`-fielded struct — see `docs/Native-Extensions.md` (capability-broker seam).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
