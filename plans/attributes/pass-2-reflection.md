# Attribute system — Pass 2: runtime reflection (the full read-back + invocation story)

Status: **planned** (not started). Branch `types-inferred-static`. Follows pass 1 (`plans/attributes/README.md`).

Pass 1 made attributes *typed, validated, and captured in a manifest* — but the manifest is **write-only**: nothing reads it back, and it sits *outside* the differential oracle (it isn't in `RunResult`). Pass 2 is the whole reflection story §9.13 describes — introspection (`attributes_of`, `type_of`), fallible by-name invocation, placement constraints, semantic roles, and the capability-gating that keeps it all closed-world — built as a sequence of green-commit slices. The first read-back slice already brings the manifest **under the differential oracle**.

## Decisions locked (from the design discussion)

1. **`attributes_of::<T>()` returns materialized attribute records, paired with their target** — `List<Attributed<T>>` where `Attributed<T> { target: string, value: T }` and `value` is a real `Route { … }` instance built from the manifest. (Faithful to §9.13 "an attribute *is* a value"; feasible because an attribute's type is source-declared, so its shape exists. The routing use case `register(a.value.path, a.target)` works directly.)
2. **`type_of(value)` fidelity = A + B:** **(A)** statically-concrete arg → compiler emits the precise recursive `Type` constant (full fidelity, "the *same* `Type` the checker uses"); **(B)** `dyn`/union arg → runtime head-constructor, composing with `is`/`.as<T>()` for type-safe use.
3. **"Construct" is not a capability** — a constructor is only a convention (an associated fn returning `Self`), so the invocation tier is one fallible primitive: `value.method(args)` / `Type.assoc_fn(args)` by name.
4. **`@reflectable` / capability-gating is the closed-world story for DCE** — the static-resolution design (2A) makes the reflected-upon set statically known, so the tree-shaking-roots set falls out for free. This whole piece is **folded into the DCE/AOT milestone** (§9.8), where the tree-shaker that consumes the root-set actually exists — see *Folded into prerequisite milestones*.

## Constraint guardrails (from §9.13 / §9.8.1 — every slice honors)

- **Closed-world.** Reflection only ever touches types that already exist; never needs the compiler at runtime; never blocks tree-shaking.
- **Introspection ≠ invocation.** Read-only introspection and fallible (`Result`) by-name invocation are distinct slices.
- **`type_of` returns the checker's `Type`** — a prelude `Type` ADT mirroring the lattice, pattern-matchable; not a parallel hierarchy.
- **Differential-clean by construction.** Both backends construct reflection values identically (the `Ordering` precedent — shapes compare structurally by name+variant).
- **No infinite regress / attributes stay inert data.** `attributes_of` returns instances; it never reifies the manifest structure itself.

## Structural foundation (parity)

The manifest lives **only** on the VM-side `Module`; the tree-walker has none. Both backends already share the front-end (`Program` AST via `lang-loader`/`lang-db`). So the foundation is a **shared reflection artifact** built once from the AST and read by both backends — single source of truth, no parity drift.

## Slices (each a green commit; ordered by dependency)

### P2.0 — Shared reflection artifact (parity foundation; no user-facing API)
A shared `ReflectionInfo` built at/after check-time from the AST: the attribute **manifest** (builder lifted out of `lang-compiler` so both backends use it) + a **type registry** (every declared record/class/enum → kind, ordered fields, variants). Attached to `Linked` + a `lang-db` salsa query; both `lang-eval` and the VM `Module` consume the *same* artifact. Verified by unit tests (both backends see identical info); differential unchanged (pure infrastructure).

### P2.1 — `attributes_of::<T>()` → materialized `List<Attributed<T>>`
Prelude function with turbofish type-arg `attributes_of::<T>()` (new parse surface; `<T>` a `TypeRef` resolved at compile time — closed-world, reusing the `.as<T>()` threading). Gated on `T: Attribute` (reuse pass-1 E0029). Introduce the built-in generic record `Attributed<T> { target: string, value: T }` (alongside `Option`/`Result` as a prelude generic). Mechanism: collect manifest entries for attribute-type `T`; for each, materialize a `T` record from its stored args (the positional+named→field mapping A3 already validated) and pair it with its annotated declaration's name. Needs a runtime **build-record-by-type-name-and-values** path in both backends (factor `eval_object`; name→interned-shape build in the VM). **Closes the oracle gap** — both backends print identical materialized attribute data.

### P2.2 — the `Type` ADT + runtime `type_of` (fidelity B) — **DONE**
Register a **prelude enum `Type`** in both backends + checker (the `Ordering`/`PRELUDE_TYPES` template), mirroring the lattice: `Int Float Bool String Unit Dyn`, `List(Type) Map(Type,Type) Set(Type) Option(Type) Result(Type,Type)`, `Named(name, args: List<Type>)`, `Fn(params: List<Type>, ret: Type)`, `Union(members: List<Type>)`, `Record/Class/Enum(name, members…)`. (Recursive ADT — confirm a variant may carry its own enum type.) One runtime op/builtin (both backends) builds the head-constructor `Type` for a value (`List(Dyn)`, `Named("Route")`). Conformance: `type_of` + `match` on int/string/list/record/`dyn`, identical both backends.

**As built:** `type_of(value)` is a **keyword** (mirroring `attributes_of`) parsing to `Expr::TypeOf`, lowering to one new op `Op::TypeOf { dst, src }` in **both** backends (no skips). The `Type` enum has **14 variants** — `Named(name: string, args: List<Type>)` subsumes the tentative `Record/Class/Enum` split (faithful to the lattice, which has only `Named`). The shared vocabulary is `lang_ast::reflect::TypeRepr` (+ `TYPE_ENUM`, `TypeRepr::variant_name`): each backend classifies its native value into a `TypeRepr` (`vm_type_repr`/`eval_type_repr`, both keyed on the public `type_name()` kind order) and builds the enum from it (`build_type_value`), so the reflected value is structurally identical by construction (the `Ordering` precedent). Matchability needs **no compiler-side enum registry** — `Op::MatchVariant` matches by shape-name+variant+arity at runtime; the checker registers `Type` in `register_prelude`/`register_type_enum` (+ `PRELUDE_TYPES`) so arms type-check and payload bindings carry `Type`. Fidelity B = runtime head-constructor (element/arg types erased to `Dyn`); the compile-time full-fidelity path (P2.3) will build a precise `TypeRepr` from the checker's inferred type and reuse `build_type_value`. No new diagnostic code (next free still **E0030**). Conformance 187 / differential 181 / 0-skipped.

### P2.3 — `type_of` compile-time full fidelity (fidelity A) — **DONE**
When the checker knows the argument's **concrete** static type, emit the precise recursive `Type` constant (`List<int>` → `Type.List(Type.Int)`); fall back to P2.2's runtime path only for `dyn`/union. Cost: thread the checker's inferred type to the backends at `type_of` call sites (a resolved-type table keyed by call-site span, or a check-time rewrite carrying the resolved `Type`) — scoped to `type_of` args, not a full per-expression map. Conformance: `List<int>` → full depth; a `dyn`-held list → `List(Dyn)`; identical both backends.

**As built (the first checker→backend type channel).** The checker harvests, per `type_of` site (keyed by `Expr::TypeOf` span), the operand's concrete static type as a `reflect::TypeRepr`, exposed by a new pure entry point **`lang_check::resolve_type_of_sites(program) -> HashMap<Span, TypeRepr>`** (`type_to_repr_top` returns `None` for a `dyn`/union/`Unknown` top type → that site stays on the runtime path; `type_to_repr` totalizes nested holes/unions to `Dyn`, since runtime erases generics anyway). **Both backends call `resolve_type_of_sites` on the same program** (the parity mechanism — like `reflect::build`): the compiler stores it on `ModuleCompiler.type_of_sites`, the tree-walker on `Interpreter.type_of_sites`. Lowering an `Expr::TypeOf` now branches: a site present in the map → new op **`Op::TypeOfStatic { dst, repr }`** (VM builds the constant via the existing `build_type_value`) / the tree-walker builds it directly; absent → P2.2's runtime `Op::TypeOf` / `eval_type_repr`. **The operand is still evaluated in both fidelities** (for side effects); only its classification differs. This adds a **production dependency `lang-compiler`→`lang-check` and `lang-eval`→`lang-check`** (no cycle — `lang-check` depends on neither); the differential holds because both backends harvest identical maps from one program. No new diagnostic code. **PERF (deferred):** compile/eval now re-run the checker internally — the perf milestone should thread a single shared check artifact instead of three checker passes. Conformance 188 / differential 182 / 0-skipped / backends agree.

### P2.4 — attributes on functions/methods (expand targets) — **DONE**
Today `#[...]` attaches only to type decls. Extend the parser/AST/manifest/checker so attributes attach to `fn`/method declarations too (prerequisite for method-routing and `AttachableTo`'s `valid_target`). `attributes_of` now surfaces method targets. Conformance + differential.

**As built:** `FnDecl` gained an `attrs: Vec<Attribute>` field. The parser's attribute cluster (`attr_value`/`attr_arg`/`attr_decl`, the last yielding a bare `Attribute`) was lifted **above** `fn_decl` so both `fn_decl` and `method` can take leading `#[...]`; the type-decl path now reuses `attr_decl` via `let attribute = attr_decl.map(Decorator::Attr)`. **`@derive` stays type-only** — `fn_decl`/`method` accept only `#[...]`, so `@derive fn …` fails the grammar (correct: derive is codegen for types). `reflect::build` walks `Stmt::Fn` (keyed by bare fn name) and each class method (keyed **qualified `Class.method`** so same-named methods across classes stay distinct); a function contributes to the **manifest only**, not the type registry. The checker calls `check_attrs(&decl.attrs)` in `check_fn`, so the E0029 capability gate + the all-fields construction check (E0009/E0007/E0005) reach functions and methods (incl. `impl`-block methods, which flatten through `method`). `attributes_of::<T>()` surfaces fn/method targets automatically (no query change — they are ordinary manifest entries). No new diagnostic code, no runtime/backend change (attributes are compile-time manifest data). Conformance 190 / differential 184 / 0-skipped / backends agree.

### P2.4b — attributes on fields/properties (completes target expansion) — **DONE**
Target expansion isn't complete without **fields/properties** — the most common attribute target (validation `#[Required]`/`#[MaxLength]`, serialization `#[JsonName]`, ORM `#[Column]`). A clean mirror of P2.4, and a prerequisite for P2.5 (whose `valid_target` must distinguish target *kinds* — type vs method vs field). `FieldDecl` gained `attrs: Vec<Attribute>`; `record_field` and `class_field` take leading `#[...]` (reusing the same `attr_decl`; class body still disambiguates field-vs-method by the token after the attributes). `reflect::build` pushes each field's attrs via `push_field_attrs`, keyed by qualified **`Type.field`** (mirroring `Type.method`), for both records and classes. The checker validates field attrs in the field loops of `check_record`/`check_class` (E0029 gate + construction check). No new diagnostic code, no backend/runtime change. Conformance 191 / differential 185 / 0-skipped / backends agree.

### P2.4c — attributes on enum variants (final declaration site) — **DONE**
The last declaration site, completing the target set (types, functions, methods, fields, **variants**). `VariantDecl` gained `attrs: Vec<Attribute>`; the `variant` parser takes leading `#[...]` (reusing `attr_decl`); plain, algebraic, and backed variants all accept them. `reflect::build` keys variant attrs by qualified **`Enum.Variant`** (mirroring `Type.field`/`Type.method`); the checker validates them in `check_enum`'s variant loop. No new diagnostic code, no backend/runtime change. **Every declaration site can now be annotated.** Conformance 192 / differential 186 / 0-skipped / backends agree.

### P2.5 — `AttachableTo` / `valid_target` (placement constraints)
An attribute record may `impl AttachableTo { fn valid_target(t: Target): bool }` constraining *where* it attaches; the checker enforces it at the use site (new **E0030**). Introduce a `Target` value the predicate inspects (kind: type/method, return type, …). Conformance: a `#[Route]` restricted to methods returning `Response`, misuse → E0030.

### P2.6 — by-name invocation (the fallible primitive)
The single invocation primitive: `value.method(args)` / `Type.assoc_fn(args)` resolved by name at runtime, returning `Result` (miss on name/arity → `Err`). Needs a **first-class type value in the VM** (the tree-walker already has `Value::Type`; the VM compiles associated calls statically) + a new dynamic-dispatch op in both backends. Reuses the existing method/associated-fn tables. Conformance: dynamic call succeeds vs. returns `Err`, identical both backends.

### P2.7 — `SemanticRole` (labeled dependency graph)
An attribute may `impl SemanticRole { fn role(): Role }`; the compiler evaluates `role()` at manifest-build time and records a `(declaration, Role)` index beside the `(declaration, attribute)` one. Introduce the `Role` prelude enum (EntryPoint/PersistenceBoundary/TrustBoundary/Sink/Layer/Custom…). Expose the index to user code (and, later, to MCP/lints — external). Conformance + manifest unit tests.

## Folded into prerequisite milestones (not pass-2 slices)

Two pieces of the reflection story have hard structural prerequisites that live in other milestones, so they are built **there**, as part of the work that unblocks them — not as orphaned slices here. Recorded in `plans/deferred.md` against their host milestones.

- **Capability-gating + `@reflectable` tree-shaking roots** → folded into the **DCE / AOT compile-mode milestone** (§9.8). The reflected-upon root-set is only meaningful to a tree-shaker, which is exactly what that milestone builds; the gating is implemented where the eliminator that consumes it lives. (Reflection behaves identically gated or not — this is a binary-size optimization, invisible to semantics.)
- **Cross-`dyn` element-type recovery ("C")** — recovering `List<int>`'s `int` *after* it crossed a `dyn` boundary — → folded into the **reified-generics / packed-value-types milestone** (M2, §3.1). It requires type arguments carried in shapes at runtime ("generics fall out of shapes"), which is precisely that milestone's mechanism; the reflection refinement rides along with it.

Until those milestones land, pass 2 delivers: full-fidelity `type_of` for everything statically known (P2.3), head-constructor + narrowing for genuine `dyn` (P2.2), and ungated (all-resident) metadata — correct in every interpreted/dev mode, where there is no tree-shaker to gate against.

## Out of scope (genuinely external, not ours to build here)
- MCP/agentic-tooling and LSP **consumption** of the manifest/role index (we expose the index; the tools that query it are separate).
- `eval` / open-world dynamism (the one feature that forfeits closed-world; explicitly opt-in, separate, and not part of reflection).
- User-defined derives (no comptime — out of scope per §9.13).

## Diagnostics
Codes assigned per slice: `attributes_of` reuses **E0029**; `AttachableTo` placement violation is **E0030** (P2.5); by-name invocation failures are runtime `Result::Err`, not static codes. Next free after this pass: **E0031**.
