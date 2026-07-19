//! **Call-site-typed openness** (typed-call-seam): a native extension *outside* `std.json` declares
//! a call-site-typed function — `testext.build.make_default::<T>(): T` — and a program calls it under
//! a turbofish. The checker types the call as the turbofish `T` (a real struct instance, an `int`, a
//! `string`, an `Option`, a `List`), and both backends run the program to byte-identical output. This
//! is the proof the seam is registry-driven, not `json`-hardcoded: nothing about `make_default` is
//! known to the checker or either backend except its `ExtModule::typed_functions` declaration and its
//! `typed_dispatch`, which builds a default value of `T` straight from the checker-resolved
//! `TypeRecipe` (zero ints, empty strings, `none` options, empty lists, recursively-defaulted structs).
//!
//! An **integration test** (own process) because the fixture unit installs into the process-global
//! default registry — the same single-registry path the CLI uses — which happens once per process.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtFn, ExtModule, Extension, NativeOut, NativeValue, RetTy, Scalar, TypeArgWrap, TypeRecipe,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The fixture extension: `testext.build.make_default::<T>(): T` -------------------------------

const BUILD_TYPED_FNS: &[ExtFn] = &[ExtFn {
    name: "make_default",
    params: &[],
    // Plain call-site-typed: the result is the turbofish `T` itself.
    ret: RetTy::TypeArg(TypeArgWrap::Plain),
}];

/// Build a default value of the call-site type `recipe` as a neutral `NativeOut` tree — the payoff
/// that the recipe seam already carries everything a general extension needs: zero scalars, empty
/// strings, `none` optionals, empty containers, and recursively-defaulted structs (built by name, so
/// the backend materializes a real instance of `T`).
fn default_out(recipe: &TypeRecipe) -> NativeOut {
    match recipe {
        TypeRecipe::Int => NativeOut::Scalar(Scalar::Int(0)),
        TypeRecipe::Float => NativeOut::Scalar(Scalar::Float(0.0)),
        TypeRecipe::F32 => NativeOut::Scalar(Scalar::F32(0.0)),
        TypeRecipe::Bool => NativeOut::Scalar(Scalar::Bool(false)),
        TypeRecipe::Str => NativeOut::Str(String::new()),
        TypeRecipe::Unit => NativeOut::Unit,
        TypeRecipe::Option(_) => NativeOut::None,
        TypeRecipe::List(_) => NativeOut::List(Vec::new()),
        TypeRecipe::Map(_) => NativeOut::Map(Vec::new()),
        TypeRecipe::Struct {
            name,
            fields,
            has_validator,
        } => NativeOut::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(f, r)| (f.clone(), default_out(r)))
                .collect(),
            has_validator: *has_validator,
        },
    }
}

fn build_typed_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
    recipe: &TypeRecipe,
) -> Result<NativeOut, StdError> {
    match func {
        "make_default" => Ok(default_out(recipe)),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct TestExtension;

impl Extension for TestExtension {
    fn name(&self) -> &'static str {
        "testext"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "build",
            typed_functions: BUILD_TYPED_FNS,
            typed_dispatch: Some(build_typed_dispatch),
            ..ExtModule::DEFAULTS
        }]
    }
}

static TESTEXT: TestExtension = TestExtension;

// --- The program and the two-backend comparison -------------------------------------------------

const PROGRAM: &str = r#"
use testext.build

struct Point {
    x: int
    y: int
    fn sum(): int { return self.x + self.y; }
}

// A struct built entirely from the recipe — a real instance, so its method is callable.
p = build.make_default::<Point>();
echo p.x;
echo p.y;
echo p.sum();

// Scalars, strings, optionals, and lists each default per their recipe.
echo build.make_default::<int>();
echo "[${build.make_default::<string>()}]";
echo build.make_default::<?int>();
echo build.make_default::<List<int>>().len();
"#;

const EXPECTED_STDOUT: &str = "0\n0\n0\n0\n[]\nnone\n0\n";

#[test]
fn a_non_std_extension_declares_a_call_site_typed_function_on_both_backends() {
    // The fixture unit joins std in the process-global default registry — the same single-registry
    // assembly the CLI's composed toolchain performs.
    noeta_stdlib::registry::install_with_extras(&[&TESTEXT]);

    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "typed_call_seam.noe", PROGRAM);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let tokens = noeta_db::tokens(&db, src);
    let parsed = noeta_db::ast(&db, src);
    assert!(
        tokens.0.diagnostics.is_empty() && parsed.0.diagnostics.is_empty(),
        "fixture program must parse cleanly: {:?} {:?}",
        tokens.0.diagnostics,
        parsed.0.diagnostics
    );
    // The turbofish call type-checks against the extension's declared signature — no `json` hardcode.
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "fixture program must check cleanly: {:?}",
        checked.diagnostics
    );

    // Reference (Core-IR interpreter) and VM run the same checked program — the exact differential
    // recipe, on a program the std-only corpus cannot express.
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
        "backends must agree on the typed-call program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    assert_eq!(reference.stdout, EXPECTED_STDOUT);

    // A wrong-arity turbofish on the extension's function is a static error validated through the
    // DECLARED signature (`make_default` takes no arguments) — arg-checking is registry-driven, not
    // `json`-specific. (Same #[test] as the positive: `install` runs once per process.)
    let arity_src = "use testext.build\nx = build.make_default::<int>(5);\necho x;\n";
    let arity_db = LangDatabase::default();
    let arity_source = Source::new(SourceId::FIRST, "typed_call_arity.noe", arity_src);
    let arity = noeta_db::source_program(&arity_db, &arity_source, noeta_lexer::Edition::DEFAULT);
    let arity_checked = noeta_db::checked(&arity_db, arity);
    assert!(
        arity_checked
            .diagnostics
            .iter()
            .any(|d| d.code == noeta_diagnostics::DiagnosticCode::TypeMismatch),
        "a 1-argument call to a 0-argument typed function must be E0007, got {:?}",
        arity_checked.diagnostics
    );
}
