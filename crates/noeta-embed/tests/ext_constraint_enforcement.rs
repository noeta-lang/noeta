//! **Behavioural coverage for ABI-declared constraints no shipped extension exercises.**
//!
//! Three times now a `noeta-ext-abi` field that *declares a constraint* — where something may
//! attach, what shape a deriving type must have, what layout a bundle requires — has shipped with
//! nothing enforcing it (`ExtTier.sites`, then `ExtDirective.max_args`/`named_keys`). Each was
//! found by reading, not by a failing test, because the conformance corpus **structurally cannot
//! reach** the code: the corpus runs programs against the *std* extension, and std declares these
//! fields either trivially or not at all. `Inspect` is std's only `ExtDerive` and its `validate` is
//! `None`; both shipped `ExtBundle`s declare `ConstraintLayout::Any`. So "the corpus is green" says
//! exactly nothing about the enforcement written for those fields — the gap that let the argument
//! contract's diagnostics ship untested.
//!
//! A **fixture extension** is the only way to reach them, because the constraint is declared by the
//! extension author and there is no other author. This file declares one that exercises each
//! constraint in both directions — the violating program is rejected AND the conforming one is
//! accepted, so a gate that simply rejected everything would fail here too.
//!
//! Its own test binary on purpose: the extension registry installs **once per process**, so a
//! session-scoped fixture registry must not share a binary with tests that expect the std default.
//! It shares that constraint (and this shape) with `tests/instance_registry.rs`.

use noeta_embed::{Error, Session};
use noeta_ext_abi::registry::{
    BundleFn, BundleReceiver, ConstraintField, ConstraintLayout, EnumBacking, ExtBundle, ExtClass,
    ExtDerive, ExtDeriveMethod, ExtEnum, ExtField, ExtFn, ExtModule, ExtTier, ExtTrait,
    ExtTraitMethod, ExtType, ExtVariant, Extension, NativeOut, NativeValue, PackedConstraint, RetTy,
    Scalar, SigType, TierSite, VariantValue,
};
use noeta_ext_abi::{CtxError, CtxOut, ErrorKind, Host, Slot, StdError};

// --- The fixture extension -----------------------------------------------------------------------

