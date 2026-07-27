//! **Native-declared structs** (fielded unification): an extension *outside* `std` declares real
//! language **structs** — VALUE types with structural equality and copy-on-assign, the value-type
//! twin of the reference `ExtClass` (`ext_class_seam.rs`) — and a program constructs them (natively
//! and in source), reads/mutates their fields, compares them structurally, copies them, passes them
//! into native code, and dispatches their methods.
//!
//! A synthetic extension rather than an `std` consumer, deliberately (mirroring `ext_class_seam.rs`
//! and `ext_enum_seam.rs`): the whole path is registry-driven, so nothing about `Point2` is known to
//! the checker or either backend except its [`ExtStruct`] declaration. The corpus's
//! `differential_backends_agree` oracle cannot reach it (std declares no native struct), so the
//! differential assertion lives here: the tree-walker reference and the bytecode VM must agree on the
//! materialized struct values (structural equality, copy-on-assign, fields), which is what proves
//! `NativeOut::Instance` with `FieldedKind::Struct` builds an identical struct-kind object on both
//! sides.
//!
//! **The load-bearing test** is [`native_structs_have_value_semantics`]: it proves structural
//! equality (`==` compares fields, NOT identity) and copy-on-assign (a mutation through one binding
//! does not affect a copy) — exactly the two properties that distinguish a value struct from a
//! reference class, and the pair a class fixture's identity/aliasing tests would *fail*. The gate's
//! `ExtFielded::kind` constraint anchors its exerciser here. A second load-bearing case is the
//! **in-place-mutation rejection**: a struct dispatch that returns a `NativeOut::InstanceUpdate` is a
//! runtime error on both backends (a value type has no in-place mutation).
//!
//! An **integration test** (own process) because the fixture installs into the process-global default
//! registry — once per process — the single-registry path the CLI uses.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtField, ExtFn, ExtModule, ExtStruct, Extension, FieldedKind, NativeOut, NativeValue, RetTy,
    Scalar, SigType,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The native value struct ---------------------------------------------------------------------

/// A pure-data **value struct**: all-public fields, **source-constructible** (`Point2 { x, y }`).
/// Unlike the reference `Point` class (`ext_class_seam.rs`), a `Point2` has structural equality and
/// copy-on-assign — value semantics the object model derives from its struct-kind shape. `y` is `mut`
/// (assignable), `x` is not; both public. Declared through [`Extension::structs`] with
/// `..ExtStruct::STRUCT_DEFAULTS`, so its [`ExtFielded::kind`] is [`FieldedKind::Struct`].
const POINT2: ExtStruct = ExtStruct {
    name: "Point2",
    namespace: "geo",
    fields: &[
        ExtField {
            name: "x",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "y",
            ty: SigType::Int,
            is_public: true,
            is_mut: true,
        },
    ],
    methods: &[
        // A value-type "mutator": `shifted(dx, dy)` returns a **new** `Point2` (value semantics — it
        // does NOT mutate the receiver), proving a struct method dispatches to native code and its
        // return crosses back as a fresh struct value.
        ExtFn {
            param_names: &[],
            name: "shifted",
            params: &[SigType::Int, SigType::Int],
            ret: RetTy::Concrete(SigType::Named("Point2")),
        },
        // A deliberately-wrong method: it returns a `NativeOut::InstanceUpdate` (in-place mutation),
        // which is CLASS-ONLY. Both backends must reject it at runtime for a struct receiver — a value
        // type has no in-place mutation. Proves the `FieldedKind::Struct` in-place-mutation guard.
        ExtFn {
            param_names: &[],
            name: "bad_mutate",
            params: &[],
            ret: RetTy::Concrete(SigType::Unit),
        },
    ],
    dispatch: point2_dispatch,
    ..ExtStruct::STRUCT_DEFAULTS
};

