# CLI foundations arc

The goal is a CLI story for Noeta programs: today `args.all() -> List<string>` is the *entire*
surface (one conformance test, zero examples, zero docs, no `plans/` mention), so writing a CLI
means hand-rolling flag parsing over a string list — and a usage error cannot even be reported on
stderr, because no stderr surface exists.

The destination is `para/cli`: a reflection-driven framework where **the function signature is the
spec** — required parameters become positionals, defaulted ones become flags, `List<string>`
becomes variadic — discovered via `attributes_of`, reflected via `params_of`, dispatched via
`invoke`. That is the same machine `para/aether` already uses for HTTP routing, pointed at argv.

It is deliberately *not* built on `@derive` or `ExtDirective::expand`:

- `@derive` cannot generate declarations. `plan_derive` returns `Vec<FnDecl>` spliced onto the
  decorated type, so a clap-style `@derive(Parser)` cannot produce the associated `parse()`
  constructor that makes clap-derive ergonomic.
- `ExtDirective::expand` (unmerged, `worktree-directive-expand`) is compile-time codegen, but its
  `DirectiveCtx` deliberately carries only the target's *name* — not the declaration's shape — so
  it cannot read the parameters a CLI generator exists to read. That branch's own ABI doc draws the
  line: "A directive is *not* readable at runtime, by design — an extension that wants
  runtime-visible metadata declares an attribute instead." `--help`, dispatch and error text are
  all runtime consumers, so attributes are the sanctioned half.

Building on reflection also keeps `para/cli` pure Noeta, matching `para/api`; `expand` is a native
Rust hook and would force a native package.

## Why the prerequisites come first

The reflection surface is not currently trustworthy enough to build on, and the defects are the
same shape: **one code site knows a rule its neighbour doesn't**. They are fixed first, in their
own slices, so `para/cli` is built on ground that holds.

## Slices

| # | Slice | Branch | State |
|---|---|---|---|
| 1 | `BuiltinTy` enum — collapse ~17 stringly-typed name→kind/arity tables to one | `slice-builtinty` | in flight |
| 2 | `.{ }` target-typed struct literals + inlay hint | `slice-dot-brace` | in flight |
| 3 | Free-function `invoke("name", args)` | — | queued |
| 4 | `env.get -> ?string` | `slice-env-get` | done |
| 5 | `optional` on `ParamSig` | — | queued |
| 6 | Parameter attributes (`#[Arg(...)]` on a parameter) | — | queued |
| 7 | std/host: stderr, stdin, TTY detection | — | queued |
| 8 | `para/cli` itself | — | queued |

### 1. `BuiltinTy` — the drift fix

Built-in type-constructor names are matched as raw strings (`"List" | "list"`) in ~17 independent
places. A string match can never be exhaustive, so the copies drifted. Measured: adding a
hypothetical container fails **loudly at ~6 sites and silently at ~17** — and the loud ones are the
already-correct ones.

Live proof, not hypothesis:

```
fn f(a: i32, c: f64, d: int): void
params_of("f")  ->  a: Type.Named(i32, [])   c: Type.Named(f64, [])   d: Type.Int
a: i32 = 5; type_of(a)  ->  Type.Int
```

`params_of` and `type_of` disagree about the same type, because `scalar_repr`
(`noeta-ast/src/reflect.rs`) knows `f32` but not `f64` or the fixed-width ints, while
`Type::from_ref` (`noeta-types/src/lib.rs`) knows all of them. A DI-style framework that reflects a
parameter type and matches it against a runtime value would fail on any `i32` parameter.

The mechanism is **already in-house, one level down**: `ring1.rs` enumerates ring-1 methods as
`ListMethod`/`SetMethod`/… precisely so "adding a method will not compile until *both* backends
handle it — the differential's static guard". This slice applies that pattern one level up.

### 2. `.{ }` — target-typed struct literals

`{ ... }` stays a map (keys are expressions, unchanged); `.{ ... }` is a struct literal whose type
comes from the expected type; `T { ... }` stays explicit. A three-way split with no overlap.

This deliberately replaces an earlier idea of changing map-literal key semantics so unquoted keys
became string literals. That was rejected: `{1: "a"}` is the *only* literal form for
`Map<int, string>`, and four of the five supported key kinds (int, fixed-width int, key-capable
native, key-capable `@packed` struct) have no other spelling — so the change would remove a
capability rather than move it. The residual footgun is narrow, since an unknown bare key is
already a loud E0005.

Absorb only a concrete `Type::Named` in `symbols.records`; union, unresolved generic and `?Foo` all
give E0023. `?Foo` is **not** auto-peeled: `T` is never implicitly `?T` here (`a: ?int = 5` is
E0007 exactly like `a: ?List<int> = [1,2,3]`), so `some(.{ ... })` is the correct spelling and
`.{ }` stays consistent with `[...]`.

## Testing note — the oracle gap

The differential oracle compares the two backends against *each other*, so it is blind whenever
both are wrong in the same way. The `type_ref_repr` arg erasure is exactly that: both backends call
one shared function and agree, and its own doc says they "agree across the differential by
construction". That sentence is true and is why nobody noticed.

Catching this class needs an oracle that compares against a *specification* rather than another
implementation — e.g. a round-trip property: for any `TypeRef`, lowering to `TypeRepr` and
rendering should reproduce the source spelling. The codebase has extensive differential coverage
and almost no property coverage.
