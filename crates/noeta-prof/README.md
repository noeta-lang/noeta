# noeta-prof

The built-in dev profiler / flamegraph, the `noeta profile` subcommand's engine.

- **Takes in:** a `.noe` program (or bundle), run through the same load → check → compile → VM pipeline as `noeta run`, pinned to **tier-0** (the JIT unarmed by default) so every frame is interpreter-executed and observable at an op boundary.
- **Emits:** a per-function report (instrumenting mode: exact call counts + self/total time) or a folded-stack flamegraph (sampling mode: wall-time or deterministic op-weighted), rendered as speedscope JSON or an SVG (via `inferno`).

A dev-time introspection tool over the production bytecode VM, sibling to `noeta dap`/`noeta lsp` in the dev-tooling cluster. Because its signal is wall time and call structure — not program output — it lives outside the differential oracle, like DAP/LSP. Two collectors ride one per-op seam on the VM (`noeta_vm::ProfileHook`): the *instrumenting* profiler and the *sampling* profiler; this crate owns both collectors, `proto → name @ file:line` resolution, and report rendering. An optional `Alloc` mode attributes every allocated byte to its call path via a counting global allocator (from `noeta-alloc-probe`). The `jit` cargo feature arms the tier-1 JIT under sampling (`--jit`) so hot prototypes run native and their wall time is sampled at the JIT trampoline, with tier-1 frames marked distinctly (`TIER1_MARKER`) in the output.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
