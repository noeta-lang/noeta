# lang-runtime

The real host: real-disk IO, the real process environment, and a per-isolate async runtime.

- **Takes in:** the `lang_stdlib::Host` and `Executor` capability traits.
- **Emits:** `RealHost` (real-disk fs + real `env`/`args`) and `RealExecutor` (a real `tokio` executor).

This is the non-sandbox side of the host/executor split. Where `SandboxHost`/`SandboxExecutor` (in `lang-stdlib`) are the deterministic in-memory world the conformance differential always runs, `RealHost` and `RealExecutor` are what the CLI, REPL, and (later) server give a real program — real process environment/args and real-disk file IO. They are **never** used in the differential, so determinism is not their job; they are integration-tested outside the corpus, keeping `skipped == 0`.

Disk IO runs on a per-isolate `tokio` `current_thread` runtime, matching the shared-nothing isolate model (no work-stealing across heaps). The `Host` surface is still synchronous, so each IO method drives its future to completion with `block_on` at the leaf and returns a plain value. Building the IO path on `tokio` now means the `async`/`await` surface layered on later is additive (these `tokio::fs` calls get `await`ed instead of `block_on`-ed) rather than a rewrite. The filesystem is real-disk (paths relative to the process working directory) with a directory hierarchy mapping onto `tokio::fs`. This crate is `unsafe`-free.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
