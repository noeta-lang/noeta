# The reflection intrinsics — one operand contract, then a namespace

Status: **scoping**. No code changed. The measurements below are from a read-only survey of `main` at `d6db1de24`; verify before acting on any line number.

The question that prompted this: *should the reflection primitives move under a standard-library namespace?* The answer is yes eventually, and **that is the second half of the work, not the first**. The first half is smaller, cheaper, fixes every known bug in this surface, and is a prerequisite for the second anyway.

---

## The finding

The thirteen intrinsics do not share an operand contract. At the opcode level they split five ways:

| what the opcode takes | intrinsics | n |
|---|---|---|
| **string-name register** | `params_of`, `returns_of`, `field_specs_of`, `variants_of`, `construct` | 5 |
| value register | `type_of`, `fields_of`, `traits_of`, `from_bytes` | 4 |
| compile-time `NameId`, no register | `attributes_of`, `roles_of` | 2 |
| slot register holding an `int` index into `Module::type_args` | `attributes_of`'s `dynamic` operand | 1 |
| no opcode — const-folded, plus two bespoke per-channel ops | `type_name` | 1 |
| mixed value + string registers | `invoke` | 1 |

A type parameter's instantiation reaches a body on one of **two per-instantiation channels** — the receiver's reflected type tag, and the hidden type-argument slot. Both are decided in one place (`Checker::record_type_param`) and both *deliver a name*.

So: the five string-keyed surfaces plus `type_name` route through one helper (`Lowerer::type_param_name_atom`) and reach **both** channels for free. The others cannot, and the reason is mechanical rather than accidental — `Op::TypeArgName`/`Op::TypeSlotName` produce a **string**, while `Op::AttributesOf { dynamic }` consumes an **int index** and indexes `module.type_args` itself. The two are structurally incompatible.

**The correspondence is the whole argument.** Today's known gaps are:

| intrinsic | gap | operand contract |
|---|---|---|
| `attributes_of` | works on the slot channel, `E0035` on the receiver tag | slot index |
| `roles_of` | reaches neither channel; misdiagnoses a forwarded `E` | no register at all |
| `from_bytes` | reaches neither channel; misdiagnoses a forwarded `T` | baked schema index |
| `type_name` | works — via two bespoke ops written only for it | no opcode |

Four gaps. Four non-conforming contracts. The same four. Nothing else in the surface is broken, and the five that share a contract have never needed individual attention — when the turbofish forwarding landed, they came along for free and `attributes_of` structurally could not.

That is the parallel-path shape precisely: not thirteen copies of one function, but **thirteen independent answers to "how do I name the type I am asked about"**, where a capability added to one form cannot propagate to the others.

---

## What this is *not*

Three tempting framings the survey rules out.

**It is not the thirteen match arms.** Adding a fourteenth intrinsic costs ~38 edits across 20 files in 12 crates, and ~30 of those are mechanical (span, `mentions`, `has_await`, qualify, liveness, freevars, regalloc, disassembly, state-machine). That is real friction, but collapsing it needs a shared *node shape* (one `Expr::Reflect { which, operand }`), which is a separate refactor with its own risk and no bug attached to it. Do it later or never; it is not why anything is broken.

**It is not the parser.** Every one of the thirteen has a call-shaped surface the general grammar already covers — `Expr::Call` for the value-operand forms, `Expr::TypedCall` (the `typed_fn_call` production that already serves `gen::<T>(x)`) for the turbofish forms. Delete the thirteen `#[token]` attributes tomorrow and **nothing fails to parse.** The tree-sitter grammar already documents this: it has no rule for them at all, allow-listed with the reason *"a reflection primitive is not a grammar token."*

**It is not blocked on the extern ABI.** A native function can receive a type argument today — `TypedDispatch`/`TypedTypeDispatch` with `RetTy::TypeArg`, live in-tree for `json.parse::<T>` and `resp.json::<T>()`. But see the caveats in stage 3; the ABI carries a *structural recipe*, not a type identity, and that gap is the real cost of the namespace move.

---

## Stage 1 — one operand contract *(the whole point)*

