# noeta-conformance

The conformance harness — the executable spec runner.

- **Takes in:** a `.noe` corpus (`// expect:` headers)
- **Emits:** a pass/fail `Report` (human text or JSON), with by-file and by-stage narrowing. Powers both the language's own suite and user-facing `noeta test`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
