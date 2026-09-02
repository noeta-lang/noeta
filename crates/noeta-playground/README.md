# noeta-playground

The in-browser toolchain — the engine behind the noeta.dev playground.

- **Takes in:** `.noe` source text as a UTF-8 string, across a hand-rolled `(ptr, len)` wasm ABI (see `abi.rs`; no `wasm-bindgen`).
- **Emits:** JSON strings — [`check_source`] (diagnostics), [`run_source`] (stdout/exit code/diagnostics/traceback), and `fmt_source` (canonical formatting) — plus IDE operations (`hover_source`, `complete_source`, `definition_source`, `signature_source`) and a debug session (`debug_source`).

Compiled to `wasm32-unknown-unknown`, this crate puts the real pipeline — the same lexer → parser → checker → compiler → VM that `noeta run` uses — in a visitor's tab, executing on the deterministic `SandboxHost` (in-memory fs, seeded PRNG, logical clock: exactly the conformance world, so playground output is oracle-grade). Salsa is the compile path on purpose (not the lighter direct pipeline): it's what `noeta-ide` sits on, so hover/completion/go-to-def in the browser were an additive change rather than a second pipeline. Built `cdylib` (the browser artifact) plus `rlib` (so the JSON core stays natively unit-testable). The embedder is expected to run the module in a Web Worker and terminate it on timeout — the runaway-loop guard, since the VM deliberately has no fuel counter.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
