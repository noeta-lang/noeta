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
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtClass, ExtField, ExtFn, ExtModule, ExtType, Extension, FieldedKind, NativeOut, NativeValue,
    RetTy, Scalar, SigType,
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

// --- The BALANCED native-state handle (boundary 2: by-value arg-IN of a native-state class) --------
//
// `GuardBox` above is deliberately *unbalanced* (its `Drop` counts a destructor firing; its
// `clone_box` fabricates a fresh box that does NOT count) so the destructor tests read an exact
// firing count with no clone noise. That same asymmetry is what made a native-state class crossing
// **arg-IN** unobservable: marshalling the instance `clone_box`es its extern-handle field into the
// seam, and the clone drops when the marshalled `NativeValue` drops — with `GuardBox` the drop would
// register as a spurious destructor. `BalancedGuard` closes that gap: EVERY live box (a born one or a
// `clone_box`ed one) increments [`BAL_LIVE`]; every `Drop` decrements it. So the clone/drop pair a
// by-value arg-IN performs is Rc/Arc-balanced and nets zero — residency returns to baseline AND
// `BAL_LIVE` returns to zero, which is exactly the round-trip proof the arg-IN of a state-holding
// class was missing. [`BAL_CLONES`] separately records that the `clone_box` actually fired, so the
// test proves it *exercised* the extern-field marshalling rather than trivially passing.

/// Net live `BalancedGuard` boxes: `+1` on birth, `+1` on `clone_box`, `-1` on `Drop`. Zero at a
/// clean run's end (every clone matched by a drop, every construction by its destructor).
static BAL_LIVE: AtomicIsize = AtomicIsize::new(0);
/// How many times `clone_box` fired — the arg-IN marshalling of the extern-handle field. Nonzero
/// proves the by-value cross actually cloned the native state (not that it was skipped).
static BAL_CLONES: AtomicUsize = AtomicUsize::new(0);

/// A balanced native-state handle: reference-counted-style bookkeeping so a clone/drop pair nets
/// zero. Unlike [`GuardBox`], a clone is a *live* box that must be dropped, so [`BAL_LIVE`] tracks
/// residency rather than a one-way destructor tally.
#[derive(Debug)]
struct BalancedGuard;

impl BalancedGuard {
    /// A freshly *constructed* guard (native construction, `kit.open_res`) — counts as one live box.
    fn born() -> BalancedGuard {
        BAL_LIVE.fetch_add(1, Ordering::SeqCst);
        BalancedGuard
    }
}

