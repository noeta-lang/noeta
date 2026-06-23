# Slice S6 — `dyn` operations + checked narrowing (`x.as<T>()`)

Status: **done** (conformance 166 / differential 160 matched, 0 skipped, backends agree). Branch `types-inferred-static`.

S6 builds the explicit way *out* of `dyn`. `dyn` is the open top: every type widens into it implicitly (`T <: dyn`), but narrowing back out is never implicit — `dyn <: T` is false in the lattice. The sanctioned narrowing is `x.as<T>()`, which returns `?T` (`Option<T>`): `some(x)` when the runtime value is a `T`, `none` otherwise. This is the **one new runtime op in the entire type-system track** — it lands in *both* backends (a `Narrow` bytecode op for the VM, a matching `eval_expr` arm for the tree-walker), so the differential stays at **0 skipped**.

## Surface and disambiguation

`as` becomes a **keyword** (`AsKw`). That is the whole disambiguation story: the member-access postfix matches an *identifier* after the dot, never the keyword, so `.as<T>()` cannot be read as member access followed by a `<` comparison. The turbofish `<T>` is therefore unambiguous, and a nested generic target (`x.as<List<int>>()`) closes its own `<…>` against the outer brackets (`>>` lexes as two `>` tokens). The trailing `()` mirrors a method-call surface. No corpus or stdlib program used `as` as an identifier, so promoting it to a keyword is free.

A new AST node `Expr::As { expr, ty, span }` (not desugared) carries the target `TypeRef`, so M1 types it precisely and points diagnostics at the `as`.

## Static typing — E0028

`x.as<T>()` synthesizes `Option<T>` (`Type::from_ref(ty)` wrapped in `Option`). Two checks:

- The target type is validated like any annotation (`check_type_ref` → `E0013` on an unknown type).
- Narrowing only makes sense out of the open top. If the source expression's static type is already **concrete** (not `dyn` and not an un-inferred hole — `!defers_to_runtime()`), there is nothing dynamic to narrow, and the checker emits **`E0028 InvalidNarrow`**. A hole/`dyn` source is accepted (a hole defers, `dyn` is exactly the point).

`E0028` is the diagnostic code reserved for S6 in the README; this consumes it, so the next free code is **E0029**.

## Runtime semantics — erased, head-constructor match

Generics are erased, so narrowing tests the **head constructor only**: `x.as<List<int>>()` checks "is the runtime value a list", trusting the element type from the annotation (it is never re-checked — there is nothing at runtime to check it against). This is the standard erasure behavior (Java-style unchecked casts) and is consistent with the track's "the runtime is already the `dyn` path, generics fully erased" foundation. The match table:

- Primitives/collections (`int`/`float`/`bool`/`string`/`void`/`List`/`Map`/`Set`) match on the value's runtime kind.
- `Option`/`Result` and user records/classes/enums match by **shape name** (an enum matches its enum name, an object its type name).
- `dyn`/`Any` as a target always matches (narrowing to the open top is a no-op).

Both backends key on the **same canonical kind strings** that `Value::type_name` already returns (`"int"`, `"list"`, …) for the primitive/collection cases, and on the shape name for `Named`. The tree-walker matches `TypeRef` directly (`runtime_matches`); the compiler reduces the `TypeRef` to a `NarrowTarget` (head constructor, generic args dropped) that the VM's `narrow_matches` tests — so the two decide every narrowing identically, proven by the per-kind conformance cases under the differential gate.

## Mechanism (where the code lives)

- **lang-lexer**: `as` → `AsKw` (token + label + describe).
- **lang-ast**: `Expr::As`; `span()`, pretty-printer (`(as <ty> …)` via a new `type_ref_str`).
- **lang-parser**: a postfix at call/member binding power matching `. as < type > ( )` → `Expr::As`.
- **lang-check**: a `synth` arm → `Option<T>`, with `E0013` (target) and `E0028` (concrete source). `check`-mode falls back to synth + subsume, so a narrowing in check position is subsumed against its expectation for free.
- **lang-eval (M0)**: an `eval_expr` arm building `some(value)`/`none` via `builtin_enum`, gated by `runtime_matches(value, ty)`.
- **lang-bytecode**: `NarrowTarget` (head-constructor enum) + `Op::Narrow { dst, src, target, some_shape, none_shape }` (the two `Option` shape indices resolved at compile time; the op cannot fail, so it carries no span) + disassembly.
- **lang-compiler**: an `Expr::As` arm emitting `Op::Narrow`, with `narrow_target(ty)` mirroring `runtime_matches`. `freevars` recurses through `As` like `Try`.
- **lang-vm (M1)**: an `Op::Narrow` arm + `narrow_matches`, constructing the `some`/`none` enum from the pre-resolved shapes (retaining the payload on the some path).

## Oracle posture

The checker is shared, so `E0028`/`E0013` rejections are identical on both backends. The runtime op lands in both backends with matching match logic, so every accepted narrowing runs identically and `--differential` stays at **0 skipped** (the new runtime cases are *matched*, and the `E0028` case is a shared compile-error verdict, also matched). Baseline at S6 start: conformance 163 / differential 157 matched / 0 skipped. After S6: **166 / 160 / 0**.

## Why this gates S8

A declared union (`int | string`) is a **closed `dyn`** — a `dyn` with a static, finite membership set. Its exhaustive discrimination rides on exactly this `x.as<T>()` narrowing (one arm per member), which is why S8 is sequenced after S6. Nothing in S8 lands here; S6 only builds the narrowing primitive it will reuse.

## Verification (before commit)

- `cargo run -q -p lang-cli -- test` → 166 passed, 0 failed.
- `cargo run -q -p lang-cli -- test --differential` → 160 matched / 0 skipped / backends agree.
- `cargo test --workspace` → green (3 new `lang-check` unit tests: clean `dyn` narrow, `E0028` concrete source, `E0013` unknown target; 1 new `lang-parser` snapshot for `.as<T>()` incl. a nested generic target).
- `cargo clippy --all-targets` + `cargo fmt --all --check` → clean.
- New conformance cases: `narrowing/narrow_runtime.lang` (primitives + collection + `?? ` unwrap, hit and miss), `narrowing/narrow_user_type.lang` (user class hit + miss), `narrowing/narrow_concrete_rejected.lang` (`E0028`).
