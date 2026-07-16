# L1 — User-defined traits (design)

**Why:** typed `Controller` / `Middleware` / `ServiceProvider` / `Resource` contracts for aether, plus
polymorphic collections (`List<dyn Middleware>`). Today traits are a closed built-in set (18 variants,
one required method each, no `trait` keyword, no dynamic dispatch). Build it right.

## Grounding facts (from the machinery map)

- No `trait` keyword / trait-decl AST node. Lexer has only `ImplKw`/`ForKw`.
- `BuiltinTrait::from_name` (`noeta-types/src/traits.rs:128`) is the closed-set boundary, consulted at
  ~15 sites (E0014 paths, `record_trait_impls` filter `check lib.rs:6092`, bound checks `:6594`/`:6658`,
  both backends' operator dispatch). `record_trait_impls` **silently drops** non-built-in names — trap.
- A built-in trait has exactly ONE required method (`required_method()`). User traits need a method LIST.
- **No dynamic dispatch anywhere.** `dyn` = top type (`Type::Dyn`), not `dyn Trait`.
- **KEY LEVER:** both backends already resolve `recv.method(args)` and `invoke(recv,"m",args)` by the
  value's *runtime type name* against a per-type method table (eval `TypeDef.methods:HashMap<String,Closure>`;
  VM `methods:HashMap<(type,method),proto>`). So dynamic dispatch through a trait object reuses this —
  **no new vtable value representation required.**
- Standalone `impl Trait for T { methods }` is unimplemented (E0015 at `check lib.rs:3576`); in-body
  `impl Trait { methods }` already flattens methods into the type's table at parse.
- Two backends must stay byte-identical (differential + session-parity oracle).

## Surface syntax

```noe
trait Middleware {
    fn handle(req: Request, next: Handler): Response
}

trait ServiceProvider {
    fn register(app: App): void
    fn boot(app: App): void { }          // default method (UT5, optional)
}

class Logger {
    impl Middleware {                    // in-body impl
        fn handle(req: Request, next: Handler): Response { ... }
    }
}

impl Middleware for Cors { ... }          // standalone impl (lift E0015 for user traits)

fn use_one<M: Middleware>(m: M) { ... }    // generic bound
pipeline: List<dyn Middleware> = [...]     // trait object collection (UT4)
for m in pipeline { m.handle(req, next) } // dynamic dispatch by runtime type
```

Explicit `dyn Trait` (not bare `Middleware` as a type) — consistent with Noeta's explicit-`dyn` stance.

## Data model

New `TraitDef { name, type_params: Vec<TypeParam>, methods: Vec<TraitMethodSig>, span }` where
`TraitMethodSig { name, params: Vec<Type>, ret: Type, default: Option<FnBody>, span }`.
Program-level `user_traits: HashMap<String, TraitDef>` in the checker, alongside `BuiltinTrait`.
A resolution helper `resolve_trait(name) -> Trait { Builtin(BuiltinTrait) | User(&TraitDef) }` replaces
bare `from_name` at the sites that must also see user traits (registration, bound checks, impl checks).

New type: `Type::DynTrait(String)` (trait name), spelled `dyn <Trait>`; `TypeRef` parses it.

## Status: UT1–UT4 DONE ✅ (UT5 deferred)

- **UT1** `30a173d7` — `trait` decl + E0053. **UT2** `4b3e4070` — impl + dispatch (idempotent
  `hoist_standalone_impl_methods` in IR lowering + VM surface pass). **UT3** `3d4317ea` — `<T: Trait>`
  bounds (E0025/E0014), method call on bounded param. **UT4** `8a58bcf5` — `dyn Trait` objects
  (checker+parser only; runtime dispatch already worked). Corpus 620/620, differential green.
- **UT5 (default-method fallback) DEFERRED**: an omitted default method isn't hoisted onto the
  implementor, so calling it fails at runtime; conformance allows omission. Do when the framework
  needs a real default (e.g. `ServiceProvider.boot()` no-op).
- **Gotcha:** rebuild `noeta-conformance` after any parser/checker change (it embeds them) — a stale
  binary silently reports old behavior.

## Slices (each green + committed; differential oracle per slice)

- **UT1 — declare.** `TraitKw` lexer token; `TraitDecl` AST + `Stmt::Trait`; parser (`trait Name { sigs }`,
  method sigs with optional default body); checker registers into `user_traits`; duplicate/empty/invalid
  → new E-code. Trait names reserved against type names (coherence). *Test:* a trait decl compiles; a
  malformed one errors. No behavior yet.
- **UT2 — impl + call.** Accept a user-trait name in in-body `impl` and standalone `impl Trait for T`.
  Conformance check: every non-default trait method present with matching arity/param/ret (reuse E0015/
  E0025). Wire standalone-impl methods into T's runtime method table in BOTH backends (mirror the
  in-body flatten; keyed by target type name). *Test:* define trait, impl for a type, call method on an
  instance → runs identically on both backends.
- **UT3 — generic bounds.** `<T: UserTrait>`: extend `satisfies` / `check_type_param_bounds` / call-site
  enforcement (`check lib.rs:6587`, `6656`) to consult `user_traits` + recorded impls. *Test:* bounded
  generic accepts an impl type, rejects a non-impl type (E0025).
- **UT4 — trait objects.** `Type::DynTrait`; a value coerces to `dyn Trait` iff its type impls the trait;
  method call on a `dyn Trait` receiver type-checks against the trait's method sigs and lowers to
  **runtime by-name dispatch** (reuse the invoke/method-lookup path) in both backends. `List<dyn T>`
  heterogeneous. *Test:* heterogeneous list, iterate + call → each concrete impl runs; a missing method
  is impossible by construction (coercion gate).
- **UT5 — default methods (optional).** A trait method body used when an impl omits it. Defer if needed.

## Riskiest points (watch)
1. Every `from_name` site must consult the user registry or intentionally not (operators stay builtin-only).
2. Keep eval + VM method-table wiring in lockstep for standalone-impl flatten (UT2) and dyn dispatch (UT4).
3. Coherence: one impl per (type, trait); trait vs type name collisions.
4. `dyn Trait` coercion is the soundness gate for UT4 — a value only becomes `dyn Trait` if it provably
   impls it, so runtime by-name dispatch can never miss.
