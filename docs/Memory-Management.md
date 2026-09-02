# Memory Management

Memory is managed by **compiled reference counting**. The program is lowered to an intermediate representation where the compiler inserts reference-count operations, frees each value at its last use, and rewrites unique-owner mutations into in-place updates. A cycle collector backstops the one case reference counting cannot handle.

## Value semantics and copy-on-write

Structs and collections have **value semantics**: a "mutation" of a shared value logically copies. Values are immutable by default, so uniqueness is the path to mutation, and the same uniqueness check unlocks a fast path. A value that is the provably-unique owner (refcount 1) is mutated *in place* rather than copied, which is what makes `acc ~= [x]` in a loop cost O(n) rather than O(n²).

## In-place reuse

A dedicated IR pass (`noeta-ir-passes`, `reuse.rs`) recognizes **self-updates**, a binding rebound to a value computed from its own old contents, and marks the constructor with an *in-place-reuse token*:

```text
acc = Type { ...acc, f: v }      →  Object { spread: acc, reuse: true }
acc = acc ~ rhs   (from ~=)       →  Binary { op: Concat, lhs: acc, reuse: true }
m   = m.set(k, v) (from m[k]=v)   →  CallMethod { reuse: true }
x.f = v                           →  the self-update shape, reuse: true
```

Because the input is the *same binding* the result rebinds, the old value is dead the instant the op finishes. The decision lives in the shared IR, so both backends reuse at the same point by construction: the VM lowers a marked constructor to `MakeStructInPlace`, and the reference interpreter moves the base out and mutates via `Rc::get_mut`.

