# Slice S5: Trait coherence — overlap/uniqueness (E0027)

Status: **done** (conformance 163 / differential 157 matched, 0 skipped, backends agree). Branch `types-inferred-static`.

Trait coherence makes bound satisfaction and dispatch **decidable and unambiguous**: every `(type, trait)` pair must have exactly one implementation. This is the precondition the `type-system-direction` memory records ("needs trait coherence so bound satisfaction is decidable at compile time") for `satisfies` to be a total, single-answer function rather than a "pick one of several competing impls" guess.

## The orphan half is structural — there is nothing to check

The classic orphan rule (you may implement a trait for a type only if you own the trait *or* the type) is **unrepresentable** in this language, so it needs no enforcement pass:

- **Every trait is built-in.** There are no user-defined traits — `impl`/`@derive` names are validated against the fixed `BuiltinTrait` registry (`E0014`). A trait is therefore always "local".
- **`impl` blocks live only inside a class body.** There is no free-standing `impl Trait for ForeignType`. The type an `impl` block applies to is always the class that lexically encloses it — i.e. the type you are declaring and own. You cannot write an impl for a type you imported.

So a coherence *violation* of the orphan kind cannot be spelled. The only coherence rule with teeth here is the **overlap/uniqueness** half.

## The overlap/uniqueness rule (what S5 enforces) — E0027

A trait may be implemented **at most once** per type, counting both implementation sources:

1. a `@derive(T)` directive (the compiler synthesizes the impl from the type's fields), and
2. an `impl T { }` block (a hand-written impl; classes only).

A second implementation of an already-implemented trait is **`E0027 ConflictingTraitImpl`**, reported at the later occurrence with a help line naming that it is already implemented above. The three concrete shapes, all one code:

- `@derive(Comparable, Comparable)` — a trait named twice in `@derive(...)` (covered by `conflicting_derive.lang`).
- `@derive(Display)` **and** `impl Display { }` — derive + hand-written impl of one trait; which `to_string` wins is ambiguous (covered by `conflicting_derive_impl.lang`).
- two `impl Add { }` blocks — duplicate hand-written impls (covered by `conflicting_impl_blocks.lang`).

## Mechanism

A single front-end pass, `Checker::check_coherence(derives, impls)`, called from `check_record` / `check_class` / `check_enum` (records and enums pass an empty `impls` slice — they carry no `impl` blocks). It walks the implementation occurrences in **source order** — `@derive(...)` directives lead the declaration, `impl` blocks sit in the body, so derives-then-impls is exactly textual order — keeping a `HashMap<&str, Span>` of first occurrences. The first time a trait name reappears, it emits `E0027` at the reappearance. Pointing at the *second* occurrence (and naming the first in the help) matches the existing diagnostic style (`@derive(Add)` → `E0014` at the trait-name span).

This is a pure front-end check: no AST, runtime, bytecode, or VM change. The checker is shared, so the rejection is identical on both backends and the differential holds by construction.

## Diagnostics

- New: **`E0027 ConflictingTraitImpl`** — a type implements the same trait more than once.
- Append-only note: this consumes **E0027**; the next free code is **E0028** (reserved for S6 `dyn` narrowing if it needs one).

## Why this is safe / corpus impact

`satisfies` already treated `trait_impls[type]` as a set (it answers "does this type implement T at all"), so single-vs-multiple impl never changed an *accept*; S5 only adds *rejections* for programs that wrote a redundant/competing impl. No corpus program did, so the existing corpus stays green and the new conformance cases are all error cases.

## Oracle posture

The checker is shared by both backends, so every new static rejection is identical on both and `--differential` stays at **0 skipped** (an `E0027`-rejected program produces the same compile-error verdict on each backend, so it counts as *matched*, not skipped). Baseline at S5 start: conformance 160 / differential 154 matched / 0 skipped. After S5: **163 / 157 / 0**.

## Verification (before commit)

- `cargo run -q -p lang-cli -- test` → 163 passed, 0 failed.
- `cargo run -q -p lang-cli -- test --differential` → 157 matched / 0 skipped / backends agree.
- `cargo test --workspace` → green (4 new `lang-check` unit tests: duplicate derive, derive+impl, two impl blocks, and a coherent distinct-traits control).
- `cargo clippy --all-targets` + `cargo fmt --all --check` → clean.
