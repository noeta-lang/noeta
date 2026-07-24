//! **Native-declared traits** (native-extensibility S3): an extension *outside* `std` declares a
//! real language **trait** — a contract user types `impl`/bound on (3a) AND a dynamic-dispatch
//! surface over native values (3b) — and a program implements it, binds on it, and dispatches a
//! trait method through `dyn Trait` over both a user value and a **native** (`ExtType`) value.
//!
//! A synthetic extension rather than an `std` consumer, deliberately (mirroring `ext_class_seam.rs`
//! / `ext_enum_seam.rs`): the whole path is registry-driven, so nothing about `Widget`/`Button` is
//! known to the checker or either backend except its [`ExtTrait`]/[`ExtType`] declaration. The
//! corpus's `differential_backends_agree` oracle cannot reach it (std declares no native trait), so
//! the differential assertion lives here.
//!
//! **The load-bearing test is the dynamic-dispatch one** (`native_trait_contract_and_dynamic_dispatch
//! _agree_on_both_backends`): a native `Button` laundered through `dyn Widget`, calling `describe()`,
//! must dispatch to the **native** method — the same extern-method seam a directly-typed extern
//! value uses — identically on the tree-walker reference and the bytecode VM. That is what proves 3b
//! rides the existing extern-method dispatch (no Object-arm change), and that a user type and a
//! native type coexist behind one `dyn`.
//!
//! Two **check-only** tests pin the 3a contract diagnostics — an incomplete `impl` is E0015, a
//! bound violation is E0025 — the native-trait twins of the built-in-trait checks.
//!
//! An **integration test** (own process) because the fixture installs into the process-global
//! default registry — once per process — the single-registry path the CLI uses.

use std::any::Any;

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    EnumBacking, ExtClass, ExtEnum, ExtField, ExtFn, ExtModule, ExtStruct, ExtTrait,
    ExtTraitMethod, ExtType, ExtVariant, Extension, FieldedKind, NativeOut, NativeValue, RetTy,
    SigType, VariantValue,
};
use noeta_stdlib::{ExternBox, ExternValue, Host, StdError};
use noeta_vm::VmBackend;

// --- The native value behind the `dyn`: an extern type that implements the native trait -----------

/// A `Button` — an opaque extern value whose `describe()` method is the native implementation of the
/// `Widget` trait's contract. Held/aliased with reference semantics like any extern value; laundered
/// through `dyn Widget` in the 3b test, where its method call must reach `button_dispatch`.
#[derive(Debug, Clone)]
struct ButtonBox {
    label: String,
}

