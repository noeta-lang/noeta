# UUIDs via the deterministic host seam — `std.id` grows up

**Status: PLANNED (started 2026-07-06).** Branch `uuid-host-seam` (worktree off main). The follow-on
track the prelude-redesign arc recorded: UUIDs belong in `std.id`, and they must flow through the
deterministic Host seam or v4 (random) / v7 (time-based) would break the differential.

## What the survey found

- The `Host` seam is `FileSystem + Rng + Clock + Env`. `Rng` is the **user-facing seeded PRNG**
  (SplitMix64; `random.seed` rewinds it) — on RealHost too, so real runs currently have NO real
  entropy anywhere. `Clock` is logical-monotonic (starts at 0) — no wall time anywhere either.
- So UUIDs need **two new seam capabilities**, not just plumbing:
  1. **Entropy** — raw random bits with real-entropy semantics on RealHost, an *independent* seeded
     SplitMix64 stream on SandboxHost. Independent of `Rng` on purpose: generating a UUID must not
     perturb the user's `random` stream (observable!), and `random.seed(42)` must not rewind UUIDs.
  2. **Wall time** — `clock_unix_ms()`: real `SystemTime` on RealHost; on SandboxHost a **fixed
     epoch base + the logical clock**, so v7 UUIDs are deterministic and advance under `sleep`.
- `id` is currently a **virtual module** (its one function, `next_id`, is a backend builtin reading
  a per-VM counter duplicated in BOTH backends: `Vm.next_id` and eval's `IdGen`). The virtual-module
  intercept in `call_native_module` errors on any name outside the virtual table, so registry
  functions under `id` would be unreachable today.

## Design

**Unify the id domain in the Host** (build-it-right over the hybrid fallthrough): the Host gains an
id-counter capability, `next_id` becomes a registry function like `uuid`/`uuid_v7`, and the `id`
module stops being virtual. This deletes the duplicated per-backend counters — `next_id` agreement
across backends becomes by-construction (one shared dispatch), like every other registry module.
REPL continuity holds (a session owns one host); isolate workers get a fresh host (counter restarts
at 1 — identical to today's fresh-VM behavior).

New surface (all `-> string`, canonical hyphenated lowercase):

| Function | Version | Sandbox behavior |
|---|---|---|
| `id.next_id() -> int` | — | host counter 1, 2, 3, … (unchanged semantics) |
| `id.uuid() -> string` | v4 (random) | deterministic — drawn from the sandbox entropy stream |
| `id.uuid_v7() -> string` | v7 (unix-ms + random) | deterministic — fixed epoch + logical clock |

`uuid()` is v4 because that is the "just give me a UUID" default; v7 is named explicitly (its
selling point — time-ordered keys — deserves an explicit opt-in).

## Slices

- **U1 — seam extension.** `Entropy` capability (`entropy_u64()`) + `clock_unix_ms()` on `Clock`;
  SandboxHost: independent fixed-seed SplitMix64 + `FIXED_EPOCH_MS + logical_ms`; RealHost: OS
  entropy (`getrandom`) + `SystemTime`. `Host` = the five capabilities.
- **U2 — the id module de-virtualized.** Host id counter (`Ids` capability — the sixth); registry
  `id` module (`next_id`, `uuid`, `uuid_v7`); drop `("id", …)` from `VIRTUAL_MODULES`; delete the
  per-backend counters (eval `IdGen` + the seed threading feeding it, `Vm.next_id`).
  *Deviation from plan:* `Op::NextId`/`Builtin::NextId` were DELETED, not repointed — the opcode's
  only emitter sat behind a `Resolved::Prelude` check that stopped matching when P2c removed
  `next_id` from `PRELUDE_NAMES`, so the "direct-call fast path" this plan wanted preserved was
  already dead code; every live call was dispatching through the virtual intercept.
- **U3 — tests + docs.** Conformance: exact-value UUID expectations under the sandbox (differential
  holds by shared dispatch); CLI real-host test (format, uniqueness, v7 time-ordering); wiki `id`
  section; memory.

Differential-green + leak-0 per slice. Deferred: a first-class `Uuid` type (string is v1);
`std.crypto` remains its own future track.
