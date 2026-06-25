# The `resource` kind — a first-class reference type for stateful resources

**Status: design backlog, NOT scheduled.** This is a sequencing/shape document, not a slice plan
and not gated work. It captures the design and the trigger conditions so the insight isn't lost; it
is picked up when one of the triggers below fires.

## Why this exists

Phase 5.2 settled that **class instances are value-semantic (COW)** — so after it, *every*
user-declarable kind carries data by value:

| Kind | Semantics |
|---|---|
| `record` | value, structural, immutable (+ packed potential) |
| `class` | value, nominal, `mut` fields + methods + `destruct` |
| `enum` | value, sum type |

There is **no user-declarable reference-semantic kind.** Yet the language already *has* reference
semantics — `FileHandle` (5.2b): a stateful external resource with identity, mutated in place through
its methods, shared by every alias. But that is a privileged **built-in**: a user who wants to write
their own stateful resource (a DB connection, a connection pool, a ring buffer, a mutex, a channel, an
RNG stream) cannot express its reference semantics today. Reference semantics is a hidden built-in
privilege rather than something the language lets you name and opt into.

A **`resource` kind** is the user-facing name for the decision 5.2b made about `FileHandle`. It
completes the taxonomy: value kinds (record/class/enum) carry data; **one reference kind (`resource`)
quarantines everything "spooky"** — shared mutation, identity, IO, non-sendability, must-close — and
makes it opt-in and legible, matching the language's "dangerous/powerful features are opt-in and
clearly marked" principle.

## What a `resource` is

A `resource` declaration means, all at once:

1. **Reference semantics.** Aliasing shares state; a mutation through one binding is visible through
   every alias (`b = a; a.advance()` is visible via `b`). This is the `FileHandle` behaviour, now
   nameable — the opposite of the value-semantic default, and *deliberately* so.
2. **Must-close / RAII.** A resource *requires* a `destruct` block (the release/close logic), reusing
   the existing deterministic, refcount-driven destruction (Phase 4) — close-at-last-drop falls out
   for free. The common "forgot to close" bug class is structurally addressed.
3. **`!Send` across isolates.** The architecture's isolate model (§126: "no shared mutable objects;
   communication by copies or immutable handles") *must* statically reject sending a stateful resource
   across an isolate boundary. `resource` is exactly that marker — the type-level distinction between
   sendable data and non-sendable resources.

`fs.open` would then *return* a `resource`; users could declare their own (`resource DbConn { … }`).
`FileHandle` is the **prototype** of the kind — it already establishes the semantics; the kind
generalizes and names it.

## Why a dedicated kind (not a trait, not linear types)

Three precedent camps:

- **No dedicated kind — reference type + destructor suffices.** Swift (`FileHandle` is a `class` +
  `deinit`; `Data`, the byte buffer, is a COW *value*), Rust (a struct owning an fd + `Drop` + move).
  Resource-safety falls out of the value/reference split + ownership.
- **A marker trait + scoped cleanup.** C#/Java `IDisposable`/`AutoCloseable` + `using`/
  try-with-resources; Clojure `with-open`. Lightweight, opt-in.
- **First-class linear/resource types.** Austral (linear, use-exactly-once), Haskell `LinearTypes`,
  Vale (generational references). Strongest static guarantees (no double-close, no use-after-close),
  largest type-system cost.

**Recommendation: a lightweight dedicated kind, camp-2-flavoured, built on machinery that already
exists** (traits + `destruct` + refcount destruction). A dedicated *kind* (rather than just a marker
trait on a class) is justified here because the three properties above are a *bundle* that also
changes representation (reference vs value) and isolate-sendability — more than a trait conventionally
carries, and exactly the value/reference fork Swift draws with `struct` vs `class`.

**Explicitly not (yet): full linear types.** Refcount + `destruct` already give deterministic
close-at-last-drop, covering the common case. Statically-enforced use-exactly-once / no-double-close /
no-use-after-close is a future hardening, naturally co-located with the effects/concurrency work — not
part of introducing the kind.

## Relationship to pointers / references (why the kind makes general references unnecessary)

A recurring question: *with value semantics + COW, should the language add pointers or references?*
Short answer: **no general-purpose references — `resource` is the controlled re-introduction of
reference semantics for the cases that legitimately need it, and COW + arena indices cover the rest.**
Unpacking what "references" are actually wanted *for*:

