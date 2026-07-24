//! **Native-declared built-in directives** (native type-declaration unification, Slice D): an
//! extension *outside* `std` declares real language types that carry the built-in directives a `.noe`
//! type gets from its `Decorators` — `@semantic` on an enum and `@validated` on a struct — through
//! the unified [`ExtFielded::directives`] / [`ExtEnum::directives`] channel. The checker's
//! `seed_ext_directives` pass translates each into the *same* `Symbols` table a `.noe` declaration
//! seeds (`semantic_enums`, `validated_types`), so a native type is indistinguishable from a `.noe`
//! one to every downstream consumer.
//!
//! A synthetic extension rather than an `std` consumer (mirroring `ext_struct_seam.rs` /
//! `ext_enum_seam.rs`): the whole path is registry-driven, so nothing about `cfg.*` is known to the
//! checker or either backend except its declaration + its `directives`. The two load-bearing proofs:
//!
//! * [`native_semantic_enum_is_a_role_vocabulary`] — a native `@semantic` enum is accepted where a
//!   `@semantic` enum is *required* (`roles_of::<Stage>()`), while an otherwise-identical enum that
//!   lacks the directive (`Plain`) is rejected there — so the directive, and nothing else, is what
//!   registers the enum into `semantic_enums`.
//! * [`native_validated_struct_bars_construction_and_validates_at_a_door`] — a native `@validated`
//!   struct bars bare literal construction from outside its `impl` (E0060), while an otherwise-
//!   identical struct without the directive (`Loose`) constructs freely; and a recipe door
//!   (`json.try_parse::<Config>`) materializes the struct and runs its native `validate`, rejecting a
//!   bad value identically on both backends. The validator running composes Slice C's
//!   `traits:["Validate"]` (→ `satisfies(Validate)`) with a native `validate` method body.
//!
//! Integration tests (own process) because the fixture installs into the process-global default
//! registry — once per process — the single-registry path the CLI uses.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    AttrTarget, EnumBacking, ExtEnum, ExtField, ExtFn, ExtModule, ExtStruct, ExtTypeDirective,
    ExtVariant, Extension, FieldedKind, NativeOut, NativeValue, RetTy, Scalar, SigType,
    VariantValue,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The native @semantic enum + a non-semantic control ------------------------------------------

/// A `@semantic` enum — its fieldless variants are role names. Carries the directive through
/// [`ExtEnum::directives`]; the checker seeds it into `semantic_enums`, so `roles_of::<Stage>()`
/// (which *requires* a `@semantic` enum) type-checks.
const STAGE: ExtEnum = ExtEnum {
    name: "Stage",
    namespace: "cfg",
    variants: &[
        ExtVariant {
            name: "Alpha",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Beta",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Stable",
            fields: &[],
            value: VariantValue::None,
        },
    ],
    backing: EnumBacking::None,
    directives: &[ExtTypeDirective::Semantic],
    ..ExtEnum::DEFAULTS
};

/// The control: identical shape, but **no** `@semantic` directive. `roles_of::<Plain>()` must be
/// rejected — the only difference from `Stage` is the directive, so the rejection proves the
/// directive is what registers the semantic status.
const PLAIN: ExtEnum = ExtEnum {
    name: "Plain",
    namespace: "cfg",
    variants: &[
        ExtVariant {
            name: "One",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Two",
            fields: &[],
            value: VariantValue::None,
        },
    ],
    backing: EnumBacking::None,
    ..ExtEnum::DEFAULTS
};

// --- The native @validated struct + a non-validated control --------------------------------------

/// A `@validated` value struct with a public `port` field. It advertises the built-in `Validate`
/// trait (Slice C — so `satisfies(Validate)` is true and a recipe door gains a validator) and answers
/// a native `validate(): Result<void, string>` through its dispatch. The `@validated` directive
/// (via [`ExtFielded::directives`]) additionally bars bare literal construction outside its own `impl`
/// (E0060).
const CONFIG: ExtStruct = ExtStruct {
    name: "Config",
    namespace: "cfg",
    fields: &[ExtField {
        name: "port",
        ty: SigType::Int,
        is_public: true,
        is_mut: false,
    }],
    methods: &[ExtFn {
        name: "validate",
        params: &[],
        ret: RetTy::Concrete(SigType::Result(&SigType::Unit, &SigType::String)),
    }],
    dispatch: config_dispatch,
    traits: &["Validate"],
    directives: &[ExtTypeDirective::Validated],
    ..ExtStruct::STRUCT_DEFAULTS
};