**Move `attributes_of`, `roles_of` and `from_bytes` onto the string-name operand contract the other five already use.**

That is the entire fix. It is not a library move, it needs no new mechanism, and it makes the receiver-tag channel arrive at all thirteen without touching any of them individually — because they would all be reading the one helper that already reaches both channels.

Sequenced by what unblocks what:

1. **`Op::AttributesOf`'s `dynamic` operand becomes a name-string register.** This is the contract change the previous agent scoped out of. `materialize_attributes` already takes `&str`, which is promising but unverified (see open question 3). Bytecode contract change ⇒ `FORMAT_VERSION` + `MODULE_LAYOUT_DIGEST` bump, VM + reference interpreter + regalloc + liveness + disassembly.
2. **`roles_of` gains a register operand.** Currently `NameId`-only with no register. Blocked on open question 2 — whether the roles index is fully present at runtime for an arbitrary enum name, or filtered at build time to the names a compile-time query mentioned. **Settle this first; it decides whether stage 1 is three intrinsics or two.**
3. **`from_bytes`** bakes a `PackedLayout` at check time, so it is the odd one out: it may genuinely need the layout rather than the name. If so, the honest outcome is a *tailored* `E0058` rather than the current "requires a packable element type", which sends the author to fix the wrong thing.
4. **Retire `Op::TypeArgName`/`Op::TypeSlotName` as `type_name`-specific.** Once every intrinsic reads a name, these are the shared name-resolution ops, not two ops written for one caller.

**The gate that makes this stick.** A census in the shape of `crates/noeta-check/tests/site_policies.rs`: one row per intrinsic, classified by operand contract, asserting that every intrinsic taking a type-name operand resolves it through the shared helper — so a fourteenth cannot quietly invent a sixth contract. That is the difference between fixing four bugs and closing the class.

**Two live defects close as a side effect** (both found by the survey, both currently misdiagnosed rather than wrong):

- `roles_of::<E>()` on a forwarded parameter reports *"requires a `@semantic` enum, but `E` is not one"* — `E` may well be one at every call site; the real reason is that no channel carries it.
- `from_bytes::<T>()` reports *"requires a packable element type"* for the same reason.

`plans/backlog.md` claims both *"stay clean checker errors"*. That claim is stale.

**Size:** ~1 op contract change, ~2 checker arms, ~1 census. Bytecode format bump. Call it a week of careful work, most of it in the two backends.

---

## Stage 2 — de-reserve the words

Delete the thirteen `#[token]` attributes; let the existing `Expr::Call`/`Expr::TypedCall` productions carry them.

Three things break, all downstream of parsing, and only the third is real work:

1. Thirteen dead parser productions and five `choice`-tuple slots — **which frees arity headroom the tuple is currently capped against**, a standing tax on adding any new production.
2. **The reserved-name diagnostic stops firing.** `fn look(type_name: string)` is currently rejected with "this name is taken". After stage 2 the words are shadowable. That is arguably correct — it is what `signal`/`sleep` did — but it is a deliberate language decision, not a side effect to discover later.
3. **There is no resolution mechanism to replace them.** `Ident("type_of")` must resolve to *something*. This is what stage 3 provides, which is why stage 2 alone is not shippable.

Everything else is cheaper than it looks, because audit row 11 already made the vocabulary derived: IDE completion and highlight are **unaffected** (they ask the lexer), and `highlights.scm` + `noeta.tmLanguage.json` regenerate with one command rather than two hand-edits.

---

## Stage 3 — `use std.reflect`

The namespace move proper. **This is where the real cost is, and it is not where the bugs are.**

The ABI seam exists but is narrower than it looks. `TypedDispatch` hands a native function a `&TypeRecipe` — a *structural build recipe*, not a type identity:

- `T = int` arrives as `TypeRecipe::Int` with **no name at all**, and every name-keyed reflection registry keys on a name.
- `Checker::type_to_recipe` **declines** classes, generic structs, extern types, traits, non-string-keyed maps and payload-carrying enum variants. A declined `T` is a checker error and the native function never runs — so a `std.reflect` on today's ABI would be *less* capable than the intrinsics it replaces.
- `RetTy::TypeArg` pins the return to `T`/`Option<T>`/`Result<T,E>`. `attributes_of::<T>(): List<Attributed<T>>` is inexpressible.
- The forwarded path already resolves the `Module::type_args` entry and then **drops `entry.name`** before the boundary.

