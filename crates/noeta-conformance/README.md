# lang-conformance

The conformance harness — the executable spec runner.

- **Takes in:** a `.lang` corpus (`// expect:` headers)
- **Emits:** a pass/fail `Report` (human text or JSON), with by-file and by-stage narrowing. Powers both the language's own suite and user-facing `lang test`.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
