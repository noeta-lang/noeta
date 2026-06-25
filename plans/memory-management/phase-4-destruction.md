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

## 4.4 Temp last-use destructor firing (the §2 completion exposed by 4.3)

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

**4.4 closes it** by extending last-use destructor firing to temporaries: a temp whose single use does
**not** transfer ownership into a persistent location (a binding, a container, a return) gets a
destructor-aware drop at that use — the VM via a `release_value` (not plain `release`) at the consuming
op / register overwrite, the IR-interpreter via `destroy_value` on the consumed-and-discarded value.
The 4.3 construction-temp release (the compiler's `consume_operand`/`release_consumed`, which makes an
aggregate the sole owner of an inline-built element) is the same idea applied to *constructor* operands;
4.4 generalizes it to *every* transiently-consumed temp. Tests: `Leaf.new("x").method()` fires `drop x`;
`b.inner.tag` fires the projected field's destructor at the temp's death; differential agrees.

## Verification gate

- Conformance (expanded) + **differential 0 skipped / agree** on the new destructor output.
- Leak oracle residency `== 0` via prompt destruction.
- miri on the recursive field-destruction / reentrancy paths.
- Bench: destructor-heavy workloads reclaim promptly; destructor-free code unchanged (plain-release path
  untouched for non-`destruct` values).
- The AST-walker reference oracle (kept since Phase 1) is now re-evaluated: it predates last-use
  destruction, so either teach it the new semantics or retire it here (decide; Phase 7 finalizes).