**The missing piece is small and additive**: a dispatch signature receiving the whole `TypeArgInfo { name, recipe }` rather than `&TypeRecipe`. The data already sits in `Module::type_args`; the boundary just projects the name away. Do that and stage 3 becomes a migration rather than a new machine.

**Precedent, and an honest warning.** Four constructs have left the prelude or the keyword set:

| construct | mechanism | cost |
|---|---|---|
| `signal`/`computed`/`effect` | virtual modules, then a full native `Extension` | 26 files; deleted `Value::Reactive`, `HeapKind::Reactive`, three `Builtin`s and the whole virtual-module mechanism. **Measured `r_set_flush +48.8%`, which missed its ≤35% gate and was accepted** as "the structural floor of 3 boundary crossings" |
| `sleep`/`all`/`race` | `std.task` + `NativeCtx` | 12 files; six per-backend drive loops deleted. Named `task` not `async` *because* `async` is a keyword and `use std.async` would not parse |
| `len`/`map`/`filter` | methods on builtin collections | 36 files; `Builtin::{Len,Map,Filter,Sum}` still exist — a surface change, not de-intrinsification |
| `channel` | lexer stopped emitting the token; parser recognises it contextually | **5 files** — but it kept `Expr::Channel` and the whole IR path |

**No commit in-tree has ever moved a lexer keyword *and* its `Expr::` variant into a namespaced library function.** Stage 3 would be the first, combining `channel`'s lexer technique with the reactivity migration's registry work. The reactivity precedent is the one to weigh: it accepted a measured 48.8% regression on a hot path as the price of removing the special case. Reflection is not on a hot path, which is the argument for expecting a better outcome — but the number should be measured, not assumed.

**Corpus cost:** 170 `.noe` files use at least one intrinsic (97 in `tests/conformance/reflection/` alone). Each needs one `use` line; the call spellings survive. That is the same shape the reactivity and task moves paid, at ~10× the file count.

---

## Recommendation

**Do stage 1. Then decide about 2 and 3 with the bugs already gone.**

Stage 1 is where every known defect lives, it is a fraction of the cost, and it is a prerequisite for the others regardless — a `std.reflect` whose functions disagreed about how to name a type would ship the same class of bug into a new mechanism, which is the outcome this whole exercise exists to avoid.

Stages 2 and 3 buy real things: thirteen names returned to users, parser arity headroom, and the removal of a special case that the audit's own evidence says is where drift breeds. But they buy no bug fixes, and they carry a measured precedent of accepting a performance regression to remove a special case. That trade deserves its own decision, made when it is the only thing on the table.

---

## Open questions, in the order they block work

1. **Is the roles index complete at runtime for an arbitrary enum name, or filtered at build time to the names a compile-time query mentioned?** Decides whether stage 1 covers three intrinsics or two. Settled by reading `materialize_roles` and `ReflectionInfo`'s role-binding table. **Most load-bearing unknown in this document.**
2. **Is `materialize_attributes` total on a runtime name?** It takes `&str` today, which is promising, but the interned-`NameId` path may carry a precondition the string path lacks. Settled by reading both backends' implementations.
3. **Does `para.aether.openapi` use turbofish reflection forms, and at how many call sites?** `para` is a sibling repo, not in this tree, so the survey could not check. Affects stage 3's migration cost only.

## Found while surveying, not fixed

- `noeta-ide/src/lib.rs`'s `tier_name_at::in_expr` ends in `_ => None` and **already misses twelve `Expr` variants** — so `@html { … }` tier hover inside `construct::<T>(@html{…})` silently returns nothing today. Low severity, exactly the shape audit row 7 fixed for jump targets, and the only remaining silent-miss wildcard the survey found in this surface.
- `plans/backlog.md`'s claim that `roles_of`/`from_bytes` "stay clean checker errors" is stale; both misdiagnose.
