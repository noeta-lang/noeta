# SessionChecker — incremental type-checking at the prompt

**Status: PLANNED (recon complete, slices not started).** The follow-on `plans/repl-on-vm` §"Incremental
type-checking" sketched and `plans/tooling-unification` deferred: a persistent typing environment across
REPL/debug-console entries, so an entry gets real `E0xxx` diagnostics *before* it runs. Today both
prompts are deliberately checkerless — an ill-typed entry surfaces as a runtime message.

## Recon findings (what makes this tractable)

- **The checker has no global state** — one `Checker` instance per run, ~9.7k LOC, five clean phase
  methods: `register_prelude → collect → compute_relevance → check_semantic_roles → check_program`.
- **Fields partition cleanly**: ~23 registry fields that must persist across entries (types, fns,
  methods, traits, enums, records, destructor sets…), ~11 per-check scratch fields (current fn
  context, loop depth…), and 3 outputs (`diags`, `sites`, `relevance`).
- **`collect` is what makes forward references work** (register everything, then check bodies) — so
  per-entry checking is sound as long as each entry is collected before its own body checks;
  *cross-entry forward references correctly become errors* (you can't call what isn't defined yet —
  that IS prompt semantics).
- **`compute_relevance` is a whole-registry fixpoint** — must re-run over the accumulated registry
  per entry (an entry-2 `destruct` class can make an entry-1 type reachable). O(types²) worst case;
  trivial at prompt scale.
- **Site maps are `Span`-keyed and `Span` carries `SourceId`** — per-entry sources cannot collide,
  so the accumulated `Sites` bundle can be handed to any consumer whole (span lookups only ever hit
  the right entry).
- **Rebinding needs no REPL policy** (settled in plans/repl-on-vm): re-`mut x` re-declares (even
  retyped), bare `x = e` reassigns under the stability rules (E0006/E0007), only reserved names
  (E0046/E0049) refuse. The SessionChecker just runs the language's own rules against the
  accumulated env.
- **`check_program` builds a fresh global `Env` internally** — the one core refactor: it must accept
  a persistent env instead.

## Slices

| # | Slice | Delivers | Notes |
|---|-------|----------|-------|
| **C0** | The seam in noeta-check | `SessionChecker` (same crate — no fork): a persistent `Checker` + persistent global `Env`; `check_program` refactored to `check_program_in(&mut self, program, &mut Env)` (whole-program path passes a fresh one — behavior-identical, checked by the full suite); per-entry scratch reset; per-entry diagnostics drain. | The load-bearing refactor. Gate: all existing checker tests + conformance unchanged. |
| **C1** | `SessionChecker::check_entry` | `check_entry(&Program) -> CheckedEntry { diagnostics, sites }`: collect(entry) → re-fixpoint relevance over the accumulated registry → roles → check body against the persistent env → commit bindings/declarations. | Unit tests: within-entry forward ref OK; cross-entry use of an earlier fn/type OK; cross-entry forward ref errors; E0006/E0007 across entries; re-`mut` retype legal; E0046/E0049 reserved names. |
| **C2** | REPL opt-in (diagnostics gate) | `noeta repl --check` + `:check on/off`: each entry checked first; diagnostics render (session-relative spans — per-entry SourceIds already exist) and the entry is **skipped**; clean entries run exactly as today (codegen stays checkerless). | Deliberately NOT threading sites into REPL codegen — the diagnostics are the user value; optimized REPL codegen is C5 (optional tail). |
| **C3** | Debug-console checking | The launch's `check_all` becomes session-flavored (keep the `Checker` alive, exactly as T3 kept the `ModuleCompiler`); console fragments check before running — `E0xxx` at the console. Frame-local wrapper params bind as `dyn`/Unknown first (no false positives; runtime `TypeRepr` → checker-type refinement is a follow-on). | The console analogue of C2, riding the same `check_entry`. |
| **C4** | Oracle + default decision | Conformance: a checked-session differential — per-entry `check_entry` diagnostics vs a **whole-program re-check of the accumulated source** (the sketch's oracle). Then decide with the user whether `--check` becomes the default. | The behavior change users feel (entries can be rejected) stays opt-in until proven. |
| **C5** | *(optional)* Checked REPL codegen | Thread each entry's sites into a checked `extend` variant so the REPL gets site-driven codegen (packed lists, `type_of` fidelity). | Perf tail, not correctness; pick up only if REPL perf ever matters. |

## Risks / honest costs

- **C0 touches the checker's core loop** — mitigated by the refactor being shape-only (fresh-env
  callers unchanged) and the full conformance suite as the gate.
- **Cross-entry retyping vs. stale earlier compiles** (an entry that retypes `x` leaves an
  entry-1-compiled fn stale): inherent to REPL redefinition, same as the checkerless prompt today —
  documented, not solved here.
- **Frame-local types at the console start as `dyn`** (C3) — under-constrained (some errors won't
  be caught in expressions touching locals) but never wrong. Refinement via runtime `TypeRepr` is a
  natural follow-on once the plumbing exists.