const KERN_FNS: &[ExtFn] = &[
    ExtFn {
        name: "noop",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
    // Returns a value of the backed enum below, so a program has something to call `.value()` on.
    ExtFn {
        name: "tone",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Tone")),
    },
    // Returns an instance of the native class below, so a program has one to access fields on
    // (the visibility/mutability constraints under test are on *field access*).
    ExtFn {
        name: "widget",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Widget")),
    },
];

/// A **native class** with a public, a public-mutable, and a private field — the `ExtField.is_public`
/// and `ExtField.is_mut` constraints under test. No shipped extension declares a native class, so the
/// checker's E0035 (private-field access) and E0033 (non-`mut` assignment) arms never run against the
/// corpus for a native class; this fixture is their exerciser.
const FX_CLASSES: &[ExtClass] = &[ExtClass {
    name: "Widget",
    namespace: "fx",
    fields: &[
        // Public + read-only: readable outside, but assigning it is E0033.
        ExtField {
            name: "label",
            ty: SigType::String,
            is_public: true,
            is_mut: false,
        },
        // Public + mutable: readable and assignable outside.
        ExtField {
            name: "tag",
            ty: SigType::Int,
            is_public: true,
            is_mut: true,
        },
        // Private: reading it from outside the class is E0035.
        ExtField {
            name: "secret",
            ty: SigType::Int,
            is_public: false,
            is_mut: false,
        },
    ],
    ..ExtClass::DEFAULTS
}];

fn kern_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "noop" => Ok(NativeOut::Scalar(Scalar::Int(0))),
        "tone" => Ok(NativeOut::Variant {
            enum_name: "Tone".to_string(),
            variant: "Warm".to_string(),
            variant_index: 0,
            fields: vec![],
        }),
        // A `Widget` instance — fields in declared slot order (label, tag, secret).
        "widget" => Ok(NativeOut::Instance {
            class: "Widget".to_string(),
            fields: vec![
                ("label".to_string(), NativeOut::Str("w".to_string())),
                ("tag".to_string(), NativeOut::Scalar(Scalar::Int(0))),
                ("secret".to_string(), NativeOut::Scalar(Scalar::Int(0))),
            ],
        }),
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

/// A **string-backed** native enum — the `ExtEnum.backing` constraint under test. No shipped
/// extension declares a backed enum, so the checker's `.value()`-typing arm (a `String`-backed
/// enum's `.value()` is `string`) has never run against the corpus; this fixture is its exerciser.
const FX_ENUMS: &[ExtEnum] = &[ExtEnum {
    name: "Tone",
    namespace: "fx",
    variants: &[
        ExtVariant {
            name: "Warm",
            fields: &[],
            value: VariantValue::Str("warm"),
        },
        ExtVariant {
            name: "Cool",
            fields: &[],
            value: VariantValue::Str("cool"),
        },
    ],
    backing: EnumBacking::Str,
}];

/// A bundle requiring **column** layout — the [`ConstraintLayout`] arm no shipped bundle declares
/// (`vec.Kernels` and the package-manager fixture's `fx.Pixels` are both `Any`), so the checker's
/// row/column arms of `constraint_mismatch` have never run against a real binding.
///
/// The methods are never called here: a bundle *binding* is inert until a method call, and the
/// constraint is validated at the `impl` site. That is precisely the surface under test.
const COLS_BUNDLE: ExtBundle = ExtBundle {
    name: "Cols",
    constraint: PackedConstraint {
        fields: &[ConstraintField::F32, ConstraintField::F32],
        layout: ConstraintLayout::Column,
    },
    methods: &[BundleFn {
        sig: ExtFn {
            name: "sum_all",
            params: &[],
            ret: RetTy::Concrete(SigType::F32),
        },
        receiver: BundleReceiver::Bulk,
    }],
    ctx_dispatch: cols_dispatch,
};

fn cols_dispatch(
    method: &str,
    _ctx: &mut dyn noeta_ext_abi::NativeCtx,
    _recv: Slot,
    _args: &[Slot],
) -> Result<CtxOut, CtxError> {
    Err(CtxError::Std(StdError {
        kind: ErrorKind::UnknownName,
        message: format!("no bundle method `{method}` (this fixture binds, it does not run)"),
    }))
}

/// The derive validator under test: a type deriving `Checked` must declare a field named `id`.
///
/// A validator is an *arbitrary* author predicate, so what matters is that the checker calls it at
/// the declaration and surfaces its message verbatim — not what this particular one decides.
fn checked_validate(type_name: &str, fields: &[(String, String)]) -> Option<String> {
    if fields.iter().any(|(name, _)| name == "id") {
        return None;
    }
    Some(format!(
        "`{type_name}` cannot derive `Checked`: the recipe needs an `id` field"
    ))
}

const FX_DERIVES: &[ExtDerive] = &[ExtDerive {
    name: "Checked",
    methods: &[ExtDeriveMethod {
        name: "checked_id",
        arity: 0,
        handler: "json.stringify",
    }],
    validate: Some(checked_validate),
}];

/// A **native trait** with one required method — the `ExtTrait.methods` constraint under test. No
/// shipped extension declares a native trait, so the checker's E0015 (incomplete-impl) arm has never
/// run for a native trait's contract against the corpus; this fixture is its exerciser. A user type
/// `impl Renderable for T` must define `fn render(): string` or the impl is E0015.
const FX_TRAITS: &[ExtTrait] = &[ExtTrait {
    name: "Renderable",
    namespace: "fx",
    methods: &[ExtTraitMethod {
        sig: ExtFn {
            name: "render",
            params: &[],
            ret: RetTy::Concrete(SigType::String),
        },
        has_default: false,
    }],
}];

/// A tier restricted to **methods**. std has no tier that attaches to methods but not to functions,
/// so the annotation-site gate has only ever been observed saying "yes".
const FX_TIERS: &[ExtTier] = &[ExtTier {
    name: "audit",
    sites: &[TierSite::Method],
    config: None,
    text: None,
    expr: None,
    handler: None,
}];

struct FxExtension;

impl Extension for FxExtension {
    fn name(&self) -> &'static str {
        "fx"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "kern",
            functions: KERN_FNS,
            dispatch: kern_dispatch,
            bundles: &[COLS_BUNDLE],
            ..ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[]
    }
    fn tiers(&self) -> &'static [ExtTier] {
        FX_TIERS
    }
    fn derives(&self) -> &'static [ExtDerive] {
        FX_DERIVES
    }
    fn enums(&self) -> &'static [ExtEnum] {
        FX_ENUMS
    }
    fn classes(&self) -> &'static [ExtClass] {
        FX_CLASSES
    }
    fn traits(&self) -> &'static [ExtTrait] {
        FX_TRAITS
    }
}

