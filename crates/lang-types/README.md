# lang-types

The type lattice: the vocabulary the M1 type checker reasons in.

- **Takes in:** `TypeRef` (the surface type-annotation AST, from `lang-ast`).
- **Emits:** `Type` — the lattice (`Int`/`Float`/`Bool`/`String`/`Unit`, `List`/`Map`/`Option`/`Result`, `Named`, `Fn`, the inference variable `Var`, and the **gradual top** `Unknown`), plus `Type::from_ref` (the structural desugar, including `?T` → `Option<T>`) and the `is_numeric`/`is_gradual` predicates the checks key off.

It also owns the **built-in trait registry** (`BuiltinTrait`, `BUILTIN_TRAITS`, `operator_trait`): the fixed set of traits an `impl` block or `#[derive(...)]` may name (`Add`/`Sub`/`Mul`/`Div`/`Concat`, `Equatable`/`Comparable`/`Display`/`Clone`, …), each recording its required method + arity, the infix operator it overloads (if any), and whether it is derivable. This is the single source of truth `lang-check` validates against, and its operator → method map is lock-stepped to `BinaryOp::overload_method` (the form the backends use) by a unit test so the two cannot drift.

Pure data, no inference logic — that lives in `lang-check`. `Unknown` is compatible with everything and is the fallback wherever a precise type cannot be inferred; it is what makes the checker *gradual* (an un-inferable expression never produces a false-positive error). `TypeId` interning (architecture §) is a throughput optimization deferred until a benchmark justifies it; today `Type` is a plain owned tree.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
