//! **Native-derived trait associated types** (ExtBundle→ExtTrait convergence, slice 1b): a native
//! trait declares an associated type whose concrete `Type` is *derived from the implementing type's
//! element* — the `AssocDerivation` mechanism that generalizes the bundle ABI's element-relative
//! returns (`RetTy::ElemWide`/`ElemFloat`) into a first-class trait contract.
//!
//! The fixture: a native trait `Reduce` declares `type Wide` (`Widen`), `type Mag` (`FloatPromote`),
//! and methods `sum(): Self::Wide`, `length(): Self::Mag`, `widen_all(): List<Self::Wide>`. A native
//! `@packed` struct `Accum { a, b, c: int }` advertises `Reduce`. At seed time `seed_ext_traits`
//! folds each derivation over the struct's uniform `int` element into `trait_assoc[("nm.Accum",
//! "Reduce")]` — `Wide → int` (widen of i64 is identity), `Mag → float` (int promotes to f64). A
//! concrete `v.length()` then resolves `Self::Mag` to `float`, `v.sum()` to `int`, and
//! `v.widen_all()` to `List<int>` — through the SAME `trait_assoc` table a `.noe` `type Item = …`
//! binding uses (slice 1a).
//!
//! **The load-bearing property** is that the derived type is the RESOLVED type, not a `dyn` hole: the
//! differential test uses each result in a type-constrained position (a `float`/`int`/`List<int>`
//! parameter) that a broken resolution would either mis-run or a wrong concrete type would reject;
//! the check-only test proves the *direction* of the derivation — `v.length()` (an `int` element
//! promoted) is `float`, so passing it where an `int` is required is a static error, which it would
//! NOT be if the associated type erased to a hole or resolved to the bare element.
//!
//! A synthetic extension (like `ext_trait_seam.rs`): no shipped `std` declaration has a native trait
//! with associated types, so the corpus cannot reach this path — the differential assertion lives
//! here. An integration test (own process) because the fixture installs into the process-global
//! default registry once per process.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    AssocDerivation, ExtAssocType, ExtField, ExtFn, ExtModule, ExtStruct, ExtTrait, ExtTraitMethod,
    ExtTypeDirective, Extension, FieldedKind, NativeOut, NativeValue, PackedLayoutKind, RetTy,
    Scalar, SigType,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The native trait with native-derived associated types ---------------------------------------

/// `Reduce`: a native trait whose `type Wide`/`type Mag` are DERIVED from the implementing type's
/// element (`Widen` / `FloatPromote`), and whose methods name them as `Self::Wide` / `Self::Mag` /
/// `List<Self::Wide>` — the ABI form of the `.noe` `Self::Name` projection.
const REDUCE: ExtTrait = ExtTrait {
    name: "Reduce",
    namespace: "nm",
    methods: &[
        ExtTraitMethod {
            sig: ExtFn {
                param_names: &[],
                name: "sum",
                params: &[],
                ret: RetTy::Concrete(SigType::Assoc("Wide")),
            },
            has_default: false,
            ..ExtTraitMethod::DEFAULTS
        },
        ExtTraitMethod {
            sig: ExtFn {
                param_names: &[],
                name: "length",
                params: &[],
                ret: RetTy::Concrete(SigType::Assoc("Mag")),
            },
            has_default: false,
            ..ExtTraitMethod::DEFAULTS
        },
        ExtTraitMethod {
            sig: ExtFn {
                param_names: &[],
                name: "widen_all",
                params: &[],
                // `List<Self::Wide>` — the `RetTy::ListElemWide` analog; the concrete resolution must
                // recurse into `List<_>`.
                ret: RetTy::Concrete(SigType::List(&SigType::Assoc("Wide"))),
            },
            has_default: false,
            ..ExtTraitMethod::DEFAULTS
        },
    ],
    assoc_types: &[
        ExtAssocType {
            name: "Wide",
            derivation: AssocDerivation::Widen,
        },
        ExtAssocType {
            name: "Mag",
            derivation: AssocDerivation::FloatPromote,
        },
    ],
    dispatch: None,
    // No structural `Self`-constraint (slice 3): `Reduce` binds any implementing type; the
    // constraint path is exercised by `ext_self_constraint_seam.rs`.
    self_constraint: None,
};

// --- The native @packed struct that implements it ------------------------------------------------

/// `Accum`: a native `@packed` struct with a uniform `int` element, advertising `Reduce`. Its
/// element (`int`) feeds the derivations: `Wide → int`, `Mag → float`.
const ACCUM: ExtStruct = ExtStruct {
    name: "Accum",
    namespace: "nm",
    fields: &[
        ExtField {
            name: "a",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "b",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "c",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
    ],
    methods: &[
        ExtFn {
            param_names: &[],
            name: "sum",
            params: &[],
            ret: RetTy::Concrete(SigType::Assoc("Wide")),
        },
        ExtFn {
            param_names: &[],
            name: "length",
            params: &[],
            ret: RetTy::Concrete(SigType::Assoc("Mag")),
        },
        ExtFn {
            param_names: &[],
            name: "widen_all",
            params: &[],
            ret: RetTy::Concrete(SigType::List(&SigType::Assoc("Wide"))),
        },
    ],
    dispatch: accum_dispatch,
    traits: &["Reduce"],
    kind: FieldedKind::Struct,
    directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
};

/// Read `Accum`'s three `int` fields off the marshalled instance.
fn accum_fields(recv: &NativeValue) -> (i64, i64, i64) {
    let get = |fields: &[(String, NativeValue)], k: &str| -> i64 {
        fields
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| match v {
                NativeValue::Scalar(Scalar::Int(i)) => Some(*i),
                _ => None,
            })
            .unwrap_or(0)
    };
    match recv {
        NativeValue::Instance { fields, .. } => {
            (get(fields, "a"), get(fields, "b"), get(fields, "c"))
        }
        _ => (0, 0, 0),
    }
}

