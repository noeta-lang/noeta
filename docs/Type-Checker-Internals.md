# The Type Checker

The user-facing behavior of the type system is on [The Type System](Type-System). This page is how the checker (`lang-check` over `lang-types`) actually works.

## Bidirectional checking with local inference

The engine is **bidirectional checking + local inference — deliberately not Hindley–Milner.**

Subtyping is load-bearing here: `dyn` widening, directional method resolution, and struct-width subtyping all depend on it, and subtyping defeats HM's symmetric unification. Bidirectional checking accommodates subtyping cleanly by splitting into two modes:

- **Checking** — propagate an *expected* type down into an expression (e.g. into a function argument, against the parameter type).
- **Synthesis** — infer a type *up* from an expression (e.g. a binding's initializer).

Signatures are required at named boundaries (which give the checker its expected types); bodies are inferred locally. There is no whole-program reconstruction.

## The type lattice

`lang-types` is the pure-data `Type` lattice: `Int`/`Float`/`Bool`/`String`/`Unit`, `List`/`Map`/`Option`/`Result`, `Named`, `Fn`, unions, and the top `Unknown`. (The Hindley–Milner inference-variable slot was removed once the engine settled on bidirectional-with-subtyping.) `Type::from_ref` structurally desugars surface annotations (including `?T → Option<T>`); predicates like `is_numeric`/`is_gradual` are what the checks key off.

`lang-types` also owns the **built-in trait registry** — `BuiltinTrait`, `BUILTIN_TRAITS`, and `operator_trait` — the fixed set an `impl` block or `@derive(...)` may name. Each entry records its required method and arity, the operator it overloads, and whether it is derivable. The registry's operator→method map is lock-stepped to the backends' `BinaryOp::overload_method` by a unit test, so the checker's view and the runtime's view of operator dispatch cannot drift.

## Gradual by construction, static at the boundaries

`check(&Program) -> Vec<Diagnostic>` is the body of the `checked` salsa query. Every expression gets an inferred `Type`, with `Unknown` as the fallback wherever a type is genuinely not yet known, and **a check suppresses itself on an `Unknown` operand** — so a diagnostic fires only when types are concretely known and unambiguously wrong. That tolerance is *interior* only: holes are eliminated at typed boundaries (a missing signature is E0022, an un-inferable binding is E0023), which is what makes the system inferred-*static* rather than gradual. Representative checks:

| Check | Code |
|---|---|
| Non-exhaustive `match` | E0011 |
| `?` on a non-fallible value | E0012 |
| Arithmetic type mismatch | E0007 |
| Unknown trait / invalid impl | E0014 / E0015 |
| Unknown type | E0013 |
| Missing signature at a boundary | E0022 |
| Cannot infer a binding | E0023 |
| Bound not satisfied | E0025 |
| Conflicting trait impls (coherence) | E0027 |
| Invalid `.as<T>()` narrow | E0028 |

The inferred-static track layered on required signatures at named boundaries, checked arguments and returns, the E0023 "cannot infer" endpoint, bounded generics, trait coherence, `dyn` narrowing, and declared unions — while tolerating an *interior* inference hole by design. The result is the "inferred-static" contract: no holes at named boundaries, inference in the interior, and `dyn` as the one explicit escape.

**Generics are erased**: `class Box<T>` parses, checks (a type parameter is treated as `Unknown`), and runs with `T` erased to one shape.

## Why it can't drift from the backends

The checker runs upstream of *both* execution backends, so a rejected program never reaches either — its diagnostics are its entire observable result, identical regardless of backend. Combined with the operator-table unit test and the shared `lang-stdlib` semantics, the type system's decisions are pinned to the runtime's behavior by construction, and the differential oracle proves it on every program in the corpus.

## See also

- [The Type System](Type-System) — the surface: type forms, `dyn`, unions, narrowing.
- [Generics & Traits](Generics-and-Traits) — bounds and the built-in trait set.
- [Architecture & Pipeline](Architecture-and-Pipeline) — where the checker sits and the salsa graph it lives in.
