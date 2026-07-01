# Deferred backlog — items pushed past the slice that introduced them

Slices are marked **done** when they meet their gate, but several deliberately left
non-gate work for later. Recorded *only* inside a done slice, such an item is effectively
invisible — a future planner reading the roadmap never sees it. This file is the single
discoverable home for every cross-slice deferral, so nothing is lost between milestones.

**Discipline:** when a slice defers something non-gate, add a row here in the same commit.
When a later slice picks it up, strike the row and point to the slice that closed it. Each row
names the **source slice** (the rationale lives there) and a concrete **trigger** — the
condition that should make us actually do it, so we neither do it prematurely nor forget it.

Nothing here is a correctness gap in shipped behavior: each item is either **latent**
(unreachable by the current corpus, so the differential's `0 skipped` gate still holds),
a **perf** optimization that is invisible to `RunResult`, or a **hardening** task. If any
"latent" item ever becomes reachable, it stops being optional — see its note.

## VM completeness (latent — would break `0 skipped` if a program reached it)

The compiler returns [`Unsupported`] for these, and the differential harness *skips* such a
program. No corpus case exercises any of them today, so the gate holds — but the moment a
program does, that program silently leaves the differential.

**Closed by slice F1 (`plans/followups/slice-f1-upvalues.md`)** — the VM now has real upvalue
machinery (closed cells; see [`lang_compiler::freevars`]):

| Item | Source | Status |
|---|---|---|
| ~~True upvalues — a nested closure capturing an enclosing **function's local**~~ | M1.2, M1.5 | **Done (F1)** — `closures/capture_param`, `transitive_capture` |
| ~~Nested `fn` declarations inside a function body~~ | M1.2 | **Done (F1)** — `closures/counter_nested_fn`, `recursive_nested_fn` |
| ~~Bare `x = expr` non-local reassignment **inside a function**~~ | M1.2, M1.5 | **Done (F1)** — captured-local + global reassignment (`closures/global_mutate_from_fn`) |

Still open (narrowed from the original cluster):

| Item | Source | Trigger to implement |
|---|---|---|
| A closure **inside a method** capturing `self` or a field (method-context capture) | M1.2, F1 | A closure in a method body that reads `self`/a field; needs `self` threaded as a capturable upvalue |
| ~~A **reference** to a prelude collection builtin (`len`/`map`/`filter`/`sum`) as a value~~ | M1.2 | **Done (F2)** — `Payload::NativeFn`; `closures/builtin_as_value`, `builtin_value_on_object`. Still open: the `Ok`/`Err`/`some` constructors, `panic`, `next_id` as first-class *values* (exotic; need hand-matched runtime arity/error text) |
| Forward / mutual capture among nested `fn`s (a closure capturing a local declared *after* it) | F1 | A program with forward references between nested closures; needs pre-declared cells for all celled locals |

## Type checker / inference hardening

> **Superseded by a direction decision (2026-06-22): the language targets an INFERRED-STATIC type system with an explicit `dyn` escape — not the gradual/`Unknown`-tolerant checker shipped today.** Annotations stay optional because inference reconstructs them; an un-inferable type is a *compile error*, and `dyn`/`Any` is the only sanctioned dynamic boundary (the sole place runtime trait dispatch survives). This **absorbs the two rows below and the bounded-generics row in the next section** into one milestone-scale type-system track (≈ M1.7 + M1.8 redone under a soundness mandate, plus trait coherence and the `dyn` story), and it *gates* the packed-types/SIMD perf work. The inference engine is **bidirectional checking with local inference, not classical Hindley–Milner** — subtyping (`dyn` widening, directional method resolution, record width) is load-bearing and defeats HM's unification core; see the checker row below for the full rationale. Note the track is a **reclassification, not an addition**: everything that runs today is already the `dyn` path (all current trait dispatch is runtime shape lookup), so the work demotes today's behavior to the marked `dyn` fallback and builds a static dispatch layer on top — and static types buy *soundness now, perf only later* (once the packed-types tier consumes them). **In progress** on branch `types-inferred-static` (planning + slice docs in `plans/types/`): S0 lattice/`dyn`, S1 bidirectional engine, S2 mandatory signatures (E0022), S3a/S3b stdlib typing + argument checking, and **S3c inference completion** are done (forward propagation, optional binding annotations, list-building L1–L3 — `~` concatenation + `[..xs, x]` spread + declaring-scope assignment so accumulators infer forward — and **E0023 CannotInfer** for an undeterminable binding). Remaining: S4 bounded generics (E0024), S5 trait coherence (E0025), S6 `dyn` narrowing `x.as<T>()`, S8 *declared* (never inferred) union/intersection types, S7 finalize. Notable course-corrections recorded there: the language had no list-building, so the accumulator case was *built* (L1–L3) rather than designed around, which also removed any need for a backward-inference solver. Rationale + full status in the `type-system-direction` memory.

| Item | Source | Trigger |
|---|---|---|
| Promote `E0006 ImmutableAssignment` from a **runtime** check to a **static** one (the "M1.7b" ownership/immutability analysis — no `slice-07b` was ever written) | M1.7 | Folded into the inferred-static track above |
| Static inference engine for the inferred-static checker — **bidirectional checking with local inference, NOT classical Hindley–Milner** (the checker is gradual/`Unknown`-tolerant today) | M1.7 | Folded into the inferred-static track above (becomes *the* checker, not a permissive add-on). **Engine decided: bidirectional, not HM.** Classical unification-based HM requires *no subtyping* (it solves symmetric `t1 = t2`); we make subtyping load-bearing via `dyn` (top type, implicit widening + checked `.as::<T>()?` narrowing), directional method/overload resolution, and likely record width subtyping — any one defeats HM. The HM-preserving routes are worse (drop subtyping → rigid language; MLsub/algebraic subtyping → exotic union/intersection types, non-local errors). We also already require boundary annotations, which forgoes HM's no-annotation guarantee while still paying its costs; bidirectional instead consumes those signatures as propagation anchors (subtyping lives in check-mode subsumption, errors stay local) and decomposes cleanly into salsa queries (HM's global `let`-generalization solve does not). Rationale in the `type-system-direction` memory. |
| ~~Struct/class *literal* field values are not type-checked against declared field types~~ | function-types slice investigation (2026-06-30) | **Done (2026-06-30, same arc as function types).** The object-literal synthesis arm now checks each provided field value against the declared field type (`E0007`), reusing the field-default machinery (`arg_assignable` + `erase_type_params`); the type's own parameters are erased to `dyn` (inferred from the value), so a generic field stays permissive while a concrete field is enforced. Conformance `structs/literal_field_types_checked.lang` + negative `diagnostics/struct_literal_field_mismatch.lang`. |

