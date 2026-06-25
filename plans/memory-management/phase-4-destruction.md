# Phase 4 — Expanded deterministic destruction

The one **observable** change in the track: `__destruct` runs at **last use** for locals, nested
scopes, and **object fields** — not just globals. Because Phase 3 put `drop`s at the same IR points for
both backends, this is now a matter of *turning destructor-firing on at those drops* — and it is
**correct by construction**, since both backends execute the same RC-annotated Core IR. This is the
payoff of the shared-IR foundation: the riskiest semantic change becomes a one-program change, not two
hand-matched interpretations.

## 4.1 The semantic change (per `destructor-order-spec.md`)

- A `destruct`-bearing value's destructor fires when its last owning reference dies (RC→0) at the
  IR-computed drop point, **in any scope** — generalizing today's globals-only rule.
- **Order:** reverse construction (LIFO) within a scope — guaranteed by the IR's drop ordering (Phase 3
  places drops in reverse-construction order; both backends honor it).
- **Container before contained:** destroying an object runs its `destruct`, then releases fields
  depth-first (a field reaching zero runs its own `destruct`). Pinned in the spec; both backends
  implement the same walk.
- Reassignment / `?` / `break` / loop-iteration boundaries destroy abandoned values in spec order.

This **adds observable output** for programs that previously leaked locals silently. The conformance
corpus is expanded; both backends must emit the identical new output (the differential proves it).

## 4.2 Mechanism (small, because Phase 3 did the placement)

- **VM:** the destructor-relevant drops (annotation from Phase 3) lower to the destructor-running
  release (`release_value`) for *all* scopes, not just globals; field release becomes the
  container-before-contained recursive destructor walk in `free`/`release_value`.
- **Tree-walker:** at each IR drop of a `destruct`-bearing value, run `__destruct` if it holds the last
  reference (`Rc::strong_count == 1`) — the **same condition, same IR point** as the VM. Field order
  matched to the spec. Rust `Rc` still frees the memory; the *observable* destructor already ran at the
  shared point.

Both backends therefore run destructors at the **same IR points, under the same runtime condition, in
the same order** → identical output by construction. The two differ only in the (unobservable) memory
mechanism (Rust `Rc` vs manual RC) — exactly the independence README §2 keeps.

## 4.3 Tests (this is where subtle bugs surface — and the oracle catches them)

New `tests/conformance/gc/`: local destruction order, nested-scope order, container-before-field,
loop-iteration destruction, `?`/`break` interactions, **reentrant** destructors (a `destruct` that
constructs/destroys), and **aliased** values (RC>1 → destruction deferred to the true last reference,
identical in both backends). Expect iteration; the differential + leak oracle are the proof.

## 4.4 Temp last-use destructor firing — DONE (the §2 completion exposed by 4.3)

4.1–4.2 fire destructors at the drop points the IR pass places for **source variables** (`DropVar`).
**ANF temporaries** — the unnamed single-use `let %n = …` intermediates — are reclaimed differently:
the IR-interpreter *moves* them (`Frame::take`) and the VM *retains-then-plain-releases* their
registers (at reuse / frame teardown). Neither path runs `__destruct`. So a destructor-bearing value
whose **last owning reference is an unnamed temp** never fires its destructor:

- a method/field receiver that is a temp — `Leaf.new("x").announce()` (the receiver `Leaf` is dropped
  with no `drop x`);
- a field projected out of a container that then dies while the projection is live —
  `b.inner.tag` (the `b.inner` temp outlives `b`, becomes the Leaf's last owner, and is plain-dropped);
- a discarded expression-statement result of a destructor-bearing type.

This is **pre-existing** (independent of 4.3) and **consistent across both backends** (the differential
stays green — both fail to fire identically), so it is a latent correctness hole, not a divergence. It
is a deviation from **spec §2** ("last use of the last owner … in *any* scope" — a temp is an owner),
and 4.3 makes it visible because container-field projection is a common way to create such a temp.

**4.4 closed it.** The gap is narrower than "every transient consumer": the failing cases are all
**receiver temps** (`Resource.new().use()`, the `b.inner` projected in `b.inner.tag`, `N.new(3).val()`)
plus **discarded bare-statement results** — function *arguments* already fire (they become named
params), and aggregate *elements* fire via the 4.3 container walk. So 4.4 fires a destructor at exactly
two new points, runtime-gated so non-destructor values are untouched:

- **Receiver temps of field / method / index access.** When the receiver atom is an `Atom::Temp` (owned,
  single-use by ANF — a named `Var`/`self` receiver is borrowed and fires at its own drop), the receiver
  is destroyed after the access. IR-interpreter: `call_method`/`eval_index` consume the receiver, so it
  is cloned for the call and the held copy is `destroy_value`d (last-reference-gated, so a method
  returning `self` defers correctly); `Field` borrows, so the receiver is destroyed after the read.
  VM: the compiler emits a destructor-aware `Op::Drop` of the receiver register after
  `CallMethod`/`LoadField`/`Index` (`drop_temp_receiver`) — the drop's read also keeps the receiver live
  across the access, so coalescing can't fuse it with the destination (same soundness lever as 4.3).
- **Discarded bare-statement results.** `Stmt::Drop(temp)` was a VM no-op and an eval silent slot clear;
  both now release destructor-aware (`Op::Drop{relevant:true}` / `destroy_value`), so `Resource.new();`
  fires at the statement.

`relevant: true` routes through `release_value`, which runtime-gates on reachability (a non-destructor
or immediate result/receiver just frees), so no static per-temp type channel was needed. Gates:
conformance 248 (+4 gc: temp_receiver_method, temp_field_projection, discarded_result,
temp_receiver_nested), differential 240/0-skipped/agree, leak/drop-audit/ir-corpus green, miri clean on
the firing paths, +1 VM unit test, 1 disasm golden (the discarded-`?`-result drop); fib hot-path golden
byte-identical (no temp receivers / bare statements there, so the dispatch hot path is unaffected).

## Verification gate

- Conformance (expanded) + **differential 0 skipped / agree** on the new destructor output.
- Leak oracle residency `== 0` via prompt destruction.
- miri on the recursive field-destruction / reentrancy paths.
- Bench: destructor-heavy workloads reclaim promptly; destructor-free code unchanged (plain-release path
  untouched for non-`destruct` values).
- The AST-walker reference oracle (kept since Phase 1) is now re-evaluated: it predates last-use
  destruction, so either teach it the new semantics or retire it here (decide; Phase 7 finalizes).
