# lang-types

The type lattice: the vocabulary the type checker reasons in.

- **Takes in:** `TypeRef` (the surface type-annotation AST, from `lang-ast`).
- **Emits:** `Type` — the lattice (`Int`/`Float`/`Bool`/`String`/`Unit`, `List`/`Map`/`Option`/`Result`, `Named`, `Fn`, unions, and the top `Unknown`), plus `Type::from_ref` (the structural desugar, including `?T` → `Option<T>`) and the `is_numeric`/`is_gradual` predicates the checks key off. (The unused Hindley–Milner `Var` slot was removed once the engine settled on bidirectional-with-subtyping rather than HM unification.)

It also owns the **built-in trait registry** (`BuiltinTrait`, `BUILTIN_TRAITS`, `operator_trait`): the fixed set of traits an `impl` block or `@derive(...)` directive may name (`Add`/`Sub`/`Mul`/`Div`/`Concat`, `Equatable`/`Comparable`/`Display`/`Clone`, …), each recording its required method + arity, the infix operator it overloads (if any), and whether it is derivable (`Attribute` is a capability trait — implementable but not derivable). This is the single source of truth `lang-check` validates against, and its operator → method map is lock-stepped to `BinaryOp::overload_method` (the form the backends use) by a unit test so the two cannot drift.

Pure data, no inference logic — that lives in `lang-check`. `Unknown` is compatible with everything and is the fallback wherever a precise type is genuinely not yet known (e.g. an erased generic parameter); it is what lets the inferred-static checker tolerate an *interior* inference hole while still requiring signatures at named boundaries. `TypeId` interning is a throughput optimization deferred until a benchmark justifies it; today `Type` is a plain owned tree.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