/// `Accum`'s native method dispatch: `sum` returns the widened accumulator (an `int`), `length` the
/// float promotion (an `f64` magnitude proxy — here the plain float of the sum), `widen_all` a
/// `List<int>` of the widened elements.
fn accum_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let (a, b, c) = accum_fields(recv);
    match method {
        "sum" => Ok(NativeOut::Scalar(Scalar::Int(a + b + c))),
        "length" => Ok(NativeOut::Scalar(Scalar::Float((a + b + c) as f64))),
        "widen_all" => Ok(NativeOut::List(vec![
            NativeOut::Scalar(Scalar::Int(a)),
            NativeOut::Scalar(Scalar::Int(b)),
            NativeOut::Scalar(Scalar::Int(c)),
        ])),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

// --- The module that constructs the native value -------------------------------------------------

const KIT_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "accum",
    params: &[SigType::Int, SigType::Int, SigType::Int],
    ret: RetTy::Concrete(SigType::Named("Accum")),
}];

fn kit_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let arg = |i: usize| -> i64 {
        match args.get(i) {
            Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
            _ => 0,
        }
    };
    match func {
        "accum" => Ok(NativeOut::Instance {
            class: "Accum".to_string(),
            fields: vec![
                ("a".to_string(), NativeOut::Scalar(Scalar::Int(arg(0)))),
                ("b".to_string(), NativeOut::Scalar(Scalar::Int(arg(1)))),
                ("c".to_string(), NativeOut::Scalar(Scalar::Int(arg(2)))),
            ],
            kind: FieldedKind::Struct,
        }),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct NmExtension;

impl Extension for NmExtension {
    fn name(&self) -> &'static str {
        "nm"
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
        &[ACCUM]
    }
    fn traits(&self) -> &'static [ExtTrait] {
        &[REDUCE]
    }
}

static NM: NmExtension = NmExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&NM]));
}

// --- Helpers -------------------------------------------------------------------------------------

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero), returning the shared stdout. Mirrors `ext_trait_seam.rs`.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_assoc_seam.noe", program);
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
        "backends must agree on the native-assoc program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// The check-only diagnostics of a program (parse asserted clean), for the derivation-direction gate.
#[track_caller]
fn check_diagnostics(program: &str) -> Vec<String> {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_assoc_check.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    checked
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

// --- Tests ---------------------------------------------------------------------------------------

/// **The differential exerciser.** Each native-derived associated type is used in a type-constrained
/// position — `v.sum()` where an `int` is required, `v.length()` where a `float` is required, and
/// `v.widen_all()` where a `List<int>` is required — so the program checks clean ONLY IF each
/// `Self::Name` resolved to (at least a supertype of) the derived type, and both backends must build
/// identical output. A wrong CONCRETE resolution would reject at check; the direction gate below
/// proves it is not merely an erased hole.
#[test]
fn native_derived_associated_type_resolves_on_both_backends() {
    const PROGRAM: &str = r#"
use nm.kit
use nm.Reduce

fn need_int(x: int): int {
    return x
}

fn need_float(x: float): float {
    return x
}

fn sum_list(xs: List<int>): int {
    mut t = 0
    for x in xs {
        t = t + x
    }
    return t
}

v = kit.accum(1, 2, 3)

// `Self::Wide` (Widen of the `int` element) resolves to `int`.
echo need_int(v.sum())

// `Self::Mag` (FloatPromote of the `int` element) resolves to `float`.
echo need_float(v.length())

// `List<Self::Wide>` resolves — nested — to `List<int>`.
echo sum_list(v.widen_all())
"#;
    let stdout = run_both_agree(PROGRAM);
    assert_eq!(stdout, "6\n6.0\n6\n");
}

/// **The derivation-direction gate.** `v.length()`'s associated type is `FloatPromote` over an `int`
/// element — it must be `float`, not the bare `int` element and not an erased hole. Passing it where
/// an `int` is required is therefore a static error; that error would NOT fire if the projection
/// resolved to `int` (a broken/identity derivation) or to a `dyn` hole (no resolution at all). So the
/// diagnostic's presence — naming `float` — pins the derivation direction end to end.
#[test]
fn a_float_promoted_associated_type_is_not_the_integer_element() {
    const SRC: &str = r#"
use nm.kit
use nm.Reduce

fn need_int(x: int): int {
    return x
}

v = kit.accum(1, 2, 3)
echo need_int(v.length())
"#;
    let diags = check_diagnostics(SRC);
    assert!(
        diags.iter().any(|m| m.contains("float")),
        "expected a type-mismatch diagnostic naming `float` (the FloatPromote derivation of the \
         `int` element); a hole or an identity derivation would produce none, got {diags:?}"
    );
}
