# Working in this repository (a guide for AI agents)

This is a **Noeta** project. Noeta is a general-purpose programming language: source files end in `.noe`, the whole toolchain is the single `noeta` binary, and the manifest is `noeta.toml`.

> **If you are not fluent in Noeta, read [SYNTAX.md](SYNTAX.md) before writing any code.** It is the complete language reference — generated from the toolchain's own embedded documentation, so it matches the installed compiler exactly. Do not guess syntax or standard-library calls from other languages.

## Project layout

- `src/main.noe` — the entry point. Top-level statements execute top to bottom (there is no `main` function). Every other `.noe` file under `src/` is a module. **A module's import path is derived from where its file sits** — the package's name plus the file's path below `src/`, `/` becoming `.` — so there is nothing to declare: `src/models.noe` is `<package>.models` and `src/deep/nested.noe` is `<package>.deep.nested`. Name files with a lowercase single word; every directory name and file stem has to be a legal identifier, because it is spelled out in somebody's `use`.
- `noeta.toml` — the manifest: package identity, dependencies, and build targets.
- `noeta.lock` — pinned dependency resolution. **Commit it.**
- Tests, benchmarks, docs, and debug code live *inside* the source files as tier blocks (`@test { … }`, `@bench { … }`, `@doc { … }`, `@debug { … }`) — there is no separate test directory. A normal build strips every tier block; the matching tool (or a `--target`) activates them.

## The feedback loop (CLI)

Use these constantly; they are fast:

| Command | What it does |
|---|---|
| `noeta check .` | Type-check everything without running — including inside `@test`/`@bench`/`@debug` blocks, with no `--target`. **Run this before claiming code compiles.** `--format json` for machine-readable diagnostics. |
| `noeta run src/main.noe` | Type-check and execute. Add `--target development` to compile `@debug` blocks in. |
| `noeta test` | Run every file's `@test` blocks. **Run this before claiming a change works.** Naming one file (`noeta test src/main.noe`) tests only that file — an entry does not carry its modules' tests. |
| `noeta fmt .` | Format to the canonical style. Run after editing. |
| `noeta bench src/main.noe` | Run `@bench` blocks, measured. |
| `noeta doc src/main.noe` | Extract `@doc` documentation. |
| `noeta repl` | Interactive REPL for quick experiments (`--load <file>` bootstraps a session). |
| `noeta build src/main.noe` | Compile to a `.noeb` bundle; `--exe` for a standalone executable. |
| `noeta add` | Add a dependency to `noeta.toml` and refresh `noeta.lock` (never edit the lockfile by hand). |

Exit codes: `0` success, `1` diagnostics/runtime failure, `2` unreadable input.

Build targets (from `noeta.toml`): the **baseline** (no `--target`) ships no tiers and is the production shape; `--target development` layers the std dev tiers back in; `--target production` is an explicit name for the baseline. Targets decide what a *build* contains — they are not a checking switch: `noeta check` covers every tier block regardless, and names in its summary which tiers it looked inside (`checked 3 files (tiers: test, debug): …`).

## The agent surface (`noeta mcp`)

The toolchain ships an MCP server — the same compiler queries the IDE uses, exposed as tools. If your harness supports MCP, register it (for Claude Code: `claude mcp add noeta -- noeta mcp`) and prefer these tools over guessing:

**Understand the language and libraries**
- `docs_search` / `docs_get` — search and read the embedded language guide. First stop before writing unfamiliar Noeta.
- `stdlib_api` — enumerate the *real* standard-library surface. Use instead of inventing stdlib calls; the signatures are ground truth.
- `examples_find` — real, runnable example programs for a feature or concept.
- `explain_diagnostic` — what an `E0xxx` code means, with programs that trigger it.

**Understand this codebase**
- `check` — type-check and get diagnostics (the compile feedback loop). Point it at a file or at the **project directory** and it answers exactly as `noeta check` does, reading inside tier blocks; `tiers_checked` names which it covered.
- `type_at` — the inferred type at a position: the compiler's answer, not a guess.
- `definition` / `references` / `symbols` / `completions` / `signature` — navigation.
- `module_graph` — the module/`use` import graph.
- `trace` — unfold the static call path from a function.
- `reflect` — the `@role`/`@semantic` architectural graph and declared types.
- `project_docs` / `doc_browse` / `doc_page` — this project's own `@doc` documentation.
- `ast` / `bytecode` / `pipeline` — parse tree, VM disassembly, per-stage health.

**Execute and verify**
- `run` — run a program under liveness limits; reports stdout, exit, traceback.
- `eval` — one-shot expression evaluation.
- `test` — run `@test` blocks and report each case.
- `format` — canonical formatting.

**Debug interactively**
- `debug_start` / `debug_inspect` / `debug_step` / `debug_eval` / `debug_stop` — a full breakpoint debugger over the production VM.

## Ground rules

1. **Never claim Noeta code compiles without running `noeta check`** (or the MCP `check` tool) on it. It reads inside tier blocks too, so a `@test` body that does not compile is an error there, not a surprise at `noeta test`.
2. **Verify behavior with `noeta test`** — add or extend a `@test` block beside the code you change.
3. **Don't invent APIs.** Look them up: `stdlib_api`, `docs_search`, or SYNTAX.md.
4. Run `noeta fmt .` before finishing; the formatter is safe and idempotent.
5. `noeta.lock` changes only through `noeta add` / `noeta update`.
