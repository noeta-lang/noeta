# noeta-dap

`noeta dap` — the Debug Adapter Protocol server.

- **Takes in:** DAP requests over stdio from an editor's debug UI.
- **Emits:** a `.noe` program run under the *production* bytecode VM (JIT unarmed, so every frame is tier-0 and inspectable), with breakpoints and stop-on-entry support (`stopped`/`continue`).

A stdio adapter, sibling to `noeta lsp`, that drives the *production* compile+run pipeline (loader, checker, compiler, VM — the same path `noeta run` takes), not the salsa IDE queries the LSP uses: the program compiles with debug info and a `DapDebugger` attaches to the run, pausing at resolved breakpoints. Three roles, decoupled by channels so a paused program never blocks the protocol loop: a **reader** thread decoding and dispatching requests, a **run worker** compiling and executing the program (emitting events including `stopped` while paused), and a single **writer** thread owning stdout, serializing every response/event through one `Writer`. A second channel carries resume commands from the reader to a paused worker; an `AtomicBool` lets the reader abandon a still-running (not paused) worker on disconnect. Only the DAP wire framing is hand-rolled; JSON payloads ride `serde_json`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
