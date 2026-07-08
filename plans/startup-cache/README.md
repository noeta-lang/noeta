# Transparent startup cache — skip the front-end on unchanged sources

**Status: C0–C3 DONE, branch `startup-cache` (worktree, off `main`@`fe14117`).** Signed off 2026-07-07
after a three-agent investigation of the run pipeline, the salsa DB, and the `.noeb` envelope.
`run`/`dump`/`build` share a transparent cache through one `compile_whole_file` seam, guarded by a
semantic-invisibility differential and hermetic tests; C4 (hygiene + `cache clear`) remains.

This delivers the M3 roadmap item *"startup cache"* in its **transparent** form. The AOT arc already
shipped the *explicit* form (`noeta build` → a `.noeb` you run instead of source, which skips the
front-end); see `plans/aot/README.md:42`. What's missing is the automatic version: a plain
`noeta run app.noe` that compiles on first run and **reuses the compiled bytecode on every subsequent
run** until the sources or the toolchain change — no build step, no artifact to manage.

## Why this is worth doing (and precisely when)

Measured on the release binary, the front-end cost (`.noe` recompile vs pre-built `.noeb`) scales with
program size:

| Program | `run .noe` | `run .noeb` | front-end saved |
|---|---|---|---|
| `echo 1` | 2.58 ms | 2.39 ms | ~0.2 ms — noise under the ~2.4 ms process floor |
| fib loop (~10 lines) | 6.66 ms | 5.70 ms | ~1 ms — irrelevant to a human |
| 6000 lines / 1200 typed fns | **124.9 ms** | **6.6 ms** | **~118 ms — a 19× startup win** |

So the cache is worthless for small scripts (buried under the fixed ~2.4 ms process floor) and
transformative for large programs invoked repeatedly — CLI tools in a loop, `noeta test`/`bench` on a
big module during a dev cycle, git hooks, serverless cold starts. The long-lived `noeta serve` case
does **not** need it (it compiles once at boot and amortizes over the process lifetime — that's the
whole point of the server architecture, and why an opcache-style cache is a PHP workaround we don't
otherwise need).

## The two facts from the investigation that shape everything

**1. `noeta run` does not use salsa, and there is no import closure to compute.** The run path is a
direct, in-process pipeline — `noeta_loader::load` → `noeta_check::check_all` →
`noeta_compiler::compile_with_sites` (`noeta-cli/src/main.rs:422/243/281`). `noeta-db` (salsa) is used
only by `noeta-lsp` and `noeta-conformance`. Routing `run` through salsa to get "dependency mapping"
would be a large detour for no gain, because **Noeta's module model is directory-flat**: a program's
modules = the entry file **+ every `.noe` sibling in the same directory**
(`noeta-loader/src/lib.rs:98` `read_siblings`), imported or not. There is no transitive cross-directory
closure. So "which files affect this compile" is a directory glob, not a graph query — and a
*conservative over-approximation* (hash every sibling, even unimported ones) is exactly correct,
because adding/removing/editing any sibling can change linking, and over-hashing can only cause an
extra recompile, never a stale hit.

**2. The `.noeb` envelope already is a versioned, self-invalidating bytecode container.**
`noeta_bundle::write(&Module)` → bytes; `noeta_bundle::read(bytes)` → `Module`, and `read` **rejects a
runtime-version mismatch before decoding** (`noeta-bundle/src/lib.rs:152`, compares
`env!("CARGO_PKG_VERSION")`). `Module::encode`/`decode` (`noeta-bytecode/src/lib.rs:1364/1369`) are
lossless and self-contained — a decoded Module runs through the identical path as a freshly compiled
one (the VM re-interns shapes on load). **So a cache file is literally a `.noeb` blob**, and we reuse
the conformance-proven serialization wholesale rather than inventing a cache format.

## Design

### Storage — `~/.cache/noeta/`, never `/tmp`

