//! **The declared-constraint gate**: every field of an extension-facing ABI declaration type is
//! classified here, and the classification is checked against the tree.
//!
//! ## The bug class
//!
//! Three times a field of one of these types shipped *declaring a constraint* that nothing
//! enforced:
//!
//! 1. [`ExtTier::sites`](noeta_ext_abi::registry::ExtTier::sites) — "which declarations this tier
//!    may attach to", read by no one.
//! 2. [`ExtDirective::max_args`](noeta_ext_abi::registry::ExtDirective::max_args) and
//!    `named_keys` — the same failure, in code written *during the arc that fixed the first one*.
//! 3. The argument-contract diagnostics that closed (2) then had no test, because the conformance
//!    corpus structurally cannot reach them: the corpus runs programs against **std**, and no std
//!    declaration sets those fields. "The corpus is green" was silent about that code.
//!
//! A declared-but-unenforced constraint is worse than no constraint — it tells an extension author
//! the compiler checks their contract when nothing does. Patching each instance as it is discovered
//! guarantees a fourth, so this file makes the property structural rather than remembered.
//!
//! ## What the gate does
//!
//! [`TABLE`] classifies **every** `pub` field of every ABI declaration type as one of:
//!
//! - [`Verdict::Constraint`] — the field states a RULE. It must name a live **enforcer** (the
//!   function that acts on it) *and* a live **exerciser** (the test or corpus case that watches the
//!   rule fire). This is the class that has failed three times.
//! - [`Verdict::Data`] — the field is material the compiler consumes to build, type, or dispatch
//!   something. Wrong data yields a wrong program, not a false promise, and the ordinary tests
//!   notice. It must still name a reader: a field nothing reads at all is a bug whatever its kind.
//! - [`Verdict::Prose`] — human-facing text (hover docs, help strings). Nothing can enforce it; the
//!   reason is recorded so "no enforcement" is a decision rather than an omission.
//!
//! Two properties are then checked:
//!
//! - **Completeness** — the type list is parsed out of the ABI sources and every `pub` field must
//!   appear in `TABLE` exactly once. Adding a field to `ExtDirective` fails this test until it is
//!   classified. This is the half that would have caught all three historical instances *at the
//!   commit that introduced them*, which is the whole point: it fires on the ADD, not on the
//!   eventual discovery. (The trick is borrowed from `noeta_diagnostics`'s `all_list_guard`, which
//!   keeps its hand-maintained `ALL` honest by counting variants out of its own source.)
//! - **Liveness** — each named enforcer/reader/exerciser anchor must still exist: the file must be
//!   present, must contain the anchor's needle, and (for enforcers and readers) must mention the
//!   field name. Deleting the reader, or renaming the function around it, fails here.
//!
//! ## What it deliberately does not do
//!
//! It does not try to *infer* whether a field is read, by grepping for `.field` across the
//! workspace. Field names like `name`, `params`, `fields` and `sites` collide with unrelated
//! structs in every crate, so an inferring gate would answer "yes, read" for a field nothing reads
//! — a false negative on exactly the case it exists to catch. Naming the enforcing site is more
//! work per field and vastly more precise, and it makes the audit itself machine-checked instead of
//! prose in a commit message.
//!
//! The cost is honest: this gate reads source text, so renaming an anchored function fails it and
//! someone must re-point the anchor. That is a deliberate trade — the failure is loud, local, and
//! forces a human to re-confirm the constraint is still enforced, which is the property at stake.

use std::path::{Path, PathBuf};

/// A named site in the tree that must still exist: `needle` is a substring stable across ordinary
/// edits (a function signature, a test name, a corpus expectation), not a line number.
struct Anchor(&'static str, &'static str);

impl Anchor {
    /// The workspace-relative source file this anchor points into.
    fn file(&self) -> &'static str {
        self.0
    }
    /// A substring that must still be present there.
    fn needle(&self) -> &'static str {
        self.1
    }
}

enum Verdict {
    /// The field states a rule about programs or about the declaring extension. Needs an enforcer
    /// and an exerciser.
    Constraint(Anchor, Anchor),
    /// The field is material the compiler consumes. Needs a reader.
    Data(Anchor),
    /// Human-facing text: the string records why nothing enforces it.
    Prose(&'static str),
}

struct Row(&'static str, &'static str, Verdict);

impl Row {
    /// The declaring ABI type.
    fn ty(&self) -> &'static str {
        self.0
    }
    /// The field on it.
    fn field(&self) -> &'static str {
        self.1
    }
    fn verdict(&self) -> &Verdict {
        &self.2
    }
}

