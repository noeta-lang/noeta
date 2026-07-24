//! **A native `@validated` struct auto-validates through the JSON recipe door** (native
//! type-declaration unification, Slice E follow-up). A native extension declares a value struct that
//! advertises `traits:["Validate"]` + `@validated` (`ExtTypeDirective::Validated`) and a native
//! `validate` body; a program decodes it with `json.try_parse::<Percent>(json)`. The door must
//! materialize a REAL native struct-kind instance (not a plain object) AND run its validator
//! automatically — a bad value's rejection becomes the door's `Result.Err(JsonError)` — identically
//! on both backends, with the leak oracle at zero.
//!
//! This is the exerciser for the recipe-door gap the arc's E2 note flagged: `type_to_recipe` already
//! builds a `TypeRecipe::Struct` for a native struct (its qualified identity is in the checker's
//! `records`/`type_kinds` tables, `has_validator` set from `satisfies(Validate)`), and `json` emits
//! `NativeOut::Struct { has_validator: true }` from it — but neither backend used to MATERIALIZE a
//! native struct from that node or DISPATCH its `validate`:
//!
//! - the tree-walker's recipe path called `construct_object(&qualified_name)`, a scope lookup that
//!   fails for a native struct (scope-bound only under its short name) — E `UnknownName`;
//! - the VM built the struct-kind object fine, but its validator re-entry (`run_method_handle`)
//!   never fell through to the native `find_class_method` → `call_native_class_method` seam, so a
//!   native `validate` body was unreachable.
//!
//! No shipped extension declares a native `@validated` struct, so the std-only corpus structurally
//! cannot reach either path; this fixture is their only exerciser.
//!
//! An **integration test** (own process) because the fixture installs into the process-global default
//! registry — once per process — the single-registry path the CLI uses.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtField, ExtFn, ExtStruct, ExtTypeDirective, Extension, NativeOut, NativeValue, RetTy, Scalar,
    SigType,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The native `@validated` value struct --------------------------------------------------------

/// `Percent { value: int }` — a value struct whose invariant is `0 <= value <= 100`. It advertises
/// the built-in `Validate` trait (`traits:["Validate"]` → `satisfies(Validate)` true → the recipe's
/// `has_validator`) and carries `@validated` (`ExtTypeDirective::Validated` → the E0060 construction
/// ban). Its native `validate` body rejects an out-of-range value with a message; the recipe door
/// re-enters it after building the instance and turns a rejection into the door's `Result.Err`.
const PERCENT: ExtStruct = ExtStruct {
    name: "Percent",
    namespace: "pct",
    fields: &[ExtField {
        name: "value",
        ty: SigType::Int,
        is_public: true,
        is_mut: false,
    }],
    methods: &[ExtFn {
        name: "validate",
        params: &[],
        // The `Validate` contract's shape: `Result<void, string>` (Ok on a valid value, Err(message)
        // on a rejection). The recipe re-entry reads its `Err` payload as the rejection message.
        ret: RetTy::Concrete(SigType::Result(&SigType::Unit, &SigType::String)),
    }],
    dispatch: percent_dispatch,
    traits: &["Validate"],
    directives: &[ExtTypeDirective::Validated],
    ..ExtStruct::STRUCT_DEFAULTS
};

/// `Percent`'s instance-method dispatch. `validate` reads the receiver's `value` field off the
/// marshalled `NativeValue::Instance` and returns `Result::Err(message)` when out of range, else
/// `Result::Ok(())` — a native validator body reached through the same fielded seam a native class
/// method takes.
fn percent_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let value = match recv {
        NativeValue::Instance { fields, .. } => fields
            .iter()
            .find(|(k, _)| k == "value")
            .and_then(|(_, v)| match v {
                NativeValue::Scalar(Scalar::Int(n)) => Some(*n),
                _ => None,
            })
            .unwrap_or(0),
        _ => 0,
    };
    match method {
        "validate" => {
            if !(0..=100).contains(&value) {
                Ok(NativeOut::Err(Box::new(NativeOut::Str(format!(
                    "percent out of range: {value}"
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

struct PctExtension;

impl Extension for PctExtension {
    fn name(&self) -> &'static str {
        "pct"
    }
    fn modules(&self) -> &'static [noeta_stdlib::registry::ExtModule] {
        &[]
    }
    fn structs(&self) -> &'static [ExtStruct] {
        &[PERCENT]
    }
}

static PCT: PctExtension = PctExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&PCT]));
}

/// Every fixture test touches the shared process-global registry; each holds this for its whole run
/// so the installs do not race (mirrors `ext_struct_seam.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// --- Helpers -------------------------------------------------------------------------------------

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero). Returns the shared stdout.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_validated_recipe_seam.noe", program);
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
        "backends must agree on the native-validated recipe program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

// --- Tests ---------------------------------------------------------------------------------------

/// The load-bearing case: `json.try_parse::<Percent>` on a **valid** JSON value materializes a real
/// native `Percent` instance (its `value` field is readable), and on an **invalid** one the native
/// validator runs and the door returns `Result.Err(JsonError)` carrying the validator's message —
/// identically on both backends, leak-free.
#[test]
fn a_native_validated_struct_auto_validates_through_the_recipe_door() {
    const PROGRAM: &str = r#"
use pct.Percent
use std.json
use std.json.JsonError

// Valid input (value in range): the door builds a real native struct; its field is readable.
echo match json.try_parse::<Percent>("{\"value\": 50}") {
    Ok(p) => "ok: ${p.value}",
    Err(e) => "bad: ${e.message()}",
}

// Invalid input (value out of range): the native validator runs bottom-up and rejects, so the
// recoverable door surfaces `Result.Err(JsonError)` with the validator's own message.
echo match json.try_parse::<Percent>("{\"value\": 200}") {
    Ok(p) => "ok: ${p.value}",
    Err(e) => "bad: ${e.message()}",
}
"#;
    let out = run_both_agree(PROGRAM);
    assert_eq!(
        out, "ok: 50\nbad: percent out of range: 200\n",
        "valid decodes to a real instance; invalid runs the native validator and rejects"
    );
}

/// Contrast the recoverable door with the **unrecoverable** one: `json.parse::<Percent>` (no
/// `Result` wrapper) on a valid value returns the instance directly and its native method still
/// dispatches — the same struct-kind object, reached through a different door.
#[test]
fn the_plain_recipe_door_materializes_a_dispatchable_native_struct() {
    const PROGRAM: &str = r#"
use pct.Percent
use std.json

// `json.parse` (unrecoverable) on a valid value: a real native `Percent`, field readable, and its
// native `validate` still dispatches (returns Ok for an in-range value).
p = json.parse::<Percent>("{\"value\": 75}")
echo p.value
echo match p.validate() {
    Ok(u) => "valid",
    Err(m) => "invalid: ${m}",
}
"#;
    let out = run_both_agree(PROGRAM);
    assert_eq!(
        out, "75\nvalid\n",
        "the plain door yields a real native struct whose native method dispatches"
    );
}
