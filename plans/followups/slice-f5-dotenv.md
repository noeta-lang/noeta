# Slice F5 — `.env` support folded into `std.env`

Status: **done**

> **Follow-on to M2.2** (`plans/m2/slice-02-env-args.md`). **Determinism posture:** `env.parse` is pure (no host). `env.load` reads the file through the `FileSystem` capability (sandbox = deterministic in-memory Vfs) and overlays the sandbox env fixture — so both backends agree by construction and it stays fully inside the differential. No new host capability, no env mutation.

## Goal
Add native support for the `.env` standard **inside the existing `std.env` namespace** — a pure parser and a file loader that returns the environment with a `.env` file's defaults applied under the cross-ecosystem-standard precedence (real environment wins).

## Why this shape
Across every major ecosystem (Node `dotenv`, Node built-in `--env-file`, python-dotenv, Ruby dotenv, godotenv, Rust dotenvy, PHP phpdotenv, Docker Compose) the default is identical: **an existing environment variable takes precedence; `.env` only fills in what is not already set.** `.env` is a source of *defaults* for local development, never an authority over injected production secrets. That precedence makes dotenv the same concept `env` already models — "the environment, with a file's defaults applied" — so it belongs in one namespace, not a separate `std.dotenv`.

Returning a plain `Map<string, string>` (not a new value type, not a host mutation) keeps `env` **read-only** — honouring the deliberate M2.2 decision that ruled out `env.set` — while reusing `Map`'s `.get`/`.keys`, so a loaded env reads with the same grammar as any config bag.

## Scope
- **In:**
  - **`env.parse(text: string) -> Map<string, string>`** — the pure `.env` parser. No host coupling, no precedence, never errors (malformed lines are skipped, dotenv-style). Entries returned in key order (sorted, `BTreeMap`-backed) for determinism.
  - **`env.load(path: string = ".env") -> Map<string, string>`** — read the file via `host.fs_read` (guarded by `fs_exists`), parse it, then **overlay the ambient host environment on top** (host value wins on every key). The result is the full merged environment (file keys ∪ host keys), sorted. A **missing file is tolerated** — returns the ambient-env-only map, no error (the "defaults for local dev, file optional" model; matches python-dotenv / node-dotenv `config()`). Optional path defaults to `".env"` via `SigType::Optional`.
  - Parser grammar (the widely-shared `.env` subset): `KEY=VALUE` lines; `#` full-line comments; blank lines skipped; optional `export ` prefix; single-quoted values literal; double-quoted values with `\n \t \r \\ \"` escape expansion; unquoted values whitespace-trimmed with trailing ` #`-comment stripping. Keys `[A-Za-z_][A-Za-z0-9_.]*`; non-matching lines skipped.
- **In (follow-up pass — landed):**
  - **`${VAR}` / `$VAR` interpolation.** Resolves against `base` (ambient env, via `parse_dotenv_with_env`) first, then earlier same-file keys, then empty string — matching dotenv-expand (ambient wins). `\$` is a literal `$`. Single-quoted values do **not** interpolate. Layered onto the parser with no public API change (`env.parse` passes an empty base).
  - **Multi-line double- and single-quoted values** spanning physical lines (newlines are part of the value, up to the closing quote); parsing resumes after the close.
- **Out (permanent):**
  - **`env.set` / mutating `env.get` to see `.env`** — out permanently per M2.2 (would reopen the read-only decision and add hidden global state). The Map-return covers the use case; the standard precedence is safest without it.
  - **`${VAR:-default}` shell-style default expansions** — not implemented; plain `${VAR}` / `$VAR` only. Can layer on later if wanted.

## Checklist (vertical slice)
- [ ] Parser: `parse_dotenv(&str) -> BTreeMap<String, String>` in `crates/noeta-stdlib/src/env.rs`, with unit tests over the grammar (quoting, escapes, comments, export, whitespace, malformed-line skip).
- [ ] Signatures: add `parse` + `load` `ExtFn`s to `ENV_FNS` (`registry.rs`), `load`'s path `Optional`, both returning `Map(String, String)`. (No checker change — signatures are read from the registry.)
- [ ] Dispatch: `env.parse` / `env.load` arms in `env_dispatch` (`registry.rs`), returning `NativeOut::Map` (sorted). `load` reads via `host.fs_read`/`fs_exists` and overlays `host.env_keys`/`env_get`.
- [ ] Bytecode / VM / IR: none — `NativeOut::Map` already materializes in both backends (json uses it); `env` stays `deep_marshal: false` (args are strings).
- [ ] Conformance: `std/env_dotenv_parse.noe` (pure parse of a literal, quoting/comments), `std/env_dotenv_load.noe` (seed a `.env` in the Vfs, load it, assert merged map + host-wins precedence on an overlapping key), `std/env_dotenv_load_missing.noe` (missing file → ambient-env map, no error). All identical on both backends under `--differential`.
- [ ] Snapshots: none expected (no new diagnostic).

