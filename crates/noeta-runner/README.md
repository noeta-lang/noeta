# noeta-runner

The lean production runtime's shared execution core (dev-deps D3).

- **Takes in:** a compiled `Module`, or `.noe` source to compile via the L2 front end.
- **Emits:** [`run_module_real_host`] and the `compile` module (`compile_real`/`compile_whole_file`/`resolve_providers`) — a `noeta-runner` binary and a library both `noeta-cli` and standalone deploys share.

The native analogue of `noeta-wasm-runner`: VM + real `Host` (`noeta-host-real`) + runtime extensions, extended with the compiler so it can run `.noe` source (PHP-style deploy) as well as a `.noeb`/stapled bundle — but **none** of the dev toolchain: no fmt, no formatter/parser (`malva`), no LSP/DAP/MCP. The toolchain is excluded *structurally* — those crates simply aren't dependencies, so a shipped artifact built on this crate cannot reach them. `noeta-cli` depends on this library so the CLI's `run`/`build --exe` path and the standalone `noeta-runner` binary share one execution core — the drift firewall. `noeta-pm` is pulled with only default features (not `registry-http`/`provenance`/`keyless`), so the lean runtime links manifest and target/tier resolution but none of the publish/network/crypto surface. Defaults to `jit` on (mirroring `noeta-cli`); `--no-default-features` yields a Cranelift-free interpreter-only build.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
