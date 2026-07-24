//! **Native trait default bodies** (ExtBundle→ExtTrait convergence, slice 2): an extension declares
//! a real language **trait** whose method carries `has_default: true` AND a trait-level default-body
//! `dispatch` (a [`CtxTypeDispatch`], the receiver as slot 0 — the same shape [`ExtBundle`] uses). A
//! type that does not itself provide the method adopts the **trait's** native body; a type that does
//! provide it overrides. This is the second capability a bundle had that a trait lacked, re-homed onto
//! the trait — a fully-defaulted native trait now behaves like a bundle bind while still permitting an
//! override, strictly more capable than the must-be-empty bundle.
//!
//! The differential oracle cannot reach this (std declares no native trait with a default dispatch), so
//! a synthetic `fx` extension carries the fixture and the both-backends assertion lives here — mirroring
//! `ext_trait_seam.rs`. Four cases, each observable in the shared stdout:
//!
//! - **Source (2a) — native advertisement:** a native `Chip` advertises `Gadget` through its `traits`
//!   list (the "empty bind") and declares no `tag` → `chip.tag()` dispatches to the **trait's** native
//!   default body on both backends.
//! - **Source (2b) — user empty impl:** a user `Uc` with an explicit empty `impl Gadget for Uc {}`
//!   adopts the same native default — the bundle-bind analogue made a trait.
//! - **Source (1) — override:** a native `Chip2` that DECLARES `tag` wins over the trait default.
//! - **Source (3) — `.noe` default hoist intact:** a `.noe` trait's default body still hoists onto an
//!   empty impl, unchanged by slice 2.
//!
//! Integration test (own process) because the fixture installs into the process-global default
//! registry once — the single-registry path the CLI uses.

use std::any::Any;

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtFn, ExtModule, ExtTrait, ExtTraitMethod, ExtType, NativeOut, NativeValue, RetTy, SigType,
};
use noeta_stdlib::{CtxError, CtxOut, ExternBox, ExternValue, Host, NativeCtx, Slot, StdError};
use noeta_stdlib::{ctx_arity, no_method_error};
use noeta_vm::VmBackend;

// --- The native value that adopts the trait default (Chip: advertises Gadget, declares no method) ---

/// A `Chip` — an opaque extern value that advertises the native `Gadget` trait but declares **no**
/// `tag` method of its own, so a `chip.tag()` is answered by the trait's default-body dispatch (source
/// 2a). The receiver still rides as slot 0 into that dispatch; the body here reads nothing off it.
#[derive(Debug, Clone)]
struct ChipBox;

impl ExternValue for ChipBox {
    fn type_identity(&self) -> &'static str {
        "fx.Chip"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<ChipBox>().is_some()
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<chip>")
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `Chip`'s own method dispatch — it declares no methods, so any call is an error (a `tag()` never
/// reaches here: the checker resolves it to the trait's default-body route instead).
fn chip_dispatch(
    _recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    Err(StdError {
        kind: noeta_stdlib::ErrorKind::UnknownName,
        message: format!("no method `{method}`"),
    })
}

const CHIP: ExtType = ExtType {
    name: "Chip",
    namespace: "fx",
    methods: &[],
    dispatch: chip_dispatch,
    traits: &["Gadget"],
    ..ExtType::DEFAULTS
};

// --- The native value that OVERRIDES the trait method (Chip2: advertises Gadget, declares `tag`) -----

/// A `Chip2` — an extern value that advertises `Gadget` AND declares its own `tag` method: source (1),
/// the type's own body wins over the trait default. Proves the priority.
#[derive(Debug, Clone)]
struct Chip2Box;

impl ExternValue for Chip2Box {
    fn type_identity(&self) -> &'static str {
        "fx.Chip2"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Chip2Box>().is_some()
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<chip2>")
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn chip2_dispatch(
    _recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match method {
        "tag" => Ok(NativeOut::Str("chip2:override".to_string())),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no method `{method}`"),
        }),
    }
}

