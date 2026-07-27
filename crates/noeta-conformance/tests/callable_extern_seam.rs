//! **`Callable` on extern types** (http arc H10): a native extension *outside* `std` registers a
//! type with a `call` method, and a program invokes its values directly — `adder(2, 3)`.
//!
//! Extern types already participate in every other protocol the language exposes (`Display`,
//! `Error`, `Equatable`, `Index`); being uncallable was an inconsistency, not a decision. Closing
//! it is what lets a native extension hand user code something **callable** — a middleware's
//! `next`, a generated operation handle — without inventing a parallel callback mechanism.
//!
//! Proven here with a synthetic extension rather than an `std` consumer, deliberately: the seam is
//! registry-driven, so nothing about `Adder` is known to the checker or either backend except its
//! `ExtType::methods` declaration. A test that rode on an `std` type would prove less.
//!
//! An **integration test** (own process) because the fixture unit installs into the process-global
//! default registry — the same single-registry path the CLI uses — which happens once per process.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtFn, ExtType, Extension, NativeOut, NativeValue, RetTy, Scalar, SigType,
};
use noeta_stdlib::{ExternValue, Host, StdError};
use noeta_vm::VmBackend;

// --- The fixture extension: a `Counter` whose values are invocable ------------------------------

/// A value that adds its own base to whatever it is called with. Deliberately carries state, so
/// the test proves the RECEIVER reaches the dispatch — not merely that some function ran.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Adder {
    base: i64,
}

impl ExternValue for Adder {
    fn type_identity(&self) -> &'static str {
        "testcall.Adder"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Adder>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<adder {}>", self.base)
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

const ADDER_METHODS: &[ExtFn] = &[
    // The `Callable` protocol's method. Nothing marks it special at registration — the protocol
    // is structural, exactly as it is for a user type's `impl Callable`.
    ExtFn {
        param_names: &[],
        name: "call",
        params: &[SigType::Int, SigType::Int],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        param_names: &[],
        name: "base",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
];

fn adder_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let adder = recv
        .as_any()
        .downcast_ref::<Adder>()
        .expect("an Adder receiver");
    let int = |i: usize| match args.get(i) {
        Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
        _ => 0,
    };
    match method {
        "call" => Ok(NativeOut::Scalar(Scalar::Int(adder.base + int(0) + int(1)))),
        "base" => Ok(NativeOut::Scalar(Scalar::Int(adder.base))),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

const MAKE_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "adder",
    params: &[SigType::Int],
    ret: RetTy::Concrete(SigType::Named("Adder")),
}];

fn make_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "adder" => {
            let base = match args.first() {
                Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
                _ => 0,
            };
            Ok(NativeOut::Extern(noeta_stdlib::ExternBox::new(Adder {
                base,
            })))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct TestCallExtension;

impl Extension for TestCallExtension {
    fn name(&self) -> &'static str {
        "testcall"
    }
    fn modules(&self) -> &'static [noeta_stdlib::registry::ExtModule] {
        &[noeta_stdlib::registry::ExtModule {
            name: "make",
            functions: MAKE_FNS,
            dispatch: make_dispatch,
            ..noeta_stdlib::registry::ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[ExtType {
            name: "Adder",
            namespace: "testcall",
            methods: ADDER_METHODS,
            dispatch: adder_dispatch,
            ..ExtType::DEFAULTS
        }]
    }
}

static TESTCALL: TestCallExtension = TestCallExtension;

const PROGRAM: &str = r#"
use testcall.make

add10 = make.adder(10)

// Invoked as a value — `add10(...)` dispatches to the registered `call` method, receiver first.
echo add10(2, 3)

// The receiver genuinely reaches the dispatch: a different base gives a different answer.
echo make.adder(100)(2, 3)

// The ordinary method surface is unaffected by the type also being callable.
echo add10.base()

// A callable extern value is an ordinary value: it passes through a closure parameter and is
// invoked there, which is the shape a middleware's `next` relies on.
apply = fn(f) { return f(1, 1) }
echo apply(add10)
"#;

const EXPECTED_STDOUT: &str = "15\n105\n10\n12\n";

#[test]
fn an_extern_type_with_a_call_method_is_invocable_on_both_backends() {
    noeta_stdlib::registry::install_with_extras(&[&TESTCALL]);

    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "callable_extern_seam.noe", PROGRAM);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "fixture program must parse cleanly: {:?}",
        parsed.0.diagnostics
    );
    // The call type-checks through the registered `call` signature — so a wrong argument is a
    // static error, not a runtime surprise.
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "fixture program must check cleanly: {:?}",
        checked.diagnostics
    );

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
        "backends must agree on the callable-extern program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    assert_eq!(reference.stdout, EXPECTED_STDOUT);

    // Arguments are checked through the DECLARED `call` signature: a wrong-typed argument is
    // E0007, exactly as it would be for `add10.call("x", 3)`.
    let bad_src = "use testcall.make\nx = make.adder(1)(\"nope\", 2)\necho x\n";
    let bad_db = LangDatabase::default();
    let bad_source = Source::new(SourceId::FIRST, "callable_extern_argtype.noe", bad_src);
    let bad = noeta_db::source_program(&bad_db, &bad_source, noeta_lexer::Edition::DEFAULT);
    let bad_checked = noeta_db::checked(&bad_db, bad);
    assert!(
        bad_checked
            .diagnostics
            .iter()
            .any(|d| d.code == noeta_diagnostics::DiagnosticCode::TypeMismatch),
        "a wrong-typed argument to a callable extern must be E0007, got {:?}",
        bad_checked.diagnostics
    );
}
