# Track I — lazy iterator protocol (the foundation)

Parent: `plans/coroutines/README.md`. **Status: IN PROGRESS** (branch `main`, per repo convention).
Decisions locked there: explicit Rust-style (`xs.iter()…` opt-in; eager `xs.map` untouched), PHP-ish
names (`Iterable.iter()` / `Iterator.next() -> ?T` / `collect()`), single-`next()` form.

## Representation decision (settled against the codebase 2026-06-30)

There is **no lang-source prelude** — every builtin (`map`/`filter`/`sum`/`len`/…) is a Rust-native
`Builtin`, and the only reference-semantic value today is `FileHandle(Rc<RefCell<FileHandle>>)` (eval)
/ a heap `Payload::FileHandle` (VM). So an iterator is a **native reference object**, built exactly on
the `FileHandle` template (the "first mutable heap value type"; an iterator is the second):

- **eval:** a new `Value::Iter(Rc<RefCell<IterState>>)`; **VM:** a new heap `Payload::Iter` reached via
  `with_iter_mut`, mirroring `with_file_handle_mut`.
- An iterator is **reference-semantic** (calling `next()` advances shared state) → a `class`, like
  `FileHandle`, not a value `struct`. No COW.
- The state is **per-backend** (it wraps the backend's own source `Value` + a cursor), *not* shared in
  lang-stdlib the way `FileHandle`'s string cursor is — because a list/set/map is represented
  differently in each backend. The semantics are trivial (advance a cursor), so the differential
  oracle guards equivalence; this is the `call_vec`-style parallel-impl pattern, not the shared-code
  pattern. Method dispatch routes through a shared `IterMethod` enum (exhaustive in both backends, like
  `FileHandleMethod`) so neither backend can silently miss a method.

## The hard part, and how the slicing dodges it first

`map`/`filter` must call a **user closure from inside `next()`**. The eager `map`/`filter` already
call closures (per element, inside one `Builtin` invocation) in both backends, so the mechanism
exists — but doing it inside a *method-dispatch* handler (`next()` on a heap object) in the register
VM needs validating. So:

- **Closure-free adapters first** (`take`/`drop`/`enumerate`/`zip`/`chain`) + terminals
  (`collect`/`count`/`sum`) — these need no closure, so they exercise the whole protocol +
  reference-object machinery with zero closure-from-`next` risk.
- **Closure adapters (`map`/`filter`) after** the closure-from-`next` mechanism is proven.

## Sub-slices (each its own green, in-oracle commit)

- **I.1a — the spine. ✅ DONE** (2026-06-30). `Value::Iter(Rc<RefCell<IterState>>)` (eval) /
  `Payload::Iter { list, cursor }` (VM, a GC **node** owning one child — its backing list — like
  `Cell`, not a leaf) + shared `IterMethod` (next/collect). `xs.iter()` on list/set/map → a cursor
  iterator (set/map first build a list of elements/values, the `for` order; a list shares its
  backing). `it.next() -> ?T` (`some`/`none`, advancing the shared cursor), `it.collect() -> List<T>`
  (drains the rest). Checker: `Iterator<T>` = `Type::Named("Iterator",[T])` (const `ITERATOR`,
  mirroring `FILE_HANDLE`); `iter()` on list/set/map returns it (map → value type); `next()`→`?T`,
  `collect()`→`List<T>` via `method_return`/`method_params`. VM refcounts are manual and
  miri-verified (`iter` retains its list; `iter_next` retains the element it hands out; set/map build
  a retained backing list then `release` the local ref). Conformance `iterators/spine.lang` (incl. an
  **alias-shares-cursor** check — the reference-semantics property). Differential 326/0-skipped/agree,
  leaks 0 both, miri clean. **No closures, no `for` change** (as planned).
- **I.1b — closure-free adapters + terminals.** Split into two for review:
  - **I.1b.1 ✅ DONE** (2026-06-30): `take(n)`/`drop(n)`/`chain(other)` + `count()`. `Payload::Iter`
    (VM) / eval `IterState` became an **enum** (`List` base + adapter variants), the fused-pipeline
    state machine — each adapter holds its source iterator(s) and pulls lazily, so `drop(2).take(2)`
    streams one element at a time with no intermediate list. GC: an iterator is a node owning one ref
    per source (`children()` enumerates 1–2). `iter_next` is now the recursive driver (an adapter
    advances its source — a distinct object, so the nested `with_payload_mut` is miri-safe). Refcounts:
    `take`/`drop`/`chain` retain the receiver (the `iter()` pattern); `drop` releases skipped
    elements; `count` releases each drained element. Checker: `take`/`drop`/`chain`→`Iterator<T>`,
    `count`→`int`; `chain` takes `Iterator<T>`. Conformance `iterators/adapters.lang`; differential
    327/0-skipped/agree, leaks 0 both, miri clean (a multi-source + drop-skip lang-value unit test).
  - **I.1b.2 ✅ DONE** (2026-06-30): `enumerate()` (→ `Iterator<(int, T)>`) + `zip(other)` (→
    `Iterator<(A, B)>`, stops at the shorter source, releasing a leftover element of the longer) +
    `sum()` (drains, `int` unless a `float` appears — mirrors the eager `sum` builtin exactly, errs
    `E0007` at runtime on a non-numeric element). New `IterState` variants `Enumerate { source, index }` /
    `Zip { a, b }` (both backends; `Zip` is a two-source GC node like `Chain`); `iter_next` builds a
    `Tuple` per step (the source's retained element + the immediate index transfer into it). `sum` is
    a `Value::iter_sum` terminal in lang-value (releases each drained element; on a non-numeric one
    returns its type name as `Err` for the backend's diagnostic) mirrored by the tree-walker inline.
    Checker: `enumerate`→`Iterator<(int,T)>`, `sum`→`int`/`float`/numeric-hole; `zip`'s param is
    `Iterator<dyn>` (accepts any `Iterator<B>`, rejects non-iterators) and its **precise**
    `Iterator<(A,B)>` result is assembled at the call site (`synth_call`) where both element types
    are in scope — `method_return` sees only the receiver. Conformance `iterators/tuple_adapters_and_sum.lang`;
    337 conformance / differential 328/0-skipped/agree, leaks 0 both, miri clean (a tuple-adapter +
    zip-leftover-release + sum-error-release lang-value unit test).
- **I.1c — closure adapters. ✅ DONE** (2026-06-30): `map(f)`/`filter(f)` as lazy `Iter`s that call a
  user closure from inside `next()` — the slice that validated the closure-from-`next` path. **The pull
  driver was restructured to thread an applier and hold no heap borrow across a source pull or a
  closure call.** lang-value's `iter_next` became `iter_next_apply<E>(self, apply: &mut dyn FnMut(Value,
  Value) -> Result<Value, E>) -> Result<Option<Value>, IterAbort<E>>`: each node reads its [`IterShape`]
  (child values + counters copied out) under a *short* borrow, then recurses / runs the closure with
  **no** borrow held, then writes any cursor change under another short borrow — so a user closure that
  re-enters the same iterator cannot alias a live `&mut` (UB) — and writes back the counter after. New
  `IterAbort<E>` enum (`Closure(E)` carries the backend's call error; `FilterNotBool(name)` a non-bool
  predicate verdict) the backend maps to its native error (`Abort` / `Unwind`). The VM applier is
  `|f,a| self.call_value(f, vec![a], span)` (reentrant call, the proven eager-`map` mechanism); the
  tree-walker mirror made `iter_advance`/`iter_value_next` `&mut self` methods that snapshot the same
  `IterShape` and call `self.call(...)`. Refcounts: `map` lets `apply` consume the source element and
  returns the result; `filter` retains the element once for the predicate call and hands it back or
  releases it (and on a closure error / non-bool verdict releases the held reference). Adapters own a
  reference to the closure (a GC child). Checker: `filter`→`Iterator<T>`, `map`→`Iterator<R>` (R = the
  closure's return, resolved at the call site like `zip`); params are typed (`map`: `Fn(T)->dyn`,
  `filter`: `Fn(T)->bool`, so a wrongly-typed *typed* closure is a static E0007; an untyped-param
  closure defers to the runtime check). Conformance `iterators/closure_adapters.lang` +
  `filter_predicate_not_bool.lang`; 339 conformance / differential 330/0-skipped/agree, leaks 0 both,
  miri clean (a map / filter / non-bool-error lang-value unit test with heap elements + a heap closure
  stand-in).
  - **Bench (`vm_iter_pipeline` / `vm_iter_take_pipeline`, honest):** full-drain `map→filter→sum` is
    ~14% *slower* lazy than the eager two-pass at n=1k (149µs vs 131µs) — the per-element iterator
    dispatch (shape read + recursion + per-step `call_value`) outweighs the saved allocations when
    everything is consumed (we have no monomorphization/inlining like Rust). **Laziness wins on memory**
    (O(1) extra vs O(stages·n) intermediate lists) **and on early termination**: with `take(10)` after
    `map→filter`, lazy is **~27× faster** at n=1k (4.8µs vs 131µs) and the gap widens with n (lazy is
    ~constant in n, eager O(n)).
- **I.2 — `for` over the protocol (optimization/unification, optional).** Rewrite the `for` lowering
  to drive `iter()`/`next()` instead of `iter_elements`' eager `Vec` materialization, so `for` over a
  lazy source streams. Keep a fast cursor for built-in collections. Tuple-destructuring `for (a,b)`
  rides along. Diagnostic budget: next free **E0039** (not-iterable / `next` must return `?T`).

## Verification (every sub-slice)

`cargo run -q -p lang-conformance` (+ `--differential` 0-skipped / agree, `--check-leaks` 0 both);
`cargo test --workspace`, clippy `--all-targets`, fmt; **miri when `lang-value` is touched** (I.1a adds
a `Payload`, so miri runs). Bench the perf claims (I.1b/I.1c). New conformance cases per slice
(round-trip, fused pipeline, early-stop `take`, empty source, the not-iterable error).