static FX: FxExtension = FxExtension;

// --- Helpers -------------------------------------------------------------------------------------

fn load(src: &str) -> Result<Session, Error> {
    Session::builder().with_extensions(vec![&FX]).load(src)
}

#[track_caller]
fn accepts(src: &str) {
    if let Err(err) = load(src) {
        panic!("expected `{src}` to check clean, got {err:?}");
    }
}

#[track_caller]
fn rejects(src: &str, needle: &str) {
    match load(src) {
        Err(Error::Check(diags)) => assert!(
            diags.iter().any(|d| d.contains(needle)),
            "expected a diagnostic containing {needle:?}, got {diags:?}"
        ),
        other => panic!("expected `{src}` to be rejected, got {other:?}"),
    }
}

// --- ExtTier.sites -------------------------------------------------------------------------------

/// `ExtTier.sites` restricts where a tier's **annotation** form may attach — the field that shipped
/// declared-but-unenforced, and the original instance of this bug class.
///
/// `fx.audit` declares `Method` only, so the same `@audit fn …` is legal inside a class body and a
/// misplacement at the top level. Both directions matter: the accepting case is what distinguishes
/// a working gate from one that rejects the tier outright.
#[test]
fn an_extension_tier_site_restriction_is_enforced() {
    accepts("class Ledger {\n  @audit fn entries(): int { return 1; }\n}\necho 1\n");
    rejects(
        "@audit fn entries(): int { return 1; }\necho 1\n",
        "does not apply to",
    );
}

// --- ExtDerive.validate --------------------------------------------------------------------------

/// `ExtDerive.validate` is the derive's compile-time shape check. std's only derive (`Inspect`)
/// passes `None`, so the checker's `validate(...)` call has never run — the field is a promise to
/// a third-party derive author that nothing had ever demonstrated keeping.
#[test]
fn an_extension_derive_validator_gates_the_declaration() {
    accepts("@derive(Checked)\nstruct Row { id: int; label: string }\necho 1\n");
    // The validator's own message reaches the user verbatim — a derive author writes the
    // diagnostic, so a gate that fired with a generic message would be a different feature.
    rejects(
        "@derive(Checked)\nstruct Row { label: string }\necho 1\n",
        "the recipe needs an `id` field",
    );
}

// --- ExtEnum.backing (native-extensibility S1) ---------------------------------------------------

/// `ExtEnum.backing` states the RULE the checker enforces on a backed enum's `.value()` accessor:
/// a `String`-backed enum's `.value()` is `string`. No shipped extension declares a backed enum, so
/// the corpus cannot reach the typing — a fixture is the only exerciser. Both directions matter: the
/// backing type is *accepted* where it fits, and *rejected* (E0007) where it does not.
#[test]
fn a_backed_ext_enum_value_type_is_enforced() {
    // `.value()` on the `String`-backed `Tone` types as `string`, assignable to a `string` binding.
    accepts("use fx.{kern}\ns: string = kern.tone().value()\necho s\n");
    // The same `.value()` is NOT an `int`: assigning it to an `int` binding is a type mismatch,
    // exactly as it would be for a `.noe` backed enum's declared scalar.
    rejects(
        "use fx.{kern}\nn: int = kern.tone().value()\necho n\n",
        "expected `int`, found `string`",
    );
}

// --- ExtField.is_public / ExtField.is_mut (native-extensibility S2) ------------------------------