const CHECK_DIRECTIVES: &str = "crates/noeta-check/src/directives.rs";
const CHECK_STDLIB: &str = "crates/noeta-check/src/stdlib.rs";
const CHECK_ARGS: &str = "crates/noeta-check/src/args.rs";
const CHECK_PRELUDE: &str = "crates/noeta-check/src/prelude.rs";
const EVAL_LIB: &str = "crates/noeta-eval/src/lib.rs";
const CHECK_TRAITS: &str = "crates/noeta-check/src/traits.rs";
const CHECK_TIERS: &str = "crates/noeta-check/src/tiers.rs";
/// The lazy **native-reflection** seam: the one place a registry declaration becomes reflection
/// data, moved out of `noeta-check::tiers` when it stopped being materialized eagerly into every
/// compiled artifact.
const AST_NATIVE_REFLECT: &str = "crates/noeta-ast/src/native_reflect.rs";
const REGISTRY: &str = "crates/noeta-ext-abi/src/registry.rs";
const STDLIB_JSON: &str = "crates/noeta-stdlib/src/json.rs";
const VM_NATIVE_CTX: &str = "crates/noeta-vm/src/native_ctx.rs";
const CLI_SERVE: &str = "crates/noeta-cli/src/cmd/serve.rs";
const CLI_LIB: &str = "crates/noeta-cli/src/lib.rs";
const EMBED_CONSTRAINTS: &str = "crates/noeta-embed/tests/ext_constraint_enforcement.rs";
const EMBED_INSTANCE: &str = "crates/noeta-embed/tests/instance_registry.rs";
const CONFORMANCE_STRUCT_SEAM: &str = "crates/noeta-conformance/tests/ext_struct_seam.rs";
const CONFORMANCE_TRAIT_SEAM: &str = "crates/noeta-conformance/tests/ext_trait_seam.rs";
const CONFORMANCE_ASSOC_SEAM: &str = "crates/noeta-conformance/tests/ext_assoc_seam.rs";
const CONFORMANCE_TRAIT_DEFAULT_SEAM: &str =
    "crates/noeta-conformance/tests/ext_trait_default_seam.rs";
const CONFORMANCE_DIRECTIVE_SEAM: &str = "crates/noeta-conformance/tests/ext_directive_seam.rs";
const LOADER_EXPAND: &str = "crates/noeta-loader/src/expand.rs";

use Verdict::{Constraint, Data, Prose};

