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

- **I.1a — the spine.** `Value::Iter` / `Payload::Iter` reference object + `IterMethod`; `xs.iter()`
  on list/set/map → a cursor iterator; `it.next() -> ?T`; `it.collect() -> List<T>`. Checker types:
  `iter(): Iterator<T>`, `next(): ?T`, `collect(): List<T>`. Round-trip `xs.iter().collect() == xs`
  is the acceptance test. Mirrors `FileHandle` end to end (construction, dispatch, display, GC leaf,
  checker). **No closures, no `for` change.**
- **I.1b — closure-free adapters + terminals.** `take(n)`/`drop(n)`/`enumerate()`/`zip(other)`/
  `chain(other)` (lazy, each a wrapping `Iter`), and `count()`/`sum()` terminals. Bench the fused
  pipeline (`xs.iter().take(k)…`) allocates O(1) intermediate vs the eager O(n).
- **I.1c — closure adapters.** `map(f)`/`filter(f)` as `Iter`s that call the closure from `next()`;
  validate the closure-from-`next` path in the VM. Fused-pipeline alloc bench vs eager `map().filter()`.
- **I.2 — `for` over the protocol (optimization/unification, optional).** Rewrite the `for` lowering
  to drive `iter()`/`next()` instead of `iter_elements`' eager `Vec` materialization, so `for` over a
  lazy source streams. Keep a fast cursor for built-in collections. Tuple-destructuring `for (a,b)`
  rides along. Diagnostic budget: next free **E0039** (not-iterable / `next` must return `?T`).

## Verification (every sub-slice)

`cargo run -q -p lang-conformance` (+ `--differential` 0-skipped / agree, `--check-leaks` 0 both);
`cargo test --workspace`, clippy `--all-targets`, fmt; **miri when `lang-value` is touched** (I.1a adds
a `Payload`, so miri runs). Bench the perf claims (I.1b/I.1c). New conformance cases per slice
(round-trip, fused pipeline, early-stop `take`, empty source, the not-iterable error).
