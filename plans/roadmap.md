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
telemetry, CRDTs + p2p, and the `para` namespace with the aether web framework — the `para`
packages now extracted to their own repos (locally at `/home/niklas/Code/para/`, pre-publish).

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

1. **User-action items**: the Spin/Fastly edge deploy (account + one deploy; the guide + verified
   local proof are done), and editions S3/S4 (awaits a deliberate post-1.0 breaking change).
2. **Design-gated**: Tauri packaging, capability-enforcement (static effect analysis — deferred by
   the owner), synced store R&D, TaskScope patterns.
3. **Trigger-gated tails**: the perf cluster — each row now carries a 2026-08-16 measurement
   saying it **appears nowhere in the profiles** of the benchmarks Noeta is behind on, so the
   trigger is a workload that exercises it (packed lists, objects, a hot extern method, a checker
   profile) rather than a decision to start — plus the arc scope-cut menus, the OTEL surface
   residuals, the attested watch ledger (design note filed), the wasm-serve component packaging, and
   the small remaining tooling rows. Each fires on its stated trigger, none on its own.

**That claim is now true, and it was audited rather than assumed.** The `backlog-burndown` arc (2026-08-12 → 2026-08-15) took the un-gated remainder: the reflection-boundary visibility holes, the two oracles that reported passes over things they never examined, four doors that shipped on one side only, two identity guards, cancellation reaching work blocked outside the interpreter, and the `noeta fmt` safety gate — which now compares programs structurally instead of comparing a hand-written rendering of them. Its six slices and their war stories are in git history; the arc directory is deleted, as the discipline below requires.

**The audit's real finding was about the backlog itself.** Of the rows it did *not* claim, 27 stated no condition at all — their trigger column recorded where the row came from, not what would start it. Those are now written: four had already shipped and were struck, one closed on a rule rather than a fix, four had a genuine condition nobody had written down (a **date**, for the Sigstore root), three OTEL rows collapsed into one, one was promoted for being a memory leak rather than a missing feature, and the arc scope-cut menus say plainly that they are inventory rather than tracked work. Only three remain bare, deliberately: `TaskScope` patterns, the synced store, and desktop packaging are R&D whose trigger is a decision to do them. **Read a row's evidence before picking it up:** re-reading the HTTP scope-cut row found three of its five items had shipped without it being touched, and the backlog still closes rows late — this arc's own slices 1, 3 and 4 shipped without striking theirs.

**Publishing is done, and is no longer the keystone it was written as.** Both repos are public and
level with `origin/main` (`noeta-lang/noeta`, `noeta-lang/noeta-registry`), the nine `para`
packages are in their own repos depending on each other through the registry, and the reservation
stubs are on crates.io and npm. An audit on 2026-08-12 struck it along with four other rows that
had shipped without being closed — the MCP/LSP composition gap, the `@validated` hover asymmetry,
the two unrecoverable process doors, and `std.tracing`'s active-span annotators (which shipped as
free functions rather than the `current_span()` the row proposed). Read a row's evidence before
picking it up: **the backlog closes rows late**, and the roadmap inherits whatever it says.

## Working discipline (unchanged)

- Work in slices; every behavior change lands with a conformance case (the iron rule).
- The differential oracle (Core-IR interpreter ↔ VM) and the leak gate stay green; fmt/clippy clean.
- Update `backlog.md` in the same commit that opens or closes a deferred item.
- When an arc grows past a few slices, give it a directory under `plans/<arc>/` with a README
  ledger; **delete the directory when the arc ships** (strike its backlog rows, move any new
  deferrals into `backlog.md`).
- `backend-mirror.md` is the standing VM ↔ reference-interpreter mirror inventory & policy — keep
  it current when touching mirrored code.