/// The classification. One row per `pub` field of every ABI declaration type; the completeness
/// check below keeps it exhaustive.
const TABLE: &[Row] = &[
    // --- FieldRecipe: one field of a call-site struct recipe --------------------------------------
    Row(
        "FieldRecipe",
        "name",
        Data(Anchor(STDLIB_JSON, "fn decode(")),
    ),
    Row(
        "FieldRecipe",
        "recipe",
        Data(Anchor(STDLIB_JSON, "fn decode(")),
    ),
    // Optionality is a rule about every document decoded into the type: an absent field is filled
    // (a `?T` is `none`, a literal default is that default) or it is a missing-field error.
    Row(
        "FieldRecipe",
        "default",
        Constraint(
            Anchor(STDLIB_JSON, "fn fill_absent_field("),
            Anchor(
                "tests/conformance/di/json_field_defaults.noe",
                "its default is not a literal",
            ),
        ),
    ),
    // --- ExtFn: one native function's static signature -------------------------------------------
    Row(
        "ExtFn",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_function(")),
    ),
    // Arity and argument types are a rule about every call site: a `SigType::Optional` marks
    // where the required run stops, and calling with fewer is E0009.
    Row(
        "ExtFn",
        "params",
        Constraint(
            Anchor(CHECK_STDLIB, "fn module_required("),
            Anchor(
                "tests/conformance/diagnostics/optional_param_arity.noe",
                "expect: error E0007",
            ),
        ),
    ),
    // Declaring names is what opts a native function into NAMED arguments, and each name then
    // states a rule about every call site: `name:` binds to the parameter it names (so the call may
    // reorder), and a label naming no parameter is E0061 rather than a silently discarded one.
    // Declaring none is the opt-out, and a label on such a callee is refused by the same code.
    Row(
        "ExtFn",
        "param_names",
        Constraint(
            Anchor(CHECK_ARGS, "fn bind_sig_args("),
            Anchor(
                "tests/conformance/functions/named_arguments_native.noe",
                "expect: error E0061",
            ),
        ),
    ),
    Row(
        "ExtFn",
        "ret",
        Data(Anchor(CHECK_STDLIB, "fn module_return(")),
    ),
    // --- ExtModule ------------------------------------------------------------------------------
    Row(
        "ExtModule",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_module(")),
    ),
    Row(
        "ExtModule",
        "functions",
        Data(Anchor(REGISTRY, "pub fn find_function(")),
    ),
    Row(
        "ExtModule",
        "dispatch",
        Data(Anchor(REGISTRY, "pub fn dispatch(")),
    ),
    Row(
        "ExtModule",
        "deep_marshal",
        Data(Anchor("crates/noeta-eval/src/ir.rs", "deep_marshal")),
    ),
    Row(
        "ExtModule",
        "ctx_functions",
        Data(Anchor(REGISTRY, "pub fn dispatch_ctx(")),
    ),
    Row(
        "ExtModule",
        "ctx_dispatch",
        Data(Anchor(REGISTRY, "pub fn dispatch_ctx(")),
    ),
    Row(
        "ExtModule",
        "ring",
        Data(Anchor(REGISTRY, "pub fn ring_of(")),
    ),
    Row(
        "ExtModule",
        "docs",
        Prose("per-function markdown for the docs browser; rendered, never checked"),
    ),
    Row(
        "ExtModule",
        "typed_functions",
        Data(Anchor(REGISTRY, "fn find_typed_function")),
    ),
    Row(
        "ExtModule",
        "typed_dispatch",
        Data(Anchor(REGISTRY, "typed_dispatch")),
    ),
    // --- ExtType --------------------------------------------------------------------------------
    Row(
        "ExtType",
        "name",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtType",
        "namespace",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtType",
        "methods",
        Data(Anchor(REGISTRY, "pub fn dispatch_method(")),
    ),
    Row(
        "ExtType",
        "dispatch",
        Data(Anchor(REGISTRY, "pub fn dispatch_method(")),
    ),
    // "May values of this type key a Map / member a Set" — a rule the checker applies to every
    // `Map<T, _>` annotation, and a promise the ABI's own debug verifier holds the author to.
    Row(
        "ExtType",
        "key_capable",
        Constraint(
            Anchor("crates/noeta-check/src/decls.rs", "fn named_key_capable("),
            Anchor("tests/conformance/types/map_key_not_capable.noe", "expect:"),
        ),
    ),
    Row(
        "ExtType",
        "ctx_methods",
        Data(Anchor(REGISTRY, "pub fn dispatch_ctx_method(")),
    ),
    Row(
        "ExtType",
        "ctx_dispatch",
        Data(Anchor(REGISTRY, "pub fn dispatch_ctx_method(")),
    ),
    Row(
        "ExtType",
        "arena_getter",
        Data(Anchor("crates/noeta-vm/src/methods.rs", "arena_getter")),
    ),
    // The traits this type claims: what makes a `T: Comparable` / `T: Error` bound accept it, and
    // (for a native `ExtTrait` name) what makes it satisfy a package-declared trait. A bound is a
    // rule, and the claim is what satisfies it.
    Row(
        "ExtType",
        "traits",
        Constraint(
            Anchor(
                "crates/noeta-check/src/prelude.rs",
                "fn native_declares_builtin_trait(",
            ),
            Anchor("crates/noeta-check/src/tests.rs", "Comparable"),
        ),
    ),
    Row(
        "ExtType",
        "deep_marshal",
        Data(Anchor("crates/noeta-eval/src/lib.rs", "deep_marshal")),
    ),
    Row(
        "ExtType",
        "typed_methods",
        Data(Anchor(CHECK_STDLIB, "fn typed_type_method(")),
    ),
    Row(
        "ExtType",
        "typed_dispatch",
        Data(Anchor(REGISTRY, "pub fn dispatch_typed_method(")),
    ),
    Row(
        "ExtType",
        "docs",
        Prose("per-method markdown for the docs browser; rendered, never checked"),
    ),
    // --- ExtEnum / ExtVariant (native-extensibility S1) ------------------------------------------
    Row(
        "ExtEnum",
        "name",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtEnum",
        "namespace",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtEnum",
        "variants",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_enums(")),
    ),
    // The backing states the RULE the checker enforces on the `.value()` accessor's type: a backed
    // enum's `.value()` is its scalar, a non-backed enum has none. No SHIPPED extension declares a
    // backed enum, so the corpus cannot reach the typing — a fixture extension is the exerciser.
    Row(
        "ExtEnum",
        "backing",
        Constraint(
            Anchor(CHECK_STDLIB, "fn native_enum_backing_type("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn a_backed_ext_enum_value_type_is_enforced(",
            ),
        ),
    ),
    // `methods` (native-extensibility S1 / Slice B): the enum's instance-method signatures. Read by
    // `find_enum_method` (which the checker's `method_return`/`method_params` and both backends' enum
    // method-call arm consult to route a native-enum method call to `dispatch`) — the `ExtFielded`
    // `methods` twin for an enum receiver.
    Row(
        "ExtEnum",
        "methods",
        Data(Anchor(REGISTRY, "pub fn find_enum_method(")),
    ),
    // `dispatch`: the one shared native-enum instance-method dispatch (reusing the neutral
    // `NativeMethodDispatch` seam), invoked by both backends' `call_native_enum_method`
    // (`(en.dispatch)(...)`) and exercised by the `ext_enum_seam` native-enum-method test.
    Row(
        "ExtEnum",
        "dispatch",
        Data(Anchor(EVAL_LIB, "fn call_native_enum_method(")),
    ),
    // `traits` (native-extensibility Slice C): the traits this enum advertises — the `ExtEnum` twin
    // of `ExtType::traits` / `ExtFielded::traits`, uniform across every native kind. A native-trait
    // name is recorded by `seed_ext_traits` into `user_trait_impls[qualified][trait]` so a native
    // enum value coerces to `dyn Trait` and its trait-method call dispatches to native code
    // (`call_native_enum_method`); a built-in name is answered on the lookup by `Checker::has_builtin_trait`.
    // No shipped extension declares a native enum with traits, so the `ext_trait_seam` dynamic-
    // dispatch test (a native `Mode` enum behind `dyn Widget`) is the only exerciser.
    Row(
        "ExtEnum",
        "traits",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_traits("),
            Anchor(
                CONFORMANCE_TRAIT_SEAM,
                "fn native_trait_contract_and_dynamic_dispatch_agree_on_both_backends(",
            ),
        ),
    ),
    // `directives` (native type-declaration unification, Slice D): the built-in directives this enum
    // carries — the `.noe` `Decorators` twin, uniform across native fielded + enum kinds. The only
    // one legal on an enum is `@semantic` (→ `semantic_enums`); `seed_ext_directives` performs the
    // table write and `Registry::validate` refuses a struct/class-only directive here. The
    // `ext_directive_seam` fixture's native `@semantic` enum (usable as a `roles_of` vocabulary, which
    // a non-`@semantic` enum is not) is the exerciser.
    Row(
        "ExtEnum",
        "directives",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_directives("),
            Anchor(
                CONFORMANCE_DIRECTIVE_SEAM,
                "fn native_semantic_enum_is_a_role_vocabulary(",
            ),
        ),
    ),
    Row(
        "ExtVariant",
        "name",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_enums(")),
    ),
    Row(
        "ExtVariant",
        "fields",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_enums(")),
    ),
    // The backing constant `.value()` returns at runtime, read by both backends' accessor.
    Row(
        "ExtVariant",
        "value",
        Data(Anchor(EVAL_LIB, "resolve_enum(&e.enum_name)")),
    ),
    // --- ExtClass / ExtField (native-extensibility S2) ------------------------------------------
    Row(
        "ExtFielded",
        "name",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtFielded",
        "namespace",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtFielded",
        "fields",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_fielded(")),
    ),
    Row(
        "ExtField",
        "name",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_fielded(")),
    ),
    Row(
        "ExtField",
        "ty",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_fielded(")),
    ),
    // `is_public` states the RULE the checker enforces on field access: a private field read/set
    // from outside its class is E0035. `seed_ext_fielded` reads it into `symbols.private_fields`
    // (the enforced table `field_visible`/`report_private_field` consult); no shipped extension
    // declares a native class, so a fixture is the only exerciser. Both directions matter.
    Row(
        "ExtField",
        "is_public",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_fielded("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn a_native_class_field_visibility_is_enforced(",
            ),
        ),
    ),
    // `is_mut` states the RULE the checker enforces on field assignment: writing a non-`mut` field
    // is E0033. `seed_ext_fielded` reads it into `symbols.mut_fields` (the enforced table the
    // field-assignment check consults); a fixture is the only exerciser.
    Row(
        "ExtField",
        "is_mut",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_fielded("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn a_native_class_field_mutability_is_enforced(",
            ),
        ),
    ),
    // `methods` (native-extensibility S3 / Pass 2a): the class's instance-method signatures. Read
    // by `find_class_method` (which the checker's `method_return`/`method_params` and both backends'
    // CallMethod Object arm consult to route a native-class method call to `dispatch`).
    Row(
        "ExtFielded",
        "methods",
        Data(Anchor(REGISTRY, "pub fn find_class_method(")),
    ),
    // `dispatch`: the one shared native-class instance-method dispatch, invoked by both backends'
    // `call_native_class_method` (`(class.dispatch)(...)`).
    Row(
        "ExtFielded",
        "dispatch",
        Data(Anchor(EVAL_LIB, "fn call_native_class_method(")),
    ),
    // `traits` (native-extensibility S3 / Pass 2b): the traits this fielded type advertises. Read by
    // `seed_ext_traits`, which records `user_trait_impls[qualified][trait]` so a native fielded value
    // coerces to `dyn Trait` (the ExtType.traits twin for a fielded receiver).
    Row(
        "ExtFielded",
        "traits",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_traits(")),
    ),
    // `kind` states the RULE distinguishing a reference **class** from a value **struct**: a struct
    // is seeded as `TypeKind::Struct` (value semantics — structural `==`, copy-on-assign) and both
    // backends materialize it with a struct-kind shape (`structural_eq = true`), whereas a class is a
    // reference type with identity + in-place mutation. `seed_ext_fielded` reads `kind` into the
    // seeded `TypeKind` (the enforcer that fixes the checker-side semantics); no shipped extension
    // declares a native struct, so the `ext_struct_seam` value-semantics test is the only exerciser.
    Row(
        "ExtFielded",
        "kind",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_fielded("),
            Anchor(
                CONFORMANCE_STRUCT_SEAM,
                "fn native_structs_have_value_semantics(",
            ),
        ),
    ),
    // `directives` (native type-declaration unification, Slice D): the built-in directives this
    // fielded type carries. On a struct/class the legal one is `@validated` (→ `validated_types`,
    // barring bare literal construction — E0060). `seed_ext_directives` performs the table write and
    // `Registry::validate` refuses `@semantic` (enum-only) here. The `ext_directive_seam` fixture's
    // native `@validated` struct (bare construction is E0060; a recipe door runs its validator) is the
    // exerciser.
    Row(
        "ExtFielded",
        "directives",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_directives("),
            Anchor(
                CONFORMANCE_DIRECTIVE_SEAM,
                "fn native_validated_struct_bars_construction_and_validates_at_a_door(",
            ),
        ),
    ),
    // --- ExtRoleTag (native type-declaration unification, Slice D3) -----------------------------
    // A native `@role` tag inside `ExtTypeDirective::Role`. Both fields state a RULE the enforcer
    // `check_role_tag` (called from `Registry::validate`) checks at assembly — `enum_name` must
    // resolve to a `@semantic` enum, `variant` must exist on it and be fieldless — AND they are the
    // data `Registry::native_roles` projects into `reflect::build`, which materializes each into a
    // `RoleBinding { target, role }`. The seam fixture's native `@attribute` + `@role` struct, applied
    // in a linked module so `roles_of()` surfaces the binding on both backends, is the exerciser.
    Row(
        "ExtRoleTag",
        "enum_name",
        Constraint(
            Anchor(REGISTRY, "fn check_role_tag("),
            Anchor(
                CONFORMANCE_DIRECTIVE_SEAM,
                "fn native_role_binding_surfaces_on_both_backends(",
            ),
        ),
    ),
    Row(
        "ExtRoleTag",
        "variant",
        Constraint(
            Anchor(REGISTRY, "fn check_role_tag("),
            Anchor(
                CONFORMANCE_DIRECTIVE_SEAM,
                "fn native_role_binding_surfaces_on_both_backends(",
            ),
        ),
    ),
    // --- ExtTrait / ExtTraitMethod (native-extensibility S3) ------------------------------------
    Row(
        "ExtTrait",
        "name",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    Row(
        "ExtTrait",
        "namespace",
        Data(Anchor(REGISTRY, "trait NominalType")),
    ),
    // `methods` states the RULE the trait contract enforces on an implementor: every non-default
    // method must be present with matching arity/types or the `impl` is E0015 (`check_user_trait_impl`).
    // `seed_ext_traits` synthesizes a `TraitDecl` from these methods; no shipped extension declares a
    // native trait, so a fixture extension is the only exerciser. Both directions matter (a complete
    // impl checks clean; an incomplete one is rejected).
    Row(
        "ExtTrait",
        "methods",
        Constraint(
            Anchor(CHECK_TRAITS, "fn check_user_trait_impl("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn a_native_trait_incomplete_impl_is_rejected(",
            ),
        ),
    ),
    // `assoc_types` (ExtBundle→ExtTrait convergence, slice 1b): the trait's native-derived associated
    // types. States a RULE — each `Self::Name` a method returns resolves, on a concrete receiver, to
    // the type the implementing type's `AssocDerivation` computes. `seed_ext_traits` reads it and
    // folds every derivation into `trait_assoc[(type, trait)]`; no shipped extension declares a native
    // trait with associated types, so the assoc-seam fixture is the only exerciser.
    Row(
        "ExtTrait",
        "assoc_types",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_traits("),
            Anchor(
                CONFORMANCE_ASSOC_SEAM,
                "fn native_derived_associated_type_resolves_on_both_backends(",
            ),
        ),
    ),
    // `ExtAssocType.name` — the associated type's name, read by `seed_ext_traits` to key
    // `trait_assoc[(type, trait)]` under it (and named from a method sig as `SigType::Assoc(name)`).
    Row(
        "ExtAssocType",
        "name",
        Data(Anchor(CHECK_PRELUDE, "fn seed_ext_traits(")),
    ),
    // `ExtAssocType.derivation` states the RULE for how the associated type is computed from the
    // implementing type's element (`Element`/`Widen`/`FloatPromote`). `seed_ext_traits` applies it
    // (`derivation.apply`) into `trait_assoc`; the assoc-seam fixture's `type Wide` (Widen over an i32
    // element → `int`) watches the rule fire on both backends.
    Row(
        "ExtAssocType",
        "derivation",
        Constraint(
            Anchor(CHECK_PRELUDE, "fn seed_ext_traits("),
            Anchor(
                CONFORMANCE_ASSOC_SEAM,
                "fn native_derived_associated_type_resolves_on_both_backends(",
            ),
        ),
    ),
    // `dispatch` (ExtBundle→ExtTrait convergence, slice 2): the trait's native default-body dispatch.
    // States a RULE — a defaulted method an implementor omits is answered by the TRAIT itself, through
    // this shared ctx dispatch (receiver as slot 0), rather than the receiver's own dispatch. Both
    // backends route it via `Registry::dispatch_trait_method`; no shipped extension declares a native
    // trait with a default dispatch, so the trait-default seam fixture is the only exerciser.
    Row(
        "ExtTrait",
        "dispatch",
        Constraint(
            Anchor(REGISTRY, "pub fn dispatch_trait_method("),
            Anchor(
                CONFORMANCE_TRAIT_DEFAULT_SEAM,
                "fn native_trait_default_bodies_agree_on_both_backends(",
            ),
        ),
    ),
    // `self_constraint` (ExtBundle→ExtTrait convergence, slice 3): the trait's structural `Self`-shape
    // constraint — the third capability a bundle had that a trait lacked. States a RULE: a native trait
    // carrying one may only be `impl`-ed for a `@packed` struct whose fields match the
    // `PackedConstraint`, or the impl is E0015 — the SAME shape check (shared `check_packed_self_constraint`)
    // `check_bundle_binding` runs for a bundle. No shipped extension declares a native trait with a
    // self-constraint, so the constraint-enforcement fixture is the only exerciser; both directions
    // matter (a matching packed struct binds clean, a non-packed / mismatched one is rejected).
    Row(
        "ExtTrait",
        "self_constraint",
        Constraint(
            Anchor(CHECK_TRAITS, "fn check_packed_self_constraint("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn a_native_trait_self_constraint_is_enforced(",
            ),
        ),
    ),
    Row(
        "ExtTraitMethod",
        "sig",
        Data(Anchor(CHECK_PRELUDE, "fn synth_trait_decl(")),
    ),
    // `has_default` decides whether an implementor may omit the method (a defaulted one is optional;
    // a required one absent is E0015). `synth_trait_decl` copies it into the `TraitMethod`, and
    // `check_user_trait_impl` reads it (`if tm.has_default { continue }`).
    Row(
        "ExtTraitMethod",
        "has_default",
        Data(Anchor(CHECK_PRELUDE, "fn synth_trait_decl(")),
    ),
    // `receiver` (ExtBundle→ExtTrait fold-in, slice 4): which receiver carries a kernel-trait method —
    // `Self` (Element) or `List<Self>` (Bulk, the accepted dual-receiver asymmetry). States a RULE
    // about which receiver a method is reachable on: `bundle_method_call` reads it to type a method on
    // a bound `T` vs a `List<T>` of one; calling a Bulk method on an element (or the reverse) is not
    // resolved. No shipped extension declares a kernel trait the corpus cannot reach with a bulk
    // method, so the `kernels_methods.noe` corpus case (which calls both `v.add(w)` and `xs.add_all(ys)`)
    // is the exerciser.
    Row(
        "ExtTraitMethod",
        "receiver",
        Constraint(
            Anchor(
                "crates/noeta-check/src/expr/member.rs",
                "fn bundle_method_call(",
            ),
            Anchor("tests/conformance/bundles/kernels_methods.noe", "expect:"),
        ),
    ),
    // --- PackedConstraint (a kernel trait's structural `Self`-constraint, `ExtTrait::self_constraint`)
    // Since the ExtBundle→ExtTrait fold-in (slice 4) the constraint lives on the trait, not a bundle;
    // it is validated at the `impl vec.Kernels for T {}` site by the same `constraint_mismatch` core.
    Row(
        "PackedConstraint",
        "fields",
        Constraint(
            Anchor("crates/noeta-check/src/subst.rs", "fn constraint_mismatch("),
            Anchor("tests/conformance/bundles/bind_errors.noe", "expect:"),
        ),
    ),
    // Row/Column: no SHIPPED kernel trait declares either (all are `Any`), so the corpus cannot
    // reach the rejecting arms. Covered by a fixture extension instead (a `Column` self-constraint).
    Row(
        "PackedConstraint",
        "layout",
        Constraint(
            Anchor("crates/noeta-check/src/subst.rs", "fn constraint_mismatch("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn a_bundle_layout_constraint_is_enforced_at_the_impl_site(",
            ),
        ),
    ),
    // Exact vs Uniform arity: whether `fields` is matched one-for-one or as "≥min of one kind"
    // (the array-ops integer/`Color` shapes). The impl-site check enforces it; the corpus watches
    // both rejecting arms fire (wrong kind, too few, wrong exact arity).
    Row(
        "PackedConstraint",
        "arity",
        Constraint(
            Anchor("crates/noeta-check/src/subst.rs", "fn constraint_mismatch("),
            Anchor(
                "tests/conformance/bundles/bind_int_color_errors.noe",
                "expect:",
            ),
        ),
    ),
    // --- Extension-declared prelude attributes ---------------------------------------------------
    Row(
        "ExtAttrField",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_ext_attribute(")),
    ),
    // The field's declared literal type: constructing the attribute with a mismatched literal
    // is a static error, exactly as for a program-declared `@attribute` struct.
    Row(
        "ExtAttrField",
        "ty",
        Constraint(
            Anchor("crates/noeta-check/src/subst.rs", "fn attr_field_type("),
            Anchor("tests/conformance/tiers/test_attrs_strip.noe", "expect:"),
        ),
    ),
    // Present ⇒ optional at construction. Absent ⇒ mandatory, and omitting it is an error.
    Row(
        "ExtAttrField",
        "default",
        Constraint(
            Anchor(AST_NATIVE_REFLECT, "AttrFieldDefault::"),
            Anchor("tests/conformance/tiers/test_attrs_strip.noe", "expect:"),
        ),
    ),
    Row(
        "ExtAttribute",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_ext_attribute(")),
    ),
    Row(
        "ExtAttribute",
        "fields",
        Data(Anchor(AST_NATIVE_REFLECT, "ext_attributes()")),
    ),
    // The attribute's namespace is its qualified identity's prefix: it projects through the one
    // `nominal_types` stream as a Struct-kind nominal, which is what makes `use std.test.{Skip}`
    // resolve on the consumer side exactly like a native type (D2).
    Row(
        "ExtAttribute",
        "namespace",
        Data(Anchor(REGISTRY, "let attributes = e.attributes()")),
    ),
    // --- ExtTier: the original instance of this bug class -----------------------------------------
    Row(
        "ExtTier",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_ext_tier(")),
    ),
    // THE original: declared for every std tier, enforced for none, for as long as it existed.
    // Std has no tier that attaches to methods but not to functions, so the gate has only ever
    // been observed saying "yes" against std — a fixture extension is what watches it say "no".
    Row(
        "ExtTier",
        "sites",
        Constraint(
            Anchor(CHECK_DIRECTIVES, "fn check_declared_sites("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn an_extension_tier_site_restriction_is_enforced(",
            ),
        ),
    ),
    Row(
        "ExtTier",
        "config",
        Data(Anchor(CHECK_TIERS, "pub fn config_attribute_for(")),
    ),
    Row(
        "ExtTier",
        "text",
        Data(Anchor(CHECK_TIERS, "pub fn text_lang(")),
    ),
    Row(
        "ExtTier",
        "expr",
        Data(Anchor(CHECK_TIERS, "pub fn expr_type(")),
    ),
    Row(
        "ExtTier",
        "handler",
        Data(Anchor(CHECK_TIERS, "pub fn expr_tier_handler(")),
    ),
    // --- ExtDirective: the second instance --------------------------------------------------------
    Row(
        "ExtDirective",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_ext_directive(")),
    ),
    Row(
        "ExtDirective",
        "sites",
        Constraint(
            Anchor(CHECK_DIRECTIVES, "fn check_declared_sites("),
            Anchor(EMBED_INSTANCE, "openapi"),
        ),
    ),
    // The second instance: shipped unread, in code written during the arc that fixed
    // `ExtTier.sites`. Unreachable from the corpus — no std declaration sets it — so the
    // exerciser has to be a fixture extension.
    Row(
        "ExtDirective",
        "max_args",
        Constraint(
            Anchor(CHECK_DIRECTIVES, "fn check_declared_args("),
            Anchor(
                EMBED_INSTANCE,
                "fn an_extension_directive_arg_contract_is_enforced(",
            ),
        ),
    ),
    Row(
        "ExtDirective",
        "named_keys",
        Constraint(
            Anchor(CHECK_DIRECTIVES, "fn check_declared_args("),
            Anchor(
                EMBED_INSTANCE,
                "fn an_extension_directive_arg_contract_is_enforced(",
            ),
        ),
    ),
    Row(
        "ExtDirective",
        "detail",
        Prose("the one-line usage shown beside the name in completion"),
    ),
    Row("ExtDirective", "doc", Prose("hover prose")),
    Row(
        "ExtDirective",
        "params",
        Prose("signature-help parameter LABELS; `max_args`/`named_keys` are the checked contract"),
    ),
    // `Data`, not `Constraint`: the compiler CALLS this hook rather than checking a rule with it.
    // The constraints around it belong to other fields — `sites` and `max_args`/`named_keys` decide
    // whether the hook runs at all, which is why a hook may assume it only ever sees a legal
    // invocation. What the hook itself owes in return (declare every file you read) is a promise the
    // compiler cannot check: an under-reporting hook is indistinguishable from an honest one until
    // its output goes stale. That asymmetry is documented on `Expansion::reads` rather than gated
    // here, because gating it would mean claiming an enforcement that does not exist.
    Row(
        "ExtDirective",
        "expand",
        Data(Anchor(LOADER_EXPAND, "plan.directive.expand")),
    ),
    // --- ExtDerive ---------------------------------------------------------------------------
    Row(
        "ExtDerive",
        "name",
        Data(Anchor(REGISTRY, "pub fn find_ext_derive(")),
    ),
    Row(
        "ExtDerive",
        "methods",
        Data(Anchor("crates/noeta-ir/src/lower.rs", "fn native_recipe(")),
    ),
    // std's only derive (`Inspect`) passes `None`, so nothing in the tree had ever called a
    // validator — the enforcement existed and had never run. A fixture derive supplies one.
    Row(
        "ExtDerive",
        "validate",
        Constraint(
            Anchor(CHECK_TRAITS, "fn check_derives("),
            Anchor(
                EMBED_CONSTRAINTS,
                "fn an_extension_derive_validator_gates_the_declaration(",
            ),
        ),
    ),
    Row(
        "ExtDeriveMethod",
        "name",
        Data(Anchor("crates/noeta-ir/src/lower.rs", "fn native_recipe(")),
    ),
    Row(
        "ExtDeriveMethod",
        "arity",
        Data(Anchor("crates/noeta-ir/src/lower.rs", "fn native_recipe(")),
    ),
    Row(
        "ExtDeriveMethod",
        "handler",
        Data(Anchor("crates/noeta-ir/src/lower.rs", "fn native_recipe(")),
    ),
    // --- ExtCapability -----------------------------------------------------------------------
    Row(
        "ExtCapability",
        "id",
        Data(Anchor(REGISTRY, ".find(|c| (c.id)() == id)")),
    ),
    Row(
        "ExtCapability",
        "state_key",
        Data(Anchor(
            VM_NATIVE_CTX,
            "self.state(decl.state_key, decl.init)",
        )),
    ),
    Row(
        "ExtCapability",
        "init",
        Data(Anchor(
            VM_NATIVE_CTX,
            "self.state(decl.state_key, decl.init)",
        )),
    ),
    Row(
        "ExtCapability",
        "build",
        Data(Anchor(VM_NATIVE_CTX, "(decl.build)(state)")),
    ),
    // --- ExtCommand (command.rs) ------------------------------------------------------------------
    Row(
        "ExtCommand",
        "name",
        // Slice-1 command binding: the exported `name` is matched against a `[trust.commands]`
        // binding's `exported` to register a dependency command under its local name (the clap
        // subcommand is then `Command::new(local)`, no longer `Command::new(ext.name)`).
        Data(Anchor(CLI_LIB, "c.name == binding.exported")),
    ),
    Row(
        "ExtCommand",
        "about",
        Prose("the one-line help shown in `noeta --help`"),
    ),
    // The declared argument set IS the CLI parser: an undeclared flag is rejected, a declared
    // one is validated and defaulted, before `run` ever sees it.
    Row(
        "ExtCommand",
        "args",
        Constraint(
            Anchor(CLI_SERVE, "clap::Arg::new(spec.name)"),
            Anchor("crates/noeta-cli/tests/cli/pm_native.rs", "fx-info"),
        ),
    ),
    Row("ExtCommand", "run", Data(Anchor(CLI_SERVE, "(ext.run)("))),
    Row(
        "ArgSpec",
        "name",
        Data(Anchor(CLI_SERVE, "clap::Arg::new(spec.name)")),
    ),
    Row("ArgSpec", "help", Prose("the argument's help text")),
    Row(
        "ArgSpec",
        "kind",
        Constraint(
            Anchor(CLI_SERVE, "ArgKind::Path =>"),
            Anchor("crates/noeta-cli/tests/cli/pm_native.rs", "fx-info"),
        ),
    ),
];

/// The ABI declaration types whose fields `TABLE` must cover, by source file.
///
/// `FieldRecipe` is the one non-`Ext*` entry: it is not something an extension *declares*, but it is
/// something an extension's typed dispatch **reads** (a call-site `TypeRecipe` is handed to it), and
/// its `default` field states a rule about programs — exactly the shape this gate exists for.
const SCANNED: &[(&str, &[&str])] = &[
    (
        "crates/noeta-ext-abi/src/registry.rs",
        &[
            "FieldRecipe",
            "ExtFn",
            "ExtModule",
            "ExtType",
            "ExtEnum",
            "ExtVariant",
            "ExtFielded",
            "ExtField",
            "ExtRoleTag",
            "ExtTrait",
            "ExtTraitMethod",
            "ExtAssocType",
            "PackedConstraint",
            "ExtAttrField",
            "ExtAttribute",
            "ExtTier",
            "ExtDirective",
            "ExtDerive",
            "ExtDeriveMethod",
            "ExtCapability",
        ],
    ),
    (
        "crates/noeta-ext-abi/src/command.rs",
        &["ExtCommand", "ArgSpec"],
    ),
];

fn workspace_root() -> PathBuf {
    // crates/noeta-ext-abi → crates → workspace root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is reachable from CARGO_MANIFEST_DIR");
    assert!(
        root.join("Cargo.toml").is_file() && root.join("crates").is_dir(),
        "expected a workspace root at {}; this gate reads the tree's sources",
        root.display()
    );
    root
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("anchor file {} is unreadable: {e}", path.display()))
}

/// The `pub` field names of `struct <ty>` in `src`, in declaration order.
fn pub_fields(src: &str, ty: &str) -> Vec<String> {
    let head = format!("pub struct {ty} {{");
    let start = src
        .find(&head)
        .unwrap_or_else(|| panic!("`{head}` not found — did the type move or get renamed?"))
        + head.len();
    let body = &src[start..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("no closing brace for `{ty}`"));
    body[..end]
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("pub "))
        .filter_map(|l| l.split(':').next())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Adding a field to an ABI declaration type must not be possible without classifying it. This is
/// the half that fires at the commit which introduces an unread constraint, rather than whenever
/// someone next reads the file.
#[test]
fn every_abi_field_is_classified() {
    let mut declared: Vec<(String, String)> = Vec::new();
    for (file, types) in SCANNED {
        let src = read(file);
        for ty in *types {
            for field in pub_fields(&src, ty) {
                declared.push(((*ty).to_string(), field));
            }
        }
    }

    let classified: Vec<(String, String)> = TABLE
        .iter()
        .map(|r| (r.ty().to_string(), r.field().to_string()))
        .collect();

    let missing: Vec<_> = declared
        .iter()
        .filter(|d| !classified.contains(d))
        .collect();
    assert!(
        missing.is_empty(),
        "these ABI fields are not classified in TABLE: {missing:?}\n\
         A field on one of these types is part of an extension author's contract. Classify it:\n\
           - Constraint — it states a rule. Name the enforcer AND the test that watches it fire.\n\
             If nothing enforces it yet, that is the bug this gate exists to catch: enforce it.\n\
           - Data       — the compiler consumes it. Name the reader.\n\
           - Prose      — human-facing text. Say why nothing can enforce it."
    );

    let stale: Vec<_> = classified
        .iter()
        .filter(|c| !declared.contains(c))
        .collect();
    assert!(
        stale.is_empty(),
        "TABLE classifies fields that no longer exist (renamed or removed?): {stale:?}"
    );

    let mut seen = classified.clone();
    seen.sort();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "TABLE classifies a field twice");
}

/// Every classified field must still have the reader/enforcer the table names. A field whose reader
/// was deleted is back to lying to extension authors, which is the state this whole gate is about.
#[test]
fn every_classified_field_has_a_live_reader() {
    for row in TABLE {
        let anchor = match row.verdict() {
            Verdict::Constraint(enforcer, _) => enforcer,
            Verdict::Data(reader) => reader,
            // Nothing in the tree to anchor. The one requirement is that the exemption was
            // *stated*: an empty reason is an omission wearing a verdict's clothes.
            Verdict::Prose(why) => {
                assert!(
                    !why.is_empty(),
                    "{}.{}: a Prose verdict must say why nothing enforces the field",
                    row.ty(),
                    row.field()
                );
                continue;
            }
        };
        let src = read(anchor.file());
        assert!(
            src.contains(anchor.needle()),
            "{}.{}: its reader anchor {:?} is gone from {} — re-point the anchor after confirming \
             the field is still read, or fix the field's enforcement",
            row.ty(),
            row.field(),
            anchor.needle(),
            anchor.file()
        );
        assert!(
            src.contains(row.field()),
            "{}.{}: {} no longer mentions `{}` — the reader this table names does not read the \
             field any more",
            row.ty(),
            row.field(),
            anchor.file(),
            row.field()
        );
    }
}

/// Every field classified as a **constraint** must have a test that watches the rule fire.
///
/// This is the third instance of the bug class, structurally: the argument-contract diagnostics had
/// an enforcer and no exerciser, and nothing noticed, because the conformance corpus cannot reach
/// code that only runs for a declaration std does not make.
#[test]
fn every_constraint_field_has_a_live_exerciser() {
    for row in TABLE {
        let Verdict::Constraint(_, exerciser) = row.verdict() else {
            continue;
        };
        let src = read(exerciser.file());
        assert!(
            src.contains(exerciser.needle()),
            "{}.{}: its exerciser anchor {:?} is gone from {} — the constraint is enforced but \
             nothing watches the enforcement fire",
            row.ty(),
            row.field(),
            exerciser.needle(),
            exerciser.file()
        );
    }
}
