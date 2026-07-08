# Higher-order native ABI — plan (final shape)

**Goal.** Make higher-order / effectful / **stateful** native functions registrable through the
extension ABI, and let extensions contribute CLI commands. Migrate the whole hardcoded `Builtin`
family out of core into the dogfooded std extension: `sleep`/`all`/`race`/`map_bounded`
(std.task), `signal`/`computed`/`effect` (std.reactive, **full migration** — graph and all),
`serve` (std.http), and the `noeta serve` command. Recorded as the follow-on flagged during the
http-server arc (`docs/Native-Extensions.md` Deferred; memory `higher-order-native-abi`).

**Scope decision (with the user, 2026-07-07):** build the persistent-state machinery (Class 3
below) NOW and iterate on it internally with reactive as the proving client — not at the
package-manager milestone. That milestone is where the ABI gets *frozen*; designing it there
means designing blind. Every existing seam (Host, ExternIo, extern types) was proven
first-party-first the same way. Churn is acceptable at this stage.

## The gap, precisely

The registry dispatch is a plain fn — value-in, value-out, plus the `Host`:

- `ModuleDispatch = fn(func, &mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>`
  (`noeta-native/src/registry.rs:223`). `NativeValue` has no closure variant and cannot have one
  (a closure is backend-specific: eval's `Closure` struct over `Rc` envs vs the VM's NaN-boxed
  `Payload::Closure { proto, upvalues }`) — and even an opaque carry would be useless, since the
  dispatch has no way to *call* it.
- Async exists only as the leaf `NativeOut::Spawn(ExternIo)` — one descriptor per call. No way to
  interleave many spawns, poll language-level futures, or advance the scheduler mid-dispatch
  (what `serve`'s reaping loop and `map_bounded`'s window need).
- No extension can own **language values across calls**: extern-type state must be plain Rust
  data (`ExternValue: Send`, `extern_value.rs:27`; backend values are not Send), method args and
  results marshal **by value** through `NativeValue`/`NativeOut`, and nothing extension-held is
  visible to the refcount discipline, the leak oracle, or the cycle collector. This is why the
  reactive graph is a backend field (`Rc<ReactiveGraph<Value/GcVal>>` in each backend).
- `ExtType` is monomorphic — no `Signal<T>`-shaped extern types; the checker's generic returns
  for the family are hand-written (`noeta-check/src/stdlib.rs:590+`: `signal(v: T) -> Signal<T>`,
  `all(List<Future<T>>) -> List<T>`), beyond `SigType`'s vocabulary.
- The CLI `Command` enum (`noeta-cli/src/main.rs:45`) is a closed clap enum; `noeta serve` had to
  be a core command.

So every such function is a hardcoded `Builtin` (`noeta-bytecode/src/lib.rs:64`), implemented
**twice** — once per backend, each with hand-maintained refcount discipline (the source of both
leak bugs during the http-server arc). Routing today: bare prelude names and selective
virtual-module imports compile to `Op::CallBuiltin` (`noeta-compiler/src/lib.rs:1724,2501`);
qualified calls intercept in each backend's `call_native_module` ahead of registry dispatch
(`noeta-eval/src/lib.rs:2707`, `noeta-vm/src/methods.rs:328`); name resolution is gated by
`VIRTUAL_MODULES` (`noeta-stdlib/src/registry.rs:220`).

## The two capability classes this arc builds

**Class 2 — orchestration (per-call).** Functions that call closures back, interleave futures,
and drive the scheduler, but own nothing past the call: `map_bounded`, `all`/`race`, `serve`.

**Class 3 — persistent ownership.** Extensions that hold language values/closures **across
calls** and run them later: reactive is the first instance; a future third-party collection
(`Tree<K, V>` storing values by reference) or ORM (`Query<User>`, lazy relations as retained
closures) is the same shape. Requires per-run extension state + retained value handles with
RC/GC integration, and generic extern types.

(Class 1 — stateless per call — already exists: pure/host fns, Rust-data extern types, ExternIo.)

## Design

### Seam 1: `NativeCtx` — opaque value slots + call-back capability (Class 2)

A second dispatch form. The extension never sees a backend `Value`; it manipulates **opaque
slots** and re-enters the backend through one capability trait:

