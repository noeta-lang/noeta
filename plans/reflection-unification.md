# The reflection intrinsics — adopt the chokepoint that already exists

Status: **DONE**. Steps 1, 2 and 3 are landed. The measurements below are from the original read-only survey of `main` at `d6db1de24` and are kept as the record of what was found; the line-level details no longer describe the tree.

**What shipped, and where it stopped.**

* **Step 1** — `attributes_of`, `roles_of` and `from_bytes` adopted the operand contract; `Op::AttributesOf` took a name-string register instead of a `Module::type_args` index.
* **Step 2** — one `Expr::Reflect { which: ReflectKind, operand: ReflectOperand }`, one `Rvalue::Reflect { which, args: ReflectArgs }`, and one dispatch per layer. The seven `ReflectOperand` arms are the closed set of contracts `ReflectKind::shape()` assigns; the ~30 mechanical walks became one arm each, delegating to `for_each_expr` / `for_each_type_ref`.
* **Step 3** — `crates/noeta-builtins/tests/reflect_surface.rs`. Its first run found three things, which is the usual result of writing one of these down: `ReflectKind::ALL` was not in the lexer order it claimed to be in, and both oracles it stands on were named wrong.

**The opcodes deliberately did not collapse**, and the reasoning generalizes. The `Expr` and `Rvalue` variants existed to be *walked* — by passes that differed only in a field name, which is what drifted. An `Op` is dispatched rather than walked: its consumers (regalloc def/use, liveness, the disassembler) already shared arms, the VM's work is irreducibly per-query, and an opcode is a serialized wire format with a size budget. Collapsing it would have bought a handful of lines and cost a bytecode format break. **The collapse is worth what the walks cost, and nothing more.**

**Two latent gaps closed on the way, both the same shape.** `attributes_of`, `type_name` and `roles_of` sat in the *leaf* group of the nested-fn collectors (the checker's and the state machine's), so a nested `fn` inside a dynamic operand would never have been hoisted. Neither had been reported. One arm cannot make that mistake — which is the argument for the collapse in miniature.

The question that prompted this: *should the reflection primitives move under a standard-library namespace?* **No — or at least, not for any reason in this document.** The namespace move buys no bug fix, and the nearest precedent (reactivity leaving the prelude) accepted a measured **48.8%** regression as the price of three boundary crossings per operation. The defects here are all in-tree, and so is the fix.

What the surface actually needs is to stop being special. Everything below is internal: no boundary crossing, no ABI change, no library.

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

## The chokepoint already exists, and three of the thirteen adopted it

`noeta_ast::TypeOperand` is `Static(TypeRef) | Dynamic(Box<Expr>)`, and its own doc already states the channel behaviour: a bare type parameter *"has no compile-time name and still resolves… lowering reads the name off that channel instead of folding a constant, so this arm means `field_specs_of(type_name::<T>())` there — the same answer, one arm."*

| adopted `TypeOperand` | kept a bare `TypeRef` |
|---|---|
| `field_specs_of` ✅ | `attributes_of` ⚠️ |
| `variants_of` ✅ | `roles_of` ⚠️ (`Option<TypeRef>`) |
| `construct` ✅ | `from_bytes` ⚠️ |
| | `type_name` (two bespoke per-channel ops) |

**The three that adopted it work. The four that did not are the four with gaps.** So this is not a design exercise — the contract exists, it is documented, it is proven by the intrinsics using it, and the fix is adoption.

Both open questions from the first draft settle in favour of doing it:

- **`materialize_roles(Option<&str>)`** filters the whole `reflection.roles` table at run time (`r.enum_name == e`), and `derive_roles` builds that table from the entire manifest — not from what a compile-time query mentioned. **Total on an arbitrary enum name.**
- **`materialize_attributes(&str)`** already takes a string in **both** backends. No `NameId`-only precondition.

So the backends need no work at all. Only the operand path does.

---

## Step 1 — adopt `TypeOperand`

`attributes_of`, `roles_of` and `from_bytes` take `TypeOperand` instead of a bare `TypeRef`; `type_name`'s two bespoke ops become the shared name-resolution path rather than two ops written for one caller.

Consequences, in order of what they unblock:

