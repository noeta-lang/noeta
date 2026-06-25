# Phase 5 — Generalized reuse + mutate-when-unique

With prompt reclamation (Phase 3) and reuse tokens represented on the IR, make reuse **systematic** —
every constructor — as an IR pass, and unify mutable state with reuse: in-place mutation *is* reuse of
a uniquely-owned cell.

## 5.1 Reuse for all constructors (IR pass)

Generalize reuse-token consumption from the P-REUSE record/list cases to **list, record, enum, and
map** constructors. The Phase-3 pass already threads a token when a uniquely-owned value's `drop`
precedes a same-shape constructor; this phase makes every constructor lowering *consume* an available
token to reuse the allocation in place.

- **Subsumes and retires** the hand-rolled COW (`ConcatInPlace`/`TakeGlobal`) and targeted record reuse
  (`MakeRecordInPlace`) detection — now instances of one IR transformation, not syntax-matched special
  cases. (Keep the in-place *ops* the VM needs; remove the bespoke *detection*.)
- Runtime `RC == 1` stays the safety net (Lean-style, measured-justified): the token says *where to
  try*, the check says *whether it's safe this run*. A wrong token → a copy, never a bug.
- Both backends consume the same tokens: VM via in-place ops; tree-walker via `Rc::get_mut` /
  make-unique at the IR point.

## 5.2 Mutate-when-unique (mutable fields & `FileHandle`)

The language has `mut` fields (grammar-present, not yet semantic) and a mutable `FileHandle`. Lower
mutation to **in-place when unique, copy-first when shared** — the *same* uniqueness machinery as reuse:

- `x.f = v` (and handle-cursor advance) mutate in place iff `x` is uniquely owned at that IR point
  (uniqueness fact + runtime `RC == 1`); otherwise copy `x` first, preserving value semantics for any
  aliased observer. O(1) on the common unique path, correct under aliasing.
- `FileHandle` becomes a uniqueness-managed mutable value like any other — retire the `Rc<RefCell<…>>`
  shared-mutable carve-out in eval and the special cell handling in the VM; mutation and reuse are one
  pass, one runtime check.
- Payoff of co-design: in-place reuse and in-place field mutation are the **same** IR/runtime path;
  mutability never surfaces as shared state. (This is also what first makes object *cycles* possible —
  hence cycles are the next phase.)

## 5.3 Cross-backend agreement

Mutation/reuse are invisible to observable behavior (value semantics: a shared observer always sees the
old value because the shared path copies). The only observable interaction — destructor timing of a
replaced field value — is already pinned by the spec + Phase 4. Differential stays green by construction.

## Verification gate

- Conformance + **differential 0 skipped / agree** (new tests: aliased mutation preserves the alias;
  unique mutation O(1); reuse across all four constructors; handle mutation under aliasing).
- Leak oracle `== 0`; static-≤-dynamic property test green.
- miri on every in-place mutation path (unique + copy-fallback, heap-bearing fields — retain/release
  accounting). clippy + fmt clean.
- Bench: the reuse matrix vs the P-REUSE numbers (match/beat — now general, no per-pattern gaps);
  mutable-field micro-bench (unique O(1) vs shared copy) vs an always-copy baseline.
