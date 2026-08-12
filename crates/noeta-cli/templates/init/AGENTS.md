# Working in this repository (a guide for AI agents)

This is a **Noeta** project. Source files end in `.noe`, the whole toolchain is the single `noeta` binary, and the manifest is `noeta.toml`.

**Do not guess Noeta syntax or standard-library calls from other languages.** [SYNTAX.md](SYNTAX.md) is the short reference; `noeta docs <query>` searches the full guide, which is embedded in the binary and matches the compiler you have installed.

## Project layout

- `src/main.noe` — the entry point. Top-level statements execute top to bottom; **there is no `main` function**.
- Every other `.noe` file under `src/` is a module, and **its import path is derived from where the file sits** — the package name plus the path below `src/`, `/` becoming `.`. `src/models.noe` is `<package>.models`; `src/deep/nested.noe` is `<package>.deep.nested`. There is nothing to declare. Every directory name and file stem must be a legal identifier, because it is spelled out in somebody's `use`.
- **Tests, benchmarks, docs and debug code live *inside* the source files** as tier blocks — `@test { … }`, `@bench { … }`, `@doc { … }`, `@debug { … }`. There is no separate test directory. A normal build strips every tier block.
- `noeta.toml` — package identity, dependencies, build targets. `noeta.lock` — **commit it**; change it only through `noeta add` / `noeta update`, never by hand.

## The feedback loop

| Command | What it does |
|---|---|
| `noeta check .` | Type-check everything without running. Reads **inside** `@test`/`@bench`/`@debug` blocks with no `--target`, so a broken test body is an error here. `--format json` for machine-readable diagnostics. |
| `noeta test` | Run every file's `@test` blocks. Naming one file tests only that file — an entry does not carry its modules' tests. |
| `noeta run src/main.noe` | Type-check and execute. `--target development` compiles `@debug` blocks in. |
| `noeta fmt .` | Format to the canonical style. Safe and idempotent. |
| `noeta docs <query>` | Search the language guide. `--page <Slug>` reads a page, `--page <Slug>#<section>` one section. |
| `noeta explain E0059` | What a diagnostic code means and how to fix it. Every error the toolchain prints carries one. |
| `noeta bench src/main.noe` | Run `@bench` blocks, measured. |
| `noeta doc src/main.noe` | Extract `@doc` documentation. |
| `noeta build src/main.noe` | Compile to a `.noeb` bundle; `--exe` for a standalone executable. |
| `noeta add` | Add a dependency and refresh the lockfile. |

Exit codes: `0` success, `1` diagnostics or runtime failure, `2` unreadable input or a usage mistake.

Build targets decide what a *build* contains, not what is checked: the **baseline** (no `--target`) ships no tiers and is the production shape, `--target development` layers the std dev tiers back in. `noeta check` covers every tier block regardless and names which ones it looked inside.

## Ground rules

1. **Never claim Noeta code compiles without running `noeta check`.**
2. **Verify behavior with `noeta test`** — add or extend a `@test` block beside the code you change.
3. **Don't invent APIs.** Look them up with `noeta docs`, or the `stdlib_api` MCP tool if it is available.
4. Run `noeta fmt .` before finishing.

## Naming

Nothing lints these and `noeta fmt` never renames anything, so they are on you. The full agreement is in SYNTAX.md; the rules most often got wrong:

- `PascalCase` types and enum cases; `snake_case` functions, methods, fields and bindings; lowercase one-word module files.
- **Acronyms are words**: `Uuid`, `HttpError`, `Ndjson` — never `UUID`, `HTTPError`.
- **No `SCREAMING_CASE`.** Bindings are immutable by default, so a constant looks like every other binding.
- **No `get_`/`set_` prefixes**, and nothing named `as_` (`as` is the language's own checked narrowing).
- Method affixes carry meaning: `is_`/`has_` predicates, `to_x`/`from_x` conversions, `with_x` copies, `try_x` the `Result`-returning twin of an aborting `x`, `x_async` the awaitable twin, `x_all` the bulk form.
- A constructor is a static `fn` returning its own type, conventionally `new`.

## If your harness speaks MCP

Register the toolchain's server — for Claude Code, `claude mcp add noeta -- noeta mcp` — and the compiler's own answers become tools: `check`, `type_at`, `definition`/`references`/`symbols`, `stdlib_api`, `docs_search`, `explain_diagnostic`, `run`/`test`/`eval`, and a `debug_*` breakpoint debugger over the production VM. Prefer them over guessing; they are the same queries the editor uses.

Without MCP, the CLI above covers the same ground.
