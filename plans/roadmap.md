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

The 2026-07-19/20 burndown + the owner-commissioned arcs that followed closed every implementable
row: the deferred-item long tail (small language follow-ups, `.await` positions, channels I.4c,
isolate env limits I.4b, nested `concurrent` A.7, safepoint cycle GC, keyed-list LiveView,
field-calls + `Callable`, DAP workers + conditional breakpoints, profiler tier-1, per-project
tree-sitter grammar, fmt follow-ons, salsa residuals), then a JSON/error/type-system sweep —
`Error` trait + `From`/`?` conversions + `@derive(Error)`, `json.try_parse::<T>`/`JsonError`,
registry-driven call-site-typed native functions, **polymorphic function values** (expected-type
instantiation, user turbofish + `T`-forwarding, generic methods, prelude constructors as values),
and **`Validate`** (recipe-boundary invariant enforcement + `@validated`) — plus advisory-intake
residuals (CVSS, feed adapters, promote) and the **para/db migration system** (`noeta migrate` +
seeds). Several latent compiler/VM/parser bugs were found and fixed en route. CUTS (owner):
reverse debugging, down migrations. What remains in `backlog.md` is exclusively **decision-gated
or trigger-gated**:

1. **Publish the toolchain + registry repos** — the keystone; a user decision (naming, visibility,
   push policy), then mechanical follow-through. Local `main` (both repos) carries everything,
   unpushed.
2. **User-action items**: the Spin/Fastly edge deploy (account + one deploy; the guide + verified
   local proof are done), and editions S3/S4 (awaits a deliberate post-1.0 breaking change).
3. **Design-gated**: Tauri packaging, capability-enforcement (static effect analysis — deferred by
   the owner), synced store R&D, TaskScope patterns.
4. **Trigger-gated tails**: the perf cluster, stdlib follow-ons, OTEL residuals, the attested watch
   ledger (design note filed), the wasm-serve component packaging, and the small remaining tooling
   rows — each fires on its stated trigger, none on its own.

## Working discipline (unchanged)

- Work in slices; every behavior change lands with a conformance case (the iron rule).
- The differential oracle (Core-IR interpreter ↔ VM) and the leak gate stay green; fmt/clippy clean.
- Update `backlog.md` in the same commit that opens or closes a deferred item.
- When an arc grows past a few slices, give it a directory under `plans/<arc>/` with a README
  ledger; **delete the directory when the arc ships** (strike its backlog rows, move any new
  deferrals into `backlog.md`).
- `backend-mirror.md` is the standing VM ↔ reference-interpreter mirror inventory & policy — keep
  it current when touching mirrored code.