/// `Point2`'s instance-method dispatch. `shifted` reads the receiver's fields off the marshalled
/// `NativeValue::Instance` and returns a **new** `Point2` value (`NativeOut::Instance` with
/// `FieldedKind::Struct`). `bad_mutate` returns an `InstanceUpdate` write-set — which the backend
/// must reject for a struct, so the mutation never happens.
fn point2_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let field = |name: &str| -> i64 {
        match recv {
            NativeValue::Instance { fields, .. } => fields
                .iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| match v {
                    NativeValue::Scalar(Scalar::Int(n)) => Some(*n),
                    _ => None,
                })
                .unwrap_or(0),
            _ => 0,
        }
    };
    let arg_int = |i: usize| -> i64 {
        match args.get(i) {
            Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
            _ => 0,
        }
    };
    match method {
        "shifted" => Ok(new_point2(field("x") + arg_int(0), field("y") + arg_int(1))),
        // Value types have no in-place mutation — the backend rejects this `InstanceUpdate`.
        "bad_mutate" => Ok(NativeOut::InstanceUpdate {
            writes: vec![("y".to_string(), NativeOut::Scalar(Scalar::Int(-1)))],
            ret: Box::new(NativeOut::Unit),
        }),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

/// Build a `Point2` value (`NativeOut::Instance` with `FieldedKind::Struct`) — the return-OUT shape
/// both a native constructor and a struct method use, materialized to a struct-kind object.
fn new_point2(x: i64, y: i64) -> NativeOut {
    NativeOut::Instance {
        class: "Point2".to_string(),
        fields: vec![
            ("x".to_string(), NativeOut::Scalar(Scalar::Int(x))),
            ("y".to_string(), NativeOut::Scalar(Scalar::Int(y))),
        ],
        kind: FieldedKind::Struct,
    }
}

// --- The module that constructs and consumes them ------------------------------------------------

const KIT_FNS: &[ExtFn] = &[
    // Native constructor: returns a real `Point2` value at the origin (return-OUT of a struct value).
    ExtFn {
        param_names: &[],
        name: "origin",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Point2")),
    },
    // Native constructor with args.
    ExtFn {
        param_names: &[],
        name: "make",
        params: &[SigType::Int, SigType::Int],
        ret: RetTy::Concrete(SigType::Named("Point2")),
    },
    // Takes a `Point2` back (arg-IN of a struct value) and reduces its fields — proves a native
    // value-struct marshals INTO a dispatch as a `NativeValue::Instance` (the registry-gated struct
    // arm), exactly like a class does, and NOT as a lossy scalar `Object`.
    ExtFn {
        param_names: &[],
        name: "sum",
        params: &[SigType::Named("Point2")],
        ret: RetTy::Concrete(SigType::Int),
    },
    // Takes ANY value (`dyn`) and reports which marshalled shape it arrived as — the direct probe of
    // the **marshal split** (R1): a native fielded struct must cross as `Instance`, while a *user*
    // value-struct (not in the registry) must keep crossing as the all-scalar `Object`, unchanged.
    ExtFn {
        param_names: &[],
        name: "describe_arg",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::String),
    },
];

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
        "origin" => Ok(new_point2(0, 0)),
        "make" => Ok(new_point2(arg_int(0), arg_int(1))),
        "sum" => {
            let field = |name: &str| -> i64 {
                match args.first() {
                    Some(NativeValue::Instance { fields, .. }) => fields
                        .iter()
                        .find(|(k, _)| k == name)
                        .and_then(|(_, v)| match v {
                            NativeValue::Scalar(Scalar::Int(n)) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(0),
                    _ => 0,
                }
            };
            Ok(NativeOut::Scalar(Scalar::Int(field("x") + field("y"))))
        }
        // Report the marshalled shape KIND the argument arrived as — the observable side of the
        // split. (Only the discriminant is reported: a `NativeValue::Object`'s `type_name` string is
        // a pre-existing cosmetic difference between the two backends for a user struct, unrelated to
        // the split; the native `Instance`'s `class` short name, by contrast, agrees on both.)
        "describe_arg" => {
            let tag = match args.first() {
                Some(NativeValue::Instance { class, .. }) => format!("instance:{class}"),
                Some(NativeValue::Object { .. }) => "object".to_string(),
                Some(NativeValue::Opaque(_)) => "opaque".to_string(),
                _ => "other".to_string(),
            };
            Ok(NativeOut::Str(tag))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct GeoExtension;

impl Extension for GeoExtension {
    fn name(&self) -> &'static str {
        "geo"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "kit",
            functions: KIT_FNS,
            dispatch: kit_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn structs(&self) -> &'static [ExtStruct] {
        &[POINT2]
    }
}

static GEO: GeoExtension = GeoExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&GEO]));
}