```rust
// noeta-native
pub type Slot = u32;                       // index into the per-call slot table
pub trait NativeCtx {
    fn host(&mut self) -> &mut dyn Host;
    // values
    fn view(&mut self, s: Slot) -> NativeValue;          // marshal on demand (shallow/deep as today)
    fn intern(&mut self, v: NativeOut) -> Slot;           // materialize a neutral value into a slot
    // closures
    fn call(&mut self, callee: Slot, args: &[Slot]) -> Result<Slot, StdError>;
    // async
    fn spawn_io(&mut self, io: Box<dyn ExternIo>) -> Slot;      // an ExternIo future, as a slot
    fn timer(&mut self, ms: u64) -> Slot;                       // a leaf timer future (sleep)
    fn poll(&mut self, future: Slot) -> Result<Option<Slot>, StdError>;  // one poll; None = pending
    fn advance_tasks(&mut self) -> Result<bool, StdError>;      // one poll_all_scopes_round
    fn advance_clock(&mut self) -> Option<u64>;                 // executor.advance()
    // persistence (Class 3, H4)
    fn state(&mut self, ext: ExtensionId) -> &mut ExtState;     // per-run extension state + arena
}
pub type CtxDispatch = fn(func: &str, ctx: &mut dyn NativeCtx, args: &[Slot])
    -> Result<CtxOut, StdError>;           // CtxOut = Slot | NativeOut
```

`ExtModule` grows an optional `ctx_dispatch` + `ctx_functions: &[ExtFn]`; the backends route a
matched name there before the plain dispatch. Each backend implements `NativeCtx` once — a
temporary wrapper over `&mut Interp` / `&mut Vm` holding the slot table. **The slot table owns
the refcount discipline centrally** (VM: retain on insert, release all on drop), so a migrated
function cannot leak the way the hand-written `Builtin::Serve` twice did. The dispatch stays a
single shared fn in `noeta-stdlib` → the differential holds by construction — the registry's
core promise, extended to orchestration code that was previously mirrored.

Rejected alternative: a state-machine/descriptor protocol (extension returns a resumable program
the backend steps — generalizing `ExternIo`). Equivalent power, but every extension function
becomes a hand-written state machine; the ctx approach keeps them straight-line Rust.

### Seam 2: signature vocabulary for the checker