/// A native `@attribute` value struct — the native analogue of a `.noe` `@attribute(Function, Method)
/// struct Route { path: string }`. Carries the directive through [`ExtFielded::directives`] with a
/// placement list; the checker seeds it into `attributes` (E0029 opt-in) + `attachable` (E0030
/// placement) keyed on `cfg.Route`, and its field is its construction contract — so `#[Route("/x")]`
/// on a fn is accepted and reflected exactly as a `.noe` `@attribute` struct.
const ROUTE: ExtStruct = ExtStruct {
    name: "Route",
    namespace: "cfg",
    fields: &[ExtField {
        name: "path",
        ty: SigType::String,
        is_public: true,
        is_mut: false,
    }],
    directives: &[ExtTypeDirective::Attribute(&[
        AttrTarget::Function,
        AttrTarget::Method,
    ])],
    ..ExtStruct::STRUCT_DEFAULTS
};

/// The control: the same shape, but **no** `@validated` directive (and no validator). Bare literal
/// construction from outside is legal — the difference from `Config` isolates the directive.
const LOOSE: ExtStruct = ExtStruct {
    name: "Loose",
    namespace: "cfg",
    fields: &[ExtField {
        name: "port",
        ty: SigType::Int,
        is_public: true,
        is_mut: false,
    }],
    ..ExtStruct::STRUCT_DEFAULTS
};

/// A native **constructor** for `Config` — the sanctioned build path for a `@validated` type (native
/// code is exempt from E0060, like a `.noe` in-`impl` constructor). Returns a real `Config` value
/// (`NativeOut::Instance` with `FieldedKind::Struct`).
const KIT_FNS: &[ExtFn] = &[ExtFn {
    name: "make",
    params: &[SigType::Int],
    ret: RetTy::Concrete(SigType::Named("Config")),
}];

fn kit_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let arg_int = |i: usize| -> i64 {
        match args.get(i) {
            Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
            _ => 0,
        }
    };
    match func {
        "make" => Ok(NativeOut::Instance {
            class: "Config".to_string(),
            fields: vec![(
                "port".to_string(),
                NativeOut::Scalar(Scalar::Int(arg_int(0))),
            )],
            kind: FieldedKind::Struct,
        }),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

/// `Config`'s validator: reject a port outside `1..=65535`. Returns a real `Result<void, string>`
/// (`NativeOut::Ok(Unit)` / `NativeOut::Err(Str)`), the shape `run_validator` reads on both backends.
fn config_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let port = match recv {
        NativeValue::Instance { fields, .. } => fields
            .iter()
            .find(|(k, _)| k == "port")
            .and_then(|(_, v)| match v {
                NativeValue::Scalar(Scalar::Int(n)) => Some(*n),
                _ => None,
            })
            .unwrap_or(0),
        _ => 0,
    };
    match method {
        "validate" => {
            if !(1..=65535).contains(&port) {
                Ok(NativeOut::Err(Box::new(NativeOut::Str(format!(
                    "port {port} out of range"
                )))))
            } else {
                Ok(NativeOut::Ok(Box::new(NativeOut::Unit)))
            }
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

// --- The fixture extension -----------------------------------------------------------------------

struct CfgExtension;

impl Extension for CfgExtension {
    fn name(&self) -> &'static str {
        "cfg"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "kit",
            functions: KIT_FNS,
            dispatch: kit_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn enums(&self) -> &'static [ExtEnum] {
        &[STAGE, PLAIN]
    }
    fn structs(&self) -> &'static [ExtStruct] {
        &[CONFIG, LOOSE, ROUTE]
    }
}

static CFG: CfgExtension = CfgExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&CFG]));
}

/// Every fixture test touches the shared process-global registry; each holds this for its whole run
/// so the installs/counters do not race (mirrors `ext_struct_seam.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// --- Helpers -------------------------------------------------------------------------------------

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero). Returns the shared stdout.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_directive_seam.noe", program);
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
        "backends must agree on the native-directive program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// Check a program and return its diagnostic codes (for the negative site/gate assertions).
