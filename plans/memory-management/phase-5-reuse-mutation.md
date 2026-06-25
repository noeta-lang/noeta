# Phase 5 — Generalized reuse + mutate-when-unique

With prompt reclamation (Phase 3) and reuse tokens represented on the IR, make reuse **systematic** —
every constructor — as an IR pass, and unify mutable state with reuse: in-place mutation *is* reuse of
a uniquely-owned cell.

## Sub-slices

The phase is sequenced as committable slices. The reuse-token machinery did **not** exist coming into
Phase 5 (Phase 3 shipped only `liveness` + `drops`; the in-place VM ops `MakeRecordInPlace` /
`ConcatInPlace` / `TakeGlobal` were retained-but-dead, emitted by nothing). So 5.1 *builds* the
reuse-analysis pass and wires it to both backends, starting with the case the retained ops already
cover.

- **5.1a — record/class self-update reuse, function-local (DONE).** The reuse-analysis pass
  (`lang-ir-passes/src/reuse.rs`, `thread_reuse`) marks a self-update `acc = Type { ...acc, f: v }`
  (an `Rvalue::Object` whose spread base is the very binding the next `Bind` reassigns) with a new IR
  token `Rvalue::Object.reuse`. Run after `insert_drops` in all three production pipelines (compiler,
  `reference_run`, `lang run`) — a pure function of the IR, so the VM and the IR interpreter consume
  identical tokens and agree by construction. Both backends consume the token when the spread base is
  a directly-held **local**: the VM emits `Op::MakeRecordInPlace { check: Runtime }` (the base register
  *is* the local's sole storage, so the op's move-out sees refcount 1 on the unique path and an alias
  copies); the IR interpreter moves the accumulator out of its binding (`take_mut`) and mutates via
  `Rc::get_mut` when unique, else copies. **Soundness beyond the runtime refcount:** reuse skips the
  old allocation's *own* `destruct`, which the copy-and-destroy baseline runs every update (spec §5),
  so the pass **excludes own-destructor types** (records never have one; classes iff they carry a
  `destruct`), derived from the IR's class declarations. The displaced *changed* field still fires its
  destructor on overwrite — VM via a new `Value::replace_slot` (retain-new, return-old-unfreed) routed
  through `release_value`, the IR interpreter via `destroy_value` on the displaced field — so reuse is
  observationally identical to copy-and-destroy and the differential stays in agreement.
  **Measured (vm_record_update_read, 8-field local read-update accumulator): ~3.1× (159 → 51 ns/iter),
  O(n) vs the copying O(n·fields).** Conformance 250 (+2 gc: replaced-field-destructor fires,
  own-destructor-excluded), differential 242/0-skipped/agree, leak unchanged (the one known Phase-6
  closure cycle), drop-audit 0, ir-corpus total, miri clean on the in-place + replaced-field paths,
  clippy+fmt clean, +3 VM unit tests, no golden churn (no golden self-updates).
- **5.1b — global accumulator reuse (`TakeGlobal` + `MakeRecordInPlace`), list self-append
  (`ConcatInPlace`) (DONE).** A top-level `mut acc` is a global, not a register; 5.1a marked the
  `reuse` token but the compiler fell back to a copying `MakeRecord`/`Op::Binary` for a global (it
  handled directly-held locals only). 5.1b (1) **threads the reuse token onto the list self-append**:
  a new `Rvalue::Binary.reuse` bit, set by `thread_reuse` on a `let %t = Var(acc) ~ rhs` immediately
  followed by `acc = %t` — but only when `rhs` does not mention `acc` (else the right side would read
  the moved-out slot; lists need no own-destructor exclusion since a concat destroys no element). (2)
  **Consumes the token for a global base in both backends**: the compiler emits `TakeGlobal` (move the
  global out so the in-place op sees unique ownership; the trailing reassignment stores the result
  back) ahead of `MakeRecordInPlace` (record update) or `ConcatInPlace` (list append), with the
  field/rhs operands resolved *before* `TakeGlobal` so a `g = T { ...g, x: g }` / `g ~= g` loads the
  live value first. The IR interpreter's record path already took the base by name (`construct_object_reuse`
  works for globals as-is); its concat path now mirrors via `take_mut` + the shared `cow_concat`. The
  same `Runtime` refcount check (not `Static` — no linearity analysis here) gates reuse, so an alias
  (`snap = g`, `h = g`) copies and preserves the other owner's view. This **re-emits all three retained
  ops** (`TakeGlobal`, `ConcatInPlace`, the global `MakeRecordInPlace`); the standing
  delete-if-unused obligation is discharged. **Measured (VM, both global accumulators):**
  `vm_accumulate` (list self-append, `acc ~= [i]`) **O(n²) → O(n)** — −83% at n=1000 up to **−97%
  (~31×) at n=8000**, the gap widening with n; `vm_record_update` (8-field global blind-overwrite)
  **~2.6× constant-factor** (−60–64% across sizes; alloc + copy-8 → `TakeGlobal` + overwrite-1
  in-place; both already O(n)). The local read-update path (`vm_record_update_read`, 5.1a) is
  unchanged (p>0.05 — no regression). Conformance 251 (+1: heap-element global self-append under the
  differential), differential 243/0-skipped/agree, leak 0 (known cycle only), drop-audit 0, ir-corpus
  total, miri clean on the in-place + `TakeGlobal` + replaced-field paths, clippy+fmt clean, +2 VM
  disasm tests (`global_self_update_lowers_to_take_global_plus_in_place_reuse`,
  `self_append_lowers_to_in_place_concat`). No golden churn (the `reuse` marker renders only when set).