`SigType` gains `Fn(&'static [SigType], &'static SigType)`, `FutureOf(&'static SigType)`, and
type variables `Var(u8)`; `noeta-check`'s SigType→Type mapping learns to bind vars from argument
types (and, for methods, from the receiver's type args) and substitute into the return. Covers
`all(List<Future<A>>) -> List<A>`, `map_bounded(List<A>, int, Fn(A) -> Future<B>) -> List<B>`,
`signal(A) -> Signal<A>`, `Signal<A>.get() -> A`. The hand-written `module_return` arms for
task/reactive and the `http.serve` special case are then deleted — the registry becomes the
single source of truth for these too.

### Seam 3: per-run extension state + retained handles (Class 3)

The key structural rule, forced by `ExternValue: Send` vs non-Send backend values, and mirroring
how reactive already works: **extension-held language values never live inside extern boxes.**
An extern value carries only plain data (ids); the values live in **per-run extension state** —
a backend-side arena reached via `ctx.state()`:

- `ExtState` = the extension's own `Box<dyn Any>` Rust state **plus a retained-value arena**:
  `retain(Slot) -> Retained`, `release(Retained)`, `get(Retained) -> Slot`. The backend impl
  keeps the arena's refcounts exact (retain on store, release on free).
- The arena is an **enumerable root set**: the leak oracle counts it and the cycle collector
  walks it (exactly how `ReactiveGraph::for_each_value`, `noeta-reactive/src/lib.rs:568`, is
  treated today) — no tracing through opaque boxes ever needed.
- Teardown: all extension arenas are released before heap teardown (as the VM clears the
  reactive graph today, returning residency to 0).
- Isolates: retained handles are per-isolate; extern values referencing per-run state are not
  `Wire`-able across isolates (as signals already must not cross).

**No scheduling hook is needed.** Reactive's flush is *synchronous inside `.set()`* —
`drive_flush` runs before `set` returns, coalescing via an `is_flushing` flag (reactivity S4).
Extension-side, the flush loop is ordinary Rust inside the `set` dispatch calling bodies via
`ctx.call`; coalescing and the E0045 runaway guard become extension state and logic.

### Seam 4: generic extern types + ctx-form method dispatch

`ExtType` gains type parameters (arity + `SigType::Var` in method signatures binding against the
receiver's args); reified generics carry extern type args in `TypeRepr` (as containers/user
types already do) so `x is Signal<int>` stays precise. A ctx-form `TypeDispatch` twin lets
extern methods take closures, call them, and reach `ctx.state()` — `Signal.update(f)` needs all
three.

### Seam 5: extension commands

```rust
pub struct ExtCommand { name, about, args: &'static [ArgSpec], run: fn(&mut dyn CommandCtx, &ParsedArgs) -> Result<(), StdError> }
trait Extension { … fn commands(&self) -> &'static [ExtCommand] { &[] } }
```

`CommandCtx` is a narrow *driver* capability — exactly what `cmd_serve` does today: load a file,
check it, optionally synthesize a trailing entry call (`http.serve(<port>, fetch)`), run on the
real host. The CLI builds clap subcommands dynamically from `ArgSpec` (positional file + typed
flags) and dispatches unmatched names to registered commands (`cargo clippy` model).

## Phases

- **H0 ✅ DONE** — `NativeCtx` + slots in `noeta-native` (`ctx.rs`); both backend impls
  (`native_ctx.rs` in each — the VM's slot table owns the RC discipline); `ctx_dispatch` routing
  in both `call_native_module`s (per-function virtual intercept, ctx arm after the plain table);
  `ExtModule::{ctx_functions, ctx_dispatch, DEFAULTS}`; checker/import gating via
  `find_function_sig`. **Dogfood = `task.sleep` migrated end-to-end** (`SigType::Future` already
  existed, so no typing prerequisite): `Builtin::Sleep` deleted everywhere, shared dispatch in
  `noeta-stdlib/src/task.rs`. Full gate green (differential, leak-0, corpus, workspace suites,
  real-host smoke).
- **H1 ✅ DONE** — `SigType::Fn(params, ret)` + `SigType::Var(u8)` (`Future` already existed —
  H0 finding); checker bind-and-substitute: `bind_params` walks declared params structurally
  against the call's argument types (through `List`/`Option`/`Map`/`Future`/`Fn`/`Optional`),
  binding each variable at its **first determined occurrence** (holes never bind);
  `sig_to_type_bound` substitutes into the remaining params (a repeated variable becomes a
  concrete expectation the ordinary argument check enforces) and the `Concrete` return. Unbound
  variable ⇒ gradual hole, never a wrong type. `module_params` now takes the arg types.
  Behavior-neutral for every var-free signature; semantics pinned by unit tests in
  `noeta-check/src/stdlib.rs` (first-occurrence-wins on a mismatched closure; empty-list-literal
  hole defers to a later concrete occurrence). Workspace green, clippy silent.
- **H2 ✅ DONE** — `all`/`race`/`map_bounded` migrated: one shared drive loop each in
  `noeta-stdlib/src/task.rs` (first users of H1's `Fn`/`Var` signatures — the checker recovers
  `T` by substitution; `future_elem` + the hand-written task arms deleted). `NativeCtx` grew
  `cancel`, `wake_generation`/`wait_external_wake` (the VM's isolate-wake term in the deadlock
  condition; the tree-walker returns constant/false = its old condition), `is_list`/`type_name`
  (message-parity validation), `call_with_element` (fused per-item call), and two ownership
  refinements: `poll` **spends** the future slot on Ready (result takes over its index in
  place), `make_list` **consumes** element slots. `ErrorKind::Panic` + `panic_error` carry the
  deadlock/empty-race panics. Deleted: `Builtin::{All,Race,MapBounded}` (both enums, all six
  arms), the task `VIRTUAL_MODULES` entry. **Collateral checker fix:** a bare user-function
  reference now carries its full `Fn` signature type (was params-erased, which no arity-checked
  `Fn` param could ever accept — `[1,2].map(inc)` failed E0007 on main). **Bench (pinned
  interleaved A/B, quiet box):** t_map_bounded +6–8%, t_all +1–5%, reactive ±2% (untouched),
  serve no regression — within the ≤10% orchestration budget; the fixture is adversarial
  (200k no-op bodies ≈ 45ns/item seam cost), any real body dilutes it below ~5%. Learned: the
  per-item `free` discipline is what keeps the slot table window-sized and cache-hot — skipping
  frees grew it to 400k cold entries and doubled the fixture.
- **H3 ✅ DONE** — `http.serve` migrated: shared accept→dispatch→reply loop in
  `noeta-stdlib/src/serve.rs`, first exerciser of the recover-from-Abort pattern (handler abort
  → 500, loop continues; listener abort propagates). `NativeCtx` grew `drive` (backend's own
  await loop — channel/isolate terms intact — for the reply write), `option_payload` (the
  accept outcome), `with_extern` (borrow-shaped extern access for `Request::conn` / the
  handler's `NetResponse`). serve now has a real signature — `serve(int, Fn(Request) -> dyn)`
  — so handler shape is statically checked (previously unvalidated). Deleted: `Builtin::Serve`
  (both enums + both ~140-line arms), both intercepts, the checker special case. Gate: suite
  green, real-host smoke (probe + requests), loopback-throughput A/B overlaps fully (per-request
  seam cost ≪ TCP round-trip).
- **H4 ✅ DONE** — Class-3 machinery, proven by **std.cell** (the smallest Class-3 client —
  reactive minus the graph). Retained arena: `NativeCtx::{retain, retained_get, retained_set,
  release_retained}` over a per-run backend field; extern boxes carry only the plain `Retained`
  id (the `Send` rule); teardown feeds the arena into the trace roots then releases
  destructor-aware on both backends (leak-0). Per-run `state(key, init)` →
  `Rc<RefCell<Box<dyn Any>>>` (borrow-around-re-entry discipline documented; first real
  exerciser is H5's graph). Generic extern types: `SigType::Generic(name, args)` construction
  returns + receiver-seeded `Var` method signatures (`Cell<int>.set("x")` = static E0007);
  runtime reflection stays the bare nominal name (reactive handles reflect as `dyn` today, so
  parity improves at H5). Ctx-form `ExtType::{ctx_methods, ctx_dispatch}` (+`DEFAULTS`),
  receiver as slot 0, routed per-method in both backends. `cell.new(v) -> Cell<T>` with
  `get`/`set`/`update(f)` ships; 4 fixtures (aliasing/identity, re-entrant update, generic
  mismatch, destructor exactness through set-replace + teardown). `Cell` reserved (one doc
  sample + one fixture renamed their own `Cell` → `Counter`). No `Wire::Extern` exists, so
  cells cannot cross isolates. Full gate green.
- **H5 ✅ DONE** (one gate caveat, below) — **std.reactive fully migrated**: the graph is
  extension state over **stable arena cells** (a signal's content cell, a computed's memo cell,
  immutable body cells — the graph stores only ids, never displaces a value; dirtying =
  `touch()`); `Signal<T>`/`Computed<T>` = generic extern types with **declared arena reads**
  behind the extension-synced gate ("no body running, no stale memo, no flush"); flush/
  coalescing/E0045 = ordinary Rust in the dispatches (`ErrorKind::ReactiveCycle`). REPL session
  persistence generalized (ext arena/state/gates replace the graph field). **Everything deleted
  as planned** — `Value::Reactive`/`Payload::Reactive`, both intercepts, `drive_flush`/
  `read_reactive`, `Builtin::{Signal,Computed,Effect}`, the whole `VIRTUAL_MODULES` mechanism,
  the checker tables; noeta-vm/eval/value shed the noeta-reactive dep. Perf work (all measured):
  extern-method route cache in `Op::CallMethod`, borrowed-seed slot tables, `CtxOut::Retained`,
  fused `run_thunk`/`call_thunk_into`, alloc-free graph (worklist dirty walk, swapped flush
  scratch), gate caching; plus `codegen-units=1` + thin LTO (layout was a per-build lottery).
  **Bench (split gate, user-approved 2026-07-08):** reads ≤5% — `r_get_hot` **−6.1%**,
  `r_computed_memo` **+14.6%** ✓✓; writes ≤35% — `r_effect_fanout` **+29.2%** ✓, `r_set_flush`
  **+48.8% ✗** (~198ns per set→flush→effect→tracked-get cycle vs 133ns; ~+34% is the structural
  floor of the cycle's 3 boundary crossings at ~15ns each — the fixture is 100% seam-bound with
  a no-op body). Gate verdict on r_set_flush = user decision, pending. Conformance 513/513
  (all 19 reactivity fixtures byte-exact), 1009 workspace tests, clippy silent; t_*/serve
  unchanged (borrowed seeds erased H2's residual: t_map_bounded now ±0%).
- **H6** — extension commands: `ExtCommand`/`CommandCtx`, dynamic clap wiring, `noeta serve`
  migrated out of the `Command` enum. CLI integration test green.
- **H7** — docs (Native-Extensions.md rewrite — Deferred shrinks to freeze/publish +
  raw-buffer + finalizers; wiki), plan outcome, memory.

Each phase = one or more green-gate commits (differential, leak-0 residency, corpus, doc
samples). H2/H3 land before H4/H5 so the ctx seam is proven by simpler clients before the
stateful machinery builds on it.

## H-BENCH — the regression gate (baseline captured)

Harness: `tests/bench/higher-order-abi/` (`run.py`, P-PAR protocol — pinned via taskset, median
of 7; compare mode is **pinned interleaved A/B**: `run.py --compare <baseline-bin>
<candidate-bin>`, ABAB per run so drift hits both sides). Fixtures cover every surface the arc
migrates; a migration phase does not merge with a significant regression on its fixtures — gate:
**≤ 5% on the hot reactive fixtures** (r_*), sanity ≤ 10% on the orchestration ones (t_*,
serve); anything beyond gets investigated/mitigated first (method inline-cache is the H5
mitigation path).

Baseline (2026-07-07, AMD Ryzen AI 9 365, release, quiet machine, pre-arc tree @ `383ec55` +
accept-robustness fix):

| fixture | measures | gates | baseline |
|---|---|---|---|
| r_get_hot.noe | 5M `signal.get()` | H5 | 258 ms |
| r_set_flush.noe | 1M `set` + 1-effect flush each | H5 | 163 ms |
| r_computed_memo.noe | 4M memo hits + 500k recomputes | H5 | 320 ms |
| r_effect_fanout.noe | 200 effects × 10k sets (2M runs) | H5 | 259 ms |
| t_map_bounded.noe | 200k items, window 16, cheap async | H2 | 91 ms |
| t_all.noe | all() over 200k cheap futures | H2 | 220 ms |
| serve_app.noe | 500 sequential loopback GETs | H3 | ~14.4k req/s |

(Loopback req/s has client-side jitter; compare mode uses a 3-round interleaved median. First
capture on a warm machine showed 2× spread — bench only on a quiet box, medians must sit near
mins.)

## Non-goals

Freezing/publishing the ABI + dynamic multi-extension registry (package-manager milestone — this
arc *designs and dogfoods* the contract that milestone freezes). Raw-buffer/columnar ABI.
Host-coupled finalizers (GC-triggered extension callbacks — adjacent to Class 3 but a separate
decision; explicit `close()` stays). WS transport. The `Len/Map/Filter/Sum/Assert` prelude
builtins stay core (hot path, language-level).

## Risks

- **Arena exactness** (the delicate piece): retain/release across ctx re-entry — an effect body
  running via `ctx.call` may itself call `set` (re-entering the same extension state). The
  eval/VM arena impls must be re-entrancy-safe; the leak oracle catches misses, the differential
  catches order divergence.
- **Reactive perf**: handles become `Payload::Extern` boxes (vs packed immediates) and
  `.get()`/`.set()` go through ctx dispatch (vs integer-compare intercepts). Bench the reactive
  suite before/after (perf mandate); mitigate with the method inline-cache; a by-ref slot path
  means no value marshalling on `get` (returns a retained handle, O(1)).
- **Re-entrancy of `ctx.call`**: re-enters the interpreter/VM run loop while the dispatch holds
  the ctx borrow — same shape as today's `Builtin` impls calling `self.call_value`, but across a
  trait object; the wrapper must not cache raw pointers across calls.
- **JIT**: verify the JIT has no special knowledge of the migrating `Builtin`s (believed none —
  orchestration ops are not compiled).
- **First-class references**: `f = task.sleep` / `s = reactive.signal` must keep working through
  the registry function-handle path once the virtual-module binding is deleted.