**Where the binding lives does not change the cost.** A binding held in a global slot rather than a register (see [where a top-level binding lives](The-Virtual-Machine#where-a-top-level-binding-lives)) has to be read into a register to be used. So a self-update that *reads its own receiver*, like `m[k] = m.get_or(k, 0) + 1`, leaves that read behind as a second reference the rest of the statement never uses.

The compiler releases that read at its last use, so the update still sees the sole owner it really is. A read-modify-write costs the same at the top level as inside a function, and the same whether the key is spelled inline or bound to a name first.

**Soundness.** The token says only *where to try*; the **runtime refcount decides**, reusing only when the count is 1. A token on an aliased base falls back to a copy, so a purely syntactic match is safe. One static constraint remains: reuse means the displaced base's own `destruct` never fires, so a self-update is reuse-eligible only when the type has no own destructor. Every struct qualifies, and a class qualifies when it is destructor-free. The *changed* field's displaced value still has its destructor fired on overwrite by both backends, which keeps reuse observationally invisible.

## Precise reference counting via an ANF IR

Precise RC is a program transformation over an IR rather than over the AST, because the AST does not model the values RC operates on. Temporaries, the receiver copy in `obj.field` and register moves have no AST node, while A-normal form (ANF) names *every* intermediate value. Both backends execute the same RC-annotated IR, so reclamation order is one program, which is what makes last-use destructor ordering identical across them. The model is Lean 4's (precise RC + reuse + a runtime `RC == 1` check), adapted to shared-nothing isolates.

**The IR** (`noeta-ir`) is ANF: `Atom`s (trivially-evaluable operands), `Rvalue`s (the compound operations), `Stmt`s (including `Bind` and `DropVar`), and structured control flow with scope markers. Lowering itself adds no RC annotations. Three passes fill them in: **liveness** (last-use analysis), **drop insertion**, and **reuse** (the tokens above).

Drop insertion places a value's release at one of three points:

| Placement | Where the drop lands |
|---|---|
| Last use | immediately after the value's final read |
| Scope exit | owned locals still live at a scope's end, in reverse-construction order |
| Early exit | values abandoned at a `return`/`break`/`continue`, innermost scope first, before the terminator |

## The safety invariant

Correctness rests on the runtime refcount, a value being freed when its count reaches zero, plus scope teardown for whatever a pass missed. Static analysis is therefore an optimization input: every inserted `drop` is conservative in the "**never too early**" direction, so a *late* drop costs promptness while an *early* drop would be a use-after-free and is impossible by construction. A bug in an RC pass costs performance and never memory safety. Non-local exits a static pass cannot bracket (`?` propagation, panic) are caught by a runtime teardown backstop.

**Deterministic `__destruct` at last use.** An object's destructor runs synchronously when its last reference drops, at the precise IR point liveness identified rather than at scope end. Children are destroyed container-before-contained: the aggregate runs its own `destruct` block first, then releases its fields in declared order, its enum payload positionally, and its collection elements in iteration order, each firing its own destructor at *its* last reference. Both backends walk the same IR, so both walk that order.

**A value built for a call belongs to the caller.** Arguments are evaluated before the call, so `m.get_or(key, Fallback.new())` builds the fallback whether or not the key is present, and whichever branch the callee takes, the caller owns what it built. An unnamed argument is destroyed right after the call returns, newest first, and the destruction is refcount-gated: a fallback the callee *kept*, stored or handed back as the result, is left to its real owner and dies at that owner's own last use.

So a discarded argument's `destruct` runs at the call, and a returned one's runs where the result dies. When building the default is the part you want to avoid rather than merely reclaim, reach for `m.get(key) ?? build()`, whose right side is evaluated only on a miss.

**Verification.** Every phase is gated by the same oracles [Contributing](Contributing#testing-architecture) describes:

| Gate | What it asserts |
|---|---|
| The leak oracle | heap residency is 0 at clean exit — both backends, whole corpus, empty allowlist |
| The refcount-anomaly oracle | every unreachable object's refcount equals its in-edges from the garbage set, so a skipped retain/release is caught |
| The skipped-destructor oracle | every object of a type declaring `destruct` ran its destructor — the one wrong answer freeing memory correctly can still produce |
| The static-≤-dynamic property test | the analysis never claims a death before the real one |
| The differential oracle | reference interpreter ↔ VM output is byte-for-byte identical |
| miri | every refcount/collector `unsafe` path is UB-free |

## Cycle collection

Reference counting cannot reclaim reference cycles. Under value semantics an ordinary object cannot form one, because a shared mutation copies; the cycles that exist are closure and scope self-captures, which become reachable once mutable fields exist. A backup collector (`noeta-gc`) reaps them, running only at *safepoints*, the designated execution points (loop back-edges, frame transfers, scheduler rounds) where the heap is in a consistent, collectable state. See [In-run safepoint collection](#in-run-safepoint-collection) below.

`noeta-gc` owns the *policy* (`retain`/`release` and the collection algorithm); the `unsafe` refcount/graph *mechanism* lives in `noeta-value`'s heap. Two collectors ship:

| | **Trace** (default) | **TrialDeletion** |
|---|---|---|
| Per allocation | one live-registry insert | nothing |
| Per release | prompt refcount free | buffer a candidate root |
| At collection | mark from roots, sweep the unmarked | trial-decrement the buffered subgraph (Bacon–Rajan) |
| Wins on | cyclic garbage | acyclic churn, at zero per-allocation cost |

Every `noeta` run uses `Trace`; an embedding host selects the other through the VM's run options. Either collector *identifies* garbage and hands it back as a `Garbage` set, and **the VM reclaims it**, running each fresh member's `__destruct` while the dead subgraph is still allocated and before freeing it, because a destructor needs the interpreter and the collector has none. Intra-cycle destruction order is a best-effort deterministic tie-break by the monotonic `seq` counter. Both collectors reach residency 0 on the whole corpus, on both backends, miri-clean.

## In-run safepoint collection

Collection also runs **during** execution, so a program building cycles in a loop has *bounded* peak residency rather than growth until exit. The trigger is allocation pressure: a thread-local watermark over the live count (`Trace`) or the candidate buffer's growth (`TrialDeletion`), step `NOETA_GC_THRESHOLD` (default 10k objects), re-armed geometrically so genuinely-live residency pays a vanishing collection frequency.

The VM polls one thread-local bool at taken loop back-edges, frame transfers, and each scheduler drive round, so a program parked on `.await` still collects. Tier-1 native code never polls; it rejoins those sites at every bail, call, and return, so compiled frames are never interrupted at an unsafe point. Trigger state is thread-local, so every worker isolate collects its own heap at its own safepoints.

The semantic rule that keeps this invisible: **a safepoint collection never runs a destructor.** A `destruct` block is the only observable memory-management effect, and its firing is tied to the last owning release, an event cyclic garbage never produces, so cycle-destructor timing belongs to the exit collection. Destructor-free garbage reclaims immediately and unobservably.

A dead component that *does* contain a destructor-bearing member is **deferred to exit**. Garbage is partitioned at weakly-connected-component granularity, so no reclaimed member can reference a deferred one, and the deferred component is left allocated for the exit collection, which reclaims it with the same members, the same reverse-`seq` order, and the same output it would otherwise have produced.

Because immediate reclamation is unobservable, the two backends need no synchronized collection points. Each collects its own way, and each carries a hard safety net.

The VM traces from its enumerated roots (register windows, upvalues, globals, channel buffers, extension arena, embed handles, scheduler-held tasks) and **aborts any collection whose garbage set does not exactly refcount-balance its internal in-edges**, so a missed root costs liveness until exit rather than safety. The reference interpreter runs trial deletion over the `Rc` graph seeded from its weak candidate registries, every Rust-held value being a counted owner, and verifies the dead set's strong counts the same way.

A program that builds cycles in a loop therefore holds a bounded peak, whichever backend runs it.

## See also

- [The Virtual Machine](The-Virtual-Machine) — the register discipline and heap layout this all runs on.
- [Concurrency Internals](Concurrency-Internals) — why non-atomic refcounts are sound (shared-nothing isolates).
- [Contributing](Contributing) — the leak oracle and miri gates in the test suite.