XDG cache dir: `$XDG_CACHE_HOME/noeta/` else `$HOME/.cache/noeta/` (macOS `~/Library/Caches/noeta`,
Windows `%LOCALAPPDATA%\noeta\cache`). Created mode `0700`, **per-user**. This is a security boundary,
not just a location: a cache file is executable bytecode, so a world-writable shared dir (as a naïve
`/tmp/noeta-cache` would be) is a cache-poisoning vector — another user could substitute malicious
bytecode that our `noeta run` loads and executes with the caller's privileges, skipping the compiler.
Per-user `0700` closes that. `/tmp` is doubly wrong: cleared on reboot (re-pays every compile) and
shared. No new crate — compute XDG manually.

Cache file: `<cache-dir>/<keyhex>.noeb`, written via `noeta_bundle::write`, read via
`noeta_bundle::read`.

### The cache key — hash of everything that changes the compiled Module

Key material, hashed with `sha2::Sha256` (already a workspace dep; run once per invocation over a few
KB of source — speed is irrelevant, collision-resistance is worth it since a collision = running the
wrong bytecode):

1. **Source content** — for the entry file **and every `.noe` sibling in its directory**: hash each
   `(relative_name, bytes)` pair in sorted order. Content, not mtime (mtime lies across git checkouts).
   Sorted + name-tagged so adding/removing/renaming a sibling changes the key.
2. **Runtime version** — `noeta_bundle::RUNTIME_VERSION` (`CARGO_PKG_VERSION`). The envelope also
   gates on this at `read`, but folding it into the *key* means we don't even probe a stale-format file.
3. **⚠ Binary build identity** — mtime + size of `std::env::current_exe()`. **This is the mandatory
   correctness fix**: the envelope's version gate compares the *released* `CARGO_PKG_VERSION` string,
   which does **not** change when the language developer rebuilds `noeta` at the same crate version
   after editing the compiler. Without this, a cache written before a local `cargo build` would be
   silently loaded after it → stale/wrong bytecode. Hashing the running binary's identity guarantees
   any rebuild ⇒ new key ⇒ clean miss. Non-negotiable for a dev-facing default-on cache.
4. **Active tier/profile set** — the resolved tiers (`run` = none; `test`/`bench` activate their own
   tier; `--tier`/`--profile` union in). These transform the program before compile
   (`activate_tiers`), so `run`, `test`, and `bench` of the same file get **distinct** cache entries —
   no cross-contamination, for free.

### Hooks in the run path

- **Key build (pre-scan).** Before `noeta_loader::load`, glob the entry dir's `.noe` files and read
  their bytes (the same I/O the loader does — `read_siblings` logic hoisted) to compute the key.
- **Lookup / hit.** If `<keyhex>.noeb` exists and `noeta_bundle::read` succeeds → hand the `Module` to
  `run_module_real_host` (`main.rs:534`, the *exact* function the existing `.noeb` bundle runner uses).
  Front-end fully skipped.
- **Miss.** Compile as today (`load` → `check_all` → `compile_real`, `main.rs:312`), run, and **write**
  the `.noeb` to cache on a **background thread** (encode + temp-file + atomic `rename` into place, so
  first-run latency is unaffected and a concurrent reader never sees a torn file). Cache write is
  **best-effort** — a failed write (disk full, read-only cache dir) logs at most and never fails the
  program.
- **Opt-out.** `NOETA_NO_CACHE=1` (env, all commands) and `--no-cache` (per-invocation flag on
  `run`/`test`/`bench`). Both bypass lookup *and* write.

### Safety invariants (the whole point)

- **Never a stale hit.** Guaranteed by keying on source content + runtime version + **binary
  identity** + tier set. Over-approximation (dir-flat sibling hashing) only ever causes extra misses.
- **Never a torn read.** Atomic temp-file + rename publish; readers see all-or-nothing.
- **Concurrent runs are safe with no lock.** Two misses of the same file both write; atomic rename =
  last-writer-wins, both correct.
- **Cache failure is invisible.** Best-effort write; corrupt/unreadable file → treat as miss and
  recompile.

## Slices

- **C0 — cache-key + store module. ✅ DONE (`3ba7e02`).** New `noeta-cache` crate (`sha2`-only, off the
  compile DAG): XDG dir resolution + `0700` create; `CacheKey`/`KeyBuilder` from (per-source name+hash,
  runtime version, binary identity, tier set) → `sha2` hex; `binary_identity()`; atomic store (temp +
  rename); best-effort load; `clear()`. 12 unit tests.