/// `ExtField.is_public` states the RULE the checker enforces on a native class's field access: a
/// field not declared `pub` is private, and reading it from outside its class is E0035 — exactly a
/// `.noe` class's default-private field. No shipped extension declares a native class, so a fixture
/// is the only exerciser. Both directions matter: the public field is *readable*, the private one is
/// *rejected*.
#[test]
fn a_native_class_field_visibility_is_enforced() {
    // Reading the **public** `label` off a native class instance types as `string`.
    accepts("use fx.{kern}\nuse fx.Widget\nw = kern.widget()\ns: string = w.label\necho s\n");
    // Reading the **private** `secret` from outside the class is E0035, exactly as for a `.noe`
    // class's default-private field.
    rejects(
        "use fx.{kern}\nuse fx.Widget\nw = kern.widget()\necho w.secret\n",
        "private field `secret`",
    );
}

/// `ExtField.is_mut` states the RULE the checker enforces on a native class's field assignment:
/// writing a field not declared `mut` is E0033. A fixture is the only exerciser. Both directions
/// matter: the `mut` field is *assignable*, the read-only one is *rejected*.
#[test]
fn a_native_class_field_mutability_is_enforced() {
    // The **mut** `tag` field is assignable in place (reference `class` semantics — no `mut` binding).
    accepts("use fx.{kern}\nuse fx.Widget\nw = kern.widget()\nw.tag = 5\necho w.tag\n");
    // The public-but-**read-only** `label` field is not `mut`: assigning it is E0033.
    rejects(
        "use fx.{kern}\nuse fx.Widget\nw = kern.widget()\nw.label = \"x\"\necho 1\n",
        "not declared `mut`",
    );
}

// --- ExtTrait.methods (native-extensibility S3) --------------------------------------------------

/// `ExtTrait.methods` states the RULE a native trait's contract enforces on an implementor: every
/// required (non-default) method must be present with matching arity/types, or the `impl` is E0015 —
/// exactly a `.noe` trait. No shipped extension declares a native trait, so the checker's
/// `check_user_trait_impl` arm has never run for a native contract against the corpus; this fixture
/// is its exerciser. Both directions matter: a complete impl checks clean, an incomplete one is
/// rejected.
#[test]
fn a_native_trait_incomplete_impl_is_rejected() {
    // A COMPLETE `impl fx.Renderable for Card` — defines the required `render` — checks clean.
    accepts(
        "use fx.Renderable\n\
         struct Card { title: string }\n\
         impl Renderable for Card { fn render(): string { return self.title } }\n\
         echo 1\n",
    );
    // An INCOMPLETE impl — the required `render` is missing — is E0015.
    rejects(
        "use fx.Renderable\n\
         struct Card { title: string }\n\
         impl Renderable for Card {}\n\
         echo 1\n",
        "must define `fn render`",
    );
}

// --- PackedConstraint.layout ---------------------------------------------------------------------

/// `PackedConstraint.layout` is validated at the `impl` site. Its `Any` arm is exercised by the
/// corpus (`tests/conformance/bundles/bind_ok.noe`); `Row` and `Column` are not, because no shipped
/// bundle declares either — so the two arms that actually *reject* something have never run.
#[test]
fn a_bundle_layout_constraint_is_enforced_at_the_impl_site() {
    const COLUMN: &str = "use fx.{kern}\n\
                          @packed(Layout.Column) struct C2 { x: f32; y: f32 }\n\
                          impl kern.Cols for C2 {}\n\
                          echo 1\n";
    const ROW: &str = "use fx.{kern}\n\
                       @packed struct R2 { x: f32; y: f32 }\n\
                       impl kern.Cols for R2 {}\n\
                       echo 1\n";
    accepts(COLUMN);
    rejects(ROW, "requires column layout");
}

/// The sibling arm of the same constraint: field **kinds** are already corpus-covered
/// (`bundles/bind_errors.noe`), but only against a std bundle. Pinned here too so the fixture's
/// constraint is checked as a whole rather than layout-only — a constraint validated field-by-field
/// in one place and layout-only in another is how these two halves would drift apart.
#[test]
fn a_bundle_field_constraint_is_enforced_at_the_impl_site() {
    rejects(
        "use fx.{kern}\n\
         @packed(Layout.Column) struct C3 { x: f32; y: f32; z: f32 }\n\
         impl kern.Cols for C3 {}\n\
         echo 1\n",
        "requires fields",
    );
}
