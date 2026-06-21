# lang-types

The type lattice: the vocabulary the M1 type checker reasons in.

- **Takes in:** `TypeRef` (the surface type-annotation AST, from `lang-ast`).
- **Emits:** `Type` — the lattice (`Int`/`Float`/`Bool`/`String`/`Unit`, `List`/`Map`/`Option`/`Result`, `Named`, `Fn`, the inference variable `Var`, and the **gradual top** `Unknown`), plus `Type::from_ref` (the structural desugar, including `?T` → `Option<T>`) and the `is_numeric`/`is_gradual` predicates the checks key off.

Pure data, no inference logic — that lives in `lang-check`. `Unknown` is compatible with everything and is the fallback wherever a precise type cannot be inferred; it is what makes the checker *gradual* (an un-inferable expression never produces a false-positive error). `TypeId` interning (architecture §) is a throughput optimization deferred until a benchmark justifies it; today `Type` is a plain owned tree.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