#[track_caller]
fn check_codes(program: &str) -> Vec<String> {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_directive_seam.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    checked
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

// --- @semantic -----------------------------------------------------------------------------------

#[test]
fn native_semantic_enum_is_a_role_vocabulary() {
    let _guard = SERIAL.lock().unwrap();
    // `roles_of::<Stage>()` requires a `@semantic` enum — it type-checks and runs (empty index) only
    // because the native `Stage` carries the `@semantic` directive. Both backends agree on the count.
    let stdout = run_both_agree("use cfg.Stage\necho roles_of::<Stage>().len()\n");
    assert_eq!(
        stdout.trim(),
        "0",
        "roles_of over a native @semantic enum yields an empty index"
    );

    // The control: the otherwise-identical `Plain` enum lacks the directive, so the same query is
    // rejected — proving the directive is exactly what confers the semantic status.
    let codes = check_codes("use cfg.Plain\necho roles_of::<Plain>().len()\n");
    assert!(
        codes.iter().any(|c| c == "E0031"),
        "roles_of over a NON-@semantic native enum must be rejected (E0031); got {codes:?}"
    );
}

// --- @validated ----------------------------------------------------------------------------------

#[test]
fn native_validated_struct_bars_construction_and_validates_at_a_door() {
    let _guard = SERIAL.lock().unwrap();

    // The directive's static job: a bare literal of a `@validated` type outside its own `impl` is
    // E0060. The `Loose` control (same fields, no directive) constructs freely.
    let bad = check_codes("use cfg.Config\nx = Config { port: 8080 }\necho x.port\n");
    assert!(
        bad.iter().any(|c| c == "E0060"),
        "bare construction of a native @validated struct must be E0060; got {bad:?}"
    );
    let ok = check_codes("use cfg.Loose\nx = Loose { port: 8080 }\necho x.port\n");
    assert!(
        ok.is_empty(),
        "the non-@validated control must construct cleanly; got {ok:?}"
    );

    // The directive's runtime composition: a `Config` built through the sanctioned door (the native
    // `kit.make` constructor, exempt from E0060) runs its native `validate` — which advertises the
    // built-in `Validate` trait (Slice C) and answers through the type's dispatch — so a bad value is
    // a recoverable `Err` and a good one an `Ok`, identically on both backends.
    let stdout = run_both_agree(
        "use cfg.kit\n\
         fn msg(port: int): string {\n\
         \x20   return match kit.make(port).validate() {\n\
         \x20       Ok(_) => \"ok: ${port}\",\n\
         \x20       Err(m) => \"bad: ${m}\",\n\
         \x20   }\n\
         }\n\
         echo msg(8080)\n\
         echo msg(70000)\n",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec!["ok: 8080", "bad: port 70000 out of range"],
        "the native validator must run and reject a bad value identically on both backends"
    );
}

// --- @attribute ----------------------------------------------------------------------------------

#[test]
fn native_attribute_struct_gates_application_and_placement() {
    let _guard = SERIAL.lock().unwrap();

    // The directive's opt-in: `#[Route(...)]` is accepted on a fn only because the native `Route`
    // struct carries `@attribute` — resolved through the same `use` machinery a `.noe` attribute is
    // (`use cfg.Route` binds `Route -> cfg.Route`, the checker's gate keys on it). Construction is the
    // struct's fields, so a well-formed `#[Route("/x")]` checks clean.
    let ok = check_codes("use cfg.Route\n#[Route(\"/x\")]\nfn handler(): void { return }\n");
    assert!(
        ok.is_empty(),
        "a native @attribute applied at a permitted site must check clean; got {ok:?}"
    );

    // No `use`: there is no global attribute namespace, so a bare `#[Route]` is the unknown-attribute
    // error (E0029) — the native attribute is namespace-scoped exactly like a `.noe` one.
    let bare = check_codes("#[Route(\"/x\")]\nfn handler(): void { return }\n");
    assert!(
        bare.iter().any(|c| c == "E0029"),
        "a native @attribute applied WITHOUT its `use` must be E0029; got {bare:?}"
    );

    // Placement: `Route` declared `@attribute(Function, Method)`, so applying it to a **struct** is
    // E0030. This is the AttrTarget payload doing its job — the native placement list seeds the same
    // gate a `.noe` `@attribute(Function, Method)` does.
    let misplaced = check_codes("use cfg.Route\n#[Route(\"/x\")]\nstruct S { id: int }\n");
    assert!(
        misplaced.iter().any(|c| c == "E0030"),
        "a native @attribute at a forbidden site must be E0030; got {misplaced:?}"
    );

    // The construction contract is the struct's fields: a wrong-typed argument is E0007, identical to
    // a `.noe` `@attribute` struct's construction gate.
    let badarg = check_codes("use cfg.Route\n#[Route(42)]\nfn handler(): void { return }\n");
    assert!(
        badarg.iter().any(|c| c == "E0007"),
        "a wrong-typed native @attribute argument must be E0007; got {badarg:?}"
    );
}