impl ExternValue for ButtonBox {
    fn type_identity(&self) -> &'static str {
        "fx.Button"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other
            .as_any()
            .downcast_ref::<ButtonBox>()
            .is_some_and(|b| b.label == self.label)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<button {}>", self.label)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `Button`'s method dispatch — the native implementation of the `Widget` trait method. Reached both
/// for a directly-typed `Button.describe()` and for a `describe()` on a `dyn Widget` holding a
/// `Button` (3b): the runtime keys dispatch off the extern value, not the static `dyn` type.
fn button_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match method {
        "describe" => {
            let label = recv
                .as_any()
                .downcast_ref::<ButtonBox>()
                .map(|b| b.label.clone())
                .unwrap_or_default();
            Ok(NativeOut::Str(format!("button:{label}")))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

/// The native `Button` type: declares `describe(): string` as an ordinary extern method, AND
/// advertises that it implements the native `Widget` trait through the existing `traits` list — the
/// 3b coercion channel `seed_ext_traits` reads into `user_trait_impls["fx.Button"]["Widget"]`.
const BUTTON: ExtType = ExtType {
    name: "Button",
    namespace: "fx",
    methods: &[ExtFn {
        name: "describe",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: button_dispatch,
    traits: &["Widget"],
    ..ExtType::DEFAULTS
};

// --- The native trait -----------------------------------------------------------------------------

/// The native `Widget` trait: one required method `describe(): string`. A user type implements it
/// (3a); a native `Button` advertises it (3b). Keyed into the user-trait machinery by its imported
/// short name, gated by `use fx.Widget`.
const WIDGET: ExtTrait = ExtTrait {
    name: "Widget",
    namespace: "fx",
    methods: &[ExtTraitMethod {
        sig: ExtFn {
            name: "describe",
            params: &[],
            ret: RetTy::Concrete(SigType::String),
        },
        has_default: false,
    }],
    // No associated types (the native-derived assoc path is exercised by `ext_assoc_seam.rs`).
    assoc_types: &[],
    // No native default-body dispatch — `describe` is required (has_default: false); the trait-default
    // path (slice 2) is exercised by `ext_trait_default_seam.rs`.
    dispatch: None,
    // No structural `Self`-constraint (slice 3) — exercised by `ext_self_constraint_seam.rs`.
    self_constraint: None,
};

// --- The OTHER native value kind: a native CLASS that implements the trait (Pass 2b) --------------

/// A native **class** `Panel` — a real class-kind `Object`, the second native value kind behind a
/// `dyn Widget` (Pass 2b). It advertises `Widget` via `traits` (seeded into `user_trait_impls`) and
/// implements the trait method `describe()` as a native class method (dispatched through the Pass-2a
/// Object-arm branch). Proves `dyn Widget` dispatch is representation-agnostic: an ExtType (`Button`)
/// and an ExtClass (`Panel`) both dispatch their trait method to native code behind one `dyn`.
const PANEL: ExtClass = ExtClass {
    name: "Panel",
    namespace: "fx",
    fields: &[ExtField {
        name: "label",
        ty: SigType::String,
        is_public: true,
        is_mut: false,
    }],
    methods: &[ExtFn {
        name: "describe",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: panel_dispatch,
    traits: &["Widget"],
    kind: FieldedKind::Class,
    directives: &[],
};

/// `Panel`'s native method dispatch — the native implementation of `Widget.describe()` for a class
/// receiver. Reads the instance's `label` field off the marshalled `NativeValue::Instance`.
fn panel_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match method {
        "describe" => {
            let label = match recv {
                NativeValue::Instance { fields, .. } => fields
                    .iter()
                    .find(|(k, _)| k == "label")
                    .and_then(|(_, v)| match v {
                        NativeValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Ok(NativeOut::Str(format!("panel:{label}")))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

// --- The THIRD native value kind: a native ENUM that implements the trait (Slice C) ---------------

/// A native **enum** `Mode` — the third native value kind behind a `dyn Widget` (native-extensibility
/// Slice C). It advertises `Widget` via its new `ExtEnum::traits` (seeded into `user_trait_impls`
/// exactly like a class/struct) and implements the trait method `describe()` as a native enum method,
/// dispatched through `call_native_enum_method` (Slice B). Proves `dyn Widget` dispatch reaches a
/// native enum value — routed by runtime value-kind, not the static `dyn` type.
const MODE: ExtEnum = ExtEnum {
    name: "Mode",
    namespace: "fx",
    variants: &[
        ExtVariant {
            name: "Dark",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Light",
            fields: &[],
            value: VariantValue::None,
        },
    ],
    backing: EnumBacking::None,
    methods: &[ExtFn {
        name: "describe",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: mode_dispatch,
    traits: &["Widget"],
    directives: &[],
};

/// `Mode`'s native enum-method dispatch — the native implementation of `Widget.describe()` for an
/// enum receiver. Reads the case off the marshalled `NativeValue::Variant`.
fn mode_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let NativeValue::Variant { variant, .. } = recv else {
        return Err(StdError {
            kind: noeta_stdlib::ErrorKind::ArgType,
            message: "enum method called on a non-variant receiver".to_string(),
        });
    };
    match method {
        "describe" => Ok(NativeOut::Str(format!("mode:{}", variant.to_lowercase()))),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

// --- The FOURTH native value kind: a native STRUCT that implements the trait + a BUILT-IN trait ----

/// A native **struct** `Badge` — a value-kind fielded object behind a `dyn Widget` (Slice C). It
/// advertises **two** traits through one `ExtStruct::traits` list: the native `Widget` (routed to the
/// native-`ExtTrait` channel `seed_ext_traits`) AND the **built-in** `Comparable` (routed to
/// `seed_native_builtin_traits`, which `record_trait_impls` filters to). The mixed list proves the
/// two channels split a single `traits` declaration cleanly, and `Comparable` is the latent-bug-fix
/// proof: before Slice C the built-in seeding walked `types()` only, so a struct/class/enum declaring
/// a built-in trait satisfied *nothing*; now a `T: Comparable` bound accepts a `Badge`.
const BADGE: ExtStruct = ExtStruct {
    name: "Badge",
    namespace: "fx",
    fields: &[ExtField {
        name: "label",
        ty: SigType::String,
        is_public: true,
        is_mut: false,
    }],
    methods: &[ExtFn {
        name: "describe",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: badge_dispatch,
    traits: &["Widget", "Comparable"],
    kind: FieldedKind::Struct,
    directives: &[],
};

/// `Badge`'s native method dispatch — the native implementation of `Widget.describe()` for a struct
/// receiver. Reads the instance's `label` field off the marshalled `NativeValue::Instance`, exactly
/// like `panel_dispatch` (a struct and a class share the fielded dispatch seam).
fn badge_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match method {
        "describe" => {
            let label = match recv {
                NativeValue::Instance { fields, .. } => fields
                    .iter()
                    .find(|(k, _)| k == "label")
                    .and_then(|(_, v)| match v {
                        NativeValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Ok(NativeOut::Str(format!("badge:{label}")))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

// --- The module that constructs the native values -------------------------------------------------

const KIT_FNS: &[ExtFn] = &[
    ExtFn {
        name: "make",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Button")),
    },
    ExtFn {
        name: "panel",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Panel")),
    },
    ExtFn {
        name: "mode",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Mode")),
    },
    ExtFn {
        name: "badge",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Badge")),
    },
];

fn kit_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "make" => {
            let label = match args.first() {
                Some(NativeValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            Ok(NativeOut::Extern(ExternBox::new(ButtonBox { label })))
        }
        "panel" => {
            let label = match args.first() {
                Some(NativeValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            // A real native class instance (class-kind object), field in declared slot order.
            Ok(NativeOut::Instance {
                class: "Panel".to_string(),
                fields: vec![("label".to_string(), NativeOut::Str(label))],
                kind: FieldedKind::Class,
            })
        }
        "mode" => {
            // A real native enum variant (Slice C): `Mode.Dark`, its declaration index.
            Ok(NativeOut::Variant {
                enum_name: "Mode".to_string(),
                variant: "Dark".to_string(),
                variant_index: 0,
                fields: vec![],
            })
        }
        "badge" => {
            let label = match args.first() {
                Some(NativeValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            // A real native struct instance (value-kind object), field in declared slot order.
            Ok(NativeOut::Instance {
                class: "Badge".to_string(),
                fields: vec![("label".to_string(), NativeOut::Str(label))],
                kind: FieldedKind::Struct,
            })
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct FxExtension;

impl Extension for FxExtension {
    fn name(&self) -> &'static str {
        "fx"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "kit",
            functions: KIT_FNS,
            dispatch: kit_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[BUTTON]
    }
    fn classes(&self) -> &'static [ExtClass] {
        &[PANEL]
    }
    fn structs(&self) -> &'static [ExtStruct] {
        &[BADGE]
    }
    fn enums(&self) -> &'static [ExtEnum] {
        &[MODE]
    }
    fn traits(&self) -> &'static [ExtTrait] {
        &[WIDGET]
    }
}

static FX: FxExtension = FxExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FX]));
}

// --- Helpers -------------------------------------------------------------------------------------

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero), returning the shared stdout. Mirrors `ext_class_seam.rs`.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_trait_seam.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "program must parse cleanly: {:?}",
        parsed.0.diagnostics
    );
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "program must check cleanly: {:?}",
        checked.diagnostics
    );

    let eval_before = noeta_eval::live_count();
    let reference =
        noeta_conformance::reference::reference_run(&parsed.0.program, checked.sites.clone());
    let eval_after = noeta_eval::live_count();
    assert_eq!(
        eval_before, eval_after,
        "tree-walker leak oracle: heap residency must return to baseline"
    );

    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("program compiles to bytecode")
        .clone();
    let vm_before = noeta_value::live_count() as i64;
    let vm = VmBackend::new().run_module(&module);
    let vm_after = noeta_value::live_count() as i64;
    assert_eq!(
        vm_before, vm_after,
        "VM leak oracle: heap residency must return to baseline"
    );

    assert_eq!(
        reference, vm,
        "backends must agree on the native-trait program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// The check-only diagnostics of a program (parse is asserted clean; the checker's diagnostics are
/// returned for a negative assertion), for the 3a contract-error cases.
#[track_caller]
fn check_diagnostics(program: &str) -> Vec<String> {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_trait_check.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    checked
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

// --- Tests ---------------------------------------------------------------------------------------

/// **The 3a contract + 3b dynamic dispatch, differential.** A user `Card` implements the native
/// `Widget` (3a); a `<T: Widget>` bound accepts it (3a bound); and a `dyn Widget` dispatches
/// `describe()` to a `.noe` body for `Card` AND to the **native** method for a `Button` (3b) — the
/// two backends must build identical output for all of it.
const PROGRAM: &str = r#"
use fx.kit
use fx.Widget
use fx.Button

// A user type implements the native trait (3a).
struct Card {
    title: string
}
impl Widget for Card {
    fn describe(): string {
        return "card:${self.title}"
    }
}

// A `T: Widget` bound accepts the implementor (3a bound) and calls the trait method.
fn announce<T: Widget>(w: T): string {
    return w.describe()
}

// A `dyn Widget` dispatches the trait method dynamically over whatever concrete type it holds.
fn render(w: dyn Widget): string {
    return w.describe()
}

c = Card { title: "hi" }
echo announce(c)
echo render(c)

// 3b (ExtType receiver): a NATIVE extern `Button` laundered through `dyn Widget` dispatches
// `describe()` to the native method — the load-bearing case, Pass 1.
b = kit.make("go")
echo render(b)

// Directly-typed native method call, for parity with the `dyn` dispatch above.
echo b.describe()

// 3b (ExtClass receiver, Pass 2b): a NATIVE class `Panel` — the OTHER native value kind — behind
// the SAME `dyn Widget` dispatches `describe()` to its native class method. Proves the `dyn`
// dispatch is representation-agnostic: an extern value and a class object both reach native code.
p = kit.panel("cls")
echo render(p)

// Directly-typed native class method call, for parity.
echo p.describe()

// 3b (ExtEnum receiver, Slice C): a NATIVE enum `Mode` — the THIRD native value kind — behind the
// SAME `dyn Widget` dispatches `describe()` to its native enum method (`call_native_enum_method`).
m = kit.mode()
echo render(m)
echo m.describe()

// 3b (ExtStruct receiver, Slice C): a NATIVE struct `Badge` — the FOURTH native value kind — behind
// the SAME `dyn Widget` dispatches `describe()` to its native struct method (the fielded seam).
bg = kit.badge("v")
echo render(bg)
echo bg.describe()

// Built-in-trait latent-bug fix (Slice C): `Badge` also declares the BUILT-IN `Comparable` in its
// mixed `traits` list. Before Slice C the built-in seeding walked `types()` only, so this struct
// satisfied NOTHING and the `<T: Comparable>` bound below was E0025. Now `seed_native_builtin_traits`
// records it, so the bound accepts `Badge` — observable as this program checking + running clean.
fn ranked<T: Comparable>(x: T): string {
    return "ranked"
}
echo ranked(bg)
"#;

#[test]
fn native_trait_contract_and_dynamic_dispatch_agree_on_both_backends() {
    let stdout = run_both_agree(PROGRAM);
    assert_eq!(
        stdout,
        "card:hi\ncard:hi\nbutton:go\nbutton:go\npanel:cls\npanel:cls\n\
         mode:dark\nmode:dark\nbadge:v\nbadge:v\nranked\n"
    );
}

/// **3a — an incomplete `impl` is E0015.** A user type implementing the native `Widget` must define
/// its required `describe`, exactly as for a `.noe` trait — the native trait's contract reaches
/// `check_user_trait_impl` through `symbols.user_traits` keyed by the imported short name.
#[test]
fn an_incomplete_impl_of_a_native_trait_is_rejected() {
    let diags = check_diagnostics(
        "use fx.Widget\nstruct Card { title: string }\nimpl Widget for Card {}\necho 1\n",
    );
    assert!(
        diags
            .iter()
            .any(|m| m.contains("must define `fn describe`")),
        "expected an E0015 naming the missing `describe`, got {diags:?}"
    );
}

/// **3a — a `T: Widget` bound violation is E0025.** A type that does not implement the native trait
/// cannot be passed where the bound requires it, exactly as for a built-in-trait bound.
#[test]
fn a_native_trait_bound_violation_is_rejected() {
    const SRC: &str = r#"
use fx.Widget

struct Plain {
    n: int
}

fn announce<T: Widget>(w: T): string {
    return w.describe()
}

echo announce(Plain { n: 1 })
"#;
    let diags = check_diagnostics(SRC);
    assert!(
        diags.iter().any(|m| m.contains("Widget")),
        "expected a bound-violation diagnostic naming `Widget`, got {diags:?}"
    );
}
