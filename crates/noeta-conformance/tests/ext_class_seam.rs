//! **Native-declared classes** (native-extensibility S2): an extension *outside* `std` declares real
//! language **classes** — TRUE reference types with identity, fields, native state, and a
//! destructor — and a program constructs them (natively and in source), reads/mutates their fields,
//! aliases them (`==` is identity), passes them into native code, and lets the collector run their
//! destructor on collection.
//!
//! A synthetic extension rather than an `std` consumer, deliberately (mirroring `ext_enum_seam.rs`):
//! the whole path is registry-driven, so nothing about `Handle`/`Point`/`Guard` is known to the
//! checker or either backend except its [`ExtClass`]/[`ExtType`] declaration. The corpus's
//! `differential_backends_agree` oracle cannot reach it (std declares no native class), so the
//! differential assertion lives here: the tree-walker reference and the bytecode VM must agree on the
//! materialized class values (identity, fields, reference semantics), which is what proves
//! `NativeOut::Instance` builds an identical class-kind object on both sides.
//!
//! **The load-bearing tests** are the destructor ones: a native class's cleanup is the RAII `Drop` of
//! its extern-handle field, run by the RC + cycle collector on collection. `destructor_fires_on_collection`
//! proves the linear last-reference path; `mutual_reference_cycle_is_reclaimed_and_both_destructors_fire`
//! proves the cycle-collector path — both with the leak oracle at zero. These are what distinguish a
//! true reference class from a value struct.
//!
//! An **integration test** (own process) because the fixture installs into the process-global default
//! registry — once per process — the single-registry path the CLI uses.

use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtClass, ExtField, ExtFn, ExtModule, ExtType, Extension, NativeOut, NativeValue, RetTy,
    Scalar, SigType,
};
use noeta_stdlib::{ExternBox, ExternValue, Host, StdError};
use noeta_vm::VmBackend;

// --- The native-state handle: an extern value whose Rust `Drop` is the class's destructor ---------

/// Every [`GuardBox`] dropped bumps this — the observable side effect of a native class's destructor
/// firing on collection. Reset before each measured run (the two backends share the process-global
/// counter, so a run reads its own delta only after a reset).
static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The native state a `Handle` owns: an extern-handle field whose `Drop` is the destructor. Held in a
/// class field (`guard: fx.Guard`); when the object is collected — last reference or destructor-free
/// cycle reclamation — the field's box drops and this runs. This is the whole RAII-destructor story:
/// no host-coupled finalizer, just a self-contained `Drop` the collector runs on free.
#[derive(Debug)]
struct GuardBox;

