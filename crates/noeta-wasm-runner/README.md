# noeta-wasm-runner

The wasm runner: a `wasm32-wasip1` binary that runs a `.noeb` bundle on the bytecode VM.

- **Takes in:** either an embedded bundle stapled into the binary's own data section (`noeta build --wasm`), or a WASI-preopened `.noeb` file passed on argv (`wasmtime --dir . noeta-wasm-runner.wasm app.noeb`).
- **Emits:** the wasm analogue of a `noeta build --exe` artifact — VM on embedded bytecode, no compiler, no source.

Runs on `noeta_wasi_host::WasiHost` by default (the real WASI world), or the deterministic `SandboxHost` under `--sandbox` / `NOETA_WASM_SANDBOX=1` — the configuration the wasm differential oracle (W1.3) runs, asserting this runner byte-identical to a native run. (The env-var form exists because a stapled artifact's argv belongs entirely to the program, so a flag can't claim it.) Execution is cooperative and single-threaded (`SandboxExecutor`) — wasm has no OS threads. The crate is target-agnostic: it builds and behaves identically on native, which is how its integration tests drive both deployment shapes, with nothing `cfg(target_family = "wasm")`-gated.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
