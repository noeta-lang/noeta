# The Type Checker

The user-facing behavior of the type system is on [The Type System](Type-System). This page is how the checker (`noeta-check` over `noeta-types`) actually works.

## Bidirectional checking with local inference

The engine is **bidirectional checking + local inference — deliberately not Hindley–Milner.**

Subtyping is load-bearing here: `dyn` widening, directional method resolution, and struct-width subtyping all depend on it, and subtyping defeats HM's symmetric unification. Bidirectional checking accommodates subtyping cleanly by splitting into two modes:

- **Checking** — propagate an *expected* type down into an expression (e.g. into a function argument, against the parameter type).
- **Synthesis** — infer a type *up* from an expression (e.g. a binding's initializer).

Signatures are required at named boundaries (which give the checker its expected types); bodies are inferred locally. There is no whole-program reconstruction.

Both modes on one snippet:

```noe
fn weight(x: float): float { return x * 2.0 }

n = 1.5          // synthesis: no expected type, so the initializer's type flows UP — n: float
y = weight(n)    // checking: the argument `n` is checked DOWN against the parameter type float;
                 // the call then synthesizes float, which flows up into y
```

### How far the expectation travels

An expectation is only useful where it *reaches*, and some literal forms have no meaning without one: a heterogeneous `{"type": "array", "n": 1}` is a `Map<string, dyn>` or a type error, an empty `{}`/`[]` has no element type of its own, and a target-typed `.{ … }` has no name at all. So the checking mode pushes the expectation through the forms that merely *choose* between values rather than producing one:

```noe
fn schema(x: int): Map<string, dyn> {
    return match x {                              // the return type reaches BOTH arms…
        1 => {"type": "array", "n": 1},           // …so the mixed literal is a Map<string, dyn>
        _ => {},                                  // …and the empty one adopts the same type
    }
}

fn schema2(x: int): Map<string, dyn> {
    return if x == 1 then {"type": "array"} else {}   // an `if…then…else` is a desugared `match`
}
```

Because the arm is checked against the whole expression's expected type, a mismatching arm reports **on that arm**, not on the `match`. An arm in a position with no expectation — a statement-position `match`, whose value is discarded — synthesizes exactly as it always did.

## The type lattice

`noeta-types` is the pure-data `Type` lattice: `Int`/`Float`/`Bool`/`String`/`Unit`, `List`/`Map`/`Option`/`Result`, `Named`, `Fn`, unions, and the top `Unknown`. (The Hindley–Milner inference-variable slot was removed once the engine settled on bidirectional-with-subtyping.) `Type::from_ref` structurally desugars surface annotations (including `?T → Option<T>`); predicates like `is_numeric`/`is_gradual` are what the checks key off.

`noeta-types` also owns the **built-in trait registry** — `BuiltinTrait`, `BUILTIN_TRAITS`, and `operator_trait` — the fixed set an `impl` block or `@derive(...)` may name. Each entry records its required method and arity, the operator it overloads, and whether it is derivable. The registry's operator→method map is lock-stepped to the backends' `BinaryOp::overload_method` by a unit test, so the checker's view and the runtime's view of operator dispatch cannot drift.

## Gradual by construction, static at the boundaries

`check(&Program) -> Vec<Diagnostic>` is the body of the `checked` salsa query. Every expression gets an inferred `Type`, with `Unknown` as the fallback wherever a type is genuinely not yet known, and **a check suppresses itself on an `Unknown` operand** — so a diagnostic fires only when types are concretely known and unambiguously wrong. That tolerance is *interior* only: holes are eliminated at typed boundaries (a missing signature is E0022, an un-inferable binding is E0023), which is what makes the system inferred-*static* rather than gradual. Representative checks:

| Check | Code |
|---|---|
| Non-exhaustive `match` | E0011 |
| `?` on a non-fallible value | E0012 |
| `?` whose early return does not fit the declared return (`Option` → `?T`, `Result` → `Result<T, E>`) | E0012 |
| Arithmetic type mismatch | E0007 |
| Unknown trait / invalid impl | E0014 / E0015 |
| Unknown type | E0013 |
| Missing signature at a boundary | E0022 |
| Cannot infer a binding | E0023 |
| Bound not satisfied | E0025 |
| Conflicting trait impls (coherence uniqueness) | E0027 |
| `impl Trait for Type` in a package declaring neither (the coherence orphan rule) | E0070 |
| Invalid `.as<T>()` narrow | E0028 |

The resulting contract in one line: **no holes at named boundaries, inference in the interior, and `dyn` as the one explicit escape.**

**Where the suppression can surprise you.** Because a check silently stands down on an `Unknown` operand, code in `Unknown`-typed territory gets *runtime* errors where statically-typed code would get compile-time ones: a misspelled method on a `dyn` value, arithmetic on an erased generic's `T`, or a field access downstream of an expression the checker could not type will pass the check and fail (or misbehave) only when it runs. The symptom to recognize is an error that "should have been caught" pointing into code that touches `dyn` or a generic parameter — hover the operands in the editor: if one shows no concrete type, the checker never looked. The boundary rules (E0022/E0023) exist precisely to keep this territory small.

**Generics are erased**: `class Box<T>` parses, checks (a type parameter is treated as `Unknown`), and runs with `T` erased to one shape.

## Why it can't drift from the backends

The checker runs upstream of *both* execution backends, so a rejected program never reaches either — its diagnostics are its entire observable result, identical regardless of backend. Combined with the operator-table unit test and the shared `noeta-stdlib` semantics, the type system's decisions are pinned to the runtime's behavior by construction, and the differential oracle proves it on every program in the corpus.

## See also

- [The Type System](Type-System) — the surface: type forms, `dyn`, unions, narrowing.
- [Generics & Traits](Generics-and-Traits) — bounds and the built-in trait set.
- [Architecture & Pipeline](Architecture-and-Pipeline) — where the checker sits and the salsa graph it lives in.