/// Every fixture test touches the process-global `DROP_COUNT` and the shared registry, and Rust runs
/// tests in parallel by default — so a destructor-count assertion would see other tests' `Handle`
/// drops. Each test holds this for its whole run + assertions, serializing them.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Drop for GuardBox {
    fn drop(&mut self) {
        DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

impl ExternValue for GuardBox {
    fn type_identity(&self) -> &'static str {
        "fx.Guard"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<GuardBox>().is_some()
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<guard>")
    }
    // Cloning creates a second `GuardBox` (hence a second `Drop`) — a native class holding this state
    // is never cloned in these tests (reference semantics alias, and no isolate promotion), so the
    // drop count stays exact.
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(GuardBox)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

const GUARD: ExtType = ExtType {
    name: "Guard",
    namespace: "fx",
    ..ExtType::DEFAULTS
};

// --- The native classes --------------------------------------------------------------------------

/// A resource class: native state (`guard`, private — the destructor rides on its `Drop`), a public
/// read-only `label`, and a public-mutable `peer` (a `dyn` link used to form a cycle). Constructed
/// **natively** (`kit.open`), because the language cannot fabricate the extern handle.
const HANDLE: ExtClass = ExtClass {
    name: "Handle",
    namespace: "fx",
    fields: &[
        ExtField {
            name: "guard",
            ty: SigType::Named("Guard"),
            is_public: false,
            is_mut: false,
        },
        ExtField {
            name: "label",
            ty: SigType::String,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "peer",
            ty: SigType::Dyn,
            is_public: true,
            is_mut: true,
        },
    ],
};

/// A pure-data class: all-public fields, **source-constructible** (`Point { x, y }`) — proves the
/// backend `TypeInfo::Class`/`TypeDef` seeding and reference (aliasing) semantics without any native
/// state, and crosses arg-IN (`kit.sum`) as a `NativeValue::Instance`.
const POINT: ExtClass = ExtClass {
    name: "Point",
    namespace: "fx",
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
};

const FX_CLASSES: &[ExtClass] = &[HANDLE, POINT];

// --- The module that constructs and consumes them ------------------------------------------------

const KIT_FNS: &[ExtFn] = &[
    // Native constructor: returns a real `Handle` instance (return-OUT of a class value).
    ExtFn {
        name: "open",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Handle")),
    },
    // Takes a `Point` back (arg-IN of a class instance) and reduces its fields.
    ExtFn {
        name: "sum",
        params: &[SigType::Named("Point")],
        ret: RetTy::Concrete(SigType::Int),
    },
];

fn kit_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "open" => {
            let label = match args.first() {
                Some(NativeValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            // A real class instance: the native-state `guard` (an extern whose `Drop` is the
            // destructor), the public `label`, and `peer` initialized to unit. Field order matches
            // the `ExtClass` declaration.
            Ok(NativeOut::Instance {
                class: "Handle".to_string(),
                fields: vec![
                    (
                        "guard".to_string(),
                        NativeOut::Extern(ExternBox::new(GuardBox)),
                    ),
                    ("label".to_string(), NativeOut::Str(label)),
                    ("peer".to_string(), NativeOut::Unit),
                ],
            })
        }
        "sum" => {
            // arg-IN: a class instance crosses as `NativeValue::Instance`; read its fields by name.
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
        &[GUARD]
    }
    fn classes(&self) -> &'static [ExtClass] {
        FX_CLASSES
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
/// residency unchanged (leak oracle zero — measured exactly as `noeta-conformance`'s own oracle
/// does). Returns nothing; the caller asserts stdout separately.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_class_seam.noe", program);
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
        "backends must agree on the native-class program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

// --- Tests ---------------------------------------------------------------------------------------

/// Native construction + source construction + field read/mutate + **reference identity** (`==` is
/// identity, aliasing is shared) + arg-IN of a class instance — all differential and leak-free.
const IDENTITY_PROGRAM: &str = r#"
use fx.kit
use fx.Handle
use fx.Point

// Native construction returns a real class instance; read a public field.
h = kit.open("alpha")
echo h.label

// Reference identity: `g = h` aliases the SAME instance — a mutation through one is visible through
// the other, and `==` is identity.
g = h
g.peer = "linked"
echo h.peer
echo h == g

// Two distinct native constructions are NOT identical (reference identity, not structural).
echo kit.open("x") == kit.open("y")

// Source-constructed class: field read, mut-field write, arg-IN into native code, and aliasing.
p = Point { x: 3, y: 4 }
echo p.x
p.y = 10
echo p.y
echo kit.sum(p)

q = p
q.y = 99
echo p.y
"#;

#[test]
fn native_classes_construct_alias_and_round_trip_identically_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(IDENTITY_PROGRAM);
    assert_eq!(stdout, "alpha\nlinked\ntrue\nfalse\n3\n10\n13\n99\n");
}

/// **Case 3 — destructor fires on linear collection.** A single `Handle` goes out of scope at program
/// end; its extern-handle field's `Drop` runs (the destructor), and the heap returns to baseline. Run
/// on both backends, each from a reset counter, so the RAII destructor is proven on each.
#[test]
fn destructor_fires_on_collection() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ensure_installed();
    const PROGRAM: &str = "use fx.kit\nh = kit.open(\"solo\")\necho h.label\n";

    for backend in ["eval", "vm"] {
        DROP_COUNT.store(0, Ordering::SeqCst);
        let (stdout, exit, leaked) = run_one(backend, PROGRAM);
        assert_eq!(stdout, "solo\n", "[{backend}] stdout");
        assert_eq!(exit, 0, "[{backend}] exit");
        assert_eq!(leaked, 0, "[{backend}] leak oracle must be zero");
        assert_eq!(
            DROP_COUNT.load(Ordering::SeqCst),
            1,
            "[{backend}] the one Handle's destructor must fire exactly once on collection"
        );
    }
}

/// **Case 4 — a mutual-reference cycle is reclaimed and BOTH destructors fire (leak oracle zero).**
/// Two `Handle`s reference each other (`a.peer = b; b.peer = a`); when the roots die the pair is an
/// unreachable cycle. The cycle collector reclaims it (the native class is destructor-free at the
/// language level — its cleanup is the extern field's `Drop`, run as the objects free), so both
/// guards drop and the heap returns to baseline. This is the case only a true reference type — not a
/// value struct — can pass, and the one flagged load-bearing for the RAII approach.
#[test]
fn mutual_reference_cycle_is_reclaimed_and_both_destructors_fire() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ensure_installed();
    const PROGRAM: &str = "\
use fx.kit

fn make_cycle(): void {
  a = kit.open(\"a\")
  b = kit.open(\"b\")
  a.peer = b
  b.peer = a
}

make_cycle()
echo \"done\"
";

    for backend in ["eval", "vm"] {
        DROP_COUNT.store(0, Ordering::SeqCst);
        let (stdout, exit, leaked) = run_one(backend, PROGRAM);
        assert_eq!(stdout, "done\n", "[{backend}] stdout");
        assert_eq!(exit, 0, "[{backend}] exit");
        assert_eq!(
            leaked, 0,
            "[{backend}] leak oracle must be zero — the cycle must be reclaimed"
        );
        assert_eq!(
            DROP_COUNT.load(Ordering::SeqCst),
            2,
            "[{backend}] both cyclic Handles' destructors must fire on reclamation"
        );
    }
}

/// Run one program on one backend, returning `(stdout, exit_code, heap_residency_delta)`. The delta
/// is the leak oracle: zero means the run released everything it allocated.
#[track_caller]
fn run_one(backend: &str, program: &str) -> (String, i32, i64) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_class_seam.noe", program);
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