## Definition of done
- `use std.{env}; env.parse(...)` / `env.load(...)` work in both backends; conformance covers parse, load-with-precedence, and missing-file over the deterministic Vfs with `--differential` at 0 skipped / zero divergence.
- Standard precedence verified: a key present in both the sandbox env fixture and the `.env` resolves to the **host** value.
- `noeta run` against `RealHost` reads a real `.env` relative to the process (manual check, outside the differential).
- `cargo test --workspace` green; `cargo fmt --all` + `cargo clippy --all-targets -- -D warnings` clean; no new `unsafe`.

## Notes / traps
- Merged map **must** be sorted (`BTreeMap`) — the M2.2 determinism-leak lesson.
- `load` overlays host on top of file, never the reverse — the whole point is "existing env wins."
- Missing-file tolerance diverges from `fs.read`'s E0021: `load` guards with `fs_exists` and treats absence as "no overlay," it does not propagate an IO error.

## Outcome (done)

Landed `env.parse` / `env.load` as two new `ExtFn`s on the existing `env` module — no new host capability, no `env.set`, no backend or checker change (the checker reads the `Map<string,string>` return straight from the registry, and both backends already materialize `NativeOut::Map`).

- **Parser** (`crates/noeta-stdlib/src/env.rs`, `parse_dotenv`): pure `&str -> BTreeMap<String,String>`. Comments/blanks skipped, `export ` prefix stripped, single-quoted literal, double-quoted with `\n \t \r \\ \"` expansion (escape-aware closing-quote scan), unquoted trimmed with inline ` #` comment stripping, malformed lines skipped. 6 unit tests.
- **`env.parse(text)`** — the pure parser, surfaced.
- **`env.load(path = ".env")`** — reads via `host.fs_read`/`fs_exists`, parses, then overlays the ambient environment (`env_keys`/`env_get`) on top so **existing env always wins**; returns the full merged environment, sorted. Optional path via `SigType::Optional` defaults to `.env`. A missing file returns the ambient-env map (no E0021). Added a `str_map` registry helper.

**Verification:** parser unit tests 8/8; conformance corpus + `differential_backends_agree` green with the three new cases (`std/env_dotenv_parse`, `std/env_dotenv_load` — asserts host-wins precedence on `USER` and the default-path form — and `std/env_dotenv_load_missing`); `cargo test --workspace` 0 failed; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all` applied; no new `unsafe`. Manual `noeta run` (RealHost) confirmed the real-env path also honours precedence (shell `USER` overrode the `.env`'s).

**Landed commingled:** the dotenv code (`env.rs`, the `env`-module part of `registry.rs`, the three `.noe` cases, this plan in its in-progress state) was swept into the parallel package-manager commit `f96314d`; this branch (`env-dotenv`) continues from there. This slice's own commit finalizes the plan to done.

**Follow-up pass (interpolation + multi-line) — landed.** `${VAR}` / `$VAR` interpolation (ambient-wins precedence via `parse_dotenv_with_env`, `\$` literal, single-quotes exempt) + multi-line double/single-quoted values. Single expansion pass handles C-escapes and interpolation together so `\$` is unambiguous; `read_value`/`read_multiline` assemble line-spanning quoted values. `env.load` now passes the ambient env as the interpolation base *and* the whole-key overlay. +6 unit tests (20 total), +2 conformance cases (`std/env_dotenv_interpolation` proves same-file + ambient-wins + `\$` + single-quote-literal; `std/env_dotenv_multiline` proves line-spanning). Corpus + differential + `cargo test --workspace` + clippy `-D warnings` all green. **Nothing left open** except the optional `${VAR:-default}` shell-default form (noted, not requested).