/// Every fixture test touches the shared process-global registry; each holds this for its whole run
/// so the installs/counters do not race (mirrors `ext_class_seam.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// --- Helpers -------------------------------------------------------------------------------------

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero). Returns the shared stdout.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_struct_seam.noe", program);
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
        "backends must agree on the native-struct program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// Run one program on one backend, returning `(stdout, exit_code, heap_residency_delta)`. The delta
/// is the leak oracle: zero means the run released everything it allocated.
#[track_caller]
fn run_one(backend: &str, program: &str) -> (String, i32, i64) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_struct_seam.noe", program);
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
    match backend {
        "eval" => {
            let before = noeta_eval::live_count();
            let r = noeta_conformance::reference::reference_run(
                &parsed.0.program,
                checked.sites.clone(),
            );
            let delta = (noeta_eval::live_count() - before) as i64;
            (r.stdout, r.exit_code, delta)
        }
        "vm" => {
            let module = noeta_db::bytecode(&db, src)
                .0
                .as_ref()
                .expect("program compiles to bytecode")
                .clone();
            let before = noeta_value::live_count() as i64;
            let r = VmBackend::new().run_module(&module);
            let delta = noeta_value::live_count() as i64 - before;
            (r.stdout, r.exit_code, delta)
        }
        other => panic!("unknown backend {other}"),
    }
}

// --- Tests ---------------------------------------------------------------------------------------

/// Native construction (`kit.origin`/`kit.make` → `Point2`), source construction (`Point2 { x, y }`),
/// field read, and arg-IN of a struct value (`kit.sum`) — all differential and leak-free. Proves the
/// backends' `TypeInfo::Struct`/`TypeDef` seeding and that a native-constructed and a
/// source-constructed struct interchange (same struct-kind shape).
const CONSTRUCT_PROGRAM: &str = r#"
use geo.kit
use geo.Point2

// Native construction returns a real struct value; read its fields.
o = kit.origin()
echo o.x
echo o.y

// Source-constructed struct: field read, arg-IN into native code.
p = Point2 { x: 3, y: 4 }
echo p.x
echo kit.sum(p)

// Native construction with args, then arg-IN — a native-built struct round-trips.
m = kit.make(5, 6)
echo m.x
echo m.y
echo kit.sum(m)
"#;

#[test]
fn native_structs_construct_and_round_trip_identically_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(CONSTRUCT_PROGRAM);
    assert_eq!(stdout, "0\n0\n3\n7\n5\n6\n11\n");
}

/// **The load-bearing test — VALUE semantics** (the gate's `ExtFielded::kind` exerciser). Two
/// properties a value struct has and a reference class does NOT:
///
/// * **structural equality** — `==` compares fields, so two independently-built `Point2`s with
///   equal fields are equal (a class's `==` is identity, so they would be `false`);
/// * **copy-on-assign** — `b = a` is an independent copy, so mutating `b.y` leaves `a.y` unchanged
///   (a class aliases, so the mutation would be visible through `a`).
///
/// Both backends must agree, and the heap must return to baseline.
const VALUE_SEMANTICS_PROGRAM: &str = r#"
use geo.Point2

// Structural equality: equal fields ⇒ equal (value type, not reference identity).
echo Point2 { x: 1, y: 2 } == Point2 { x: 1, y: 2 }
echo Point2 { x: 1, y: 2 } == Point2 { x: 1, y: 3 }

