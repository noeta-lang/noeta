//! **Native trait structural `Self`-constraint** (ExtBundle→ExtTrait convergence, slice 3): a native
//! trait carries a `PackedConstraint` on its implementing type — the THIRD and last capability an
//! `ExtBundle` had that a trait lacked (after associated types in 1a/1b and native default bodies in
//! 2). A bundle only binds a `@packed` struct whose fields match its constraint; slice 3 gives a
//! trait that same shape check as a first-class `Self`-constraint, enforced at the user `impl` site by
//! the SAME `check_packed_self_constraint` core the bundle path runs.
//!
//! The design deliberately reuses `PackedConstraint` (not a fresh marker-trait predicate) because the
//! constraint pins the implementing type's uniform **element**, which the trait's native-derived
//! associated types (slice 1b) then derive from. This fixture proves the two agree on that element:
//!
//! The fixture: a native trait `Lanes` carries `self_constraint = AnyNumeric + Uniform { min: 2 }`
//! (a uniform numeric vector of ≥2 fields) AND `type Wide` (`Widen`), with a method `sum(): Self::Wide`.
//! A native `@packed` struct `Duo { a, b: int }` advertises it. Its uniform `int` element both
//! satisfies the constraint (numeric, 2 fields) and feeds the derivation (`Wide → int`). At seed time
//! `seed_ext_traits` folds the derivation into `trait_assoc[("nm.Duo", "Lanes")]` exactly as
//! `ext_assoc_seam.rs` does — the presence of a `self_constraint` on the trait does not disturb it.
//!
//! **Two load-bearing properties**, each with its own test:
//! - The differential exerciser (both backends) proves `v.sum()` resolves through the SAME element the
//!   constraint pins — `Self::Wide` is `int` (Widen of the `int` element), used where an `int` is
//!   required — so the constraint and the associated type agree end to end.
//! - The enforcement gate (check-only) proves a USER `impl Lanes for T {}` is rejected (E0015) unless
//!   `T` is a `@packed` struct matching the constraint — a non-`@packed` target and a mismatched-shape
//!   target both fail, with the constraint-mismatch diagnostic the bundle path produces.
//!
//! A synthetic extension (like `ext_assoc_seam.rs`): no shipped `std` declaration has a native trait
//! with a self-constraint, so the corpus cannot reach this path. An integration test (own process)
//! because the fixture installs into the process-global default registry once per process.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    AssocDerivation, ConstraintArity, ConstraintField, ConstraintLayout, ExtAssocType, ExtField,
    ExtFn, ExtModule, ExtStruct, ExtTrait, ExtTraitMethod, ExtTypeDirective, Extension,
    FieldedKind, NativeOut, NativeValue, PackedConstraint, PackedLayoutKind, RetTy, Scalar,
    SigType,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The native trait: a self-constraint AND a native-derived associated type --------------------

/// `Lanes`: a native trait whose `self_constraint` requires the implementing type to be a uniform
/// numeric `@packed` vector of ≥2 fields, and whose `type Wide` is DERIVED from that same element
/// (`Widen`). `sum(): Self::Wide` names it. The constraint and the associated type read one element.
const LANES: ExtTrait = ExtTrait {
    name: "Lanes",
    namespace: "nm",
    methods: &[ExtTraitMethod {
        sig: ExtFn {
            param_names: &[],
            name: "sum",
            params: &[],
            ret: RetTy::Concrete(SigType::Assoc("Wide")),
        },
        has_default: false,
        ..ExtTraitMethod::DEFAULTS
    }],
    assoc_types: &[ExtAssocType {
        name: "Wide",
        derivation: AssocDerivation::Widen,
    }],
    dispatch: None,
    // The slice-3 field under test: a uniform numeric vector of ≥2 fields — the SAME shape a
    // `vec.Kernels` bundle requires, now a first-class trait `Self`-constraint.
    self_constraint: Some(PackedConstraint {
        fields: &[ConstraintField::AnyNumeric],
        layout: ConstraintLayout::Any,
        arity: ConstraintArity::Uniform { min: 2 },
    }),
    ..ExtTrait::DEFAULTS
};

// --- The native @packed struct that implements it ------------------------------------------------

/// `Duo`: a native `@packed` struct with a uniform `int` element and 2 fields, advertising `Lanes`.
/// Its element satisfies the `self_constraint` (numeric, 2 fields) AND feeds the derivation
/// (`Wide → int` — widen of i64 is identity).
const DUO: ExtStruct = ExtStruct {
    name: "Duo",
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
    ],
    methods: &[ExtFn {
        param_names: &[],
        name: "sum",
        params: &[],
        ret: RetTy::Concrete(SigType::Assoc("Wide")),
    }],
    dispatch: duo_dispatch,
    traits: &["Lanes"],
    kind: FieldedKind::Struct,
    directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
    ..ExtStruct::DEFAULTS
};

