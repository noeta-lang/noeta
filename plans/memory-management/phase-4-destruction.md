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

## Verification gate

- Conformance (expanded) + **differential 0 skipped / agree** on the new destructor output.
- Leak oracle residency `== 0` via prompt destruction.
- miri on the recursive field-destruction / reentrancy paths.
- Bench: destructor-heavy workloads reclaim promptly; destructor-free code unchanged (plain-release path
  untouched for non-`destruct` values).
- The AST-walker reference oracle (kept since Phase 1) is now re-evaluated: it predates last-use
  destruction, so either teach it the new semantics or retire it here (decide; Phase 7 finalizes).
