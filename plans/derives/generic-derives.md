# Generic derives: `@derive(Trait<TypeArg>)`

Status: **planned** (not started). Branch `types-inferred-static`. In scope from the start (per the
design discussion). Independent of the roles/abstract-types/structured-args plans.

## What and why

Today `@derive(...)` takes plain trait *names* (`@derive(Comparable, Clone)`), parsed as identifiers.
Generic derives let a derive carry a **type argument** — `@derive(From<Json>)` — so a derivable trait
parameterized by a type can be generated. This is the natural extension once attributes/derives reference
types (the sibling of type-reference attribute args).

```
@derive(Into<string>)            // generate the Into<string> impl structurally
type UserId = { value: int };
```

## The mechanism (straightforward) vs. the consumer (the real decision)

The **mechanism** is a contained grammar/threading change. The genuine open question is **which
generic derivable trait(s) ship as the first consumer** — none of today's derivable traits
(`Equatable`, `Comparable`, `Display`, `Clone`, `ToJson`, `Serialize`) take a type parameter, so a
generic derive needs a trait that does *and* is structurally derivable. This must be decided at
review; the mechanism is useless (and untestable) without one.

### Candidate consumer traits (decision needed)

- **`Serialize<Format>` — recommended.** Generalize the existing nullary `ToJson`/`Serialize` into a
  format-parameterized derive: `@derive(Serialize<Json>)`, with `Format` a blessed enum (`Json`,
  …; extensible later). Codegen is structural (walk fields) parameterized by format — genuinely
  derivable, genuinely useful, and it *subsumes* `ToJson`. Cost: a `Format` vocabulary + one codegen
  path (Json to start), so the derive has a real, testable consumer from day one.
- **`From<T>` / `Into<T>` — conversion.** Classic generic traits, but **not** structurally
  auto-derivable in general (converting `T → Self` needs a field mapping the compiler can't infer);
  only the newtype/single-field case is mechanical. Narrower than it looks.
- **A phantom/marker generic** (`Tagged<T>`) — trivial to derive, but earns nothing.

**Decision (confirmed with the user):** ship the mechanism **plus `Serialize<Format>`** (Json first)
as the motivating consumer, generalizing the existing nullary `ToJson` into the format-parameterized
form. `Format` is a blessed enum (`Json` to start, extensible). `@derive(Serialize<Json>)` generates
the structural serializer with the format selecting the emitter; `ToJson` folds into
`Serialize<Json>` (keep a deprecated alias or migrate call sites — decide during implementation).

## Mechanism

- **Parser** (`lang-parser`): `derive_directive` args change from `id` to **`type_parser`**, so
  `From<Json>` / `Serialize<Json>` parse as a type with arguments. Plain `@derive(Comparable)` still
  parses (a type with no args). The `@attribute`/`@role` directives keep their own grammars.
- **AST** (`lang-ast`): `RecordDecl.derives` / `ClassDecl.derives` change from `Vec<(String, Span)>`
  to carry the parsed type (name + type args) — e.g. `Vec<(TypeRef, Span)>` or `Vec<(String,
  Vec<TypeRef>, Span)>`. `derives_trait` and every reader update accordingly.
- **Checker** (`lang-check`): `check_derives` validates (a) the trait is a derivable built-in, (b) its
  generic arity matches the supplied args, (c) each type argument resolves (`E0013`), and (d) the
  trait's derivability constraints on `T` hold. Reuse the trait registry's `derivable` flag, extended
  with arity/parameter info.
- **Compiler** (`lang-compiler`): codegen for the chosen generic trait (`Serialize<Json>` → the
  structural JSON serializer, the threaded format selecting the emitter). The existing nullary
  `ToJson` codegen is the starting point.
- **Backends**: behavior rides the generated impl (trait dispatch), so no new op; differential holds
  by the usual derive mechanism (both backends run the generated methods).

## Diagnostics

Reuse `E0013` (unknown type arg), `E0014`/the derivable-trait error (non-derivable or arity mismatch).
Confirm during implementation whether a dedicated code is warranted.

## Touch list

`lang-parser` (derive args via `type_parser`), `lang-ast` (`derives` shape + `derives_trait`),
`lang-check` (`check_derives` arity/arg validation + trait registry arity), `lang-compiler` (generic
codegen for the chosen trait), conformance `tests/conformance/traits/` (a generic derive used +
arity/unknown-arg errors, differential), checker units, docs (`02-syntax.md` derives section).

## Verification & sequencing

Conformance + differential (0-skipped, agree), workspace/clippy/fmt/miri. Independent of the other
three plans. Standard trailers. **Blocked on the consumer-trait decision above.**