1. **`Op::AttributesOf`'s `dynamic` operand becomes a name-string register.** It currently holds an `int` index and indexes `module.type_args` itself, which is why it cannot consume what `Op::TypeArgName` produces. Bytecode contract change ⇒ `FORMAT_VERSION` + `MODULE_LAYOUT_DIGEST` bump, plus VM, reference interpreter, regalloc, liveness, disassembly.
2. **`Op::RolesOf` gains a register operand.** Today it is `NameId`-only with no register.
3. **`from_bytes` is the honest maybe.** It bakes a `PackedLayout` at check time, so it may genuinely need the layout rather than the name. If it does, the right outcome is a *tailored* `E0058` rather than today's "requires a packable element type", which sends the author to fix the wrong thing.

**Two live defects close as a side effect**, both currently misdiagnosed rather than wrong:

- `roles_of::<E>()` on a forwarded parameter reports *"requires a `@semantic` enum, but `E` is not one"*. `E` may well be one at every call site; the real reason is that no channel carries it.
- `from_bytes::<T>()` reports *"requires a packable element type"* for the same reason.

`plans/backlog.md` records both as *"clean checker errors"*. That claim is stale.

---

## Step 2 — one node, one exhaustive enum

Collapse the thirteen `Expr` variants (and their `Rvalue` and `Op` twins) into:

```rust
Expr::Reflect { which: ReflectKind, operand: TypeOperand, span: Span }
```

with `ReflectKind` a **fieldless enum**. This is the answer to "can the string matching be an exhaustive match on an enum": the *intrinsic selector* can and should be, and then every dispatch over it is exhaustive by construction — a fourteenth kind is a **compile error**, not a silent gap. Exactly what audit row 7 did for jump targets.

The *type name* cannot be an enum and should not be: type names are open-world, users declare them. It is already interned as `NameId(u32)`, which is the correct form. The problem was never the string — it was that `attributes_of` took an int index where the others took a name.

**What this buys.** Adding a fourteenth intrinsic today costs **~38 edits across 20 files in 12 crates**, of which ~30 are mechanical — `span`, `mentions`, `has_await`, qualify, liveness, freevars, regalloc, disassembly, state-machine — differing only in the variant name and which sub-expression they recurse into. With one node they become one arm each, and the marginal cost drops to roughly four edits: an enum variant, a checker arm, a lowering arm, a backend implementation.

It also frees the parser's `choice` tuple, which is **at its arity cap** — a standing tax on adding any new production, paid by every future feature, not just this surface.

**Do step 1 first.** Collapsing the node while four operand contracts still disagree would bake the disagreement into the shared shape.

---

## Step 3 — the census that closes the class

One row per `ReflectKind`, in the shape of `crates/noeta-check/tests/site_policies.rs`: every kind that names a type resolves its operand through the shared helper, and no kind carries a bespoke operand type. A fourteenth cannot invent a fifth contract without the gate saying so.

Without this, steps 1 and 2 fix today's four bugs and leave the mechanism that produced them.

---

## Not doing, and why

**The namespace move (`use std.reflect`).** Buys no bug fix. The extern ABI hands a native function a `TypeRecipe` — a structural build recipe, not a type identity — so `T = int` arrives with **no name at all**, `Checker::type_to_recipe` declines classes and generic structs outright, and `RetTy::TypeArg` cannot express `attributes_of::<T>(): List<Attributed<T>>`. A `std.reflect` on today's ABI would be *less* capable than the intrinsics it replaces. Revisit only if the ABI grows a `TypeArgInfo { name, recipe }` boundary for its own reasons.

**De-reserving the thirteen words.** Independent of everything above and cheap — `channel` did it in **5 files** by having the parser recognise it contextually while keeping the IR path. Worth doing for the thirteen names it returns to users (`construct`, `invoke`, `params_of`, `type_name` are all plausible identifiers), but it is a language-surface decision with a deprecation story, and it fixes nothing. Note it also removes the "this name is taken" diagnostic, making the words shadowable.

---

## Follow-on: the LSP

Confirm the reflection surface behaves in the editor — hover, completion, go-to-definition, and tier hover — on each of the thirteen and on both channels. One gap is already known:

`noeta-ide/src/lib.rs`'s `tier_name_at::in_expr` ends in `_ => None` and **already misses twelve `Expr` variants**, so `@html { … }` tier hover inside `construct::<T>(@html{…})` silently returns nothing today. Step 2 dissolves this specific instance — one `Expr::Reflect` arm replaces twelve missing ones — which is a good illustration of why the collapse is worth more than the sum of its edits.
