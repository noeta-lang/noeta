# Slice M2.2 — Host env/args (injected sandbox + real)

Status: **done**

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** M2.1 (the `Host` boundary). **Determinism posture:** the differential runs `SandboxHost` with an **injected** env map + args list (deterministic, both backends identical); `RealHost` reads the real process environment only on `lang run`/REPL and is *not* differential-tested.

## Goal
Land the host introspection deliberately deferred from M1.10 — `use std.{env}` and `use std.{args}` — as the first real "the program reads its host" capability, deterministic in conformance by injection, real on the CLI.

## Why deferred until now
M1.10 omitted env/args because *"reading real environment is non-deterministic and would need injection."* M2.1 builds exactly that injection seam (the `Host`), so env/args is now a clean, small slice: two new native modules following the established `fs`/`time` dispatch pattern, reading from the host instead of the OS directly. Synchronous — no async required — so it lands as a fast win before the async runtime (M2.3).

## Scope
- In:
  - **`env` module:** `use std.{env}`; `env.get(key)` → `String` (missing key is a typed error), `env.vars()` → sorted `Map`/list of `(key, value)` (sorted for determinism). New `lang-stdlib::env` semantics + a `NativeModule::Env` variant.
  - **`args` module:** `use std.{args}`; `args.all()` → `List<String>` (the program's argument vector). `NativeModule::Args`.
  - `SandboxHost` gains an injected env `BTreeMap<String,String>` + args `Vec<String>` (empty by default; the conformance harness sets fixtures); `RealHost` (introduced M2.3, stubbed here or wired when M2.3 lands) reads `std::env::vars`/`std::env::args`.
  - Dispatch through `call_native_module → call_env`/`call_args` in **both** backends, mirroring `call_fs`/`call_time`.
- Out: mutating the environment (`env.set` — out of scope, non-deterministic and rarely needed); process spawning; working-directory/CWD beyond a simple read if trivially deterministic in the sandbox; anything async.

## Checklist (vertical slice)
- [ ] Grammar / AST: none (stdlib modules, like all of Ring 2).
- [ ] Checker rule: env/args functions carry real signatures the gradual checker accepts (as with the other native modules).
- [ ] Bytecode: none — `use std.{env}` binds a native-module value, calls lower generically to `Op::CallMethod`.
- [ ] VM op: `call_env`/`call_args` at the `call_native_module` seam, reading `self.host`; tree-walker mirrors exactly.
- [ ] Conformance cases: `std/env.lang` (get a known-injected key, sorted `vars()` rendering) + negative `std/env_missing_key.lang` (typed error, both backends identical); `std/args.lang` (injected argv). The harness injects a fixed env map + args list so both backends — and thus the differential — agree by construction.
- [ ] Snapshots: rendered diagnostic for the missing-key error if useful.

## Definition of done
- `use std.{env}` / `use std.{args}` work in both backends; conformance covers them over injected fixtures with `--differential` at 0 skipped / zero divergence.
- `lang run` against `RealHost` returns the real environment/args (manual/integration check, outside the differential).
- The missing-key error has a negative conformance case with a stable diagnostic code (reuse `E0021 IoError`'s family or add an append-only code — decide and record).
- fmt/clippy clean; no new `unsafe`.

## Notes / traps
- `env.keys()` **must** be sorted — unsorted host iteration order is the classic determinism leak.
- The sandbox env/args is a **fixed fixture** the host owns, never the real host's, so a conformance run can never accidentally read the real environment.
- Diagnostic code: a missing env key is arguably `IoError` (E0021) territory; if the semantics differ enough, add the next append-only `E00xx` rather than overloading — the catalog is append-only.

## Outcome (done)

Landed `crates/lang-stdlib/src/env.rs` plus two `NativeModule` variants (`Env`, `Args`) — which is all it takes to wire `use std.{env}` / `use std.{args}` end-to-end, since the compiler's `is_native_module` and both backends' binding defer to `NativeModule::from_name`. The `Host` trait gained `env_get`/`env_keys`/`args`; `SandboxHost` gained `env`/`args` fields. Both backends got `call_env`/`call_args` at the `call_native_module` seam, mirroring `call_fs`/`call_time` exactly.

**Surface:** `env.get(key)` → the value (or `E0021` if absent); `env.keys()` → sorted `List<string>` of variable names; `args.all()` → `List<string>`.

**Three judgment calls (deviations from the slice sketch, recorded):**
- **`env.keys()` instead of `env.vars()`.** A sorted list of names (reusing the `fs.list` `Vec<String>` → list pattern) is minimal and idiomatic — combined with `env.get` it covers iteration without constructing a backend-specific `Map` from native code. A map can come later if a use case demands it.
- **Missing key → `E0021 IoError`, not a new code.** Reading absent host state is an IO failure, exactly mirroring `fs.read` on a missing file (shared `env::not_found_error`, `ErrorKind::Io`). No new diagnostic code; the negative case `std/env_missing_key.lang` asserts `E0021`. (The arity/type misuse path still routes to `E0007` like every native module.)
- **Fixed fixture, not harness-time injection.** The sandbox presents a *fixed, deterministic* env (`HOME=/home/sandbox`, `USER=lang`) and args (`["lang", "run"]`) baked into `SandboxHost` — exactly analogous to the logical clock starting at 0 and the PRNG's `DEFAULT_SEED`. This keeps M2.2 self-contained (no backend-API change); the *real* host environment is read by `RealHost` (M2.3), constructed by the CLI and never in the differential.

**Verification:** conformance **97 passed / 0 failed** (`std/env.lang`, `std/args.lang`, negative `std/env_missing_key.lang` → E0021); `lang test --differential` **91 matched / 0 skipped / 100% / backends agree** (up from 88 — the two positive cases plus the runtime-error case all run identically on both backends); `cargo test --workspace` **312 passed / 0 failed** (incl. new `env.rs` unit tests); fmt/clippy clean; no `unsafe`.
