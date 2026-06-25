# Destructor-ordering specification

The contract for **when** and **in what order** a `destruct` block runs, agreed *before* the
expanded-destruction implementation (Phase 3 places the drops; Phase 4 turns destructor-firing on at
them) so both backends target one spec and the differential proves identity by construction. This
generalizes the rule the two backends already share for top-level globals — see
`lang-eval`'s `destroy_globals`/`destroy_value` and `lang-vm`'s `execute` teardown + `release_value`.

This document is **normative**: where today's behavior is narrower (globals only), the spec states
the *target*, and the relevant phase cites the section it implements.

---

## 1. The observable surface

A `destruct` block is the **only** memory-management effect a program can observe. Allocation,
reference counting, reuse, and cycle collection are all invisible; a destructor running (its `echo`s,
its mutations of captured state) is not. Therefore the spec constrains exactly one thing: **the
sequence of destructor side effects**, which both backends must reproduce identically.

Everything else in the memory system is free to differ between backends (Rust `Rc` vs manual
refcount) precisely because it is unobservable. The leak oracle separately guarantees that *all*
values are eventually reclaimed (residency 0 at clean exit); this spec governs the *order and timing*
of the observable subset.

---

## 2. When a destructor runs

**Rule (last use of the last owner).** A destructor-bearing value's `destruct` runs at the point its
**last owning reference is released and the reference count reaches zero** — i.e. at the *last use*
of the last owner, in **any** scope: global, local, nested block, loop body, or function frame.

- This is the **generalization** of today's globals-only rule (where destructors run only when the
  top-level binding table is drained at program end). The target is: a local that holds the last
  reference runs the destructor when that local dies, not deferred to program end.
- **Aliasing defers.** If more than one reference is live (RC > 1), releasing one does **not** run the
  destructor; it runs only when the *final* reference is released. Two bindings to the same instance
  ⇒ one destructor, at the second binding's death. This is what makes destruction deterministic under
  sharing and identical across backends (both key on "is this the last reference?": eval via
  `Rc::strong_count == 1`, the VM via `refcount() == 1`).
- A value **moved out** (returned, or `?`-propagated — §6) is *not* destroyed at the move site; its
  ownership transfers and its destructor runs at the new owner's last use.

**Temporaries are owners too (Phase-4.4 gap).** "Last use of the last owner" includes an unnamed ANF
**temporary** that holds the last reference (a temp method/field receiver, a field projected out of a
container that then dies, a discarded expression result). Phases 4.1–4.3 fire destructors at the drop
points placed for *source variables*; a value that lives and dies entirely as a temp does **not** yet
fire its `destruct` (it is still reclaimed — no leak — just silently). This is a known deviation from
this rule, **consistent across both backends** (so the differential holds), closed by **Phase 4.4**
(`phase-4-destruction.md` §4.4), which extends last-use firing to transiently-consumed temporaries.

**Conservative timing (the safety invariant, README §2).** Static analysis chooses *where* to place
the release; the **runtime refcount decides whether it fires**. A statically missed/late release
costs promptness (the value lives to scope/teardown, still reclaimed — never a process leak) but is
always safe; a release placed *too early* would be a use-after-free and must be impossible by
construction. So when last-use is uncertain (unanalyzable flow), the release is **omitted**, never
guessed early. The static-≤-dynamic property test (Phase 3) machine-checks this direction.

---

## 3. Order within a scope: reverse construction (LIFO)

When several destructor-bearing values in the **same scope** die together (e.g. at scope exit),
their destructors run in **reverse order of construction** — last constructed, first destroyed.

- This matches today's global rule (`destroy_globals` drains in reverse declaration order; the VM
  iterates `global_order` reversed) and RAII convention (C++/Rust drop order, Swift).
- "Construction order" is the order values were *first bound/created* in that scope, not the order of
  last use. Reassignment (§5) does not re-order surviving bindings.
- The IR makes this structural: Phase 3 emits the scope's drops in reverse-construction order, and
  both backends honor that single ordering, so LIFO holds by construction rather than by two
  hand-matched iterations.

---

## 4. Fields vs container: container before contained

When an object with fields is destroyed:

1. The **container's own `destruct` runs first** (it may still read its fields — they are all still
   live at this moment).
2. **Then its fields are released**, depth-first, in **declared field order**. A field whose refcount
   reaches zero runs *its* `destruct` (recursively, by §2/§4), before the next field is released.