- **5.1c — map/enum constructors.** No retained in-place op exists for these; reuse needs net-new VM
  ops + miri validation. Lower priority — decide value vs cost before building (surface as a decision
  point, don't silently drop).

## 5.1 Reuse for all constructors (IR pass)

Generalize reuse-token consumption from the P-REUSE record/list cases to **list, record, enum, and
map** constructors. The reuse-analysis pass threads a token when a uniquely-owned value's death
precedes a same-shape constructor (5.1a: the record self-update); this phase makes every constructor
lowering *consume* an available token to reuse the allocation in place.

- **Subsumes and retires** the hand-rolled COW (`ConcatInPlace`/`TakeGlobal`) and targeted record reuse
  (`MakeRecordInPlace`) detection — now instances of one IR transformation, not syntax-matched special
  cases. (Keep the in-place *ops* the VM needs; remove the bespoke *detection*.)
- Runtime `RC == 1` stays the safety net (Lean-style, measured-justified): the token says *where to
  try*, the check says *whether it's safe this run*. A wrong token → a copy, never a bug.
- Both backends consume the same tokens: VM via in-place ops; tree-walker via `Rc::get_mut` /
  make-unique at the IR point.

## 5.2 Mutate-when-unique (mutable fields & `FileHandle`)

The language has `mut` fields (grammar-present, not yet semantic) and a mutable `FileHandle`.

- **5.2a — `mut` field assignment `x.f = v` (DONE, `472faed`).** `mut` fields became semantic: a
  `mut` field of a class is assignable (plus `+=`/…/`??=`), with **value semantics** — in place when
  the instance is uniquely owned (`RC == 1`), copy-first when shared, so an aliased observer keeps its
  value. The desugar `x.f = v` ⟶ `x = SetField(x, f, v)` reuses the Phase-5 reuse machinery (new
  `Rvalue::SetField` / `Op::SetField` / `replace_slot`-in-place; the reuse pass marks the self-update,
  for a local *or* a `TakeGlobal`-moved global, guarding `x.f = x` to the copy path so the field is
  the pre-assignment value, not a self-cycle). New **E0033** for assigning a non-`mut`/record/missing
  field; the value is checked against the field type. **Foundational decision (confirmed with the
  user):** class instances are value-semantic-with-COW — the "reference type" wording in
  architecture §35 is a *representation* note (heap-allocated), consistent with structural equality
  and the 5.1 reuse, not aliasing semantics. Bench `vm_field_assign` ~1.8× (8-field class), constant
  factor ∝ field count. Conformance 262, differential 254/0/agree, leak clean, miri clean.

- **5.2b — `FileHandle`: a reference type, by design (DONE — decided, no backend change).** The plan
  originally proposed making `FileHandle` "a uniqueness-managed value like any other" and retiring the
  `Rc<RefCell<…>>`. Investigation (with the user) found this conflicts with the handle's nature: a
  handle is mutated **through a method call on an immutable binding** (`reader = fs.open(...);
  reader.read_line()` advances the cursor across calls), which is *inherently* reference-semantic.
  Value-semantics-COW would break the streaming API — once a handle is aliased, the cursor advance
  would be lost to a discarded copy — or require deep-copy-on-every-assignment, a *bigger* carve-out
  than the `Rc<RefCell>`. Every COW/immutability-first language draws the same line: data is value-
  semantic, **resources (files/sockets/streams) are reference types or effect handles** — Swift makes
  the byte buffer (`Data`) a COW value but `FileHandle` a reference *class*; Haskell's `Handle` lives
  in `IO`; Clojure/OCaml use host references; Erlang a process; Roc/Koka thread state functionally
  behind effects. **Decision: handles keep reference semantics** (option A). No backend change — both
  backends already mutate the handle in place and agree; eval's `Rc<RefCell>` is the correct minimal
  encoding of that interior mutability in safe Rust (not a carve-out), and the VM's heap-cell mutation
  is its ordinary write path. Deliverable: pin the reference semantics under the differential
  (`tests/conformance/std/fs_handle_alias.lang` — aliasing shares the cursor) and document the
  rationale (handle module doc). If handles ever become value-semantic, the consistent path — per
  Swift/Rust — is a `mut` binding + a mutating-receiver method (`mut self`), **not** COW.

- Note (cycles): `mut` fields make object *cycles* possible (`node.next = node` falls to the copy
  path, but a built-up cycle still leaks under refcounting) — reclaimed by the Phase-6 cycle
  collector, as planned.

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
