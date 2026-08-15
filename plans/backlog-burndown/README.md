# Arc — Backlog burndown (2026-08)

Status: **every slice is done** (2026-08-15). One follow-up remains — the 25 provenance-only rows below — and this directory is deleted once that is settled.

## How this list was derived

Every one of the 97 rows in [`backlog.md`](../backlog.md) was checked against the tree on 2026-08-12. Five had shipped without being closed and are now struck. **55 remain open; this arc claims thirteen of them, and the other 42 are examined in [Gates](#gates) below** — where the roadmap's claim that everything left is "decision-gated or trigger-gated" turns out to be half true.

They fall into two shapes, and neither is a feature request:

- **Correctness holes found while doing something else.** A visibility rule enforced on one door and not on its siblings; an identity that is qualified in one direction and short in the other. Each was measured when found, and each row carries its reproduction.
- **Doors that exist on one side only.** A registry endpoint with no client verb. A published feature whose output never names the thing a reader needs next.

Neither shape has a trigger to wait for, because the trigger already fired — that is *how they were found*. They sit in the backlog because the session that found them was doing something else and closed its own scope honestly.

## Gates

The 42 rows outside this arc are not touched by it, but they were examined, because "gated" is a claim that decays: a gate is only real if someone would notice it opening.

**25 of the 42 state no condition at all.** Their trigger column holds *provenance* — `native-otel metrics-logs, §deferred`, `keyless-signing v-next`, `p2p arc`, `crypto arc scope cuts` — which records where the row came from, not what would start it. That is not a gate; it is a deferral with a citation. Nothing about these rows will ever change on its own, and nobody is watching them. They need a condition written, or a decision to drop them.

**17 state a real condition**, and they divide by whether anyone is positioned to observe it:

- **Shipped since, on evidence this audit read wrong — 1.** Multi-source `From` waited for "a real pipeline funneling ≥2 error types into one wrapper", and this audit called `para/ai` that pipeline: it funnels `HttpError` and `JsonError` into `AiError` and maps `JsonError` by hand. **That reading was wrong, and re-reading the source is what showed it.** Those sites enrich with `out.target`/`out.name` — context a `from(value: JsonError)` never receives — and two of the three build an `AiError.Decode` from no `JsonError` at all; every `json.try_parse` there is a `match`, not a `?`. A second `From` impl could not have expressed one of them, so the workaround was never blocked work. The row shipped anyway (`d830d3d42`), on a condition that *is* observable — a backed enum's built-in `from` and a declared `impl From<Raw>` contended for one method slot, so declaring a conversion broke `Plan.from("free")`. **The lesson is the method:** a source comment naming a language constraint says the author met it, not that lifting it would change the code. Read what the workaround does before counting it as demand.
- **Fires as a diagnostic the moment someone writes the natural thing — 4.** Payload-carrying variant constructors (`map(Shape.Circle)` is `E0007`), trait-method generics (`E0058`), `From<Source>` as a bound, checked narrowing. These are sound gates *provided somebody is writing Noeta and reporting what they hit* — today that is this repo's own example and `test-*` apps.
- **Fires only if a profiling practice exists — 5.** The whole performance cluster (now held outright — see follow-up 3): "a workload demonstrating its value", "a checker profile showing clone cost", "a hot extern-method workload", "keyed-lookup profile", "packed bulk math in user code as a demonstrated bottleneck". Nobody profiles *user* workloads today; the instructions-retired ratchet guards the VM against regression, which is a different question. **These five cannot fire as written.** Either something starts measuring user-shaped programs, or they should say "not before real users" and stop pretending to be triggered.
- **Fires only with an ecosystem — 4.** User-code SIMD demand, "a plugin that can't be compiled in", an ORM-style consumer for `Members`/`DynamicCall`, a second `[suggests]` use case. Correctly YAGNI, and correctly out of reach until there are third-party authors.
- **Fires on a deliberate decision — 2.** Editions S3/S4 (needs a breaking change worth shipping), per-dialect migrations (needs a project targeting SQLite and Postgres at once).
- **Vague — 1.** REPL JIT says "demand", which is not observable. Give it a number or drop it.

Three follow-ups fall out of this, and none is in the slices below because they are bookkeeping rather than code:

1. ~~**Re-check multi-source `From`**~~ — done, and the answer was that the gate had *not* fired the way this audit claimed; see the corrected bullet above. It shipped on a different, real condition. `para-ai/ai.noe:287` still states the lifted rule as a live constraint and needs a follow-up in that repo — the workaround it describes stays correct, only its stated reason is now false.
2. **Give the 25 provenance-only rows a condition**, or drop them. A row nobody will ever notice is not a backlog item.
3. ~~**Decide what the performance cluster is really waiting for.**~~ — decided (2026-08-14): **held, and none of the five is a quick win.** They are a JIT loop-shape recognizer, monomorphic specialization, interning the checker's core `Type` tree, VM inline caches plus an ABI-facing argument projection, and a map-key probe adapter — each substantial, and none carrying a measurement that says which is worth doing. Their triggers now say so in `backlog.md` instead of naming a profile nobody runs. Any of them may be picked up early on a specific measurement that names it; absent that, doing one is a guess the instructions-retired ratchet would then lock in as the new floor.

## Slices

| # | Slice | Rows | Ready? |
|---|---|---|---|
| 1 | Visibility holds at the reflection boundary | 3 | yes |
| 2 | The oracles see what they claim to see | 2 | ✅ **done** |
| 3 | Doors that exist on one side only | 4 | yes |
| 4 | Two small identity guards | 1 | yes |
| 5 | Cancellation reaches blocking work | 2 | ✅ **done** |
| 6 | The fmt safety gate stops being a proxy | 1 | ✅ **done** |

**Slice 2 goes first.** It is the only one that changes what the *other* slices can prove.

---

## Slice 2 — The oracles see what they claim to see

Status: **done** (2026-08-12)

### Goal

Close the two places where a gate reports a pass over something it never examined.

### Scope

- In: the conformance single-file path not linking; the docs gate ignoring ` ```toml ` blocks.
- Out: any new oracle. This is about the two that already exist lying by omission.

### Why first

`run_case` (the in-memory single-file path) calls `run_source`; only `run_case_path` calls `run_linked`. Anything the loader's rewrite tables decide is therefore invisible to a single-file case — measured, `variants_of::<http.Framing>()` answers correctly under `noeta run` and `[]` in the harness, because `add_native_type_aliases` never ran. Every slice below lands conformance cases under the iron rule, and a case that cannot see linking is a case that can pass while the behavior it pins is broken. Fixing the harness first is what makes slices 1, 3 and 4 provable.

The `toml` half is the same failure in the docs gate: fenced ` ```toml ` blocks are explicitly ignored, so every manifest example in the wiki — the `[directives]`, `[targets.*]` and `[dependencies]` shapes a reader copies — is unverified prose. This is what let `[trust.commands]` and the tier live-set drift into the docs unchecked.

### Checklist

- [x] `run_case` links — through `noeta_loader::link` with an empty sibling pool; both paths now share `outcome_of_linked` and the divergent 87-line pipeline is deleted
- [x] `tests/conformance/reflection/native_enum_through_module_import.noe`, plus `linking_is_what_resolves_a_module_qualified_native_type` — the same source both ways, 3 linked against 0 unlinked, so the claim is measured rather than asserted
- [x] `doc_toml_blocks_are_valid_manifests`, with a ` ```toml ignore ` escape
- [x] a misspelled `[targests]` table fails the gate (verified, then reverted)

### Outcome

Both met. The corpus went 1193 → 1194 cases with **0 failures**, so linking every single-file case regressed nothing. The toml gate found **three real doc bugs** on its first run: an illegal package name (`acme/noeta-lint` — a hyphen is not an identifier) and two `[directives]` examples naming a provider their own `[dependencies]` never declared, one of them two lines below the sentence saying that is a manifest error.

---

## Slice 1 — Visibility holds at the reflection boundary

Status: todo (after slice 2)

### Goal

A private field is private through every door, not just the ones that were checked when the rule landed.

### Scope

- In: the three reflection/JSON doors that still cross the boundary.
- Out: the visibility rule itself (E0035/E0076/E0077 shipped and are not in question).

### The rows

- **`fields_of(value)` reads a private field's value** — verified by running it: on `class Box { pub label: string; secret: int }`, `fields_of(b)` from outside answers `[FieldEntry {name: "label", value: "hi"}, FieldEntry {name: "secret", value: 42}]`. `b.secret` is an error; the reflective read is not. This is the read half of the hole the construct-guards work closed on the write side.
- **A JSON decode of a native fielded struct sets its private fields** — `type_to_recipe` builds its `TypeRecipe::Struct` from `symbols.records`, which carries every field regardless of visibility, so `json.try_parse::<T>` writes a field a source literal may not (E0035) and `construct` now refuses.
- **A native fielded value's narrowing identity is its short name** — `d.as<std.http.Frame>()` answers `none` while `d.as<Frame>()` answers `some`, and `type_of` reports the qualified name. Consistent across both backends, so this is a decision to make once, not a divergence to chase.

Grouped because they are one question asked at three doors: *does the visibility rule survive the trip through reflection?* Answering it once, at the recipe/entry seam, is the fix; answering it three times is the bug returning.

### Checklist

- [ ] Decide the rule: does reflection observe visibility, and is the answer the same for `fields_of` (read), `json.try_parse` (write) and `as<T>` (identity)?
- [ ] Checker/runtime change at the shared seam, not per-door
- [ ] Conformance cases per door, negative cases included
- [ ] Both backends, differential green
- [ ] `docs/Attributes-and-Reflection.md` states the rule

### Done when

A private field is unreadable through `fields_of`, unwritable through a JSON decode, and `as<T>` accepts exactly one spelling of a native type's identity — each pinned by a conformance case that fails before the change.

---

## Slice 3 — Doors that exist on one side only

Status: todo

### Goal

Four places where a feature shipped without the surface a user reaches it through.

### The rows

- **`noeta rotate`** — `POST /v1/scopes/{scope}/rotate` shipped with the registry hardening and has no client verb; rotating a token today means calling the endpoint by hand. Promote its wire shape into the canonical fixtures while adding it, as the other registry verbs did.
- **`noeta add` should print the full import path** — it reports ``using import root `para` `` and leaves the reader to guess `use para.cli.{…}`, which is the quickstart's #3 friction point.
- **`noeta publish` never opens the tag it releases** — a publish that pushes a tag should be able to show you what it pushed.
- **Out-of-order migration detection** (para/db) — the runner gates history by checksum and deleted-file checks but says nothing when a newly added migration sorts *before* an applied one, which is what a rebase produces. A warning, deliberately not an error.

### Done when

Each has its verb or its line of output, with a CLI test driving the real binary; `noeta rotate`'s wire shape is in the canonical fixtures.

---

## Slice 4 — Two small identity guards

Status: todo

### Goal

Two places where a guard exists but is not applied on every path.

- **A path/git dependency may declare a built-in scope for itself.** `reserved::is_builtin` guards the registry *selector* and the import-root key, never the identity a local tree declares — so a tree saying `[package] name = "std/fs"` resolves without complaint. Low urgency (pointing a `path` at a tree is already a trust decision), but the guard exists and simply is not asked here.
- **The `@validated` / native-narrowing residual** from slice 1, if the identity decision leaves one.

### Done when

A local tree declaring a reserved scope is refused with the same diagnostic the registry selector gives, pinned by a `noeta-pm` test.

---

## Slice 5 — Cancellation reaches blocking work

Status: **done** (2026-08-15). Its design note is deleted with it, as `plans/README.md` prescribes.

### Goal

Close the last two places a cancel cannot reach, using the seam that already shipped for the third.

### Why now

The design note is finished and its two measurements are the load-bearing part: the headline example blocks on **our own `Condvar`**, not a syscall, so it needs a flag check and a `notify_all` rather than a self-pipe; and **ending the wait is not ending the work** — a `spawn_blocking` body outlives the runtime drop, so a leaf must *return* `Interrupted` rather than be abandoned (abandoning is what segfaulted the allocator, and there is a regression guard for it).

The trigger reads "a caller that must bound a hostile native read without killing the process". para/ai's MCP client is that caller — the row itself records it working around both holes in `mcp.noe`. The sibling row (`noeta test` cases parked in a long `sleep` leak a thread each) shares the `CancelWake` seam and should ride along: `CancelFlag` is still a bare `Arc<AtomicBool>`, which is the plumbing the row names as its only blocker.

### Slice order (from the design note)

process reads → sync http + streaming → `os_proc_wait` → `fs` (the last needs a stated decision between leaving it, chunking, and non-blocking opens)

### Done when

A worker blocked in a host read stops at the next safepoint after a cancel, with the leaf returning `Interrupted` rather than being abandoned; a timed-out `@test` case is joined rather than detached; the existing allocator-segfault regression guard stays green.

### Outcome

All three met, and the plan's slice order held: process reads, then sync http + streaming, then `os_proc_wait`, then the stated decision to leave `fs`. Two things the build found that the design did not.

**`Interrupted` had to be honored, not reported.** The plan's sentence — "the worker's very next safepoint turns it into the ordinary cancellation unwind" — was an assumption, and it was wrong: an `StdError` unwinds as a diagnostic, so the first end-to-end run reported `isolate panicked: read_line stopped…` and the parent's `join()` re-raised it. A *cancelled* worker was becoming a *failed* one. The fix belongs in `std_dispatch_error`, beside the identical interception `Exit` already had.

**That function was reached from 1 of 30 sites.** The other 29 spelled `self.error(stdlib_error_code(e.kind), span, e.message)` inline — [one rule spelled thirty times](../../AGENTS.md), and the reason the first fix appeared to do nothing. All of them now route through it, which also gives `Exit` the interception those sites silently lacked.

The `noeta test` half cost less than the row priced: keeping the flag as its own `Arc<AtomicBool>` *inside* `CancelSignal` rather than inline left the JIT's baked immediate untouched, so the "wants `--jit-differential --cancel-poll` as its gate" caveat did not apply.

---

## Slice 6 — The fmt safety gate stops being a proxy

Status: **done** (2026-08-15). Its design note is deleted with it.

### The decision, and the claim that did not survive checking

The arc framed this as: option B, or wait for a trigger — where the trigger on offer was an **open formatter defect** whose "whole class is what the proxy cannot see."

**That reading was wrong, and re-reading the code is what showed it.** `format_source` has two safety failures: the output fails to parse at all, and the output parses to a *different* AST. Option B replaces the second. The open defect is the **first** — a parse failure, caught before any AST comparison runs. The proxy did not miss it; it caught it loudly, and fmt declined. Option B would not have found it one minute sooner.

Reproduced and narrowed rather than taken on trust: still open at `feb13f8f7`, and the 40-line fuzz case reduces to `b = (match a { _ => a } << 0 >> 0)` under `[fmt] wrap = true` — one user-settable knob, `<<` followed by `>>` only, and only with a block-bodied `match` as the leftmost operand. It is a contained bug in the wrapped-binary-chain rule, and it is **not** what this slice is about. It stays open, as its own row.

The stopgap had also moved further than the note credited: every `Pretty` arm already bound every field by name, so the note's headline claim — *"the next field added to the AST starts un-rendered, and nothing fails"* — was no longer true either. A new field was already a compile error.

So B was taken on the argument that survives both corrections: **the failure modes are not symmetric.** A printer that forgets a field, or renders two values alike, makes the gate blinder — it approves a rewrite it should have refused, which is what happened twice. A walk that forgets a span makes it stricter — fmt declines and leaves the file untouched. The proxy can be wrong; the walk can only be incomplete.

### Outcome

`noeta_ast::normalize` + the derived `PartialEq`. Shaped as a private trait so the type system decides what carries a span rather than the author. Two risks were checked rather than assumed and neither materialized: decorator order is not in the AST (`Decorators` is a struct of named fields), and the whole corpus formats green under the strict gate in all three configs — so no canonicalization was hiding in the printer. The walk's own completeness is a corpus property (`every_span_is_erased_across_the_corpus`), ablated to confirm it bites.

---

## Working discipline

Unchanged, and it applies to every slice above: implement as a vertical slice, land conformance cases with each behavior change (the iron rule), keep the differential oracle and leak gate green, update `backlog.md` in the **same commit** that closes a row, and delete this directory when the arc ships — moving any new deferrals into `backlog.md` first.