This is **container-before-contained**, the RAII-natural order: an aggregate tears down its own
invariant before its parts disappear, and the parts are exactly the structure `lang-value`'s
recursive `free` already walks (it releases `slots`/`data`/elements after the container box). Phase 4
makes the field destructors *fire* at that walk; the order is pinned here.

- **Declared field order** (not hash/storage order) is the tie-break for which field destructor runs
  first, so the two backends — eval's `BTreeMap<String, Value>` fields and the VM's shape-ordered
  slots — agree. Both iterate the **shape's declared field order**.
- Collections (list/map/set) release their elements in iteration order (index order for lists;
  sorted-key order for maps/sets — already canonical and identical across backends).

---

## 5. Reassignment

Reassigning a binding **immediately** destroys the value it displaced, **before** the new value takes
the slot (subject to §2 aliasing: only if the displaced value's last reference is the one being
overwritten).

- This is today's behavior and is retained verbatim (`lang-eval` `AssignOutcome::Assigned(displaced)`
  → `destroy_value`; the VM's `release_value` on the overwritten register/global).
- Ordering with the new value: the **displaced** value's destructor runs at the assignment point; the
  new value's destructor runs later, at *its* own last use. (Conformance: `mut x = R("first"); x =
  R("second")` ⇒ `close first` at the reassignment, `close second` at scope end.)

---

## 6. Early exits: `?`, `break`, `continue`, `return`, panic

As control leaves a scope by any path, the values **live at the abandoned point** are destroyed in
**reverse-construction order** (§3), exactly as if the scope ended normally there.

- **`return` / `?`-propagation:** the value being returned or propagated is **moved out** — it is
  *not* destroyed as the frame unwinds; its destructor runs at the caller's last use of it (§2). All
  *other* live locals in the abandoned frame *are* destroyed (reverse-construction) as the frame is
  torn down. `?` on an `Err`/`none` is the same: the `Err`/`none` value moves out to become the
  function's result; the rest of the frame is destroyed.
- **`break` / `continue`:** the loop-body scope's live values are destroyed at the boundary (a
  `continue` destroys this iteration's locals before the next iteration; a `break` destroys them and
  then the loop's own scope unwinds outward). Loop-iteration boundaries are destruction points.
- **Panic / abort unwinding:** an aborting program still destroys the live values in scopes it unwinds
  through, in reverse-construction order, so observable cleanup is deterministic up to the abort
  point. (The VM's abort path already releases every frame register; Phase 4 makes destructor-bearing
  ones *fire* their `destruct`.)

---

## 7. Reentrancy and cycles

- **Reentrant destructors.** A `destruct` may itself construct and destroy values (it runs like a
  parameterless method with the instance's fields and `self` in scope — `lang-eval`'s `destroy_value`
  builds exactly this scope). Values it creates follow the same rules; its own destruction completes
  before control returns to the site that triggered it. A destructor that resurrects `self` (re-binds
  it elsewhere) raises the refcount above zero again; the destructor having already run, it will run
  **again** at the new last use — programs should not rely on this, but the behavior is defined by
  §2 (RC-driven) and identical across backends.
- **Cycle-reclaimed objects (Phase 6).** When the cycle collector reaps a group of mutually-
  referencing objects, each still runs its `destruct` (the cycle made them unreachable ⇒ their last
  external reference is gone). Intra-cycle order has no construction-order LIFO to appeal to, so the
  collector pins a **deterministic tie-break** — allocation order (ascending) — and both backends
  use the same tie-break so the destructor sequence agrees. (Specified here; implemented in Phase 6.)

---

## 8. What each phase implements

| Section | Rule | Lands in |
|--------:|------|----------|
| §2 | last-use, RC-driven, all scopes | Phase 3 (placement) + Phase 4 (firing) |
| §3 | reverse-construction LIFO | Phase 3 (drop ordering on the IR) |
| §4 | container-before-contained, declared field order | Phase 4 |
| §5 | reassignment destroys displaced | already shipped; preserved |
| §6 | `?`/`break`/`continue`/`return`/panic | Phase 3 (move-out vs death) + Phase 4 |
| §7 | reentrancy | Phase 4; cycle tie-break Phase 6 |

The **differential oracle** proves both backends realize this spec identically; the **leak oracle**
proves nothing escapes it; the **static-≤-dynamic property test** proves §2's timing is never early.
