//! **Extern-type qualified-identity coexistence** (audit-2 Finding 1): two extension units
//! register the same SHORT type name (`Counter`) under distinct namespaces (`acme.metrics` /
//! `bcorp.stats`), one program imports and constructs both, and `is`/`.as<T>()` narrowing and
//! method dispatch each resolve to the right type — with deliberately different method semantics
//! per unit, so a mis-routed dispatch cannot produce the pinned output. Both backends run the
//! program and must agree exactly (the same reference-vs-VM comparison the corpus differential
//! performs), because identity flows through the one shared seam:
//! `ExternValue::type_identity` = the qualified `namespace.name`.
//!
//! This is an **integration test** (its own process) because the two fixture units install into
//! the process-global default registry — the same single-registry path the CLI uses — which may
//! happen only once per process; everything runs inside the single #[test].

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtFn, ExtModule, ExtType, Extension, NativeOut, NativeValue, RetTy, Scalar, SigType,
};
use noeta_stdlib::{ErrorKind, ExternValue, Host, StdError};
use noeta_vm::VmBackend;

// --- The fixture counter value, shared shape, per-unit identity and semantics ------------------

/// One counter value used by both units; `identity` distinguishes them at runtime and `step`
/// gives each unit's `bump` different observable semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counter {
    identity: &'static str,
    label: &'static str,
    step: i64,
    scale: i64,
    n: i64,
}

impl ExternValue for Counter {
    fn type_identity(&self) -> &'static str {
        self.identity
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Counter>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<{} {}>", self.label, self.n)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

const COUNTER_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "bump",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &[],
        name: "value",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
];

/// The one shared method dispatch: behavior differs only through the receiver's own fields, so a
/// value that dispatched through the wrong `ExtType` would still compute from ITS state — which
/// is why the two units also get different `step`/`scale`, making any routing mistake visible.
fn counter_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let c = recv
        .as_any_mut()
        .downcast_mut::<Counter>()
        .expect("receiver is a Counter");
    match method {
        "bump" => {
            c.n += c.step;
            Ok(NativeOut::Unit)
        }
        "value" => Ok(NativeOut::Scalar(Scalar::Int(c.n * c.scale))),
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no method `{method}` on Counter"),
        }),
    }
}

// --- Unit A: `acme.metrics` — bump += 1, value = n --------------------------------------------
//
// Both constructors name their return type by its QUALIFIED identity: with two `Counter`s in the
// assembled registry the short spelling would resolve to whichever unit registered first, so a
// short-name-sharing extension writes the qualified form (checker `qualified_extern` resolves
// either spelling).

const ACME_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "counter",
    params: &[SigType::Int],
    ret: RetTy::Concrete(SigType::Named("acme.metrics.Counter")),
}];

fn acme_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match (func, args.first()) {
        ("counter", Some(NativeValue::Scalar(Scalar::Int(n)))) => {
            Ok(NativeOut::Extern(noeta_stdlib::ExternBox::new(Counter {
                identity: "acme.metrics.Counter",
                label: "acme-counter",
                step: 1,
                scale: 1,
                n: *n,
            })))
        }
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct AcmeExtension;

impl Extension for AcmeExtension {
    fn name(&self) -> &'static str {
        "acme"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "metrics",
            functions: ACME_FNS,
            dispatch: acme_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[ExtType {
            name: "Counter",
            namespace: "acme.metrics",
            methods: COUNTER_METHODS,
            dispatch: counter_method_dispatch,
            ..ExtType::DEFAULTS
        }]
    }
}

static ACME: AcmeExtension = AcmeExtension;

// --- Unit B: `bcorp.stats` — bump += 2, value = n * 10 -----------------------------------------

const BCORP_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "counter",
    params: &[SigType::Int],
    ret: RetTy::Concrete(SigType::Named("bcorp.stats.Counter")),
}];

fn bcorp_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match (func, args.first()) {
        ("counter", Some(NativeValue::Scalar(Scalar::Int(n)))) => {
            Ok(NativeOut::Extern(noeta_stdlib::ExternBox::new(Counter {
                identity: "bcorp.stats.Counter",
                label: "bcorp-counter",
                step: 2,
                scale: 10,
                n: *n,
            })))
        }
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct BcorpExtension;

impl Extension for BcorpExtension {
    fn name(&self) -> &'static str {
        "bcorp"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "stats",
            functions: BCORP_FNS,
            dispatch: bcorp_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[ExtType {
            name: "Counter",
            namespace: "bcorp.stats",
            methods: COUNTER_METHODS,
            dispatch: counter_method_dispatch,
            ..ExtType::DEFAULTS
        }]
    }
}

static BCORP: BcorpExtension = BcorpExtension;

// --- The program and the two-backend comparison -------------------------------------------------

const PROGRAM: &str = r#"
use acme.metrics
use bcorp.stats
use acme.metrics.Counter
use bcorp.stats.Counter as BCounter

a: Counter = metrics.counter(3);
b: BCounter = stats.counter(3);

// `is` narrowing keys on the qualified identity: each value matches its own type only.
echo a is Counter;
echo a is BCounter;
echo b is BCounter;
echo b is Counter;

// Method dispatch routes by the receiver's identity: acme bumps by 1 and reports n;
// bcorp bumps by 2 and reports n * 10. A cross-routed dispatch could not print this pair.
a.bump();
b.bump();
echo a.value();
echo b.value();

// The same holds from behind `dyn` — the runtime type test, not the static type, decides.
d: dyn = a;
echo d is Counter;
echo d is BCounter;
echo d.as<BCounter>();
echo d.as<Counter>();

// Display shows each value's own form.
echo a;
echo b;
"#;

const EXPECTED_STDOUT: &str = "true\nfalse\ntrue\nfalse\n4\n50\ntrue\nfalse\nnone\nsome(<acme-counter 4>)\n<acme-counter 4>\n<bcorp-counter 5>\n";

#[test]
fn same_short_name_extern_types_coexist_on_both_backends() {
    // The two fixture units join std in the process-global default registry — the same
    // single-registry assembly the CLI's composed toolchain performs.
    noeta_stdlib::registry::install_with_extras(&[&ACME, &BCORP]);

    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "extern_identity.noe", PROGRAM);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let tokens = noeta_db::tokens(&db, src);
    let parsed = noeta_db::ast(&db, src);
    assert!(
        tokens.0.diagnostics.is_empty() && parsed.0.diagnostics.is_empty(),
        "fixture program must parse cleanly: {:?} {:?}",
        tokens.0.diagnostics,
        parsed.0.diagnostics
    );
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "fixture program must check cleanly: {:?}",
        checked.diagnostics
    );

    // Reference (Core-IR interpreter) and VM run the same checked program — the exact
    // differential recipe, on a program the std-only corpus cannot express.
    let reference =
        noeta_conformance::reference::reference_run(&parsed.0.program, checked.sites.clone());
    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("fixture program compiles to bytecode")
        .clone();
    let vm = VmBackend::new().run_module(&module);

    assert_eq!(
        reference, vm,
        "backends must agree on the coexistence program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    assert_eq!(reference.stdout, EXPECTED_STDOUT);
}
