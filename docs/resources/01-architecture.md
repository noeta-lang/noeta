# Architecture: Runtime, Compiler, and Features

*Working title: the language is referred to here as **the language** / **the runtime**. Name TBD.*

This document describes the architecture of a **new programming language, built from scratch in Rust** — a persistent, reactive runtime with a real type system, deployable to any surface (CLI, web, desktop) as a single binary. Its surface is broadly PHP-like and PHP's design informed several decisions (often by showing what *not* to do), but it is its own language: it does not parse PHP source, does not aim to run the existing PHP ecosystem, and PHP familiarity is incidental rather than a design goal. Much of the design is framed by analysis of where PHP's architecture constrains it and how a clean Rust implementation dissolves those constraints. (Bridging Composer/Laravel/Zend is out of scope; see the implementation plan's strategy gate.)

---

## 1. Guiding principles

1. **Coherent surface, powerful spine.** The surface is designed for clarity and consistency first; it happens to read as broadly PHP-like (curly braces, familiar keywords), but PHP resemblance is incidental, not a goal — choices are made on their own merits, not to maximize familiarity. The underlying semantics borrow the best ideas from modern language implementations (V8/JSC hidden classes, CPython 3.11+ specialization, Rust's ownership and type system, ML-family algebraic data types).
2. **Powerful features are opt-in and degrade gracefully.** A developer writing a simple handler reaches for the full apparatus only when it helps — but the *simple* path is still the principled one: a recoverable error is `do_thing()?`, absence is `?T`, a value is immutable unless marked `mut`. "Scaling down" means less ceremony, not a less-safe parallel mechanism. The language scales down to easy code and up to rigorous code along one coherent set of primitives.
3. **Persistence is the keystone.** The runtime is a long-lived process by default, not die-after-every-request. Nearly every headline capability — bundled server, reactivity, connection pooling, background work, and the absence of any need for a shared-memory bytecode cache (the role PHP's opcache fills) — follows from this single decision.
4. **One runtime, editions as a front-end-only concept.** Versioned semantics of *this language* are expressed through *editions* that change parsing, defaults, and desugaring — never the object model or GC, which are runtime-wide invariants. (Editions govern this language's own evolution, not compatibility with Zend PHP.)
5. **Stand on the Rust ecosystem.** Parsing infrastructure, async I/O, HTTP serving, LSP plumbing, and most of the standard library are wrapped from mature crates rather than built from scratch. The irreducible core (type checker, compiler, VM, object model, semantics) is what we actually build.

---

## 2. Why PHP's own architecture limits it

Understanding the constraints we are escaping clarifies the design. PHP's difficulty with runtime generics — the discussion that started this — is downstream of three specific 1990s-era decisions:

- **`zval` carries concrete runtime type, not parameterized type.** A PHP value is a tagged union whose type tag is `IS_ARRAY`, `IS_OBJECT`, etc. There is no slot to store a type argument, so `Collection<User>` and `Collection<Post>` are indistinguishable at the value level.
- **`zend_class_entry` is interned per class name and shared immutably via opcache.** Class identity is keyed by name and cached in shared memory across requests. Reification would require class identity to depend on call-site type arguments unknown at class-compile time, which the shared-immutable opcache model cannot safely express.
- **Native `array` is an untyped `HashTable`, not an object.** The single most-used container has no class entry on which to hang a type parameter, and lives on the hottest code path in the engine.

Our design replaces all three: **shapes** (hidden classes) give per-instance type identity, **inline caches** make type checks cheap guards rather than per-element loops, and a **per-isolate compilation cache** removes the shared-immutable constraint. Generics then stop being a special feature and become a natural consequence of how object identity already works.

---

## 3. Value representation

**NaN-boxed 64-bit values.** Rather than a fat `enum Value` (16+ bytes, branchy), values are NaN-boxed: doubles stored natively, everything else encoded in the NaN payload with a small type tag, pointers in the low 48 bits. In Rust this is an `unsafe` newtype `struct Value(u64)` with safe accessors. This is the LuaJIT / JavaScriptCore approach and keeps values pointer-sized and cache-friendly.

Reference types (strings, objects, lists, maps, closures) point to GC-managed heap objects.

### 3.1 Packed value types and flat arrays (numeric / SIMD performance)
The NaN-boxing + shapes design above is optimized for *dynamic-language generality and dispatch speed*. That goal is in genuine tension with *numeric/data throughput*, and the type system must account for both — performance is a **type-system (layout) concern**, not only a runtime (dispatch) concern. Inline caches and the specializing VM make *dispatch* fast; they do nothing for *data layout*. SIMD and numeric performance are layout problems, decided by the type system: whether a value is boxed, whether an array is flat, whether a struct is packed.

The tension, concretely:
- A NaN-boxed value is 64 bits with a tag — great for "could hold anything," wrong for packed numerics. A `Vec3` is three `f32`s (96 bits of data); SIMD wants four `f32`s contiguous in a 128-bit register with no tags, no boxing, no indirection.
- A shaped object has a header + shape pointer + slots. An array of 10,000 `Vec3`s as shaped objects is 10,000 heap objects — cache-hostile and un-vectorizable. Numeric/game/ECS code wants a *flat contiguous buffer* with no per-element object overhead.

So the type system distinguishes two categories, and gives the second a flat, unboxed, cache- and SIMD-friendly representation:

- **Flexible dynamic objects (the default):** shaped, heap-allocated, suited to polymorphism and dynamic dispatch — everything described in §4.
- **Packed value types:** a `record` whose fields are all primitives (or other packed value types) is laid out **unboxed and contiguous** — no header, no shape, value semantics, passed by value. This is what makes `Vec3`, `Quat`, `Mat4`, colors, and similar SIMD-amenable. It is a natural extension of the existing **record** type (§4.1): records are already value-semantics and structural; packed value types add a predictable-layout guarantee for the all-primitive case. Structural update and `Clone` (§4.2) already fit value-copy semantics.
- **Flat typed arrays:** a `List<T>` of a packed value-type (e.g. `List<Vec3>`, `List<f32>`) is stored as a **flat contiguous buffer**, not an array of object pointers — the single most important representation for games, numerics, and entity-component-system layouts. This requires a generics carve-out: `List<f32>` / `List<Vec3>` get specialized flat layouts rather than the general boxed-object array. That is the same "specialize the hot case" instinct the inline caches embody (§4), applied to *layout* instead of *dispatch* — monomorphized flat storage for packed element types.

**SIMD** then has contiguous, tagless data to work on: either the compiler auto-vectorizes hot loops over flat typed arrays, or (more reliably near-term) the numeric/3D-math stdlib binds **SIMD-backed native kernels** (glam-style, via the native FFI §10) for the hot operations, while the operator-trait surface (§9.2) keeps the *syntax* elegant (`position + velocity * dt` for vectors, dot/cross as methods). Both halves are needed: operator traits give the elegant surface, packed value types give the fast layout.

**Why this is designed in, not bolted on.** Languages that planned for value-type layout (C#'s `struct` vs `class` + `Span<T>`, Swift value/SIMD types) are competitive at numerics; those that did not (early Java, Python) are permanently slow at it and escape via native libraries (NumPy). Layout is **representation-level and the hardest thing to retrofit** — it touches the value representation, the array implementation, generics specialization, and the FFI at once — so the boxed-object-vs-packed-value-type distinction belongs in the design from the start, even though packed types and flat-array specialization can be *implemented* after the dynamic core works. The capability/DCE discipline (§9.8.1) means the numeric machinery only ships when used.

---

## 4. Object model: shapes and inline caches

Objects are property bags, but not backed by per-object hashmaps (the naive approach that makes dynamic languages slow).

- **Shapes (hidden classes).** Each object holds a pointer to a *shape* describing its property layout. Properties live in a flat inline slot array indexed by the shape. Adding a property transitions to a new shape via a cached transition tree.
- **Inline caches.** Every property access and method call site caches the shape it last saw and the resolved slot/method. A monomorphic call site (one shape) resolves in a couple of instructions.
- **Generics fall out of shapes.** A `Collection<User>` is a shape carrying a type-parameter slot. The shape *is* the per-instance type identity PHP's shared class entry cannot provide. Generic validation becomes a guard in the inline cache, elided once a site proves monomorphic — not an O(n) per-element loop.
- **Proxies and magic methods are faster, not just supported.** `__get`/`__set`/`__call`/`ArrayAccess` map to interception points: an object whose shape records "intercepts reads" specializes its call sites straight to the interceptor, memoizing the dispatch decision PHP re-derives on every access. Transparent proxies (structurally indistinguishable from their target, sharing its shape) become possible — an upgrade ORMs cannot get from PHP today.

### 4.1 Object creation: one checked primitive, no privileged constructor
There is no special constructor. The single creation *primitive* is the **all-fields record literal** (`Money { amount: ..., currency: ... }`), which the compiler requires to assign **every** field — this is the one syntactic choke point where full initialization is guaranteed before an object can escape. Everything else builds on it:

- **Constructors are ordinary associated functions returning `Self`** (or `Result<Self, E>` / `Option<Self>`) that produce a value via the literal internally. `new` is convention, not a keyword; a type may have any number of named constructors (`new`, `zero`, `from_cents`, `parse`) as equals. This dissolves PHP's split between the privileged `new` keyword and factory-method workarounds.
- **Field-initialization safety is preserved** without a special constructor: the all-fields literal is the checked choke point, so no function can return a partially-initialized object.
- **Encapsulation via literal visibility.** The all-fields literal is *private by default* for types that enforce invariants (only the type's own functions may use it, forcing outsiders through validated constructors like `parse`), and may be made public for plain data records where direct construction is fine. This replaces the constructor's role as the controlled entry point.
- **No promotion needed.** Fields are declared once in the record/class body and filled by the literal, so PHP 8's constructor-promotion ergonomics come for free rather than as a special signature form.

This is the Rust struct-literal model. It is what makes "creation is just a function" safe rather than a hole in initialization guarantees, and it is why constructors can be inferred by tooling purely from their signature (associated function, no `self`, returns the enclosing type).

### 4.2 Structural update (functional update / clone-with-changes)
Immutable-by-default (§9.1) makes "the same value but with one field changed" a constant need; without a primitive for it, immutability is tedious (hand-copying every unchanged field). The all-fields literal extends with a **spread** to provide it:

```
a = Money.new(500, USD);
b = Money { amount: 300, ..a };     // new Money; amount overridden, all other fields from a
```

This is the proven functional-update primitive (Rust `Point { x: 5, ..old }`, Elm `{ old | x = 5 }`, Kotlin `old.copy(x = 5)`), unified into the creation literal rather than added as a separate feature:

- **Same checked choke point.** The spread fills every field the caller did not name, so the compiler's full-initialization guarantee (§4.1) still holds — a structural update cannot produce a partially-initialized object.
- **Same visibility rule.** If a type's all-fields literal is private, outsiders cannot spread-update it either — correct for invariant-enforcing types.
- **Shallow by default; `Clone` is the customization hook.** The spread copies slots shallowly, which is the right default precisely *because* immutability makes shared substructure safe to alias (no one can mutate it). This is distinct from the `Clone` trait (§9.2), which is user-defined behavior for cases where a shallow slot-copy is wrong (deep-copying a contained collection, resetting a cached field). The two compose as layers of one spectrum: structural update is the language primitive; where a field's type customizes `Clone`, the spread honors it. Deep duplication is opt-in (`..a.deep_clone()`), never silent — silent deep-clone-on-every-update would be a performance trap.

The novelty for persistence (§9.12 reactive persistence): because a structural update *names exactly the changed fields*, the change set is structurally explicit — dirty-tracking is free and precise, rather than reverse-engineered by snapshot comparison as ORMs do today.

---

## 5. Memory management

The decisive constraint is `__destruct` semantics. PHP promises **deterministic destruction**: an object's destructor runs synchronously when its last reference drops, in program order. Real code depends on this for resource cleanup (file handles, transactions, locks).

**Decision: refcount + cycle collector as the runtime-wide floor.**

- Synchronous `__destruct` for the acyclic common case (count hits zero → destructor runs immediately).
- A tracing cycle collector (PHP's "gc roots" model) reclaims reference cycles, where destruction order is best-effort — matching PHP's existing weaker guarantee for cycles.
- Because the runtime is shared-nothing per isolate (see §7), refcounts are **non-atomic**, avoiding the major performance tax of atomic refcounting.

**Tracing GC as an internal optimization only.** Objects of classes with no `__destruct` (statically known at class definition) may be managed by a tracing collector to avoid per-object refcount overhead. This must never change observable semantics. The hybrid is a throughput/memory optimization, not a semantic option — because liveness strategy is a property of *the object*, not of the code referencing it, and cross-heap references (a traced object holding a destructor object) otherwise silently weaken the determinism guarantee.

**Why not tracing-by-default:** it would make `__destruct` best-effort for *all* code, including code that never opted in. GC strategy cannot be edition-gated (see §8) because two files that share an object cannot disagree about how that object's liveness is determined.

---

## 6. Execution: tiered, register-based, specializing

- **Register-based bytecode** (Lua/Dalvik style), not a stack VM — fewer dispatches, friendlier to later JIT work.
- **Tier 0 — baseline interpreter** with inline caches at every property access and call site.
- **Tier 1 — specializing interpreter** (CPython 3.11+ model): hot opcodes rewrite themselves in place to type-specialized variants (`LOAD_ATTR` → `LOAD_ATTR_SHAPE_HIT`) once a site proves monomorphic. This delivers most of the performance of a JIT with far less maintenance burden.
- **Optional future JIT** via copy-and-patch compilation (CPython 3.13 model) — portable and far more tractable in a Rust codebase than an LLVM or hand-rolled assembler backend. Not part of the initial vision.

The bytecode optimizer passes (PHP's `opcache.optimization_level` equivalent) live in the normal compile pipeline, not as a bolt-on extension.

**Incremental compilation (unified with the LSP).** The compiler is structured as a graph of memoized **queries** (`salsa`, the framework `rust-analyzer` is built on) rather than straight-line passes: "the AST of file X," "the type of function Y" are queries that track their dependencies, so when an input changes only the transitively-affected queries recompute. This is not a separate feature — it is *the same query graph* that powers a responsive LSP (§9.6), so incremental rebuilds and instant editor feedback fall out of one system. It is also the same dependency information that classifies the blast radius of a change for HMR (§9.14). Build the compiler this way from the M1 checker onward; the discipline (queries, not mutable passes) is paid once and yields several wins — fast rebuilds, responsive tooling (LSP), HMR blast-radius classification, agent-local change verification (plan §7), and rule-based static analysis (§9.17) — all the same underlying capability: *knowing precisely what depends on what, and recomputing only the minimum.*

---

## 7. Concurrency: isolates plus message passing

PHP code is overwhelmingly not thread-safe; it assumes shared-nothing per request. The only model compatible with that reality:

- **Isolates.** Each unit of concurrency gets its own heap. No shared mutable objects. Communication by passing copies or immutable handles (Erlang processes / Web Workers / Ruby Ractors model).
- **Per-isolate GC.** Each isolate collects independently — no stop-the-world across the whole process, and non-atomic refcounts as noted.
- **Intra-isolate async** (`async`/`await` over a real scheduler) for I/O-bound concurrency — formalizing what PHP Fibers (8.1) began.
- **Inter-isolate channels** for CPU-bound parallelism.
- **Shared-memory parallelism stays inside the runtime.** Rust handles it safely internally; userland never sees `Arc<Mutex<T>>`-style primitives, which would recreate every data race PHP is currently free of.

This composes with everything else: per-isolate persistent heaps are what remove any need for a PHP-opcache-style shared-memory cache and enable in-process caching, pooling, and background work.

### 7.1 Async and structured concurrency (the everyday case)
The isolate model above is the *parallelism* story (CPU-bound work, true multi-core). The far more common everyday need is **I/O-bound concurrency** — making many network/DB/file calls that mostly wait — and that is served *within* an isolate by `async`/`await` over a real scheduler (`tokio`, §plan). This is central to the server/web positioning, not peripheral, so it is specified rather than gestured at.

- **`async`/`await` as the surface.** An `async fn` returns a future; `await` suspends until it resolves, freeing the isolate to make progress on other tasks. A request handler awaiting a DB query does not block other requests in the same isolate. This is the model TypeScript/Rust/Python developers already expect, and it formalizes what PHP Fibers (8.1) began.
- **Structured concurrency, not loose task-spawning.** Concurrent work is *scoped*: a `concurrent { ... }` block spawns child tasks whose lifetime is bounded by the block — the block does not complete until all children do, child errors propagate out *at the block boundary*, and cancellation cascades to children. Critically, **`spawn` is only legal inside such a scope** (a `concurrent` block or an `async fn` body) — `spawn` with no owning scope is a compile error, which is what makes the orphaned-task and leaked-error failure modes of bare `go`/dangling-promise *impossible by construction* rather than merely discouraged. Genuinely long-lived background work (queue workers, the p2p node §9.15, schedulers) is runtime/isolate-owned, not block-scoped `spawn`, so the scope rule stays absolute. This is the structured-concurrency model (Swift, Kotlin, Trio). Awaiting many things concurrently (`all`, `race`, bounded-parallelism `map`) are library functions over this.
- **The two layers compose cleanly.** *Intra-isolate async* (cooperative, single-heap, for I/O) and *inter-isolate channels* (parallel, separate heaps, for CPU work) are distinct and combinable: an isolate handling requests with async I/O can offload a CPU-heavy job to a worker isolate via a channel and `await` the result. Async is the default reach-for tool; isolates are the escalation when you need real parallelism.
- **Errors and cancellation are typed.** Async operations return `Result` like everything else (§9.1); `?` propagates through `await` (`let row = query(...).await?`). Cancellation is explicit and cooperative, surfaced as a typed outcome rather than an exception.
- **Persistent-runtime synergy.** Because the runtime is persistent (§ keystone), async resources that are expensive to create — connection pools, HTTP clients, background tasks, timers — live across requests rather than being rebuilt per request (PHP's per-request death made this impossible). Long-lived background work (queues, schedulers, the embedded p2p node §9.15, observability exporters) runs as async tasks or dedicated isolates within the same process.

The discipline: `async` is the everyday concurrency tool (I/O), isolates are for parallelism (CPU), shared-memory threading is never exposed to userland (§7), and concurrency is *structured* so lifetimes and errors are bounded by construction.

### 7.2 One primitive (`TaskScope`), background work, and the language/framework boundary
There is exactly **one** concurrency-ownership primitive — a **task scope** — and the language deliberately owns *nothing more*: no `background` global, no worker construct, no queue, no scheduler, no DI container. Those are patterns/frameworks built on the primitive, not language features. This keeps the language small and avoids baking a framework into it.

**`concurrent { }` is the block-lifetime form of the task scope.** Fire-and-forget work that must outlive a request is *the same primitive with a longer lifetime* — a `TaskScope` whose life is the application rather than the block. So background work needs no new mechanism; it needs the scope to exist at a different lifetime.

- **Block lifetime:** `concurrent { }` is sugar for a scope that joins at `}` (§7.1, syntax §9.4).
- **App lifetime:** a `TaskScope` constructed at startup, owned by the application, and **obtained via dependency injection** — *not* an ambient global. It is an ordinary injectable value, exactly like a DB pool or logger:

```
fn handle_signup(req: Request, tasks: TaskScope): Response {   // `tasks` injected, not ambient
    user = create_user(req)?;
    tasks.spawn(fn() => send_welcome_email(user));            // owned by the app scope; outlives the request
    return Response.ok();                                     // returns now
}
```

This de-magicks fire-and-forget: `tasks` is a visible parameter, injected, as explicit as any dependency. The structured-concurrency rule is intact — `spawn` still requires an owning scope; here the scope is the app-lifetime `TaskScope` rather than the enclosing `concurrent` block, so the task is **owned, not orphaned** (§7.1's "`spawn` needs a scope" stays absolute). The app scope **drains on graceful shutdown** (waits for in-flight tasks, or cancels after a timeout), which is the shutdown story raw fire-and-forget lacks. Because a fire-and-forget task's failure has nowhere to return (the caller is gone), `TaskScope.spawn` requires the task to handle its own errors (the spawned fn returns `()`, having logged/retried/dead-lettered) rather than silently dropping a `Result`.

**The language/framework boundary (deliberate):**
- **Language:** `TaskScope` + `concurrent { }` sugar + the `spawn`-needs-a-scope rule. The language guarantees `TaskScope` is a well-behaved value — constructible, ownable, drains on shutdown. That is the entire concurrency-ownership surface.
- **DI:** *not in the language.* The app-lifetime `TaskScope` reaches handlers by injection, provided by the framework/library layer, so it is never ambient. The language only guarantees `TaskScope` is an ordinary injectable value; *how* it is injected is a framework concern.
- **Framework / first-party extensions:** workers (a type holding an injected `TaskScope` + a channel), durable job queues (persisted jobs over the DB layer §11.4, surviving restart, retried), and schedulers (cron/interval-triggered tasks) are **patterns built on `TaskScope`**, shipped as first-party extensions (§10, `lang add`), not language constructs. A worker *is* an isolate you message (§7); a durable queue *is* a worker with a persisted mailbox; a scheduled task *is* a time-triggered `spawn`. None of these is a new language concept.

**Why this is better than the PHP world:** in PHP every tier of background work needs external infrastructure — Supervisor for workers, Redis for the queue, system cron for schedules — because PHP hosts no resident process. Here the persistent runtime hosts all of it in-process, so a single binary can be the web server *and* the job worker *and* the scheduler, collapsing the "web app + Horizon + Redis + cron + Supervisor" stack into one artifact — the same single-binary-collapses-the-stack win as the bundled server (§9.5), extended to background work.

---

## 8. Editions: front-end only

This is a **new language inspired by PHP, built from scratch** — it is *not* PHP and does not parse PHP source. Bridging the actual PHP ecosystem (Composer, Laravel, Zend-compatible syntax) is explicitly out of scope for the core design and deferred to a maybe-never phase (see the implementation plan's strategy gate). The editions mechanism below governs *this language's own* evolution over time, not Zend compatibility.

Versioned semantics follow Rust's edition model — and critically, **editions change only the front end**: legal syntax, deprecations, default `strict_types`, the namespaced standard library, non-null-by-default, whether certain constructs are allowed. They desugar to the *same* bytecode and run on the *same* GC and object model.

- Older editions of *this language* run **on this same runtime**; a future edition can tighten defaults or change syntax without breaking files written against an earlier edition.
- A file-level `edition` declaration selects the dialect. Cross-edition calls work because everything below the parser is identical.
- **What cannot be edition-gated:** GC strategy, the object model, the concurrency model — any runtime-wide invariant. Two files sharing an object must agree on how that object behaves. This is why immutable-default mutability and `Result`-based errors are *language* features available everywhere, while strictness and stdlib naming are *edition* features.

---

## 9. Feature set

### 9.1 Type system
- Gradual but real: optional annotations, name-first (`x: int`), with inference.
- **Generics** via erasure-for-storage, reification-for-identity: the shape encodes the type argument; checks are inline-cache guards, elided when monomorphic.
- **Algebraic data types** (sum types with associated data) and **exhaustive `match`** — the core "make illegal states unrepresentable" primitive PHP lacks.
- **`Result<T, E>` and `Option<T>` are the primary path** for recoverable failure and absence, with the `?` propagation operator. This is the everyday mechanism — a simple recoverable error stays a one-liner (`do_thing()?`), so the primary path is also the easy path.
- **One nullability story.** `?T` is sugar for `Option<T>` — not a separate, casual "nullable return" parallel to it. Non-null by default; absence is always `Option`, expressed as `?T` where convenient. There is no ambient `null` and no second nullability mechanism.
- **Exceptions are for the genuinely exceptional only** — unrecoverable conditions and programmer error ("this should never happen"), surfacing as a panic that unwinds the isolate (§7). They are *not* a co-equal everyday alternative to `Result`; everyday recoverable errors use `Result`, deliberately, because blessing a parallel throw-for-everything path would undercut the make-illegal-states-unrepresentable guarantee that is the point of the type system.
- **Immutable by default**; `mut` opts into mutation. This pairs with ownership analysis to elide defensive copies on copy-on-write collections — a correctness default that is also a performance signal.

### 9.2 Traits and built-in protocols (unified operator & native-type behavior)
A single mechanism — **traits** (the Rust trait / Swift protocol / Haskell type-class idea) — covers both "make a class behave like a native type" and "make objects comparable, addable, etc." In this language there is **no separate category of magic methods**: operator behavior and native-type behavior are both just stdlib traits a type implements, and operators dispatch through them (`a + b` is `Add.add`, `a == b` is `Equatable.eq`, `a < b` derives from `Comparable.compare`, `a[i]` is `Index.get`, `echo a` is `Display.to_string`). This collapses PHP's two separate weaknesses — a scattered grab-bag of magic interfaces/methods (`ArrayAccess`, `Stringable`, `Countable`, `__get`, `__call`, ...) and the total absence of operator overloading and object comparison — into one coherent feature.

**The principle:** *every PHP magic method that expresses behavior user code invokes becomes a trait; construction is ordinary functions; only destruction stays a distinct language construct, because it is the one hook invoked by the runtime rather than by user code.*

- **Construction is not special at all (no privileged constructor, no `fn construct`).** Objects are created by ordinary **associated functions returning `Self`** (or `Result<Self, E>` / `Option<Self>`), over a compiler-checked all-fields record literal (see §4.1). `new` is merely the conventional name for the most common such function; `zero`, `from_cents`, `parse`, etc. are equals, not factory-method workarounds. This unifies PHP's two creation grammars (the privileged `new ClassName(...)` keyword vs. static factory methods) into one: *creation is a function that returns an instance.* It also gives fallible constructors for free — a `parse` returning `Result<Self, _>` is something PHP's constructors cannot express. Tooling (LSP, docs) infers "constructor" with no annotation: an associated function (no `self` receiver) whose return type is the enclosing type, optionally wrapped in `Result`/`Option`. The "no receiver" rule distinguishes constructing from transforming (`fn normalized(self): Money` returns `Self` but takes a receiver, so it is a transformation, not a constructor).
- **Destruction stays a distinct language construct (a `destruct` block), deliberately *not* a trait and *not* an ordinary function.** The reason is precise, not "lifecycle": every trait and every function is invoked by an expression in user code, whereas the destructor has no call site — the GC invokes it when the last reference drops. It is also uniquely (a) not directly callable (calling it would leave a live object whose destructor has already run — use-after-destruct) and (b) GC-strategy-affecting (whether a class has one partitions it between the refcount floor and the tracing optimization, §5). Those three properties all describe destruction specifically; construction shares none of them even after unification, since constructors are still called by explicit user expressions (`Money.new(...)`). Keeping the destructor outside the trait/function world honestly signals "the runtime calls this, you don't," and preserves the guarantee that makes the trait system valuable: if something is a trait, you can implement, call, derive, and compose it — all of which `drop` would violate. (Rust's alternative — a `Drop` trait with three special rules — is coherent but buys surface symmetry at the cost of a trait member that does not behave like the others.)
- **Behavior protocols, become traits:**

| PHP magic | Trait | Lights up |
|---|---|---|
| `ArrayAccess` | `Index` / `IndexMut` | `a[i]`, `a[i] = x` |
| `Stringable` / `__toString` | `Display` | interpolation, `echo` |
| `Countable` | `Length` | `len(a)` / `a.count()` |
| `IteratorAggregate` / `Iterator` | `Iterable` | `for x in a` |
| `__invoke` | `Callable` | `a(...)` |
| `JsonSerializable` | `ToJson` | serialization |
| `__get`/`__set`/`__isset`/`__unset` | `Members` (property interception) | dynamic property access |
| `__call`/`__callStatic` | `DynamicCall` (incl. static via associated fns) | dynamic method dispatch |
| `__clone` | `Clone` | copy customization |
| `__serialize`/`__unserialize` | `Serialize` | (de)serialization |
| *(absent in PHP)* | `Equatable` | `==`, `!=` |
| *(absent in PHP)* | `Comparable` | `<`, `<=`, `>`, `>=` |
| *(absent in PHP)* | `Add`/`Sub`/`Mul`/`Div`/`Concat` | `+`, `-`, `*`, `/`, `~` |

**Why this is elegant rather than just more interfaces:**
- **One concept.** Operator overloading and native-type behavior are not two features bolted on; they are both "implement a stdlib trait." The `~` concatenation operator (syntax doc) dispatches through `Concat`/`Add` like every other operator — no special-cased operators sit outside the system.
- **Default methods compose.** Implement `compare` once and `< <= > >=` derive for free; implement `eq` and `!=` falls out. (Swift/Rust model.)
- **Derivation kills boilerplate.** `#[derive(Equatable, Comparable, Display)]` synthesizes field-wise implementations for the common value-object case; hand-written `impl` is reserved for custom logic. (Rust `#[derive]` / Swift automatic synthesis.)
- **Type-checked and discoverable.** A type *declares* the traits it implements, so the checker can require that anything used with `<` is `Comparable`, catching at compile time what PHP would silently coerce. Tooling and readers see the protocols a type participates in.
- **Static-context protocols** (constructors-as-protocols, `__callStatic`) are handled by traits carrying static/associated functions, keeping one mechanism rather than a static-magic special case.

**Fallible operators (an improvement over Rust).** Because `Result` is in the spine (§9.1), the stdlib offers both infallible protocols (`Add`, `Comparable` for total orderings) and fallible variants (`TryAdd`, `TryComparable`) returning `Result<_, E>`. The bare operators (`+`, `<`) require the infallible form; types where the operation can fail (mismatched-currency `Money`, incomparable units) implement the `Try` form and are used through a `?`-friendly method, so failure is a typed error the caller handles rather than a panic. No mainstream language with operator overloading offers this cleanly; it falls out of having `Result` already.

This is also the same interception mechanism the object model uses (§4): a class implementing `Members` or `DynamicCall` has a shape recording it intercepts, and call sites specialize straight to the interceptor — so proxies/magic-property objects are faster here than PHP's per-access magic-method re-dispatch.

### 9.3 Data-oriented programming
- Distinct `List` and `Map` types with distinct literals — ending PHP's dual-purpose-array wart.
- Value-type **records** with structural equality and cheap copying, distinct from reference objects.
- Pattern matching that destructures.
- Pipeline operator for left-to-right data flow.

### 9.4 Reactivity (differentiator)
- **Server-side signals** as a language-level primitive: `signal` / `computed` / `effect`. Computed values auto-recompute when dependencies change; effects auto-run.
- This is the missing primitive for server-driven UI (Livewire/LiveView-style). Because the runtime knows exactly which computeds changed, it can compute the minimal diff to push over the bundled WebSocket connection — a correct-by-construction LiveView, with no WASM required.
- Also yields reactive caching/invalidation: a `computed` over a query invalidates when its source signal changes.

### 9.5 Bundled HTTP/WS server (differentiator)
- The runtime ships an HTTP/1.1+2(+3) server with native WebSockets and SSE, layered over `hyper`/`axum`.
- `build` produces a single static binary that *is* the web server — no nginx, no FPM, no separate language runtime to install. Go's deployment story, with reactivity and a real type system on top.

### 9.6 Tooling as part of the runtime
- **Embedded LSP** built from the *same* parser and type checker the runtime uses — zero analyzer/runtime drift, generics/ADT-aware for free (the Rust-analyzer / `gopls` model).
- **Native toolchain**: one binary that is also `init`, `add`, `build`, `test`, `fmt`, `lint`, `lsp` — a Cargo-equivalent that absorbs rather than competes with Composer.
- **Formatter (`fmt`) — opinionated and non-configurable.** One canonical style, no options, no per-project style config (the gofmt / Prettier-without-options model). This is deliberate and consistent with the language's coherence-over-options philosophy: a single style eliminates formatting bikeshedding, makes all code in the ecosystem look uniform, and removes a whole category of config files and review arguments. The formatter operates on the lossless CST (§9.17), so it preserves what it should and normalizes the rest.
- **Lint configuration — declarative manifest, not code.** While lint *rules* are programmatic (§9.17), *which* rules apply and at what severity is declarative project config: a `[lint]` table in the project manifest selects rule-sets and sets per-rule levels (`allow`/`warn`/`deny`/`error`), e.g. `no_panic_in_handlers = "error"`. This is the right split — describing-what (config) is declarative, computing-how (the rule body) is programmatic. Inline `#[allow(rule_name)]` / `#[deny(rule_name)]` attributes scope overrides to a specific item, the same model as Rust.
- **Built-in observability**: native structured logging, metrics, OpenTelemetry-style spans auto-instrumenting HTTP handlers and DB calls, `/healthz` — natural under a persistent process, painful under die-per-request.

### 9.7 Isomorphic logic (WASM target)
- **Tier 1 — shared contracts.** Type definitions and validation rules emit both the server-side check and a client-side artifact from the one type checker. Kills the PHP/TS validation-duplication pain — the highest-value, lowest-risk slice.
- **Tier 2 — shared pure-logic modules** compiled to small WASM kernels (money math, tax/date/recurrence rules, pricing) called from JS where correctness must not drift.
- **Tier 3 — full client-in-WASM** is offered but never the default (the Blazor trap: multi-MB runtime, slow startup, costly DOM interop). WASM is for shared *logic*, not the whole client.

### 9.8 Compile mode
- **CLI binaries**: straightforward — AOT-compile bytecode, statically link the runtime. A single static binary, matching the single-binary tooling aesthetic.
- **Web-app-to-binary**: bundle the server (above). The compelling version.

#### 9.8.1 Dead-code elimination / tree-shaking
This is the mechanism that several features assume (capability-gated reflection §9.13, optional heavy extensions §9.15.2 / §10.3, "pay nothing for what you don't use"). It is specified here because the one thing that genuinely constrains dead-code elimination (DCE) is whether the program's *world is closed* — and the only feature that opens it is `eval`.

**Mechanism.** DCE is reachability analysis over the call/use graph the checker already builds (the `salsa` query graph, §6). Starting from entry points (`main`, exported server routes, registered handlers), everything transitively reachable is kept; everything else — unused functions, types, stdlib pieces, extension code — is eliminated. Granularity is function/type/module level.

**The fault line is closed-world vs. open-world — which runs between reflection and `eval`, not around "dynamism" broadly.** These are commonly lumped together as "dynamic features," but they sit on different axes and have very different costs:

- **Reflection keeps the world closed.** Even *runtime* reflection (`type_of`, `construct(type_name)`, `method.call(...)`) only ever touches the finite set of types and methods that *already exist in the program*. The compiler may not know *which* member a given call hits at runtime, but it knows the membership of the set. So reflection is **bounded** dynamism: pin the reflectable types as roots and DCE still eliminates everything else. Reflection never creates new code; it operates over compiled code that is already there. It does **not** require the compiler at runtime, and it does **not** block tree-shaking. Its only cost is that the *metadata* of reflectable types must survive into the binary — a separate, lighter concern handled by capability-gating (only types marked reflectable, or reflected-upon, keep metadata), not a DCE blocker.
- **`eval` opens the world.** `eval` turns a *string* into *running code*, so the set of code that might exist is unknown until runtime. This is the one feature that forces the compiler into the binary and makes DCE unsound — nothing can be safely eliminated because the string could name or construct anything. `eval` is **unbounded** dynamism.

So the gate is on `eval` specifically. Reflection — including dynamic, runtime reflection — lives comfortably in the default tier.

**Three tiers, static by default:**
- **Static / closed-world (default).** No `eval`. Reflection (compile-time *and* runtime) is fully available; reflectable types are pinned as roots and keep their metadata, everything else shakes. The world is closed, so **DCE is total and always supported** — smallest binaries, no embedded compiler. The overwhelming majority of apps — including those using plugin loaders, by-name construction, and serialization-by-type — live here. (This is what was previously mis-described as needing a "no-dynamic-codegen dialect"; it is simply the default, and it includes runtime reflection.)
- **Scoped extension roots (a refinement, not a separate mode).** When the set of dynamically-reached types is large or library-provided, marking modules as reflection roots is how you tell DCE what to pin. Still closed-world, still `eval`-free, still fully shakeable around the pinned set. This is bookkeeping within the static tier, not a step toward dynamism.
- **Open-world / `eval` (deliberate opt-out).** Only `eval` (and arbitrary runtime code generation) lands here. The compiler embeds itself and falls back to the interpreter tier for generated code; DCE becomes conservative or off. Larger binaries, declared explicitly in the manifest because it forfeits the small-binary guarantee. This is rare — most apps that *think* they need it actually need bounded reflection, which is free.

**Capability-gating reinforces DCE.** A node never granted blob/relay capabilities (§9.15.2), a build that never enables a feature flag, a type never marked reflectable — these make whole regions provably unreachable or metadata provably unneeded, which DCE then eliminates. Capabilities declare intent; DCE acts on it.

**Consequence:** the smallest-binary, most-optimizable configuration is the *path of least resistance* (the default), and it already includes reflection. An app pays the open-world cost only by explicitly opting into `eval` — a genuinely rare need — matching the language's static-over-dynamic through-line and keeping "any surface, single binary" honest as the feature set grows.

### 9.9 Desktop / GUI via Tauri
Desktop support is achieved by integrating with **Tauri** (itself Rust), not by binding a native widget toolkit such as GTK+. Binding GObject would mean maintaining FFI to a large, stateful C object system whose own refcounting fights this runtime's GC — a second full-time codebase that never feels native to the language. Tauri avoids the widget-binding problem entirely because its architecture is already a **two-process split**: a Rust backend and a webview frontend communicating over a message bridge. The runtime sits on one side of that channel rather than binding a toolkit.

This is near-free reuse of the persistent-runtime stack, because desktop is, architecturally, *web's mechanism pointed at a local window*:
- The persistent, isolate-based, message-passing runtime (§7) is exactly the shape of a Tauri backend (long-lived process handling commands, emitting events).
- The bundled server (§9.5) already speaks the protocols a webview frontend wants.
- Reactivity / signals (§9.4) drive a local webview's DOM with the same minimal-diff push used for remote browsers (§9.4's LiveView mechanism). Desktop reactivity comes for free from the web work.

**Two integration depths:**
- **Depth A (start here, cheap):** Tauri provides the native shell — window, webview, OS APIs, installer. The runtime serves the UI (it already has a server) and its reactivity drives the webview; almost no integration with Tauri's Rust API is required. A working desktop app demo falls out of the M2 stack at near-zero marginal cost.
- **Depth B (optional later deepening):** bind Tauri's Rust command/plugin API so developers write Tauri commands, native menus, tray, dialogs, and notifications *in this language*. A real but bounded binding surface (small and clean, unlike GObject), added incrementally — not a separate project.

**Honest tradeoff:** the frontend is web tech (HTML/CSS/JS/WASM in a webview), not native platform widgets — the same tradeoff Tauri and Electron make. This makes the language excellent at *web-UI desktop apps* (the VS Code / Figma-desktop category), not native-look apps. The view being a webview does not make the language a web language: the runtime, logic, and reactivity are the language's own; only the rendering surface is web. This rounds out an "any surface" positioning — single-binary CLI tools, reactive web apps, and single-binary desktop apps from one persistent reactive runtime.


### 9.10 No shared-memory bytecode cache needed
- PHP needs opcache because it is shared-nothing-per-request and re-parses/re-compiles every file on every request unless that work is cached in shared memory. This language has **no such component and needs none**: under a persistent process (§ keystone), bytecode is compiled once on load and stays resident, so there is nothing to re-parse per request and nothing to cache across requests.
- What *does* exist is much smaller and different in kind: an optional **on-disk startup cache** (JVM AppCDS / V8 code-caching model) so cold process startup can skip re-parsing, plus the bytecode optimizer folded into the normal compile pipeline (not a separate, configurable, sized-in-megabytes extension as in PHP). The whole "opcache" concept being a tunable add-on is itself a symptom of the request-isolation model this language does not have.

### 9.11 Deployment targets (overview)
All surfaces ride on the same persistent runtime; the runtime is built once and pointed at different shells. This is the "any surface" positioning made concrete — a single language and runtime spanning CLI, web, desktop, and shared-logic-in-the-browser.

| Surface | What it produces | What it bundles | Leans on | Section |
|---|---|---|---|---|
| **CLI tool** | Single static binary | Runtime + AOT bytecode | (core only) | §9.8 |
| **Web app** | Single static binary that *is* the server | Runtime + bundled HTTP/WS server | `hyper`, `axum`, `tokio-tungstenite` | §9.5, §9.8 |
| **Desktop app** | Single binary / native installer | Runtime + server + native webview shell | `tauri` (`wry`) | §9.9 |
| **Shared logic (browser)** | WASM module(s) consumed by a JS/TS frontend | Contracts / pure-logic kernels only — *not* the whole runtime | `wasm-bindgen`, `wasm-pack` | §9.7 |

Notes:
- The first three are **whole-program** targets sharing one runtime; only the *shell* differs (none / server / webview). Desktop is web's mechanism pointed at a local window, so it is near-free reuse of the web target rather than a separate stack.
- The WASM target is **deliberately partial** — it ships shared *logic* (validation, pure kernels), never the full runtime in the browser (the Blazor trap). It is a complement to a JS/TS frontend, not a fourth whole-program runtime.
- Reactivity / signals (§9.4) drive the view identically for the web and desktop targets (remote browser vs. local webview), so the UI programming model is one model across both.
- Dynamism handling for the AOT whole-program targets is as described in §9.8.1: **static by default** (closed world, total tree-shaking, smallest binaries), scoped dynamism for bounded plugin/by-name cases (still shakeable), and a deliberate full-`eval` opt-out (compiler embedded, DCE off) for the rare app that needs it.

### 9.12 Reactive persistence / object mapping (R&D direction)
*This subsection is a differentiating R&D bet, not a finalized design. It has real open problems (flagged below) and is scoped so the credible part ships first and the novel part is layered on once signals are proven.*

Object mapping is a crowded space; "another ORM" is not a reason to exist. The novel capability comes not from object-mapping cleverness but from pointing three assets this architecture already has at persistence — and the headline one is the same reactivity keystone used for UI, applied a third time.

**The shippable, credible layer (uses what already exists):**
- **Typed query builder.** Generics-via-shapes and a real type system make queries fully typed: `User.where(...)` returns `Query<User>`, results are `List<User>`, no stringly-typed columns. Necessary and modern (Diesel/Prisma do this), not yet novel — but the substrate the novel parts need.
- **Literal hydration eliminates partial-load bugs.** A row hydrates by filling the all-fields literal (§4.1), whose checked full-initialization choke point makes a partially-hydrated object impossible — a real bug class in lazy-loading ORMs.
- **Free, precise dirty-tracking via structural update (§4.2).** A change is `user { email: new_email, ..user }`, which *names exactly the changed fields*. The change set is structurally explicit, so efficient UPDATEs need no snapshot-comparison dirty-tracking — the thing ORMs reverse-engineer, the update syntax states outright.

**The differentiating bet (the third use of signals, §9.4):**
- **Reactive models and queries.** Every ORM struggles with *staleness*: an object loaded into memory silently diverges from the database, patched over with manual `refresh()`, TTL caches, or hand-wired events. With signals, a row loaded as a signal-backed object and a query expressed as a `computed` can stay *live* — the same minimal-diff mechanism that drives the LiveView (§9.4) drives model invalidation. Fused with the bundled WS server (§9.5), this is end-to-end reactivity from *database change* to UI update, with no manual cache-invalidation layer. No mainstream backend ORM does this because no mainstream backend language has signals; it falls out of applying the reactivity keystone to persistence.

**Open problems (why this is R&D, not a settled feature):**
- **Reactivity scope:** how far does liveness extend — a single row, a join, an aggregate? Unbounded reactivity is intractable; the boundary must be chosen deliberately.
- **Change-storm control:** a high-churn table must not re-run every dependent computed on every write; this needs batching/coalescing and probably explicit subscription granularity.
- **Consistency model:** what is true when the in-memory signal graph and the database disagree (concurrent writers, transactions, rollback)? This is the hard core and must be specified before the reactive layer is more than a demo.

**Staging:** ship the typed builder + literal hydration + structural-update dirty-tracking first (solid, uses existing machinery). Layer the reactive persistence on top only once signals are proven in the UI path — same discipline as elsewhere: ship the credible version, then the unforgettable one.

### 9.13 Reflection and attributes
The design principle here is the same one running through the language: *do at compile time, typed, what dynamic languages like PHP do at runtime, untyped.* Concretely, this is achieved **without a comptime/macro system** — a GC language aimed at application developers should not grow a second execution model at compile time (Zig-style `comptime`, C++/Rust-macro complexity, and the sharp error messages those produce). The capabilities below deliver the discoverability and codegen wins while keeping compile-time metaprogramming out of the language surface.

**Reflection — two tiers, no comptime:**
- **Built-in derives (compile-time, implemented inside the compiler).** The shape-based codegen cases — `ToJson`, `Equatable`, `Comparable`, `Clone`, ORM-hydrate, etc. — are standard derives shipped by the language/stdlib and implemented as ordinary compiler code in Rust, where they are well-tested and produce good diagnostics. Users *apply* `#[derive(ToJson)]` but cannot *write new* derives. This covers the overwhelming majority of real "reflection for codegen" needs at zero runtime cost and with no user-facing metaprogramming. User-defined derives are **deliberately out of scope**, deferred unless they prove necessary; if ever added, they would be a narrow, constrained mechanism (typed `TypeInfo` in, code through a restricted builder out), never open-ended comptime.
- **Opt-in runtime reflection (for the genuinely dynamic minority).** Plugin systems loading types by name, generic debuggers, `new` from config. Redesigned to be cleaner than PHP's: **unified with the real type system** (`type_of(value)` returns the *same* `Type` the checker uses, not a parallel `ReflectionType` hierarchy), **introspection separated from invocation** (read-only `Type` introspection vs. explicitly **fallible** invocation — `type.construct(args)` and `method.call(obj, args)` return `Result<_, _>` because calling-by-name can fail on arity/types), **pattern-matchable** (`match type_of(value) { Type.Record(r) => ..., Type.Enum(e) => ... }` instead of an `instanceof ReflectionClass` ladder), and **capability-gated** (a type is reflectable only if marked or reflected-upon, so unused metadata is eliminated by dead-code analysis — this reconciles runtime reflection with small AOT binaries; reflectable types become tree-shaking roots, §9.8.1). Critically, runtime reflection is **closed-world**: it only ever touches the finite set of types that already exist in the program, so it does *not* require the compiler at runtime and does *not* block tree-shaking — it stays in the default static build tier. It is categorically distinct from `eval` (which creates new code from strings, opening the world); see §9.8.1. The "dynamic minority" here means dynamic *dispatch over known types*, not dynamic *code creation*.

**Attributes are just records used in annotation position — no special construct.** This is a deliberate consistency choice: rather than invent a bespoke `attribute` declaration (a new language concept, and one *less* capable than the types the language already has), an attribute is an ordinary **record** (§4.1) constructed in annotation position. `#[Route("/users")]` constructs `Route { path: "/users" }` through the exact same constructor-as-ordinary-function machinery (§4.1) as any other value — all-fields-literal checking applies, and a validating constructor returning `Result` even gives fallible attributes, which PHP cannot express. This reuses machinery that already exists instead of adding a parallel one, consistent with the language's "express things in the real language, not a mini-language" discipline (the same reason operators are traits §9.2 and lint rules are functions §9.17).

The attribute's *capabilities are traits* — the "everything is a trait" model (§9.2) applied here too:
- A record is usable in annotation position iff it implements the marker trait **`Attribute`** (the equivalent of PHP's `#[Attribute]` marker — and fittingly, itself a trait).
- An attribute that constrains *where* it may be attached implements **`AttachableTo`** (or a validation trait), checked at compile time — replacing the old bespoke `valid_on` predicate with an ordinary trait impl. E.g. a `Route` that may only annotate methods returning `Response` expresses that as a trait the checker enforces.
- An attribute that wants *behavior* (normalize its path, compute a derived value) just has methods, like any record.

```
#[derive(Attribute)]
record Route { path: string }          // a plain record, marked usable as an attribute

impl AttachableTo for Route {           // optional: constrain where it attaches
    fn valid_target(t: Target): bool { return t.is_method() && t.returns(Response); }
}

class UserController {
    #[Route("/users")]                  // constructs Route { path: "/users" }; checked
    fn index(): Response { ... }
}
```

**Discovery and registration via a compiler manifest — not reflection, not comptime.** The compiler *already parses every attribute on every declaration* during the front end; that index exists the moment parsing finishes. Rather than run user processors at compile time (that would be comptime, deliberately excluded), the compiler **keeps and exposes the index it already built** as a first-class build artifact (the "manifest"). An attribute is a *value*, discoverable via the manifest, with compile-time-checkable constraints expressed as traits — but it does **not** run imperative processing code at compile time. Consumption stays ordinary:
- **Registration is automatic and zero-cost.** Consumers query the manifest — `attributes_of::<Route>()` is a lookup into the compiler-produced index, compiled in as a static table. Routing tables, DI wiring, and entity maps are built by the compiler keeping what it already knew, with no runtime scan and no boilerplate "register your providers" calls. This turns Symfony/Laravel-style attribute scanning and container compilation into *the compiler's job, done once.*
- **Discoverability is free.** The same manifest powers the LSP and static analysis (§9.17): "show every `#[Route]`," "who consumes `#[Entity]`," jump-to-all-usages — because the index is a build artifact, not something reconstructed by runtime reflection.

This is better than both PHP (attributes-as-classes, but no manifest — relies on runtime reflection to find them) and a bespoke attribute construct (a needless new concept): attributes reduce to **records + traits + the manifest**, all of which exist for other reasons, with no comptime processing.

This keeps the language surface small: built-in derives + a typed/fallible/capability-gated runtime-reflection fallback + attributes-as-records (discovered via the compiler-built manifest, capabilities via traits). No comptime, no user macros, no bespoke attribute construct — the same discoverability and codegen benefits, none of the second-execution-model complexity. (The same manifest and query model also power rule-based static analysis; see §9.17.)

### 9.14 Hot module replacement (dev experience)
Change backend code and the running program picks up the change **without restarting and without losing state** — and, fused with reactivity, the change propagates to the live UI. This is Vite-grade DX for a *backend* language, which essentially does not exist today, and it is unusually achievable here because four prerequisites are already in place:

- **Persistent process (the keystone).** State lives in memory across requests, so there is state *to preserve* across a reload — the entire point of HMR. (A die-per-request model like PHP's has nothing to hot-replace.)
- **Isolates (§7) make reload safe.** Spin up a new isolate with the new code, route new work to it, let the old isolate drain in-flight work, retire it — a clean handoff with no shared mutable state to corrupt mid-swap. The isolate boundary is the granularity at which code can be swapped safely.
- **Bytecode VM + module structure.** Recompile the changed module incrementally (the salsa graph, §6), swap its bytecode into the running VM, invalidate the affected inline caches; shape transitions handle changed class layouts without corrupting existing instances.
- **Signals (the multiplier).** The reactivity graph already tracks what depends on what, so when code behind a `computed` changes, the affected computeds re-run and the minimal diff pushes to the UI over the existing WS connection — hot reload from *code* change to live UI update, through the same machinery that drives normal reactivity.

**Discipline (the honest hard parts):**
- **State migration is schema evolution, not merge.** When a record's *shape* changes (e.g. a new field), existing instances predate it. The default is to **detect shape-incompatible changes** (via the same dependency graph that powers incremental compilation) and either apply an explicit default/migration function or **fall back to a clean isolate restart** when migration is ambiguous — never silently guess. Isolate restarts are cheap, so "restart on ambiguity" is the correct default for a dev feature where correctness beats byte-perfect preservation. (CRDTs do *not* help here — see §9.15; this is one-object schema evolution, not multi-replica conflict reconciliation.)
- **Classify the blast radius.** Some changes hot-swap safely (a function body); some do not (a type's layout, a trait's contract). The salsa dependency graph computes the reach; safe changes swap, unsafe ones trigger a scoped isolate restart.
- **Dev-only, production sealed.** HMR is a mode of the dev server (the bundled server in dev mode). The AOT/production binary is sealed and carries none of the hot-swap machinery, so HMR never weakens the single-binary production story.

### 9.15 Collaborative / local-first / p2p state (R&D direction)
*A flagged R&D direction, not a finalized feature — and explicitly **not** the HMR mechanism (§9.14).*

CRDTs (conflict-free replicated data types) solve *concurrent conflicting writes from multiple replicas converging without coordination* — a **merge** problem, distinct from HMR's **schema-evolution** problem. They are the wrong tool for HMR migration and must not be made load-bearing for the whole runtime (per-object CRDT metadata would impose large overhead on every object to serve cases that do not need it). But they are a strong candidate for a *future* capability that composes with primitives already in the design:

- **Reactive CRDTs.** A signal whose value is a CRDT lets multiple users edit the same reactive state concurrently and converge conflict-free, with the reactivity graph propagating the converged result — collaborative editing, shared dashboards, multiplayer state, built on signals (§9.4).
- **Offline/edge sync via the isomorphic tier.** Extending the shared client+server logic (§9.7) to shared *state*: the client mutates a local CRDT replica offline, the server merges on reconnect with no lost writes — the Linear/Figma-style local-first architecture, for which the existing shared-logic design is well-positioned.

Open questions (why this is R&D): which CRDT types to offer, how to bound metadata overhead, and which state is collaborative vs. plain (CRDTs are opt-in per value, never universal). Same staging discipline as reactive persistence (§9.12): a differentiating bet layered on once the base primitives are proven.

**The networking layer: native p2p / local-first tooling.** CRDTs handle data *convergence*; they do not handle peers finding each other and moving bytes — which the local-first community has found to be the genuinely harder part (NAT traversal, discovery, transport, identity, encryption). A future capability would bring this in-language so that building a local-first app is a first-class path rather than a from-scratch distributed-systems project. **[p2panda](https://p2panda.org)** (Modal Collective) is the strong candidate for the supporting layer: a set of modular Rust crates providing peer discovery (mDNS on the local network, rendezvous for remote), p2p transport with NAT traversal and relays (QUIC, built on iroh), gossip pub/sub, append-log sync, large-blob transfer, Ed25519 identity, and group encryption. It is deliberately **data-type-agnostic and CRDT-agnostic** (its crates operate over raw bytes and compose with any CRDT), and **transport-independent** (the same abstraction runs over IP or genuinely broadcast media like LoRa, BLE, or packet radio). This is exactly the right division of labor:

- **p2panda** = the networking/identity/sync/encryption layer (the hard, not-core-competency part).
- **CRDTs** (above) = the data-convergence layer.
- **This language** = the integration: signals react to synced state (a peer's change propagates through the reactivity graph to the UI like any other signal update), the **persistent runtime** hosts the embedded p2p node, and the **Tauri desktop shell** (§9.9) packages it — a combination p2panda already demonstrates with a Tauri example. The result would be local-first, offline-capable, peer-to-peer apps deployable as a single binary, built with the same reactive model as any other app.

Caveats and honesty: p2panda is pre-1.0 with APIs not yet stable, so this is a watch-and-integrate direction, not a near-term dependency. As with all of §9.15, p2p/local-first is opt-in per application — the language does not impose a networking model; it offers this as a supported path for the applications that want it. This composes cleanly with the rest of the design (signals, persistence, Tauri, the isomorphic tier) rather than requiring new runtime machinery, which is what makes it a natural extension rather than a separate product.

#### 9.15.1 What it would look like in practice (brainstorm)
Design intent: **p2p should feel like signals that happen to be shared, not a networking library bolted on.** The reactivity graph does not care whether a change originated locally or from a peer, so every `computed`/`effect` already written keeps working when its inputs become synced.

- **The primitive is a synced signal.** `synced_signal(initial, topic: "room:42/counter")` has the same `.get()`/`.set()`/`.update()` surface as a local `signal`, but changes propagate to peers on the same topic and theirs flow back. A live collaborative counter requires zero networking code — the existing reactive machinery delivers "peer's edit shows up in my UI."
- **Convergence is a trait.** A synced value's type must declare how concurrent edits merge — a `Mergeable` trait backed by a CRDT (Automerge/Loro/p2panda's own types) under the hood. This reuses the "behavior is a trait" philosophy and makes sync-safety a *compile-time* fact: the compiler knows whether a type is `Mergeable`, so you cannot accidentally sync a type with no convergence story (make-illegal-states-unrepresentable applied to sync). Non-`Mergeable` types either cannot be synced or get explicit last-write-wins.
- **Identity/trust is an explicit but light surface.** `identity()` is the node's persisted Ed25519 keypair; a topic can declare `.members([...]).encrypted()`. The default should be safe (encrypted, explicit membership) with an open public topic available for prototyping, the safe form only a couple more tokens than the loose one.
- **The node lives in the persistent runtime.** The embedded p2p node (discovery, connections, append-log store) starts with the process and runs on its own isolate without blocking request handling — a fit a die-per-request model structurally cannot offer. Node lifecycle is configured (storage path, discovery methods) rather than imperatively constructed; it is mostly invisible.
- **The network boundary stays visible, deliberately.** `SyncedSignal<T>` is a distinct type from `Signal<T>` so it is legible which `.set()`s cross the network (latency, encryption, partial failure) — p2p should be *easy, not invisible*, the same way `Result` keeps failure legible. Invisible network boundaries are how apps mysteriously hang.
- **Partial failure is surfaced, not hidden.** A synced value carries sync status alongside its value (`Synced | Syncing | Offline(since)`) that an `effect` can render ("working offline, 3 peers unreachable"). CRDTs mean data is never *lost*, but the app sometimes needs to know whether it is synced or working alone — the local-first equivalent of not pretending errors away.

Open design questions worth chewing on later:
- **Granularity — per-signal vs. per-store.** Per-signal (`synced_signal`) is simplest to demo; the richer target is a **synced store** — a whole reactive dataset that syncs, with signals as views into it. That store-level form is where this idea and reactive persistence (§9.12) *merge into one thing*: a reactive local-first database whose backend is p2p sync instead of (or alongside) SQL. Start per-signal, aim at the store.
- **History/offline/time-travel.** p2panda's append-log substrate gives history and offline-then-sync nearly for free; whether to surface `.history()` / time-travel / "what changed while offline" is a later choice, but the capability is latent in the storage model.

#### 9.15.2 Packaging: first-party extension *and* tree-shaken, not either/or
p2p support must not bloat production builds of apps that do not use it. The clean answer combines both mechanisms already in the design, rather than choosing between them:

- **It ships as a first-party native extension** (§10) — a native-FFI module wrapping the p2panda crates, maintained alongside the runtime and versioning with it (so it links the internal representation directly, with no host-ABI cost, per §10.3's first-party path). "First-party" here means *officially maintained and trusted*, not *bundled into every binary*.
- **It is pulled only when depended upon.** Like any package, the p2p extension enters a build only if the app's manifest declares it (`lang add p2p` or equivalent). An app that never imports it never links p2panda — its transitive native dependencies (iroh, QUIC stack, encryption) are simply absent from the binary. This is dependency-level exclusion, which is stronger and simpler than post-hoc tree-shaking: the code never enters the graph rather than being pruned from it.
- **Tree-shaking handles the within-dependency granularity.** For an app that *does* use p2p but only a slice of it (say, local sync without blob transfer or without rendezvous discovery), the AOT compiler's dead-code elimination (§9.8.1 — the same reachability pass that drops unused stdlib and unreached functions, and that gates runtime-reflection metadata) prunes the unused portions of the extension and the runtime glue. Capability-gating reinforces this: a node never granted blob/relay capabilities can have that machinery eliminated.

So: **first-party for trust and maintenance, dependency-gated for whole-feature exclusion, tree-shaken for within-feature granularity.** A CLI tool or a non-collaborative web app pays nothing; a local-first app pays only for the p2p surface it actually uses. This is the general shape for *any* heavy optional capability (p2p, and by the same logic the reactive-persistence and WASM tiers), not a one-off for networking.

### 9.16 Game development (application note)
This is an application area the design fits well *with calibration* — the fit depends sharply on the game type and on the GC.

**The boundary is the frame budget.** 60fps means a hard 16.6ms per frame, and the killer is *consistency*: a GC pause invisible in a web app is a visible stutter in a game. The refcount-floor GC (§5) is actually *better* here than tracing — it reclaims incrementally as references drop rather than in stop-the-world pauses, avoiding the periodic-spike stutter that plagues tracing-GC game runtimes — but any GC language still asks for care about allocation pressure, and zero-allocation hot loops (AAA, high-fidelity 3D, competitive FPS, large physics sims) are the domain of systems languages (C++/Rust), not a GC language. So:
- **Excellent for** 2D games, turn-based and strategy games, simulation/management, narrative and systems-heavy games, and tooling — anything with generous frame headroom, where logic complexity and iteration speed matter more than raw throughput. ADTs, exhaustive `match`, `Result`, and strong types are exactly right for game state machines and rules; this is a genuine sweet spot and an underserved one (most options are either too low-level or dynamically-loose like Lua/GDScript).
- **The wrong tool for** frame-budget-limited, zero-alloc 3D — there a systems language owns the work.

**The right framing is scripting layer over a native engine** (the GDScript-for-Godot / Lua model), not standalone game language. The engine (Godot via gdext, Bevy, or a custom `wgpu` + `rapier` stack) owns the frame-budget-critical hot loop in Rust; this language owns *game logic*, where GC is a non-issue. The native FFI (§10) is exactly the binding mechanism, and the persistent runtime hosts the game while the engine is a native extension. In this role the language is arguably a *better* scripting layer than the incumbents, because it adds what they lack:
- **HMR (§9.14)** — tweak a value or behavior and see it live in the running game without restart or state loss; the single most-loved game-dev iteration feature, native here.
- **Reactivity / signals (§9.4)** — map naturally to game UI (health bars, inventories, menus) and state-driven systems.
- **Type safety** — ADTs/`match`/`Result` over GDScript's dynamic typing and Lua's looseness.
- **Packed value types and flat typed arrays (§3.1)** — the layout foundation for the 3D math (`Vec3`/`Quat`/`Mat4` as packed value types with operator-trait surfaces, SIMD via native kernels) and for ECS-style flat component storage, so the *logic*-side numerics are not pathologically slow.

So the honest positioning: not "ideal for games" flatly, but **an excellent typed scripting layer for engine-hosted games, and a delightful standalone language for the large 2D/turn-based/sim/narrative category** — which connects to adjacent interests (procedural generation, board-game logic) that sit squarely in the "GC is fine, logic and types matter" zone.

### 9.17 Static analysis (rule-based, sharing the compiler's model)
Static analysis is unusually cheap here because the infrastructure a rule-based analyzer needs is **already built for other reasons** — it is the fifth capability that falls out of the `salsa` query graph (alongside incremental compilation §6, the LSP §9.6, HMR §9.14, and reflection's manifest §9.13). The substrate:
- A **lossless CST** (`rowan`, already used by the LSP) for exact source spans — what autofix and span-precise lints need.
- A **resolved, typed semantic model** (the salsa graph: name resolution, types, the call/use graph, trait impls) — the difference between syntactic linting and real semantic analysis ("this value could be `none` here," "this `Result` is never handled").
- **Incremental recomputation** (salsa) — lint-on-save, instantly, re-running only over what changed.
- The **attribute manifest** (§9.13) — rules key off annotations cheaply ("every `#[Route]` handler must return `Response`").

So an analyzer is "expose read-only queries over the salsa model + CST and let rules run against them," not a parallel analysis infrastructure. This eliminates the failure mode that plagues most ecosystems — PHPStan/Psalm/mypy/ESLint each *re-implement* a type model that perpetually drifts from the real compiler ("the linter disagrees with the compiler"). Here analysis rules query the **same** model the compiler and LSP use, so a rule can never disagree with the compiler about a type or about reachability. This is the Rust `clippy` advantage (built on the actual compiler, not a reimplementation) generalized and made pluggable — and a real differentiator for a language that sells type-driven correctness.

**Layered, by trust and by whether the analysis is opinion or guarantee:**
- **Core correctness analyses → in the compiler, always on.** Exhaustiveness (§match), unhandled-`Result`, unreachable/dead code (§9.8.1), `mut`-violation, type errors. These are language *guarantees*, not lints — not extensions, not disableable.
- **First-party lint set → maintained and blessed by the project** (the `clippy`-default-lints equivalent), compiled in or shipped as a blessed module.
- **Third-party / project rules → WASM-sandboxed analysis extensions.** A lint rule is *pure* — it takes a semantic model, returns diagnostics, touches no I/O, no native libraries. That is the ideal case for the WASM-sandboxed extension path (§10): a rule from a registry runs sandboxed with a read-only view of the analysis model and emits diagnostics, with no ability to crash the compiler or touch the filesystem. The trust-boundary model (§10.1) answers the "what kind of extension is a rule" question cleanly — **analysis rules are WASM extensions by default**, because they are exactly the pure-and-untrusted profile WASM sandboxing is for.

**Rule-authoring surface.** A rule is ordinary language code: a function that queries the read-only model and yields diagnostics, reusing the same `ariadne`-quality span reporting the compiler uses, the attribute manifest, and the call graph. No separate analyzer DSL, no reconstructing the type model:

```
#[lint(name: "no_panic_in_handlers", level: warn)]
fn no_panic_in_handlers(ctx: AnalysisContext): List<Diagnostic> {
    mut diags = [];
    for handler in ctx.items_with::<Route>() {        // attribute manifest, §9.13
        for call in handler.calls() {                  // call graph, salsa
            if call.target.name == "panic" {
                diags.push(Diagnostic.warn(call.span,
                    "panic in a request handler; return an error Response instead"));
            }
        }
    }
    return diags;
}
```

**`lang lint`** is another face of the toolchain binary (§11.2): core analyses + first-party lints + project-pulled rules, run incrementally, with LSP integration (lint-on-type) for free because it is the same query graph. The analysis query API over salsa should be a deliberate, stable *public* surface — it is what both third-party rules and the LSP consume.

---

## 10. Extending the language

Extending the language should be **as routine as adding a dependency** — no specialist knowledge, no system-wide installs, no per-version ABI dance (the PHP/PECL pain). The mechanism is chosen by an observable signal — **the trust boundary** — not by asking the author to pick a tier. The question is simply: *is this code mine/local, or pulled from a registry I don't control?*

### 10.1 Default keyed to the trust boundary
- **Distributed / third-party / untrusted code (registry path) → WASM-sandboxed by default.** Anything pulled via the package manager that the consumer did not write runs as a **WASM component** (WIT / component-model interface): memory-isolated (a broken or malicious extension cannot corrupt the host or crash the runtime), capability-gated (no OS/filesystem/network access except what is explicitly granted), and **language-agnostic** (anyone targeting WASM — Rust, Zig, C, Go, eventually this language — can author one). This is the responsible default for running other people's code at ecosystem scale, and it reuses the existing WASM toolchain (§9.7). It also makes registry extensions **portable across every surface**, including the browser-WASM target, which native extensions cannot reach.
- **Local / first-party / trusted code, and binding external native libraries → native FFI against a stable host ABI.** "I need to call this C library / system API / fast Rust routine in my own project" is native FFI — because that is where the performance lives and where WASM is actively the wrong tool (WASM is sandboxed *away* from native libraries; binding them is precisely what it cannot do well). Trusted because it is the consumer's own or vendored code.

This ties the default to *why someone is extending* rather than a performance ranking: registry code is a **trust** problem (sandbox it), local native-library binding is a **capability/speed** problem (FFI it).

### 10.2 The stable host ABI (the one irreversible piece)
Native modules must **never link against the runtime's internal representation** (NaN-boxing layout, shapes, the GC/refcount contract). Doing so would make the internal representation a public ABI that can never change — reproducing the Zend-ABI-breaks-every-version pain in Rust. Instead, native extensions link against a **small, versioned `extern "C"` host ABI** (`host.make_string`, `host.get_field`, `host.register_function`, `host.throw`, ...). Internals can then evolve freely as long as the host ABI is preserved. This ABI is the single effectively-irreversible decision in the extension story and must be **designed early and kept deliberately narrow**, because once third-party extensions depend on it, it cannot move. (WASM extensions get this stability for free — the WIT interface is a typed, versioned contract by construction, with no way to leak internal representation across it.)

### 10.3 Authoring and distribution
- **FFI bindings (the common "bind external X" case)** are generated by the toolchain from an interface declaration — the 80% case, made trivial (declare the foreign interface, the tool emits the marshaling glue).
- **Native Rust modules** are an ordinary Rust crate depending on the published **host-ABI shim crate** (not the runtime internals), dropped into the project or pulled as a package; the toolchain compiles and links it. This is the "drop a Rust project in your directory" experience — simple to author *and* forward-compatible, because it is against the stable ABI.
- **First-party / stdlib native pieces** compile *with* the runtime against internal types directly (no ABI cost), because they version together — the one place direct internal linking is correct. Note that "first-party" means *officially maintained and trusted*, not *bundled into every binary*: a first-party extension (e.g. p2p, §9.15.2) still enters a build only when the app depends on it, and unused portions are tree-shaken by AOT dead-code elimination. The default cost of an optional capability the app does not use is zero.
- **Package-manager integration is what actually makes it dead-simple.** Extensions flow through the native package manager like any other dependency (`lang add image-tools`): for registry packages the toolchain builds/loads them as sandboxed WASM by default; for local native bindings it compiles against the host ABI. No out-of-band PECL-style installs, no version-matching, no specialist knowledge.

---

## 11. Completeness: standard library, packaging, testing, and scope

The sections above cover what makes the language *differentiated*. A general-purpose language is also judged on what makes it *complete* — the everyday surface every working programmer touches. Developers adopt on differentiation but stay on completeness; this section states the baseline so the docs do not read as if the language cannot do ordinary things.

### 11.1 Standard library (layered: rich core, lean middle, first-party modules)
The stdlib is **largely a binding layer over mature Rust crates** (plan), exposed through the language's own types and the trait/operator system. Its philosophy is **layered**, not one big batteries-included blob: a *rich* core for the types every line of code touches, a deliberately *thin* always-shipped middle, and *first-party native-extension modules* (Rust/Go-style) for everything else. This keeps the common case ergonomic while keeping binaries — and mobile/CLI builds — small.

**Ring 1 — language core (always present, richly supported).** The types entangled with the language's own syntax and operator traits, which every program touches: `List`, `Map`, `Set`, ordered/sorted maps and sets, deque/queue, the packed/flat numeric arrays (§3.1), full Unicode-correct **string/text**, the numeric primitives, and `Option`/`Result`. These get a generous method surface (map/filter/fold, slicing, iteration, full string ops) — Python-generous, because they are effectively part of the language. This is the "arrays, strings and such have a solid stdlib" tier.

**Ring 2 — always-shipped std modules (thin, Go-lean).** A small set so universal that requiring an explicit add would feel broken, in the box by default: basic **file/IO and filesystem** (files, paths, streams; async-first §7.1 where it makes sense), **process / environment / args**, **basic math** (scalar), basic **random** (a general-purpose PRNG; cryptographic randomness lives in Ring 3 with crypto), **basic time** (now / sleep / measure duration / monotonic clocks — *no* timezone/calendar machinery), and **JSON** (conceded to Ring 2 rather than Ring 3 because the web positioning makes it a near-universal need). Kept deliberately thin.

**Ring 3 — first-party native-extension modules (official, opt-in via `lang add`, tree-shaken).** Everything else, even though first-party-maintained: **regex**, **timezone-aware date/time and calendar math** (binding `chrono`/`time`), **crypto** (`ring`/`rustls`), **HTTP client**, **YAML/TOML/CSV** and other serialization formats, **hashing**, **compression**, **base64/hex encoding**, the **3D/SIMD math** (§3.1), and a derive-driven `Serialize`/`Deserialize` (§9.13) surface for the format modules. These are *first-party* (trusted, maintained, versioned with releases) but **not bundled** — pulled when used, built via the extension mechanism (§10), tree-shaken (§9.8.1), zero cost when unused.

The key economy: **Ring 3 reuses the extension mechanism rather than being a separate "stdlib loading" path.** A first-party module like `regex` is distributed, built (cargo backend, §11.2), dependency-gated, and tree-shaken *identically* to a third-party package — it is simply maintained and blessed by the project. So one packaging/cost model covers core modules and ecosystem packages uniformly.

Two consequences worth noting: (1) the batteries-vs-ecosystem line is "Ring 1+2 in the box, Ring 3 and beyond opt-in," and because the entire Rust ecosystem is reachable as native extensions (§11.2), **stdlib breadth is a curation decision, not a capability limit** — the lean middle is a choice, not a weakness. (2) The layered model *is* the small-binary story applied to the stdlib itself: a minimal CLI tool or mobile build pulls in almost nothing, while a full web app opts into exactly what it uses.

### 11.2 Packaging and dependencies — three distinct layers
A common confusion to head off: **there are two dependency ecosystems, and they are not the same.**
- **The language's own package manager + registry** handles **user-library dependencies** — libraries written *in this language*. `lang add <pkg>` resolves from this registry. This is *not* cargo and *not* crates.io; a package here is a library in this language, in a clean namespace.
- **Cargo / crates.io** is used in two distinct *implementer/extension* roles, never as the user-facing package manager:
  1. Building the **runtime/compiler itself** (its own Rust dependencies — `logos`, `salsa`, `tokio`, etc.).
  2. As the **build backend for native extensions** (§10): a native extension *is* a Rust crate, so when the language's package manager builds one, it invokes cargo underneath to compile that crate and its crates.io dependencies. The user runs `lang add p2p`; the toolchain orchestrates the cargo build beneath, invisibly.

The payoff of this separation: **the whole Rust ecosystem is reachable as native extensions** (you never reimplement a database driver or codec — wrap the crate), *while* the language's own libraries live in a clean registry that is not "is this a Rust crate or a real library." Three layers — user libraries (own registry), native extensions (own front, cargo backend), the compiler (cargo directly) — each with a clear home.

### 11.3 Testing (for users, reusing the conformance harness)
Testing user code is first-class, and its infrastructure is **the same runner that powers the language's own conformance suite** (plan §6) — built once, exposed inward (conformance) and outward (user tests). The same pattern as the salsa graph serving compiler + LSP + HMR: one capability, multiple consumers.
- A native test surface (`#[test]`-style functions, or a `test "name" { ... }` block) with assertions that produce good diffs (reusing the snapshot/`insta`-style machinery).
- `lang test` discovers and runs a project's tests, in parallel, with machine-readable output (the same JSON mode used for agentic development).
- Because the runtime is persistent and isolates are cheap, tests run isolated and fast, and async tests (§7.1) are first-class.

### 11.4 Baseline data access (under the reactive-ORM bet)
The reactive ORM (§9.12) is a flagged R&D *bet*. The **ordinary, shippable baseline** is table stakes and stated here: a typed query interface and straightforward driver-backed access to common databases (Postgres first, via `sqlx`-style binding), with connection pooling living across requests thanks to the persistent runtime (§7.1). "Query Postgres today, typed, with `?`-propagated errors" must work *without* opting into anything reactive. The reactive layer is the differentiator layered on top of this baseline, not a replacement for it.

### 11.5 Scope: in, later, and out
Stated explicitly so silence does not create ambiguity:
- **In (1.0 targets):** CLI tools, web services/apps (server + reactive), desktop apps (Tauri), game-logic scripting over a native engine (§9.16), data/numeric processing (enabled by §3.1).
- **Later (not 1.0, but the design must not preclude it):** **mobile.** Tauri has a mobile story, and the runtime/reactivity/single-binary model is compatible with mobile in principle. Mobile is explicitly *not* a 1.0 target, but architectural decisions must **not build the project into a corner that forecloses it** — e.g. the deployment, UI, and runtime-hosting models should stay mobile-reachable even though mobile is deferred.
- **Out (non-goals):** **embedded / bare-metal / no-std systems programming** — a GC, persistent-runtime language is structurally the wrong tool, and this is firmly out of scope (that is Rust/C/Zig territory, reachable *from* this language via native extensions if ever needed). Hard-real-time and zero-allocation frame-budget workloads (AAA game engines, §9.16) are likewise out as *primary* targets, served instead by binding a native engine.

---

## 12. Developer experience and agentic tooling

First-class DX is a primary goal, and the language has a structural advantage in one dimension most languages lack: it is being designed and built **agentically**, for a world where a large share of development is done by agents — *including agents that have no training data for this language*, since it is new. The design turns that apparent disadvantage into an advantage. The unifying idea: an **agent is just another consumer of the same models the compiler, LSP, and runtime already expose** (the salsa query graph §6, telemetry §9.6, the VM, isolates), reached through one interface. Agentic introspection is the sixth face of the salsa spine, alongside incremental compilation, the LSP, HMR, static analysis, and reflection.

### 12.1 Structured logging API (standard, with pluggable store drivers)
A **standard, structured logging API in the stdlib** — not text smeared to stdout. Log events are structured: level, message, typed fields, and span/trace correlation (sharing the observability correlation model, §9.6). This earns its place on general-purpose merits alone (it ends the per-framework logging fragmentation most ecosystems suffer), and it is what makes logs *queryable* rather than greppable. **Where** logs go is a pluggable **driver** (stdout, file, Loki, Datadog, an embedded store) — the same standard-interface/swappable-backend pattern used elsewhere. Because logs are structured and stored, `query_logs(level: error, since: ..., where: ...)` becomes a real, structured query — the foundation for safe production debugging (§12.4).

### 12.2 Debug engine (DAP for editors, MCP for agents)
One **debug engine** over the VM — breakpoints, stepping, stack-frame and variable inspection, **app-state capture at a breakpoint** — exposed through *two* protocols: the **Debug Adapter Protocol (DAP)** for human IDEs (VS Code et al.), and the **MCP server** (§12.4) for agents. Same engine, two surfaces, because humans debug in an editor and agents debug through MCP. The persistent-runtime + isolate architecture yields a genuinely better debug substrate: an agent or human can **pause and inspect a single isolate's live state** (an in-flight request's frames, a specific heap) without the stop-the-world disruption of a typical debugger — other isolates keep serving while one is inspected.

### 12.3 Profiling and flamegraphs (structured for agents)
Built-in profiling, extending the runtime's existing instrumentation (§9.6): sampling/instrumenting profilers that produce **flamegraph data as structured data** (a tree of `{function, self_time, total_time, children}`), with a rendered visual as a secondary output for humans. Tied to telemetry, this gives a complete, agent-drivable bottleneck-finding loop: slow endpoint → dominant span → hot function → flamegraph path → regression-versus-last-run. The principle throughout §12: **agents receive structured data, never scraped human text** — a flamegraph is a tree, diagnostics are typed objects with spans, test results are structured, not terminal output.

### 12.4 Built-in MCP server (the agent's universal surface)
A **built-in MCP server** is a first-class part of the toolchain — the single interface through which an agent reaches everything, mostly by *exposing models that already exist*:
- **Semantic tools** (salsa + LSP): type-of-symbol, find-usages, diagnostics, call graph, attribute resolution, which lints fire.
- **Runtime/debug tools** (the §12.2 engine): set breakpoint, inspect app state/frames/variables, step.
- **Profiling tools** (§12.3): profile an endpoint, find bottleneck, produce flamegraph data.
- **Project/build tools**: run tests (structured results), build errors, dependency graph.
- **Log tools** (§12.1): `query_logs` over structured logs.

**Security model — dev-only by default, production opt-in via explicit per-tool allowlist.** The MCP surface is a *development* capability. Production exposes the **empty set** unless a team explicitly enumerates, tool by tool, an allowlist (e.g. read-only `query_logs` enabled, `sample_table`/`replay_event` not). This is a capability grant (§9.8.1): non-allowlisted tools are **compiled out** of the production binary, not merely flag-disabled — compile-time absence, stronger than a runtime toggle.

**App/framework-registered custom tools — free-form content, standard typed mechanism.** This is the uniquely strong part: frameworks and the user's own app register their *own* introspection tools into the same MCP server the agent already uses — a web framework registers `list_routes`/`inspect_route`/`simulate_request`, an ORM registers `query_schema`/`explain_query`/`sample_table`, a game registers `inspect_entity`/`dump_scene_graph`, the app registers domain tools like `inspect_order(id)`. The language is **agnostic about what tools exist** (free-form — mandating web/DB-shaped tools would presuppose application domains and betray general-purpose neutrality), but the **registration mechanism is standard and typed**: a tool has a name, typed parameters, a typed return, and a description, so an agent gets a uniform, self-describing, schema-typed interface (`list_tools` returns schemas) regardless of which framework registered it — and cannot call a tool with wrong types, because the type system validates it. Free-form *content*, uniform *form*. The ecosystem can *converge* on common tool names (`list_routes`) by **convention, not mandate** — emergent standardization without a central authority. This turns agentic debugging from "read logs and guess" into "call the app's own framework-aware introspection tools," which no existing language ecosystem offers as a standard.

### 12.5 The no-training-data problem, and the compiler as syntax oracle
A brand-new language's biggest agentic risk: models **confidently hallucinate** syntax that does not exist, because they have no training data for it. The antidote is making ground truth **cheaply queryable and verifiable**, which the MCP server uniquely enables:
- `check_snippet(code)` — does this parse and typecheck against the *real* compiler? Returns structured diagnostics if not.
- `explain_syntax(construct)` / `show_example(feature)` — authoritative answers from the canonical spec, not recalled guesses.

So an agent unsure how `concurrent { }` works *asks* or *tests against the real compiler* before committing, converting "guess the syntax and hope" into **propose → verify → commit**. This is the single most valuable agentic affordance for a training-data-less language: the compiler itself becomes the agent's syntax oracle. Notably, this is a *better* DX than relying on training data even once such data exists — a live correctness oracle beats recalled patterns — so the constraint forces a superior design.

### 12.6 `lang init` scaffolding (orient and teach the agent)
`lang init` scaffolds a project that *teaches the language to an agent that has never seen it*:
- **`AGENTS.md`** — orientation: what the language is, the toolchain commands (`test`/`lint`/`fmt`/`build`), how to run things, that the MCP server is available and what it exposes, and project conventions. The "how to work here" file.
- **A toolchain-generated language primer** (the `SYNTAX.md` instinct) — a concise, dense, example-driven spec of syntax and core semantics written *for an LLM to consume*, **generated and version-matched by the toolchain** from the canonical spec (not hand-written per project, so it is always correct for the installed version). The static primer orients; the live MCP oracle (§12.5) verifies — together they replace absent training data.

### 12.7 Coherence of the DX story
Almost all of §12 is *exposure of models that already exist* (salsa, telemetry, the VM, isolates) through one interface (MCP), plus two pieces that earn their place on general-purpose merits regardless: the structured logging API (§12.1) and the debug engine (§12.2, needed for DAP anyway). So agentic DX is not a parallel subsystem — it is a new *consumer* of the existing spine, the same way the LSP, HMR, and static analysis are. Everything agent-facing is dev-only by default, capability-gated, production-allowlisted, and emits structured data rather than scraped text.

---

## 13. The coherence

The major decisions are not independent — they share one fault line: **how much the runtime knows statically versus preserves dynamically.** Shapes + editions + isolates + refcounting is a coherent point in that space: enough static knowledge to be fast and to add generics/ADTs/ownership analysis, enough preserved dynamism to keep proxies and runtime reflection working. The same fault line explains where metaprogramming landed: most reflection is resolved statically (built-in derives and the compiler-built attribute manifest, §9.13), and the rest — runtime reflection — is *closed-world* (dispatch over the finite set of types that already exist), so it stays in the default static build tier without forcing an embedded compiler or blocking tree-shaking. The single genuinely open-world feature, `eval` (new code from strings), is the only thing gated behind an explicit opt-out (§9.8.1); everything else, reflection included, is closed-world and statically analyzable. This is achieved deliberately *without* a comptime/macro execution model, keeping the split a clean two-way (closed-world default vs. opt-in open-world `eval`) rather than introducing a third, compile-time-but-user-programmable layer. Persistence is the keystone that the bundled server, reactivity, observability, pooling, and the absence of any shared-memory bytecode cache (PHP's opcache role) all rest on.
