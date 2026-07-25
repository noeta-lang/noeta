# noeta-embed

Drive a live Noeta session from a host process (server-hmr E0–E2).

- **Takes in:** Noeta source (`Session::new`) and named calls (`Session::call`).
- **Emits:** a [`Session`] a host can call into by function name, hot-swap edited code into (`hot_swap`, without losing reactive state — the same swap core `noeta serve --watch` uses), and read/write values from through a deep-copy [`Value`] bridge or GC-rooted [`Handle`]s for values kept across frames.

The canonical consumer is a game engine's scripting layer: load a script, call its functions from the frame loop, and hot-swap edited code into the running session without losing reactive state. `Value` crosses the boundary by value (scalars, strings, lists, string-keyed maps); a value the host must retain across frames (an engine's entity/object references) instead goes through a `Handle` — GC-rooted, mutated in place, no copy. `Session::new` runs on the deterministic sandbox host (in-memory fs, logical clock, seeded randomness); `Session::builder` swaps in the real host, a custom `Host`, and native extensions either process-wide (`install_extensions`) or per-session (`Builder::with_extensions`, instance-registry IR5) — a session's value heap is thread-local, so concurrent sessions run one per thread, like isolates.

**Stability: none, deliberately.** This is a 0.x surface (declared unstable 2026-07-11) that adapts to its consumers until a real engine integration has exercised it — expect breaking changes between minor versions.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