const CHIP2: ExtType = ExtType {
    name: "Chip2",
    namespace: "fx",
    methods: &[ExtFn {
        name: "tag",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: chip2_dispatch,
    traits: &["Gadget"],
    ..ExtType::DEFAULTS
};

// --- The native trait, with a default-body dispatch -----------------------------------------------

/// `Gadget`'s **trait-level default body** ([`ExtTrait::dispatch`]) — a [`CtxTypeDispatch`]: the
/// receiver arrives as slot 0 (unread here), arguments after it. It answers `tag()` for any implementing
/// type that does not itself provide it. Both backends run this one shared `fn`, so the differential
/// holds by construction — exactly as a bundle's `ctx_dispatch` does.
fn gadget_dispatch(
    method: &str,
    _ctx: &mut dyn NativeCtx,
    _recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "tag" => {
            ctx_arity(method, args, 0)?;
            Ok(CtxOut::Out(NativeOut::Str("gadget:default".to_string())))
        }
        _ => Err(no_method_error("fx.Gadget", method).into()),
    }
}

/// The native `Gadget` trait: one method `tag(): string`, `has_default: true`, answered by the trait's
/// own [`ExtTrait::dispatch`]. A type binds by advertising it (native) or an empty `impl` (user), and
/// adopts the default unless it overrides.
const GADGET: ExtTrait = ExtTrait {
    name: "Gadget",
    namespace: "fx",
    methods: &[ExtTraitMethod {
        sig: ExtFn {
            name: "tag",
            params: &[],
            ret: RetTy::Concrete(SigType::String),
        },
        has_default: true,
        ..ExtTraitMethod::DEFAULTS
    }],
    dispatch: Some(gadget_dispatch),
    ..ExtTrait::DEFAULTS
};

// --- The module that constructs the native values -------------------------------------------------

const KIT_FNS: &[ExtFn] = &[
    ExtFn {
        name: "chip",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Chip")),
    },
    ExtFn {
        name: "chip2",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Chip2")),
    },
];

fn kit_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "chip" => Ok(NativeOut::Extern(ExternBox::new(ChipBox))),
        "chip2" => Ok(NativeOut::Extern(ExternBox::new(Chip2Box))),
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct FxExtension;

impl noeta_stdlib::registry::Extension for FxExtension {
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
        &[CHIP, CHIP2]
    }
    fn traits(&self) -> &'static [ExtTrait] {
        &[GADGET]
    }
}

static FX: FxExtension = FxExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FX]));
}

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero), returning the shared stdout. Mirrors `ext_trait_seam.rs`.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_trait_default_seam.noe", program);
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
        "backends must agree on the native trait-default program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// The check-only diagnostics of a program, for the negative/positive contract assertions.
#[track_caller]
fn check_diagnostics(program: &str) -> Vec<String> {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_trait_default_check.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    checked
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

// --- Tests ---------------------------------------------------------------------------------------

/// **The load-bearing differential**: all four answer sources in one program, both backends identical,
/// leak-zero. Mutating any one source's expected line (or removing `GADGET.dispatch`) breaks it.
const PROGRAM: &str = r#"
use fx.kit
use fx.Gadget
use fx.Chip
use fx.Chip2

// Source (2a): a native type ADVERTISES the native trait (its `traits` list = the empty bind) and
// declares no `tag` → the trait's native default body answers.
c = kit.chip()
echo c.tag()

// Source (2b): a USER type with an explicit empty `impl Gadget for Uc {}` adopts the SAME native
// default body — the bundle-bind analogue made a trait.
struct Uc {
    n: int
}
impl Gadget for Uc {}
u = Uc { n: 3 }
echo u.tag()

// Source (1) override: a native type that DECLARES `tag` wins over the trait default.
c2 = kit.chip2()
echo c2.tag()

// Source (1) override, user edition: a user type whose impl PROVIDES `tag` wins (not the default).
struct Ov {
    n: int
}
impl Gadget for Ov {
    fn tag(): string {
        return "ov:own"
    }
}
o = Ov { n: 1 }
echo o.tag()

// Source (3): a `.noe` trait's default body still hoists onto an empty impl (unchanged by slice 2).
trait Widget2 {
    fn label(): string {
        return "w2:${self.n}"
    }
}
struct Sw {
    n: int
}
impl Widget2 for Sw {}
s = Sw { n: 7 }
echo s.label()
"#;

#[test]
fn native_trait_default_bodies_agree_on_both_backends() {
    let stdout = run_both_agree(PROGRAM);
    assert_eq!(
        stdout,
        "gadget:default\ngadget:default\nchip2:override\nov:own\nw2:7\n"
    );
}

/// **An empty `impl` of an all-defaulted native trait is accepted** (no E0015): every method carries a
/// default answered by the trait, so the implementor may omit them all — the bundle-bind license.
#[test]
fn an_empty_impl_of_an_all_defaulted_native_trait_is_accepted() {
    let diags = check_diagnostics(
        "use fx.Gadget\nstruct Uc { n: int }\nimpl Gadget for Uc {}\nu = Uc { n: 1 }\necho u.tag()\n",
    );
    assert!(
        diags.is_empty(),
        "an empty impl adopting all defaults must check clean, got {diags:?}"
    );
}

/// **An impl may OVERRIDE a defaulted native-trait method** and is accepted (override allowed, not an
/// error) — the check-only twin of the run test's `Ov` case.
#[test]
fn an_impl_may_override_a_defaulted_native_trait_method() {
    let diags = check_diagnostics(
        "use fx.Gadget\nstruct Ov { n: int }\nimpl Gadget for Ov { fn tag(): string { return \"x\" } }\no = Ov { n: 1 }\necho o.tag()\n",
    );
    assert!(
        diags.is_empty(),
        "an impl overriding a defaulted method must check clean, got {diags:?}"
    );
}