// Copy-on-assign: `b` is an independent copy of `a`; mutating `b.y` does NOT affect `a`. A value
// `struct` is updated by REBINDING (the field-set needs a `mut` binding) — itself proof of value
// semantics: a reference class would mutate in place through the alias without `mut`.
a = Point2 { x: 1, y: 2 }
mut b = a
b.y = 99
echo a.y
echo b.y
"#;

#[test]
fn native_structs_have_value_semantics() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(VALUE_SEMANTICS_PROGRAM);
    assert_eq!(stdout, "true\nfalse\n2\n99\n");
}

/// **A native struct's INSTANCE METHOD dispatches to native code and returns a NEW value.**
/// `p.shifted(dx, dy)` has no hoisted `.noe` body; both backends' `CallMethod` Object arm route it to
/// the struct's native `dispatch`, which reads the receiver's fields off the marshalled
/// `NativeValue::Instance` and returns a fresh `Point2`. Value semantics: the original `p` is
/// unchanged (the method returns a new value rather than mutating in place). The backends must build
/// the identical value.
const METHOD_PROGRAM: &str = r#"
use geo.Point2

p = Point2 { x: 3, y: 4 }
q = p.shifted(10, 20)
echo q.x
echo q.y

// The receiver is untouched — a value-struct method returns a NEW value, it does not mutate self.
echo p.x
echo p.y
"#;

#[test]
fn native_struct_method_returns_new_value_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(METHOD_PROGRAM);
    assert_eq!(stdout, "13\n24\n3\n4\n");
}

/// **In-place-mutation rejection.** `p.bad_mutate()`'s dispatch returns a `NativeOut::InstanceUpdate`
/// write-set — an in-place mutation, which is CLASS-ONLY. Because `Point2` is a `FieldedKind::Struct`
/// (a value type), both backends must reject it at runtime and abort with a nonzero exit before any
/// mutation, and the aborted run must still leak nothing. Proves the struct-kind in-place-mutation
/// guard fires identically on both sides.
#[test]
fn native_struct_in_place_mutation_is_rejected_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ensure_installed();
    const PROGRAM: &str =
        "use geo.Point2\np = Point2 { x: 3, y: 4 }\np.bad_mutate()\necho \"unreached\"\n";

    for backend in ["eval", "vm"] {
        let (stdout, exit, leaked) = run_one(backend, PROGRAM);
        assert_ne!(exit, 0, "[{backend}] a struct in-place mutation must abort");
        assert_eq!(
            stdout, "",
            "[{backend}] nothing after the rejected mutation runs"
        );
        assert_eq!(
            leaked, 0,
            "[{backend}] the aborted run must still leak nothing"
        );
    }
}

/// **The marshal split is non-regressing (R1).** A native fielded **struct** marshals arg-IN as a
/// full `NativeValue::Instance` (the registry-gated struct arm), while a **user** value-struct —
/// which is `is_struct` too but is NOT a registered native type — keeps crossing as the all-scalar
/// `NativeValue::Object`, exactly as before the unification. This is the direct proof that widening
/// the marshal to admit native structs did not sweep user value-structs (Vec3-shaped) off their
/// existing `Object` path. Both backends must agree.
const MARSHAL_SPLIT_PROGRAM: &str = r#"
use geo.kit
use geo.Point2

struct UserVec {
  a: int
  b: int
}

// A native fielded struct resolves in the registry → crosses as a full Instance.
echo kit.describe_arg(Point2 { x: 1, y: 2 })
// A user value-struct is NOT a native type → stays on the all-scalar Object path (unchanged).
echo kit.describe_arg(UserVec { a: 3, b: 4 })
"#;

#[test]
fn native_struct_and_user_struct_take_different_marshal_paths_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(MARSHAL_SPLIT_PROGRAM);
    assert_eq!(stdout, "instance:Point2\nobject\n");
}
