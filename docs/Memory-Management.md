# Memory Management

Memory is **compiled, not traced**. There is no stop-the-world tracing GC on the hot path: the program is lowered to an intermediate representation where the compiler inserts reference-count operations, frees each value at its last use, and rewrites unique-owner mutations into in-place updates. A cycle collector backstops the one case reference counting cannot handle.

## Value semantics and copy-on-write

Structs and collections have **value semantics** — a "mutation" of a shared value logically copies. Immutable-by-default makes uniqueness the *only* path to mutation, and that same uniqueness check unlocks a fast path: a value that is the provably-unique owner (refcount 1) can be mutated *in place* instead of copied.

The motivating case is `acc ~= [x]` in a loop: if every append copied, it would be O(n²); in-place extension makes it O(n).

## In-place reuse

A dedicated IR pass (`noeta-ir-passes`, `reuse.rs`) recognizes **self-updates** — a binding rebound to a value computed from its own old contents — and marks the constructor with an *in-place-reuse token*:

```text
acc = Type { ...acc, f: v }      →  Object { spread: acc, reuse: true }
acc = acc ~ rhs   (from ~=)       →  Binary { op: Concat, lhs: acc, reuse: true }
m   = m.set(k, v) (from m[k]=v)   →  CallMethod { reuse: true }
x.f = v                           →  the self-update shape, reuse: true
```

Because the input is the *same binding* the result rebinds, the old value is dead the instant the op finishes. Putting the decision on the shared IR means both backends reuse at the *same* point by construction — the VM lowers a marked constructor to `MakeStructInPlace`; the reference interpreter moves the base out and mutates via `Rc::get_mut`.

**Soundness.** The token only says *where to try*; the **runtime refcount decides** (reuse only when count is 1). A wrong token (an aliased base) simply falls back to a copy — never a use-after-free — so a purely syntactic match is safe. The one static constraint: reuse means the displaced base's own `destruct` never fires, so a self-update is reuse-eligible only when the type has no own destructor (every struct qualifies; a class qualifies iff it is destructor-free). The *changed* field's displaced value still has its destructor fired on overwrite by both backends, keeping reuse observationally invisible.

## Precise reference counting via an ANF IR

Why an IR at all? Three reasons, the first decisive:

1. **The AST doesn't model the values RC operates on.** Temporaries, the receiver copy in `obj.field`, register moves — none has an AST node, but A-normal form (ANF) names *every* intermediate value.
2. **Precise RC is a program transformation**, which wants an IR, not a `HashMap<Span, Fact>` that two backends re-interpret.
3. **Agreement by construction.** If both backends execute the same RC-annotated IR, reclamation order *is one program* — which, for last-use destructor ordering, is materially more correct than "two interpretations that happen to coincide."

The model is Lean 4's (precise RC + reuse + a runtime `RC == 1` check), adapted to shared-nothing isolates.

**The IR** (`noeta-ir`) is ANF: `Atom`s (trivially-evaluable operands), `Rvalue`s (the compound operations), `Stmt`s (including `Bind` and `DropVar`), and structured control flow with scope markers. Lowering itself adds no RC annotations — the passes fill them in:

1. **Liveness** — last-use / liveness analysis.
2. **Drop insertion** — with three placement rules of increasing coverage: **last use** (a value dropped right after its final read), **scope exit** (owned locals still live at a scope's end, dropped in reverse-construction order), and **early exit** (values abandoned at a `return`/`break`/`continue`, dropped innermost-first before the terminator).
3. **Reuse** — the tokens above.

## The load-bearing safety invariant

Static analysis is an **optimization input, never a soundness requirement**. Correctness always rests on the runtime refcount (a value is freed iff its count hits zero) plus scope teardown for whatever a pass missed. So every inserted `drop` is conservative in the "**never too early**" direction: a *late* drop costs only promptness; an *early* drop would be a use-after-free and must be impossible by construction. A bug in any RC pass can cost performance — never memory safety. Non-local exits a static pass can't bracket (`?` propagation, panic) are caught by a runtime teardown backstop.

**Deterministic `__destruct` at last use.** When an object's last reference drops — at the precise IR point liveness identified, not at scope end — its destructor runs synchronously. Children are destroyed container-before-contained in declared order (fields, then enum payloads, then collection elements), the *same* order both backends walk because they walk the same IR.

**Verification.** Every phase is gated by: a **leak oracle** (heap residency must be 0 at clean exit, both backends, whole corpus, empty allowlist); a **static-≤-dynamic** property test (the analysis may never claim a death before the real one); the reference-interpreter ↔ VM differential; and miri on every refcount/collector path.

## Cycle collection

Reference counting cannot reclaim reference cycles. Under value semantics an ordinary object *can't* form one (a shared mutation copies) — the only cycles are closure/scope self-captures, which become reachable once mutable fields exist. So a backup collector (`noeta-gc`) is load-bearing, running only at safepoints.

`noeta-gc` owns the *policy* (`retain`/`release` and the collection algorithm); the `unsafe` refcount/graph *mechanism* lives in `noeta-value`'s heap. Two collectors were built and benchmarked head to head:

| | **Trace** (default) | **TrialDeletion** (behind a flag) |
|---|---|---|
| Per allocation | one live-registry insert | nothing |
| Per release | prompt refcount free | buffer a candidate root |
| At collection | mark from roots, sweep the unmarked | trial-decrement the buffered subgraph (Bacon–Rajan) |
| Wins on | cyclic garbage (~7–10%), simplicity, hot `free` untouched | acyclic churn (~13–17%, zero per-alloc cost) |

`Trace` is the default — simpler, safer (it never mutates the hot `free` path), and it wins on the case the collector exists for. Either collector *identifies* garbage and hands it back as a `Garbage` set; **the VM reclaims it** — running each fresh member's `__destruct` while the dead subgraph is still allocated, before freeing — because a destructor needs the interpreter, which the collector doesn't have. Intra-cycle destruction order is a best-effort deterministic tie-break by the monotonic `seq` counter. Both collectors reach residency 0 on the whole corpus, both backends, miri-clean.

## See also

- [The Virtual Machine](The-Virtual-Machine) — the register discipline and heap layout this all runs on.
- [Concurrency Internals](Concurrency-Internals) — why non-atomic refcounts are sound (shared-nothing isolates).
- [Contributing](Contributing) — the leak oracle and miri gates in the test suite.
