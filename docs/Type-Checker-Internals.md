# The Type Checker

The user-facing behavior of the type system is on [The Type System](Type-System). This page is how the checker (`noeta-check` over `noeta-types`) works.

## Bidirectional checking with local inference

The engine is **bidirectional checking plus local inference**, deliberately not Hindley–Milner.

Subtyping is load-bearing here: `dyn` widening, directional method resolution, and struct-width subtyping all depend on it, and subtyping defeats HM's symmetric unification. Bidirectional checking accommodates subtyping cleanly by splitting into two modes:

- **Checking** propagates an *expected* type down into an expression, such as into a function argument against the parameter type.
- **Synthesis** infers a type *up* from an expression, such as a binding's initializer.

Signatures are required at named boundaries, which is what gives the checker its expected types, and bodies are inferred locally. There is no whole-program reconstruction.

Both modes on one snippet:

```noeta
fn weight(x: float): float { return x * 2.0 }

n = 1.5          // synthesis: no expected type, so the initializer's type flows UP — n: float
y = weight(n)    // checking: the argument `n` is checked DOWN against the parameter type float;
                 // the call then synthesizes float, which flows up into y
```

### How far the expectation travels

An expectation is only useful where it *reaches*, and some literal forms have no meaning without one. A heterogeneous `{"type": "array", "n": 1}` is a `Map<string, dyn>` or a type error, an empty `{}` or `[]` has no element type of its own, and a target-typed `.{ … }` has no name at all.

So the checking mode pushes the expectation through the forms that merely *choose* between values rather than producing one:

```noeta
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

Because the arm is checked against the whole expression's expected type, a mismatching arm reports **on that arm** rather than on the `match`. An arm in a position with no expectation, such as a statement-position `match` whose value is discarded, synthesizes instead.

## The type lattice

`noeta-types` holds the pure-data `Type` lattice. It carries the scalars (`Int`, `Float`, `F32`, `F64`, `IntN`, `Bool`, `String`, `Bytes`, `Unit`), the containers (`List`, `Set`, `Map`, `Option`, `Result`, `Tuple`), and the constructed forms (`Named`, `Fn`, `Param`, `Union`, `DynTrait`, `Kind`).

Two of its members are the ends of the lattice, and one is not a type a program can name:

| Form | Role |
|---|---|
| `Dyn` | The **nameable top**, written `dyn`. The one explicit escape from static typing. |
| `Never` | The bottom, the type of an expression that does not return. |
| `Unknown` | The internal **inference hole**, meaning absence of information. It is not nameable, which is why it is not spelled `Any`. |

`Type::from_ref` structurally desugars surface annotations, including `?T` into `Option<T>`, and predicates like `is_numeric` and `is_gradual` are what the checks key off.

### The built-in trait registry

`noeta-types` also owns the built-in trait registry: `BuiltinTrait`, `BUILTIN_TRAITS`, and `operator_trait`. This is the fixed set of built-in traits an `impl` block or `@derive(...)` may name, alongside the traits a program declares for itself.

Each entry records its required method and arity, the operator it overloads, and whether the compiler carries a derive recipe for it. A unit test lock-steps the registry's operator-to-method map against `BinaryOp::overload_method` in `noeta-ast`, which both backends dispatch through, so the checker's view and the runtime's view of operator dispatch cannot drift.

## Gradual by construction, static at the boundaries

The `checked` salsa query runs the checker over a parsed program and returns its diagnostics alongside the site tables the later stages read, so nothing downstream re-runs the checker.

Every expression gets an inferred `Type`, with `Unknown` as the fallback wherever a type is genuinely not yet known, and **a check suppresses itself on an `Unknown` operand**. A diagnostic therefore fires only when types are concretely known and unambiguously wrong.

That tolerance is *interior* only. Holes are eliminated at typed boundaries, where a missing signature is E0022 and an un-inferable binding is E0023, and that is what makes the system inferred-*static* rather than gradual. The contract in one line: no holes at named boundaries, inference in the interior, and `dyn` as the one explicit escape.

Representative checks:

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
| Two implemented traits supplying a default body for one method name | E0027 |
| `impl Trait for Type` in a package declaring neither (the coherence orphan rule) | E0070 |
| Invalid `.as<T>()` narrow | E0028 |

Two codes appear twice, deliberately. E0012 covers both `?` positions, the operand and the early-return fit, and E0027 covers both coherence failures.

### Where the suppression can surprise you

Because a check silently stands down on an `Unknown` operand, code in `Unknown`-typed territory gets *runtime* errors where statically-typed code would get compile-time ones. A misspelled method on a `dyn` value, arithmetic on an erased generic's `T`, or a field access downstream of an expression the checker could not type will all pass the check and fail only when they run.

The symptom to recognize is an error that "should have been caught" pointing into code that touches `dyn` or a generic parameter. Hover the operands in the editor: if one shows no concrete type, the checker never looked. The boundary rules E0022 and E0023 exist to keep this territory small.

## Type parameters

**Generics are erased for dispatch.** `class Box<T>` parses, checks, and runs with one compiled shape serving every instantiation. The type argument survives separately, on the value's reflected tag and in a hidden slot on calls, which is what `type_name::<T>()`, `field_specs_of::<T>()` and `construct::<T>(…)` read.

A type parameter is its own thing in the lattice, `Type::Param`, and its **identity is the `<T>` that declared it** rather than its spelling. That is what makes `fn m<T>()` inside `class C<T>` shadow rather than collide: substitution, binding and erasure all key on the declaration site, so the class's `T` and the method's `T` are two entries in one substitution rather than one entry two things fight over.

Where the parameter is genuinely open at a boundary, such as an argument checked against a still-uninstantiated `T` or a field whose type mentions one, it **erases to `dyn`** and the boundary accepts anything. That is the erasure, and it is separate from the identity.

The spelling `T` survives for display, in diagnostics and in hover. It is not what `type_name::<T>()` answers: that returns the instantiation's qualified name, and where nothing pinned an instantiation the call aborts rather than falling back to `"T"`.

## Why it cannot drift from the backends

The checker runs upstream of *both* execution backends, so a rejected program never reaches either, and its diagnostics are its entire observable result, identical regardless of backend.

Combined with the operator-table unit test and the shared `noeta-stdlib` semantics, the type system's decisions are pinned to the runtime's behavior by construction, and the differential oracle proves it on every program in the corpus.

## See also

- [The Type System](Type-System) — the surface: type forms, `dyn`, unions, narrowing.
- [Generics & Traits](Generics-and-Traits) — bounds and the built-in trait set.
- [Architecture & Pipeline](Architecture-and-Pipeline) — where the checker sits and the salsa graph it lives in.
