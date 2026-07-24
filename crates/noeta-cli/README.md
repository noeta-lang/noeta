# noeta-cli

The `noeta` toolchain binary.

- **Takes in:** CLI args
- **Emits:** the `noeta` binary — dozens of subcommands (`run`, `test`, `bench`, `build`, `check`, `doc`, `dump`, `expand`, `repl`, `lsp`, `dap`, `mcp`, `profile`, `cache`, `fmt`, `grammar`, `init`, plus the package-manager verbs `add`/`update`/`publish`/`audit`/`key`/`claim`/`scope`/`advisory`/`watch-scope`) — all thin glue over the pipeline crates.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
