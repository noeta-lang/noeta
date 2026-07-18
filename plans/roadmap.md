# Roadmap

The single source of "what's next". Re-scan this file at the start of every work session.

## Where the project stands

The original milestone ladder (M0 walking skeleton → M1 language core → M2 differentiators → M3
long tail) is **complete and retired** — every milestone and nearly every arc that followed has
shipped. What exists today is documented where it belongs:

- **What the language and toolchain do:** the [wiki](../docs/Home.md) (the user-facing truth) and
  `ARCHITECTURE.md` (the implementation overview).
- **What has been built and how:** git history. Completed arc directories are deleted from `plans/`
  when they ship; their slice ledgers, design rationale, and war stories are in the history of this
  directory.
- **What is still open:** [`backlog.md`](backlog.md) — the single registry of every deferred item,
  scope cut, and design proposal, each with its source and trigger.

Shipped, in one breath: the bytecode VM + Cranelift JIT, NaN-boxed values, shapes + inline caches,
precise-RC + cycle GC over an ANF IR, the inferred-static bidirectional checker on a salsa graph,
traits/generics/derives/attributes + reflection, modules + editions vocabulary, the layered stdlib
over a twelve-capability Host boundary, async/generators/isolates, signals + LiveView, the bundled
HTTP server, native AOT + WASM targets (playground included), the package manager with keyless
signing + hosted-registry client, LSP/DAP/MCP + formatter + profiler + debug console, OTLP
telemetry, CRDTs + p2p, and the `para` namespace with the aether web framework.

## The frontier — good next picks

The backlog rows marked *(active)* are worth picking up on their own; the rest of the backlog is
trigger-gated. The standouts, roughly by leverage:

1. **Publish the toolchain + registry repos** — the keystone: unblocks true out-of-tree packages,
   makes every "path deps until published" caveat disappear. A user decision (naming, visibility,
   push policy), then mechanical follow-through.
2. **`@derive(FromJson)`** — now unblocked by the type-system track; the acceptance test for
   "the type is the schema".
3. **The small-language-follow-ups cluster** — match-arm blocks, `Map.get`, closure-in-method VM
   capture, generic-enum match-payload bug: each small, each user-visible.
4. **In-run safepoint cycle collection** — the one GC story gap (peak residency of cycle-building
   loops).
5. **Generic traits' default methods** — the natural next traits slice after UT5.

## Working discipline (unchanged)

- Work in slices; every behavior change lands with a conformance case (the iron rule).
- The differential oracle (Core-IR interpreter ↔ VM) and the leak gate stay green; fmt/clippy clean.
- Update `backlog.md` in the same commit that opens or closes a deferred item.
- When an arc grows past a few slices, give it a directory under `plans/<arc>/` with a README
  ledger; **delete the directory when the arc ships** (strike its backlog rows, move any new
  deferrals into `backlog.md`).
- `backend-mirror.md` is the standing VM ↔ reference-interpreter mirror inventory & policy — keep
  it current when touching mirrored code.