| Want | Value-semantic answer | Needs a pointer? |
|---|---|---|
| **Avoid copies (performance)** — pass a big value without copying | **COW / Perceus reuse** — passing is a refcount bump; a copy happens only on mutate-while-shared | **No.** COW makes "pass by `&` for speed" obsolete (cf. Swift/Roc/Clojure — you never write `&` for perf). |
| **Identity** — "is this the *same* instance?" | Data has none by design (structural equality). Genuine identity *is* a resource. | **No** — `resource` provides identity where it's legitimate. |
| **Shared mutable state** — two names, one mutable thing, mutation visible through both | Deliberately eliminated (the language's thesis). Where genuinely needed (a shared connection/counter) → a `resource`. | **No** — `resource` *is* this, named and quarantined. |
| **In-place mutation through a function** ("out-param") | Return the new value: `x = update(x)` — O(1) under COW. Optional later: `inout`-style parameter *sugar* (value-semantic copy-in/out, like Swift `inout`), **not** a pointer. | **No.** |
| **Cyclic / graph structures** (linked lists, parent pointers, graphs) | Arena + integer indices into a `List` (data-oriented, cache-friendly) for value graphs; a `resource` for genuinely shared-mutable nodes; the Phase-6 cycle collector keeps accidental cycles from leaking. | **No** — indices or resources. |

The throughline: **general pointers/references would reintroduce exactly what the language
eliminates** — aliasing-induced spooky action, the loss of value semantics, and the need for a borrow
checker to make it safe. That is a regression against the language's identity. The two motivations
that survive scrutiny are (a) *performance*, already solved by COW, and (b) *genuine shared-mutable
state / identity*, which is precisely what `resource` is for. The one thing `&mut`/pointers give that
`resource` does not — a temporary, scoped, non-owning *borrow* into part of a value — a COW/value
language does not need, because mutation is value-return, not in-place-through-a-borrow.

So `resource` does not just *relate* to the pointer question — it is the answer to it. Pointers stay
an internal compiler/runtime concept (the reuse/Perceus machinery already passes them around,
invisibly); the surface language exposes value kinds + the one `resource` reference kind.

## Sequencing — the trigger conditions

Build it when one of these fires (until then `FileHandle` is the sole, built-in instance):

1. **A second built-in resource lands** (socket, DB connection, timer, channel, mutex). Then
   "make it a reference type" stops being a one-off, and the duplication argues for the real kind.
2. **The isolate / concurrency milestone begins.** This is the strongest trigger: isolates *force* a
   static sendable-data vs non-sendable-resource distinction, and `resource` is its natural home — you
   cannot ship the isolate model without something that plays this role.

Neither is on the Phase-5/6 critical path, so this stays a backlog design until then.

## Open design questions (settle when picked up)

- **Surface syntax.** `resource DbConn { … }` as a new declaration keyword, vs `@resource class`
  reusing the class grammar, vs a built-in `Resource` marker trait that flips representation. Lean: a
  distinct `resource` keyword — the value/reference fork deserves to be as visible as Swift's
  struct/class.
- **Must-close enforcement strength.** Require a `destruct` block (compile error without one)?
  Warn on a resource that is provably never closed? Full linear "must be consumed" is the deferred
  hardening.
- **Construction.** Same associated-function-returns-`Self` model as classes, or resource-specific
  (e.g. always fallible, returning `Result<Self, E>`, since acquiring a resource can fail)?
- **`inout` parameter sugar** (separate, optional): value-semantic copy-in/copy-out params for the
  "mutate my variable through a call" ergonomic — explicitly *not* a pointer. Decide independently of
  the resource kind; it serves the value side, not the reference side.
- **Aliasing policy.** Freely aliased reference (Swift class / current `FileHandle`) vs move-only
  (Rust `File` — needs linear-move tracking). Freely-aliased is the lower-cost start and matches
  today's `FileHandle`; move-only is the stricter future option.

## Anchor

`FileHandle` (`crates/lang-stdlib/src/handle.rs`, Phase 5.2b) is the concrete prototype: its module
doc records the reference-semantics-by-design rationale, and `tests/conformance/std/fs_handle_alias.lang`
pins the shared-cursor behaviour this kind generalizes.