### Generic & operator enforcement completeness (S4 follow-ups)

Three gaps the S4 bounded-generics work surfaced and initially deferred (they were noted only in the S4.3 slice — moved here so they are not lost). Recorded with concrete triggers; being addressed in S4.4/S4.5.

| Item | Source | Status |
|---|---|---|
| **Operator-trait checking on concrete types** — an operator that maps to a trait (`+`→`Add`, `< <= > >=`→`Comparable`) should verify a concrete operand actually `impl`s/derives it, instead of being lenient. S4.3a made arithmetic *lenient* on any `Named` operand to avoid a regression once constructor results typed precisely; that masks `obj + obj` / `obj < obj` where the type does not support the operator (today a runtime error). | S4.3a | **Done (S4.4)** — unified `operand_ok` check: a trait-backed operator requires each operand to satisfy the trait (concrete via `satisfies`, type-param via its bounds); concrete failure → `E0007`, unbounded type-param → `E0025`. `==`/`!=`/`~` stay universal (structural-equality / display-concat fallbacks), matching the runtime. |
| **Body-side bound requirement beyond ordering** — S4.3c rejected only an *ordering* comparison on an unbounded type parameter; the same should hold for every operator↔trait pair (`+` needs `Add`, …). (`==`→`Equatable` is moot: equality is universal at runtime.) | S4.3c | **Done (S4.4)** — folded into the same `operand_ok` mechanism, driven by `operator_trait(op)`, so every trait-backed operator enforces the bound body-side. |
| **Track a type argument through an instance** (`Box<string>` as a value) — `Type::Named` carries no arguments, so an instance loses its `T`: `box.get()` types as `dyn` rather than `string`, and a bound that only constrains a method's receiver-shaped use is not enforced at instance-method calls. Construction enforcement (S4.3b) already guarantees a constructed instance's `T` satisfies its bounds, so this is about *precise typing*, not soundness. | S4.3b | **Done (S4.5)** — `Type::Named(name, args)`; constructors return precise arguments via instantiation, instance method/field access seed the substitution from the receiver's arguments (`box.get()` is `int`), residual parameters erase to `dyn`. Remaining sub-gap below. |
| ~~**Infer a record/object literal's type arguments from its field values** (`Box { value: 1 }` → `Box<int>`)~~ | S4.5 | **Done (S4.6)** — the object-literal arm binds each field value's type against the field's declared parameter (`bind_type_params`) and reads the arguments off in declaration order; nothing-constrained stays an empty (wildcard) argument list. `Box { value: 5 }.value` is now `int`. Covered by `generics/literal_type_argument_{inferred,mismatch}.lang`. |