impl Drop for BalancedGuard {
    fn drop(&mut self) {
        BAL_LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ExternValue for BalancedGuard {
    fn type_identity(&self) -> &'static str {
        "fx.BGuard"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<BalancedGuard>().is_some()
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<bguard>")
    }
    // A clone is a real, live box (Rc/Arc-style): bump the live count AND record the clone. Its
    // matching `Drop` will decrement `BAL_LIVE`, so the pair balances. (Constructed inline rather
    // than via `born()` so the two accounting paths stay explicit — birth vs clone.)
    fn clone_box(&self) -> Box<dyn ExternValue> {
        BAL_CLONES.fetch_add(1, Ordering::SeqCst);
        BAL_LIVE.fetch_add(1, Ordering::SeqCst);
        Box::new(BalancedGuard)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

const BGUARD: ExtType = ExtType {
    name: "BGuard",
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
    // A native **instance method** (native-extensibility S3 / Pass 2a): `h.describe()` reads the
    // instance's `label` field off the marshalled receiver and returns a rendered string. Proves a
    // class-kind object's method call routes to the class's native `dispatch` in both backends.
    methods: &[ExtFn {
        param_names: &[],
        name: "describe",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: handle_dispatch,
    ..ExtClass::DEFAULTS
};

/// `Handle`'s instance-method dispatch — the native implementation reached by `h.describe()`. The
/// receiver crosses as the whole instance (`NativeValue::Instance`), so the method reads its `label`
/// field by name, exactly as a native fn reads a class value passed arg-IN.
fn handle_dispatch(
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
            Ok(NativeOut::Str(format!("handle:{label}")))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

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
    // **Boundary 1 — in-place instance mutation.** `bump()` mutates the `mut` field `y` in place
    // (returning the new `y`) via a `NativeOut::InstanceUpdate` write-set; `bad_bump_x()` tries to
    // write the *immutable* `x` and must be rejected at runtime.
    methods: &[
        ExtFn {
            param_names: &[],
            name: "bump",
            params: &[],
            ret: RetTy::Concrete(SigType::Int),
        },
        ExtFn {
            param_names: &[],
            name: "bad_bump_x",
            params: &[],
            ret: RetTy::Concrete(SigType::Unit),
        },
    ],
    dispatch: point_dispatch,
    ..ExtClass::DEFAULTS
};

/// `Point`'s instance-method dispatch (boundary 1). `bump` reads the current `y` off the snapshot
/// receiver, then returns a write-set setting `y = y + 1` **in place** on the live instance plus the
/// new value as its result. `bad_bump_x` deliberately writes the immutable `x` — the backend must
/// reject it (proving the `is_mut` guard), so it never actually mutates.
fn point_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
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
    match method {
        "bump" => {
            let next = field("y") + 1;
            Ok(NativeOut::InstanceUpdate {
                writes: vec![("y".to_string(), NativeOut::Scalar(Scalar::Int(next)))],
                ret: Box::new(NativeOut::Scalar(Scalar::Int(next))),
            })
        }
        // Targets the immutable `x` — the backend's `is_mut` guard must reject this before any write.
        "bad_bump_x" => Ok(NativeOut::InstanceUpdate {
            writes: vec![("x".to_string(), NativeOut::Scalar(Scalar::Int(-1)))],
            ret: Box::new(NativeOut::Unit),
        }),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

/// A state-holding class built to cross **arg-IN** (boundary 2): a native-state `bguard` (private —
/// the [`BalancedGuard`] whose clone/drop is balanced) plus a public `tag`. `kit.open_res`
/// constructs it natively; `kit.tag` receives the WHOLE instance by value and reads `tag` off the
/// marshalled receiver — the marshalling `clone_box`es `bguard` into the seam, the exact path
/// `Handle` (unbalanced) could not observe. Distinct from `Handle` so the destructor tests' exact
/// firing count is untouched.
const RES: ExtClass = ExtClass {
    name: "Res",
    namespace: "fx",
    fields: &[
        ExtField {
            name: "bguard",
            ty: SigType::Named("BGuard"),
            is_public: false,
            is_mut: false,
        },
        ExtField {
            name: "tag",
            ty: SigType::String,
            is_public: true,
            is_mut: false,
        },
    ],
    ..ExtClass::DEFAULTS
};

const FX_CLASSES: &[ExtClass] = &[HANDLE, POINT, RES];

// --- The module that constructs and consumes them ------------------------------------------------

const KIT_FNS: &[ExtFn] = &[
    // Native constructor: returns a real `Handle` instance (return-OUT of a class value).
    ExtFn {
        param_names: &[],
        name: "open",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Handle")),
    },
    // Takes a `Point` back (arg-IN of a class instance) and reduces its fields.
    ExtFn {
        param_names: &[],
        name: "sum",
        params: &[SigType::Named("Point")],
        ret: RetTy::Concrete(SigType::Int),
    },
    // Native constructor for the state-holding `Res` (return-OUT of a class carrying a balanced
    // extern-handle field).
    ExtFn {
        param_names: &[],
        name: "open_res",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Res")),
    },
    // **Boundary 2:** takes a state-holding `Res` BY VALUE (arg-IN) and reads its `tag` off the
    // marshalled instance. The marshalling `clone_box`es the `bguard` extern-handle field into the
    // seam; with `BalancedGuard` the clone/drop nets zero, so this round-trips leak-free.
    ExtFn {
        param_names: &[],
        name: "tag",
        params: &[SigType::Named("Res")],
        ret: RetTy::Concrete(SigType::String),
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
                kind: FieldedKind::Class,
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
        "open_res" => {
            let tag = match args.first() {
                Some(NativeValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            // A state-holding class instance: the balanced native-state `bguard` (born → one live
            // box, balanced by its `Drop`) plus a public `tag`. Field order matches the declaration.
            Ok(NativeOut::Instance {
                class: "Res".to_string(),
                fields: vec![
                    (
                        "bguard".to_string(),
                        NativeOut::Extern(ExternBox::new(BalancedGuard::born())),
                    ),
                    ("tag".to_string(), NativeOut::Str(tag)),
                ],
                kind: FieldedKind::Class,
            })
        }
        "tag" => {
            // **Boundary 2:** the WHOLE state-holding instance crosses arg-IN as `NativeValue::Instance`,
            // its `bguard` extern-handle field `clone_box`ed into the seam. Read the public `tag`.
            let tag = match args.first() {
                Some(NativeValue::Instance { fields, .. }) => fields
                    .iter()
                    .find(|(k, _)| k == "tag")
                    .and_then(|(_, v)| match v {
                        NativeValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Ok(NativeOut::Str(tag))
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
        &[GUARD, BGUARD]
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

/// **Pass 2a — a native class's INSTANCE METHOD dispatches to native code.** `h.describe()` on a
/// native `Handle` has no hoisted `.noe` body; both backends' `CallMethod` Object arm route it to
/// the class's native `dispatch`, which reads the instance's `label` field off the marshalled
/// receiver. The two backends must build the identical value — that is what proves the Object-arm
/// native-class branch matches on both sides (the ExtType extern seam's class twin).
const METHOD_PROGRAM: &str = r#"
use fx.kit
use fx.Handle

// A native method call on a native class instance dispatches to `handle_dispatch`.
h = kit.open("m")
echo h.describe()

// The same field, read directly, for parity with what the method read.
echo h.label
"#;

#[test]
fn native_class_method_dispatches_to_native_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(METHOD_PROGRAM);
    assert_eq!(stdout, "handle:m\nm\n");
}

/// **Boundary 1 — a native method mutates its instance IN PLACE.** `p.bump()` returns a
/// `NativeOut::InstanceUpdate` write-set; both backends apply it to the live receiver's `y` slot (the
/// same primitive `p.y = v` uses), so the mutation persists after the call AND is visible through an
/// alias (`q = p`). The backends must agree on every reading — that is what proves the write-set is
/// applied identically on both sides, and that in-place mutation preserves reference identity.
const MUTATE_PROGRAM: &str = r#"
use fx.Point

p = Point { x: 3, y: 4 }
// bump() returns the new y AND mutates the instance in place.
echo p.bump()
echo p.y

// Aliasing: q is the SAME instance; a mutation through p is visible through q.
q = p
echo p.bump()
echo q.y
"#;

#[test]
fn native_method_mutates_instance_in_place_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(MUTATE_PROGRAM);
    assert_eq!(stdout, "5\n5\n6\n6\n");
}

/// **Boundary 1 guard — a write to an IMMUTABLE field is rejected at runtime.** `p.bad_bump_x()`
/// returns a write-set targeting the non-`mut` field `x`; both backends must reject it (the ABI
/// mirrors the source-level E0022-family rule) and abort with a nonzero exit before any mutation —
/// `x` stays `3`. Proves the `is_mut` guard fires identically on both sides.
#[test]
fn native_method_write_to_immutable_field_is_rejected_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ensure_installed();
    const PROGRAM: &str =
        "use fx.Point\np = Point { x: 3, y: 4 }\np.bad_bump_x()\necho \"unreached\"\n";

    for backend in ["eval", "vm"] {
        let (stdout, exit, leaked) = run_one(backend, PROGRAM);
        assert_ne!(exit, 0, "[{backend}] an immutable-field write must abort");
        assert_eq!(
            stdout, "",
            "[{backend}] nothing after the rejected write runs"
        );
        assert_eq!(
            leaked, 0,
            "[{backend}] the aborted run must still leak nothing"
        );
    }
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

/// **Boundary 2 — by-value arg-IN of a native-state class round-trips leak-free.** A `Res` (holding a
/// native `bguard` extern handle) is constructed natively, then passed BY VALUE into `kit.tag`, which
/// reads a public field off the marshalled instance. Marshalling `clone_box`es the `bguard` field into
/// the seam; with the balanced (Rc/Arc-style) guard the clone/drop nets zero, so residency returns to
/// baseline on both backends. The unbalanced `GuardBox` could not *observe* this (a clone drop reads as
/// a spurious destructor) — `BalancedGuard` makes the round-trip measurable. Asserts:
///   * both backends agree + leak oracle zero (via `run_both_agree`),
///   * `BAL_LIVE` returns to zero — every cloned handle was dropped (no leak of the marshalled field),
///   * `BAL_CLONES > 0` — the extern-handle field was actually marshalled (the cross was exercised,
///     not trivially skipped).
#[test]
fn native_state_class_crosses_arg_in_by_value_leak_free() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ensure_installed();
    BAL_LIVE.store(0, Ordering::SeqCst);
    BAL_CLONES.store(0, Ordering::SeqCst);

    const PROGRAM: &str = r#"
use fx.kit
use fx.Res

r = kit.open_res("held")
echo r.tag
// Boundary 2: the whole native-state instance crosses arg-IN by value.
echo kit.tag(r)
"#;

    let stdout = run_both_agree(PROGRAM);
    assert_eq!(stdout, "held\nheld\n");

    // Every born/cloned balanced guard was dropped — the marshalled extern-handle field did not leak.
    assert_eq!(
        BAL_LIVE.load(Ordering::SeqCst),
        0,
        "balanced native state must return to zero residency after the by-value arg-IN round-trip"
    );
    // The cross actually marshalled the extern-handle field (across both backends' runs of the arg-IN).
    assert!(
        BAL_CLONES.load(Ordering::SeqCst) > 0,
        "the by-value arg-IN must have clone_box'd the native-state field (cross exercised, not skipped)"
    );
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

/// **Native-class reflection** (reflect-holes arc): a native class answers `field_specs_of` with its
/// real field schema, and the schema it advertises is exactly the one `construct` accepts.
///
/// The corpus cannot reach this: `std` declares no native *class*, so this fixture is the only place
/// the class half of native fielded reflection is exercised, and the assertion has to be differential
/// — a dynamically constructed class instance is built by two different code paths (the VM rebuilds an
/// interned shape, the tree-walker builds a `TypeDef`), and they must produce the same value.
///
/// Three facts are pinned, and each was a live bug before the seeding landed:
///
/// 1. The schema is reported at all, with **precise field types** — including `guard`'s, which is an
///    extern-handle type and reflects under its qualified identity (`fx.Guard`), the same name
///    `type_of` gives one of its values. A registry signature spells that nominal `Guard`.
/// 2. `construct` on a native class **refuses an omission** rather than minting a class with no native
///    state behind it. That refusal is not a special case: an extension-declared field carries no
///    literal default, so every native field is mandatory and the shared construction planner rejects
///    the omission — which is why reflecting a native type's fields and letting `construct` resolve it
///    are safe to land together.
/// 3. A **fully supplied** construction produces a real instance of the class: its native method
///    dispatches (`bump()` reaches `point_dispatch` and mutates in place), and its reflected identity is
///    the class's own qualified name. Both backends agree on all of it.
/// 4. `construct` **refuses to set a private field**. `guard` is `is_public: false`, so writing it in a
///    source literal is E0035, and the reflective door now says the same thing in the checker's own
///    words. That is the other half of fact 2: an *omission* is refused because a native field has no
///    default, and a *supply* is refused because this field is private — so `fx.Handle` stays reachable
///    only through the native `kit.open` that owns its state, which is what its privacy declares. No
///    shipped extension declares a private field, so this fixture is the only exerciser for the native
///    side of the gate.
const REFLECT_PROGRAM: &str = r#"
use fx.Point
use fx.Handle

// The schema, from the qualified identity — including a private field and an extern-handle field type.
for f in field_specs_of("fx.Handle") {
    echo "handle ${f.name}: ${f.type} optional=${f.optional}"
}
echo "point: ${field_specs_of("fx.Point").map(fn(f) => "${f.name}=${f.type}").join(" ")}"
// A class is not an enum: the pair says "a class, and here is its schema".
echo "variants: ${variants_of("fx.Point").len()}"

// An omission is refused — a native field has no default, so there is nothing to fill it with.
echo "omitted: ${match construct("fx.Handle", {"label": "x"}) { Ok(v) => "Ok", Err(e) => e }}"

// SUPPLYING the private `guard` is refused too, in the checker's E0035 words — so there is no route
// to a `fx.Handle` that skips the native constructor owning its state. Positionally, `guard` is the
// first field, so it is refused before the value's type is even considered.
mut byname: Map<string, dyn> = {}
byname["guard"] = 1
byname["label"] = "x"
byname["peer"] = 0
echo "private named: ${match construct("fx.Handle", byname) { Ok(v) => "Ok", Err(e) => e }}"
mut bypos: List<dyn> = []
bypos = bypos ~ [1]
echo "private positional: ${match construct("fx.Handle", bypos) { Ok(v) => "Ok", Err(e) => e }}"

// A full construction is a real class instance: native dispatch works on it, and it reports the
// class's own identity.
made = match construct("fx.Point", [7, 8]) { Ok(v) => v.as<Point>(), Err(e) => none };
match made {
    some(p) => {
        echo "made: ${p.x} ${p.y} ${type_of(p)}"
        echo "bumped: ${p.bump()} ${p.y}"
    },
    none => { echo "made nothing" },
}
"#;

#[test]
fn native_class_reflects_its_schema_and_constructs_from_it_on_both_backends() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(REFLECT_PROGRAM);
    assert_eq!(
        stdout,
        "handle guard: Type.Named(fx.Guard, []) optional=false\n\
         handle label: Type.String optional=false\n\
         handle peer: Type.Dyn optional=false\n\
         point: x=Type.Int y=Type.Int\n\
         variants: 0\n\
         omitted: missing required field `guard` of `fx.Handle`\n\
         private named: cannot set private field `guard` of `fx.Handle` from outside it\n\
         private positional: cannot set private field `guard` of `fx.Handle` from outside it\n\
         made: 7 8 Type.Class(fx.Point, [])\n\
         bumped: 9 9\n"
    );
}