/// Read `Duo`'s two `int` fields off the marshalled instance.
fn duo_fields(recv: &NativeValue) -> (i64, i64) {
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
        NativeValue::Instance { fields, .. } => (get(fields, "a"), get(fields, "b")),
        _ => (0, 0),
    }
}

/// `Duo`'s native method dispatch: `sum` returns the widened accumulator (an `int`).
fn duo_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let (a, b) = duo_fields(recv);
    match method {
        "sum" => Ok(NativeOut::Scalar(Scalar::Int(a + b))),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

// --- The module that constructs the native value -------------------------------------------------

const KIT_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "duo",
    params: &[SigType::Int, SigType::Int],
    ret: RetTy::Concrete(SigType::Named("Duo")),
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
        "duo" => Ok(NativeOut::Instance {
            class: "Duo".to_string(),
            fields: vec![
                ("a".to_string(), NativeOut::Scalar(Scalar::Int(arg(0)))),
                ("b".to_string(), NativeOut::Scalar(Scalar::Int(arg(1)))),
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
        &[DUO]
    }
    fn traits(&self) -> &'static [ExtTrait] {
        &[LANES]
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
/// residency unchanged (leak oracle zero), returning the shared stdout. Mirrors `ext_assoc_seam.rs`.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_self_constraint_seam.noe", program);
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
        "backends must agree on the self-constraint program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// The check-only diagnostics of a program (parse asserted clean), for the enforcement gate.
#[track_caller]
fn check_diagnostics(program: &str) -> Vec<String> {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_self_constraint_check.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    checked
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

// --- Tests ---------------------------------------------------------------------------------------

/// **The accepted case resolves its native-derived associated type (constraint + 1b agree).** `Duo`
/// advertises `Lanes`, whose `self_constraint` its uniform `int` element satisfies; the SAME element
/// feeds `type Wide` (`Widen → int`). `v.sum()` is used where an `int` is required, so the program
/// checks clean ONLY IF `Self::Wide` resolved to `int` — and both backends must build identical
/// output. A self-constraint on the trait does not disturb the assoc resolution slice 1b established.
#[test]
fn a_self_constrained_native_trait_still_resolves_its_associated_type() {
    const PROGRAM: &str = r#"
use nm.kit
use nm.Lanes

fn need_int(x: int): int {
    return x
}

v = kit.duo(2, 3)

// `Self::Wide` (Widen of the `int` element the self-constraint pins) resolves to `int`.
echo need_int(v.sum())
"#;
    let stdout = run_both_agree(PROGRAM);
    assert_eq!(stdout, "5\n");
}

/// **The enforcement gate.** A USER `impl Lanes for T {}` is rejected (E0015) unless `T` is a
/// `@packed` struct matching the constraint — the SAME check `check_bundle_binding` runs for a bundle,
/// now at the trait impl site. A non-`@packed` target and a mismatched-shape target both fail; the
/// mismatched one carries the constraint-mismatch diagnostic the bundle path produces. (The required
/// `sum` is provided in each impl so the constraint diagnostic is not confounded with a missing method.)
#[test]
fn a_user_impl_of_a_self_constrained_native_trait_requires_the_shape() {
    // A non-`@packed` target: the self-constraint requires a packed struct.
    let non_packed = check_diagnostics(
        "use nm.Lanes\n\
         struct Plain { x: int }\n\
         impl Lanes for Plain { pub fn sum(): int { return self.x } }\n\
         echo 1\n",
    );
    assert!(
        non_packed.iter().any(|m| m.contains("cannot bind `Lanes`")),
        "expected the packed-target diagnostic; got {non_packed:?}"
    );

    // A `@packed` struct of the wrong shape — one field, below `min: 2` — is rejected with the SAME
    // constraint-mismatch message the bundle path yields.
    let mismatched = check_diagnostics(
        "use nm.Lanes\n\
         @packed struct One { x: i32 }\n\
         impl Lanes for One { pub fn sum(): int { return 0 } }\n\
         echo 1\n",
    );
    assert!(
        mismatched
            .iter()
            .any(|m| m.contains("requires at least 2 `numeric` fields")),
        "expected the constraint-mismatch diagnostic; got {mismatched:?}"
    );

    // The matching case is clean: a `@packed` numeric pair that provides `sum` (and binds the trait's
    // associated `type Wide`, which a user impl of a native trait must supply since the native-derived
    // form carries no default) satisfies the self-constraint — the same shape `Duo` has, hand-written.
    let ok = check_diagnostics(
        "use nm.Lanes\n\
         @packed struct MyPair { x: int; y: int }\n\
         impl Lanes for MyPair { type Wide = int; pub fn sum(): int { return self.x + self.y } }\n\
         echo 1\n",
    );
    assert!(
        ok.is_empty(),
        "a matching @packed struct must bind clean; got {ok:?}"
    );
}