## Traits / generics (the M1.8 deferred tail)

All recorded in `m1/slice-08-traits.md` under "Todo (deferred past M1.8)". None reach `RunResult` today.

| Item | Source | Trigger |
|---|---|---|
| ~~User-facing `Ordering.Less` construction; register `Ordering` as a namable prelude enum~~ | M1.8 | **Done (F3)** — `Ordering` registered as a built-in enum in both backends (`traits/ordering_construct`); construction builds the same value `.compare()` returns |
| ~~Standalone top-level `impl Attribute for X {}` + the `#[Foo(...)]`-requires-`Attribute` gate~~ | M1.8 | **Done (attribute-system pass 1, `plans/attributes/`)** — standalone `impl Trait for T {}` (A1; orphan rule = same-module target → E0013; folds into coherence E0027) is the capability mechanism for bodiless records; the `#[Foo(...)]`-requires-`Attribute` gate is **E0029** (A3) plus an all-fields construction check (E0009/E0007/E0005). Pass-1 standalone impls are empty-body markers; method-carrying standalone impls (record behavior, needs backend dispatch) stay deferred to pass 2 |
| ~~Nested-object fields in derived `Comparable` (recurse into sub-objects)~~ | M1.8 | **Done (F3)** — `compare_field` recurses into object fields in both backends (`traits/derive_comparable_nested`); non-object/non-primitive fields (e.g. lists) still bail |
| `Callable` (`a(...)`), `Members` / `DynamicCall` protocols routed to user objects | M1.8 | Objects used as functions / dynamic member dispatch (agentic/proxy surface) |
| **Call a closure-valued field/property directly: `obj.f(args)`** | coroutines Track-I investigation (2026-06-30) | A closure stored in a field *works at runtime* (you can bind `g = obj.f` then `g(args)`), but `obj.f(args)` is parsed unconditionally as **method dispatch** → `E0005` "has no method `f`". So a closure field is a second-class callable. **Trigger:** wanting member-handles / closure fields to be callable in place (a facet of the `Members`/`Callable` row above; surfaced explicitly here so it isn't lost — it would desugar `obj.f(args)` to "field access then call" when `f` is not a method). Needed for user-defined iterators that hold a `next`-style closure; *not* needed for the native iterator adapters (they hold the closure in Rust). The function-type *typing* half is being closed now (surface `(A,B) -> R` → `Type::Fn`); this is the *call-site* half. |
| Monomorphic shape specialization + bounded type parameters (`<T: Comparable>`) — generics are erased-for-storage today | M1.8 | **Bounded params fold into the inferred-static type-system track** (enforced statically, not at runtime); monomorphic specialization is the packed/perf reification path it then unlocks (M2 "packed value types"). **Also carries:** reflection cross-`dyn` element recovery — once type args ride in shapes, `type_of` recovers `List<int>`'s `int` after a `dyn` boundary (attribute-system pass 2, "P2.9", folded here) |
| Richer `#[attr(...)]` arguments (identifiers only previously) | M1.8 | **Done for literals (attribute-system pass 1, A2)** — positional + named literal args (string/int/float/bool/ident) survive into `Module.manifest` (`AttributeArg`/`AttributeValue`), verified by lang-compiler manifest tests (not `RunResult` — intrinsic). **Still deferred:** nested record-valued args (`#[Foo(inner: Bar { .. })]`) and how the manifest feeds the agentic/MCP tooling — both belong to pass 2 (the runtime reflection read-back) |

## Coroutines / async (Track A tail — the track is COMPLETE, these are follow-ons)

Track A (`plans/coroutines/track-a-async.md`) shipped `async`/`await`, a deterministic sandbox
executor + real tokio executor, suspending `sleep`/`fs.read_async` leaves, and structured concurrency
(`concurrent { }`/`spawn`). None of the rows below reach `RunResult` incorrectly today — each is a
missing *capability*, cleanly gated (E0040) or simply absent, not a latent bug.

| Item | Source | Trigger |
|---|---|---|
| ~~**Mid-expression `.await`** in unconditionally-evaluated positions (call args, operands, elements, index, member, interpolation)~~ | A.3a | **DONE (A.6a, `db3d6de`)** — `hoist_await_body` in `lang-ir/lower.rs` hoists each such await to a preceding statement-position `$hwN = <sub>.await`, left-to-right, before the flattener; checker `conditional_await_span` relaxes E0040 accordingly. In-oracle, both backends. |
| **A.6b — `.await` in a *conditionally-evaluated* position** (right operand of `&&`/`\|\|`, `??` fallback, `match`/`if…then…else` arm body) | A.6a | Still E0040. Needs per-construct **control-flow desugaring** (e.g. `a && b.await` → `mut $sc = a; if $sc { $sc = b.await }`) so the guarded await runs only when it should; `??`-fallback additionally needs Option-aware unwrap. Kept as one coherent unit so the boundary stays explainable: "await works everywhere except conditionally-evaluated positions." Condition/loop heads (`if`/`while` cond, `for` iterable) also stay rejected (repeated/guarded evaluation). |
| `all` / `race` / bounded-parallelism `map` over a `concurrent` scope | A.3b | Library functions over `TaskScope` (architecture §7.1), once the scope value is first-class beyond the block sugar |
| **Explicit cooperative cancellation**, typed (§7.1 "cancellation is a typed outcome") | A.3b | Beyond today's abandon-on-error at the join boundary — a cancellation token/outcome type and cascade-to-children |
| Nested `concurrent` interleaving with **outer** siblings | A.3b | A nested scope currently runs atomically within its task; interleaving it with the outer scope's siblings needs the scheduler to flatten scope levels |
| More async IO leaves — `fs.write_async` / `append_async` / directory ops; network/DB leaves | A.4c | Each mirrors `fs.read_async` (a leaf + `Executor` spawn/poll pair; sandbox synchronous/in-oracle, real on tokio/out-of-oracle). Network/DB await the respective stdlib surfaces |
| App-lifetime `TaskScope` via DI, workers, durable queues, schedulers (§7.2) | A (design) | Framework/first-party-extension patterns over the `TaskScope` primitive, not language constructs |
| Inter-isolate parallelism / channels (§7 CPU-bound story) | A (design) | Track A is intra-isolate async only |

## Reflection (attribute-system pass 2) — pieces folded into prerequisite milestones

Pass 2 (the runtime reflection read-back) is planned in `plans/attributes/pass-2-reflection.md` as slices P2.0–P2.7 (shared artifact, `attributes_of`, `type_of` with full + head-constructor fidelity, method attributes, `AttachableTo`, by-name invocation, `SemanticRole`). Two of its pieces have hard prerequisites in other milestones and are built **there**, not as orphaned reflection slices:

| Item | Host milestone | Note |
|---|---|---|
| Capability-gating + `@reflectable` tree-shaking roots (reflection metadata elimination) | **DCE / AOT compile-mode** (§9.8) | The reflected-upon root-set is only meaningful to a tree-shaker, which that milestone builds. Reflection behaves identically gated or not — a binary-size optimization, semantically invisible. Until then, all reflection metadata is resident (correct in every interpreted/dev mode) |
| Reflection cross-`dyn` element-type recovery ("C") | **Reified generics / packed value types** (M2, §3.1) | Needs type arguments carried in shapes at runtime; rides along with that mechanism (see the Traits/generics row above) |

## Diagnostics source attribution

| Item | Source | Status |
|---|---|---|
| ~~`SourceMap` / source attribution for a **check/runtime** diagnostic landing inside a merged-in cross-module declaration body~~ | M1.9 | **Done (slice F4, `plans/followups/slice-f4-sourcemap.md`).** Resolved via **`SourceId` on `Span`** (not the global-coordinate shift-visitor the original sketch proposed): the parser stamps each span with the file's `SourceId`, `Linked` carries a `SourceMap` (`lang-span`), and `lang-conformance`/`lang-cli` resolve each diagnostic through it. Correct by construction — no AST visitor. Covered by `tests/conformance/modules/cross_module_error/` (`E0008` rendered at `models.lang:8:12`) + loader/span unit tests. The original repro (sibling `1/0` rendered against the entry, inside a comment) now renders against the sibling. |

## Performance (invisible to `RunResult` — behavior is already correct)

| Item | Source | Trigger |
|---|---|---|
| ~~Inline caches for member access and trait-method call sites (currently a hashmap/shape lookup)~~ | M1.4, M1.8 | **DONE** (`plans/perf/p-ic-inline-caches.md`, P-IC) — per-run monomorphic inline cache (one slot per `LoadField`/`CallMethod`, compiler-assigned id → `Module.cache_slots`, VM side array keyed by raw shape pointer with an `Rc<Shape>` clone keeping it alive). Memoizes field slot / method prototype; skips the `slot_of` scan and the `(type, method)` hashmap lookup + its two `String` clones. **−22–23% on `vm_member_dispatch`**, +1.6% on field access, no regression elsewhere. Polymorphic-site guard `classes/polymorphic_call_site.lang`. |
| `gc-arena` tracing path for `__destruct`-free classes (refcount is the only path today) | M1.6 | A benchmark demonstrating refcount overhead that tracing would remove |
| ~~Lazy real-disk reads behind the `fs.open` handle (M2.5 snapshots the file at open; surface is final)~~ | M2.5 | **Done (P-LAZY, `plans/perf/p-lazy-fs-open.md`)** — the host delivers a read handle's bytes via a neutral `ReadSource` (`Snapshot`\|`Lazy(id)`); `SandboxHost` always snapshots (differential byte-identical), `RealHost` streams a line at a time through a `BufReader` registry (`fs_open_read`/`fs_read_more`). Handle stays `Clone`/`Eq` (only an id crosses the seam → `lang-value` untouched). Bench: **~31× faster** time-to-first-line on an 8 MB file (1.58 ms → 51 µs). |
| ~~Copy-on-write / unique-owner in-place list append (the `~` concat operator copies the whole left `Vec` each time, so an accumulator loop `acc = acc ~ [x]` / `acc ~= [x]` is **O(n²)**)~~ | L1 (list-building) | **DONE — both backends** (`plans/perf/p-cow-list-append.md`, P-COW). Take the accumulator out of its storage slot before evaluating the RHS of a self-concat `acc = acc ~ rhs` (guarded by "rhs doesn't mention acc" via shared `Expr::mentions`), so a uniquely-owned list is extended in place; an alias keeps the refcount > 1 → copy. **Eval** (`7d41021`): `Rc::get_mut`, scope take-out — 56× at n=8000. **VM** (this commit): `Op::ConcatInPlace` (consumes lhs; `refcount() == 1` ⇒ in-place `Value::list_extend`) + `Op::TakeGlobal` (move-out for a global accumulator) — **−93% at n=8000**, closing the eval/VM asymmetry (the manual refcount already supplies uniqueness — NO gc-arena/allocator rework was needed, contrary to the earlier guess). Celled-local/upvalue accumulators fall through to ordinary concat. Did NOT need: a persistent vector (RRB / `im::Vector`) or a mutable `.push`. |
| ~~Redundant checker passes for `type_of` fidelity A (attribute-system P2.3): `lang-compiler::compile` and `lang-eval`'s `run` each call `lang_check::resolve_type_of_sites`, which re-runs the full checker — so a CLI `run` type-checks the program ~3× (the `checked` gate + once per backend).~~ | P2.3 (reflection) | **Done** — `lang_check::check_all(program) → Checked { diagnostics, type_of_sites }` runs the checker once and returns both. Each backend kept a self-deriving default (`compile`, `Backend::run` / `run_with_host` — for the REPL, unit tests, and any caller without a precomputed map) but gained a threaded variant (`compile_with_sites`, `run_with_sites` / `run_with_host_sites`). The orchestrators now check once and thread `type_of_sites`: the CLI `run_linked`, the conformance harness (both the single-file and linked paths), and the `lang-db` `checked` query (widened to carry the map) which `bytecode` and the differential eval path read. Pure perf no-op — conformance 196/0, differential 190 matched / 0 skipped / backends agree. |

## Notes

- ~~**Stale code comments to tidy**~~ — **Done (F3)**: corrected the `lang-eval` operator-dispatch comment (comparisons *do* trait-dispatch via `Comparable`, and `Ordering` is now a namable type) and the `lang-ast` "return type not yet checked in M0" / "is M1.8b" comments (checked since M1.7; the trait wiring shipped).
- The roadmap's **M2 "Deferred follow-ups"** bullet duplicates the lazy-`fs.open` row above by design — the roadmap is the milestone index; this file is the complete cross-milestone registry.