- **C1 — wire into `cmd_run`. ✅ DONE (`efcfae0`).** Key from `read_workspace`, lookup →
  `run_module_real_host` on hit, compile + populate on miss; `--no-cache` + `NOETA_NO_CACHE`. Verified
  133 ms → 7.7 ms (17×); source edit / sibling change / rebuilt binary each force a recompile;
  `NOETA_NO_CACHE` never creates the dir.
- **C2 — one natural seam, not per-command wiring. ✅ DONE.** Extract `compile_whole_file(file, tiers,
  profile, no_cache) -> Result<Compiled, CompileFailure>` — resolve tiers → cache lookup → on miss
  load/activate/check/compile/store — and route **`run`, `dump`, `build`** through it (each collapses to
  "call the seam, use the `Module`"). Because all three compile identically (`compile_real`), they
  **share cache entries**: a `build` warms the entry a `run` reads, and vice-versa (verified). Caching
  now lives in exactly one place; adding it to a new whole-file command is free.
  - **Scope correction (was "extend to `test`/`bench`").** `test`/`bench` compile a *separate* module
    **per `@test`/`@bench` case** — a different granularity than one-module-per-file — so they do **not**
    fit this seam and are **dropped from cache scope** (per-case caching is a possible later follow-on;
    their shared cost is really the whole-file *check*, a different artifact). See non-goals.
  - **Safety rule that makes a single hook correct.** The key is `(source + tiers)`, valid only for
    commands whose program transform is identity-modulo-tiers. `serve` **injects** an `http.serve(...)`
    call before compiling → different module for the same source → it must **never** share the key, and
    stays on the uncached `run_program` path.
- **C3 — differential guard + hermetic tests. ✅ DONE.** `startup_cache_is_semantically_invisible`
  drives 6 fixtures (arith/string-interp/loop/func/list + a runtime `panic`) three ways — NOETA_NO_CACHE
  baseline, cold miss, warm hit — and asserts byte-identical (stdout, stderr, exit code) all three ways,
  plus exactly one cache entry after the cold run (proving the warm run is a real hit). The `lang()`
  helper (and `doc_samples`) now pin `NOETA_CACHE_DIR` to `CARGO_TARGET_TMPDIR`, so `cargo test` never
  touches the developer's real `~/.cache/noeta` (verified: it stays absent across a full run).
- **C4 — hygiene + docs.** `noeta cache clear` (+ maybe `cache path`/`cache info`); a size cap or LRU/age
  sweep so the dir can't grow unbounded; wiki/CLI-help note. Log a one-line notice when a cap evicts
  (no silent truncation).

## Non-goals / deferred

- **`serve` caching.** Long-lived (compiles once at boot, amortizes) *and* it injects a call before
  compiling, so it can't share the plain `(source+tiers)` key. Excluded; would need a `serve:<port>`
  key variant if ever wanted.
- **`test`/`bench` caching.** They compile per-`@test`/`@bench` case, not one module per file, so the
  whole-file cache doesn't apply. A future per-case cache (key `source+tiers+case-id`) or a cached
  *checked-program* artifact (to skip the shared whole-file check) could revisit this if the test-loop
  startup bites; out of scope now.
- **Skipping compression in the cache blob.** Reusing `noeta_bundle::write` deflate+scrambles the
  payload — wasted CPU for a private cache. A `FLAG_RAW` fast path is a later micro-opt only if a
  profile shows it; v1 reuses the envelope verbatim for correctness + conformance coverage.
- **mmap/zero-copy load.** The residual ~4 ms in a cache hit is postcard-decoding the Module. A
  memory-mappable layout would shave it, but even plain decode is 19× better than recompiling. v2.
- **A precise import closure.** Dir-flat over-approximation is correct and simpler; no salsa provenance
  query needed.
- **Cross-machine / shared cache.** Per-user local only. Binary-identity keying makes a shared cache
  across differing toolchains unsafe anyway.
