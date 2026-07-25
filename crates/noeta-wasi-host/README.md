# noeta-wasi-host

The WASI host (P-WASM W1.0) — the third `noeta_stdlib::Host`, for the `wasm32-wasip1` runner.

- **Takes in:** the `noeta_stdlib::Host` capability traits (`FileSystem`/`Env`/`Clock`/`Entropy`/`Network`/`Os`/…).
- **Emits:** [`WasiHost`] — real-but-synchronous: a preopened directory tree, embedder-granted env/args, wall time, and real entropy (`random_get`) through plain `std`, with no async runtime and no threads.

Where `SandboxHost` is the deterministic in-memory world and `RealHost` is the CLI's tokio-backed real host, `WasiHost` gives a program the world WASI exposes, cooperatively (the VM runs its isolates without OS threads under this host). It compiles and behaves identically on native targets — which is how its unit tests run — with nothing `cfg(target_family = "wasm")`-gated. Like `RealHost`, it is never differential-tested; the wasm oracle instead runs the same runner binary on `SandboxHost`. Capabilities WASI p1 cannot provide are honest runtime errors, not lying stubs: outbound/inbound HTTP arrives only with the `wasi:http` component build (P-WASM W4, see `noeta-wasm-serve`), and process spawning doesn't exist on this target. Mirroring `RealHost`, the user-facing PRNG and monotonic clock stay seeded/logical, while wall-clock time and entropy are real.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
