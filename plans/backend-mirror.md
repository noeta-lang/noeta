# The VM ↔ reference-interpreter mirror — inventory & policy

`ARCHITECTURE.md` says routing/dispatch still mirrored between the two backends is "a known debt
tracked in `plans/`". Until this file, no plan actually tracked it (audit-1 Finding 9). This is
that ledger: what is mirrored, what of it is *irreducible*, what is *liftable*, and the standing
decision for each.

## The invariant (unchanged, deliberate)

The two backends may not share a value model — that is the differential oracle's entire power
(`Rc`-based enum vs. NaN-boxed words on a manual refcount heap, same RC-annotated Core IR,
byte-identical `RunResult`s). Any code that touches value *representation* is therefore mirrored
**by design** and stays mirrored. What both backends must agree on *semantically* lives once in
`noeta-stdlib` (exhaustive method enums, host IO, float formatting, marshalling) — that part of
the story already holds.

## Inventory (as of the 2026-07 audit)

| Mirrored piece | Where | Class | Decision |
|---|---|---|---|
| Ring-1 collection method bodies' *routing* | `noeta-vm/src/methods.rs` (~1.1k) ↔ `noeta-eval/src/lib.rs` (~900 lines, "Mirrors the VM's `call_list_method`") | Irreducible routing over each backend's own value repr; the method *names* are the shared exhaustive enums, the *semantics* are shared `noeta-stdlib` bodies | KEEP mirrored; the enums + differential are the guard |
| Async scheduler poll/scope cluster | `noeta-vm/src/scheduler.rs` (667) ↔ eval's poll/scope cluster ("both round-robin identically") | The round-robin *policy* is value-model-neutral; the task storage is not | LIFT CANDIDATE: the policy (ready-queue ordering, wake rules) could become a shared pure module in `noeta-stdlib`; low urgency, revisit if the scheduler grows another rule |
| `narrow_matches` (type-test narrowing) | `noeta-vm/src/lib.rs` ↔ `noeta-eval` (comment-linked) | The *decision table* (which TypeRepr matches which test) is neutral; only value→TypeRepr extraction differs | LIFT CANDIDATE: extract the decision table next time it changes |
| Channel FIFO + bounded-capacity + rendezvous + close/auto-close **policy** | `noeta-ext-abi/src/channel.rs` (surfaced as `noeta_stdlib::channel`), both backends call `poll_send`/`poll_recv`/`producer_left` | Policy is neutral; buffers are per-repr | **LIFTED** (isolates I.4c): the decision (send/recv action from scalar state + rendezvous `SendPhase`, and the producer-count→close rule) is shared; only the buffers and the producer-hold *counting* stay per-repr. In `noeta-ext-abi` (not `noeta-stdlib`) because the `SendPhase` tag rides a `noeta-value` `Payload` and the value model depends on the contract crate, not stdlib. Differential-pinned by the `async/channel_*` corpus |
| REPL trailing-expression desugar + sentinel | `noeta-vm/src/session.rs` ↔ `noeta-eval` (verbatim twins) | Pure AST rewrite — nothing value-model about it | LIFT (tracked in the audit's T9 DRY sweep: move to `noeta-ast::desugar`, both sessions import) |
| Value equality / fixed-width arithmetic | `noeta-value/src/ops.rs` ↔ `noeta-eval/src/ops.rs` | Irreducible (operates on each repr); drift history exists (maps-equality bug) | KEEP; guard corpus `equality_over_all_kinds.noe` + differential |

## Policy

- New backend-mirrored code must state its twin in a comment at BOTH sites (`// Mirrors …`),
  and add/extend a corpus case that would catch divergence.
- Anything in the table marked LIFT/LIFT CANDIDATE should be lifted the next time its logic
  changes — not speculatively.
- `noeta-eval` is dev-only (only `noeta-conformance` links it); do not grow production behavior
  there first.
