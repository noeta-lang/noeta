# Slice M2.2 — Host env/args (injected sandbox + real)

Status: todo

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
- `env.vars()` **must** be sorted — unsorted host iteration order is the classic determinism leak.
- Default the sandbox env/args to **empty**, not to the host's, so an un-injected conformance run can never accidentally read the real environment.
- Diagnostic code: a missing env key is arguably `IoError` (E0021) territory; if the semantics differ enough, add the next append-only `E00xx` rather than overloading — the catalog is append-only.
