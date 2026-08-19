# noeta-conformance

The conformance harness — the executable spec runner.

- **Takes in:** a `.noe` corpus (`// expect:` headers)
- **Emits:** a pass/fail `Report` (human text or JSON), with by-file, by-stage and by-engine narrowing. Powers both the language's own suite and user-facing `noeta test`.

The language is implemented twice, so a report says **which engine** it is a verdict on. An expectation run executes every case's program on the reference interpreter (`noeta-eval`) *and* the bytecode VM (`noeta-vm`), checks the header against each, and attributes every failure to the engine that produced it; the summary counts what each engine ran. `--engine reference` / `--engine vm` narrows that once a failure is in hand, and `--stage lexer` / `--stage parser` stop before execution — asserting the `// expect: error` lines and nothing else, which the summary states rather than leaving to be inferred from a bare pass.

The oracles are separate commands over the same corpus and answer different questions: `--differential` holds the two backends against **each other**, `--check-leaks` measures heap residency on both, `--jit-differential` adds tier 1, and `--aot-differential` the linked `--native` artifact. `docs/Contributing.md` describes each one.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
