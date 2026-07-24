//! The **reference evaluation backend**: the Core-IR interpreter plus the shared execution
//! engine it drives ([`Interpreter`] — builtins, method dispatch, native marshalling,
//! tasks/channels — over an `Rc`-based value model and lexical [`Scope`]s).
//!
//! **Role: differential oracle, test-only.** This crate is consumed ONLY by the dev-only
//! `noeta-conformance` harness (enforced by the dependency graph — `noeta-cli` does not link it;
//! `noeta run` executes on the bytecode VM via `noeta-runner`). It interprets the same
//! RC-annotated Core IR the VM compiles from, behind the [`Backend`] trait, returning a
//! *structured* [`RunResult`] — never writing stdout or exiting — so the two backends' results
//! can be asserted byte-identical. The M0 AST tree-walker this crate began as was retired in the
//! memory-management migration (it fired destructors only at teardown, so it could not reproduce
//! last-use destruction); only the crate's *name history* survives in old comments.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::{Rc, Weak};

use noeta_ast::reflect::TypeRepr;
// The trailing-expression desugar + its sentinel live in `noeta_ast::desugar` (audit-3
// finding 10), shared with the VM session so the two backends agree by construction (the
// `session_parity` differential gates).
use noeta_ast::desugar::{REPL_VALUE, rewrite_trailing_expr};
use noeta_ast::{BinaryOp, ForPattern, Pattern, Program, TypeRef, UnaryOp};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

mod cycles;
pub mod drop_audit;
mod ids;
mod ir;
mod leak;
mod native_ctx;
mod ops;
mod value;

pub(crate) use ids::{ChannelId, ScopeId, TaskId};
pub use leak::{live_count, live_peak, reset_peak, set_safepoint_threshold};
pub use value::{IterState, ListRepr, Value};
use value::{PackedList, PackedSchema, PackedSlot, SlotKind};

// The `Backend`/`RunResult` seam moved into its own crate in M1 so the tree-walker and the
// bytecode VM are siblings (neither depends on the other). Re-exported here so existing
// `noeta_eval::{Backend, RunResult}` users keep working.
pub use noeta_backend::{Backend, RunResult};

/// The Core-IR **reference interpreter**, exposed as a [`Backend`] — the differential oracle's
/// non-VM half. (Named `IrRefBackend` in its M0 life; the AST walk was retired in the RC
/// migration and the old name kept misleading readers into treating this as a production path,
/// so it now says what it is. A deprecated alias keeps the old spelling compiling.)
#[derive(Debug, Clone)]
pub struct IrRefBackend {}

/// The pre-RC-migration name of [`IrRefBackend`], kept as a compiling alias.
#[deprecated(
    note = "renamed to IrRefBackend: the AST tree-walk was retired in the RC migration; \
            this backend interprets the Core IR"
)]
pub type TreeWalkBackend = IrRefBackend;

impl IrRefBackend {
    pub fn new() -> IrRefBackend {
        IrRefBackend {}
    }

    // The AST-walk entry points (`run_with_host`, `run_with_sites`, `run_with_host_sites`) were
    // retired in migration Phase 7; production execution is the bytecode VM (`noeta-runner`).
    // The plain [`Backend::run`] path remains for the perf benches and property tests (it lowers
    // to Core IR like everything else — there is no AST walk left), and
    // `Interpreter::run_with_sites` stays as the shared executor the IR interpreter reuses.
}

impl Default for IrRefBackend {
    fn default() -> IrRefBackend {
        IrRefBackend::new()
    }
}

impl Backend for IrRefBackend {
    /// Execute a program through the canonical **Core-IR interpreter** — the same lowering the
    /// conformance reference runs. (The AST tree-walker this crate began as was retired:
    /// it was neither a production path nor the differential oracle — the oracle is VM-vs-IR — so it
    /// was pure duplication. Lowering is total over the parsed language, so this never fails.)
    fn run(&self, program: &Program) -> RunResult {
        // The reference backend is an assembling driver in its own right (audit-6 F2): the checker
        // and lowering resolve std names against the process-default registry. Idempotent.
        noeta_stdlib::registry::default_seeded();
        // Apply the same IR passes the production paths do (`lang run` in `noeta-cli`, the conformance
        // reference): precise-RC drop insertion (with destructor relevance) and reuse-token threading.
        // Without them `reuse` is never set, so e.g. a list self-append `acc ~= [i]` copies the whole
        // accumulator each step — O(n²) instead of the O(n) in-place path the real run takes. A single
        // `check_all` yields both the `type_of` sites and the relevance, so the checker runs once.
        let checked = noeta_check::check_all(program);
        // Lower with the checker's site maps: packed-list literals stream into a flat buffer
        // (P-PACK 2.5) and `list[i].field` reads fuse to `Rvalue::IndexField` (P-PACK 2.5+). Both are
        // carried inline on the IR, so `run_ir` needs no map.
        let ir = noeta_ir::lower_with_sites(program, noeta_ir::lowering_sites!(checked.sites))
            .expect("Core-IR lowering is total over the parsed language");
        let ir = noeta_ir_passes::insert_drops(
            &ir,
            Some(&relevance_of(&checked.sites.destructor_relevance)),
        );
        let ir = noeta_ir_passes::thread_reuse(&ir);
        let deserialize_recipes = checked.sites.deserialize_recipes.iter().cloned().collect();
        let packed_type_layouts = checked
            .sites
            .packed_type_layouts
            .iter()
            .map(|l| (l.type_name.clone(), l.clone()))
            .collect();
        self.run_ir(
            program,
            &ir,
            checked.sites.type_of_sites,
            deserialize_recipes,
            packed_type_layouts,
        )
    }
}

/// The drop pass's relevance form, copied from the checker's (identical sets) — the noeta-eval
/// counterpart to `noeta-conformance`'s `to_relevance` and the compiler's `passes_relevance`, so the
/// `Backend::run` entry point annotates drops identically to the production reference and the VM.
fn relevance_of(r: &noeta_check::DestructorRelevance) -> noeta_ir_passes::Relevance {
    noeta_ir_passes::Relevance {
        locals: r.locals.clone(),
        params: r.params.clone(),
    }
}

/// A persistent evaluation session — the REPL backend. Unlike [`IrRefBackend::run`],
/// which builds a fresh interpreter per program (the clean slate the differential oracle
/// needs), a session keeps its scope and id counter alive across [`Session::eval`] calls,
/// so bindings, `fn`/`type`/`enum`/`class` declarations, and `next_id()` continuity persist
/// between REPL entries.
pub struct Session {
    interp: Interpreter,
    /// The binding names present in a fresh interpreter (the prelude — `len`, `map`, `Ok`, …), so
    /// `:bindings` can list only the *user's* bindings, not the built-ins.
    prelude: std::collections::HashSet<String>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The interpreter holds an `Rc<Scope>` whose capture graph can be cyclic; stay
        // shallow rather than recursing into it.
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    pub fn new() -> Session {
        let interp = Interpreter::new();
        let prelude = interp.scope.names().into_iter().collect();
        Session { interp, prelude }
    }

    /// Evaluate a program against the persistent scope. Returns just this batch's stdout
    /// and diagnostics; if the final statement is a bare expression, its non-unit value's
    /// display form is returned in `value` so the REPL can echo it (`1 + 2` → `3`).
    ///
    /// Execution goes through the **Core-IR interpreter** — the same canonical path as `lang run`
    /// and the conformance reference — so the REPL inherits last-use destruction and in-place reuse
    /// rather than the superseded AST-walk semantics. The batch is lowered, the precise-RC drop and
    /// reuse passes are run (exactly as the bytecode pipeline does), and the top-level statements
    /// execute in the **persistent** global scope with a fresh per-batch temporary frame, so
    /// bindings, `fn`/`type`/`enum`/`class` declarations, and id continuity survive across entries.
    ///
    /// There is **no AST-walker fallback**: lowering is total over the parsed language (no
    /// `Unsupported` is ever produced) and is purely syntactic, so every successfully-parsed REPL
    /// batch lowers. Keeping a fallback would mean a second, *divergent* semantics (the walker fires
    /// destructors only at teardown), so it is deliberately absent — the REPL has exactly one
    /// execution model, the canonical one.
    pub fn eval(&mut self, program: &Program) -> SessionOutput {
        self.interp.stdout.clear();
        self.interp.stderr.clear();
        self.interp.diagnostics.clear();
        // Per-entry trace state: the interpreter persists across entries, and the trace's
        // first-abort-wins rule would otherwise let entry 1's panic mask entry 2's.
        self.interp.abort_trace.clear();
        self.interp.call_sites.clear();

        let value = self
            .run_batch(program)
            .filter(|v| !matches!(v, Value::Unit))
            .map(|v| v.display());
        self.take_output(value)
    }

    /// `:type <expr>` (REPL meta-command) — evaluate `program`'s trailing expression and report its
    /// **runtime** type. The REPL is untyped at the session level (it runs no checker across
    /// entries), so the type is read from the produced value, like the language's `type_of`. The
    /// expression is evaluated (the only way to learn its type here), so any side effects run.
    pub fn type_of(&mut self, program: &Program) -> SessionOutput {
        let value = self.run_batch(program).map(|v| describe_type(&v));
        self.take_output(value)
    }

    /// `:drop`/`:free <name>` (REPL meta-command) — unbind `name` and run its destructor now,
    /// returning `true` if a binding existed. The REPL's top-level bindings are globals with
    /// extended lifetime (they never auto-fire), so this is how a destructor is observed or an
    /// object reclaimed interactively; any destructor output lands in the returned [`SessionOutput`].
    pub fn drop_binding(&mut self, name: &str) -> (bool, SessionOutput) {
        self.interp.stdout.clear();
        self.interp.stderr.clear();
        self.interp.diagnostics.clear();
        let found = match self.interp.scope.remove(name) {
            Some(value) => {
                self.interp.destroy_value(value);
                true
            }
            None => false,
        };
        (found, self.take_output(None))
    }

    /// The live **user** binding names (REPL `:bindings`), excluding the prelude built-ins and the
    /// internal trailing-expression sentinel.
    pub fn binding_names(&self) -> Vec<String> {
        self.interp
            .scope
            .names()
            .into_iter()
            .filter(|n| n != REPL_VALUE && !self.prelude.contains(n))
            .collect()
    }

    /// Reset to a fresh session: a new global scope, id counter, and reflection — `:reset`.
    pub fn reset(&mut self) {
        self.interp = Interpreter::new();
        self.prelude = self.interp.scope.names().into_iter().collect();
    }

    /// Lower, run the precise-RC drop + reuse passes, and execute one batch on the Core-IR
    /// interpreter in the **persistent** global scope, returning the value of a trailing bare
    /// expression (captured via the reserved sentinel binding) when the batch completes cleanly.
    /// Shared by [`Session::eval`] and [`Session::type_of`]; clears stdout/diagnostics first.
    fn run_batch(&mut self, program: &Program) -> Option<Value> {
        self.interp.stdout.clear();
        self.interp.stderr.clear();
        self.interp.diagnostics.clear();
        // A trailing bare expression is rewritten to a binding of a reserved sentinel name, so its
        // value is captured in scope (and read back) while it is evaluated exactly once on the IR
        // path.
        let (lowerable, captures_value) = rewrite_trailing_expr(program);
        let ir = noeta_ir::lower(&lowerable)
            .expect("Core-IR lowering is total over the parsed language (the REPL only feeds parsed programs)");
        // Mirror the bytecode pipeline: precise-RC drops (no checker in the REPL, so drops are
        // conservatively destructor-relevant) then reuse tokens. This entry's reflection is
        // **accumulated** into the persistent set (latest-wins), so `attributes_of`/`roles_of`/type
        // queries resolve for types declared in *earlier* entries too — the VM's `SessionCompiler`
        // accumulates identically, so the session differential stays green.
        let ir = noeta_ir_passes::insert_drops(&ir, None);
        let ir = noeta_ir_passes::thread_reuse(&ir);
        let native_roles = self.interp.reg().native_roles();
        self.interp
            .reflection
            .accumulate(noeta_ast::reflect::build(&lowerable, &native_roles));
        let flow = self.interp.run_ir_batch(&ir);
        // **Remove** (not clone) the sentinel so an evaluated trailing value never lingers in scope.
        // Keeping it bound would hold a reference to the value across entries, which would both leak
        // it and — by raising the refcount — suppress a later `:drop`'s destructor on the same
        // object. The taken value is owned by the caller (displayed / typed, then dropped). A
        // `return`/error before the sentinel binding ran yields `None`.
        let captured = self.interp.scope.remove(REPL_VALUE);
        match (flow, captures_value) {
            (Ok(Flow::Normal), true) => captured,
            _ => None,
        }
    }

    /// Drain this batch's stdout and diagnostics into a [`SessionOutput`] with the given value.
    fn take_output(&mut self, value: Option<String>) -> SessionOutput {
        SessionOutput {
            stdout: std::mem::take(&mut self.interp.stdout),
            stderr: std::mem::take(&mut self.interp.stderr),
            diagnostics: std::mem::take(&mut self.interp.diagnostics),
            value,
            trace: std::mem::take(&mut self.interp.abort_trace),
        }
    }
}

/// A readable runtime type label for the REPL `:type` command — the user-facing type name of a
/// value. Objects and enums report their declared type name (`Res`, `Status`); collections and
/// primitives report their kind (`list`, `int`); generics are erased, matching the language's
/// head-constructor `type_of` fidelity.
fn describe_type(value: &Value) -> String {
    match value {
        Value::Object(object) => object.def.name().to_string(),
        Value::Enum(e) => e.enum_name.clone(),
        Value::Type(def) => format!("type {}", def.name()),
        Value::EnumType(def) => format!("enum type {}", def.name()),
        other => other.type_name().to_string(),
    }
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

/// The outcome of one [`Session::eval`]: this batch's output, any diagnostics, and the
/// display value of a trailing bare expression (for the REPL to print).
#[derive(Debug, Clone)]
pub struct SessionOutput {
    pub stdout: String,
    /// This entry's standard-error output (`std.io`'s `err`/`errln`), the stderr twin of `stdout`.
    pub stderr: String,
    pub diagnostics: Vec<Diagnostic>,
    pub value: Option<String>,
    /// The abort traceback if this entry panicked (empty otherwise) — innermost frame first. A frame
    /// from a function defined in an *earlier* entry carries a span into that entry's (gone) text;
    /// the renderer degrades it to name-only.
    pub trace: Vec<noeta_backend::TraceFrame>,
}

// --- Functions and scopes ---

/// A built-in (native) function from the prelude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `len(x)` — element/character count of a list, map, or string.
    Len,
    /// `map(list, fn)` — a new list with `fn` applied to each element.
    Map,
    /// `filter(list, fn)` — a new list of the elements for which `fn` is true.
    Filter,
    /// `sum(list)` — the numeric sum of a list's elements.
    Sum,
    /// `Ok(x)` / `Ok()` — construct a `Result.Ok`.
    MakeOk,
    /// `Err(e)` — construct a `Result.Err`.
    MakeErr,
    /// `some(x)` — construct an `Option.some`.
    MakeSome,
    /// `panic(msg)` — abort with an unrecoverable runtime diagnostic and nonzero exit.
    Panic,
    /// `assert(cond)` / `assert(cond, msg)` — abort (a `Panic` diagnostic) when `cond` is false.
    /// The assertion primitive `@test` blocks rest on (object-model slice 6).
    Assert,
    // (The whole orchestration family — `task` at higher-order-abi H0/H2, `http.serve` at H3,
    // `signal`/`computed`/`effect` at H5 — migrated onto the registry's `NativeCtx` dispatch,
    // `noeta-stdlib/src/{task,serve,reactive}.rs`.)
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Len => "len",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Sum => "sum",
            Builtin::MakeOk => "Ok",
            Builtin::MakeErr => "Err",
            Builtin::MakeSome => "some",
            Builtin::Panic => "panic",
            Builtin::Assert => "assert",
        }
    }

    /// The prelude functions registered in every program's global scope. `none` is a
    /// prelude *value* (not a function), so it is bound separately in [`Interpreter::new`].
    const PRELUDE: &'static [Builtin] = &[
        // `Len`/`Map`/`Filter`/`Sum` left the prelude (prelude-redesign P1.2): the collection
        // METHOD forms route to the same impls; `list.len`-style handles cover value use.
        Builtin::MakeOk,
        Builtin::MakeErr,
        Builtin::MakeSome,
        Builtin::Panic,
        Builtin::Assert,
        // `Signal`/`Computed`/`Effect` left the prelude (P2a) for `use std.reactive`, and
        // `Sleep`/`All`/`Race`/`MapBounded` (P2b) for `use std.task` — all bound by
        // `declare_use` as first-class builtin values when imported.
    ];
}

/// The body of a user function: a single arrow expression, a `{ ... }` block, or a lowered
/// Core-IR function. The AST walker only ever builds the first two; the Core-IR interpreter
/// builds the third. Routing all three through the one `Closure` type is what lets the IR
/// interpreter reuse the tree-walker's call machinery (`call_closure`/`call_method_on`)
/// unchanged — one value model, one dispatch path.
/// The definition of an enum type, registered as an `EnumType` value.
#[derive(Debug)]
pub struct EnumDef {
    name: String,
    variants: Vec<VariantInfo>,
    /// `@derive(Comparable)` without a hand-written `compare` (derive-soundness S3): `< <= > >=`
    /// order by variant declaration index, then payload fields — the enum twin of
    /// [`TypeDef::derives_comparable`].
    derives_comparable: bool,
    /// `@derive(Serialize<Json>)` without a hand-written `to_json` — gates the synthesized
    /// `.to_json()` (the variant rendering `json.stringify` produces).
    derives_tojson: bool,
    /// Inherent + `impl`-block methods (the unified body, object-model slice 3), compiled to
    /// closures capturing the definition (global) scope — exactly like a struct/class's `methods`.
    /// An instance call `value.m(...)` and an associated call `Enum.f(...)` both resolve here; the
    /// distinction (a value receiver vs. a bare type-name receiver) is made at the call site.
    pub(crate) methods: HashMap<String, Rc<Closure>>,
}

impl EnumDef {
    pub fn name(&self) -> &str {
        &self.name
    }

    fn variant(&self, name: &str) -> Option<&VariantInfo> {
        self.variants.iter().find(|v| v.name == name)
    }

    /// The variant's declaration index — the primary key of derived `Comparable` on enums.
    fn variant_index(&self, name: &str) -> usize {
        self.variants
            .iter()
            .position(|v| v.name == name)
            .unwrap_or(usize::MAX)
    }

    fn method(&self, name: &str) -> Option<&Rc<Closure>> {
        self.methods.get(name)
    }
}

#[derive(Debug)]
struct VariantInfo {
    name: String,
    field_names: Vec<String>,
}

/// A constructed enum value.
#[derive(Debug)]
pub struct EnumValue {
    enum_name: String,
    variant: String,
    pub(crate) data: Vec<Value>,
    /// The variant's declaration index — derived `Comparable`'s primary key (variant order, then
    /// payload). Type metadata like the VM `Shape::variant_index`: **excluded from `PartialEq`**
    /// (equality compares name/variant/data).
    variant_index: usize,
    /// The reflected type for a **generic** enum-variant construction (runtime type-argument
    /// reflection, R2b.2), `Some` only for a generic enum — so `type_of` recovers its type arguments
    /// after a `dyn` launder. `None` for a non-generic enum / an ordinary construction. Invisible to
    /// value semantics (equality compares `enum_name`/`variant`/`data`) — the tree-walker twin of the
    /// VM's node tag. An enum value's type is invariant, so it is never cleared.
    reflect: Option<Rc<TypeRepr>>,
}

/// Structural equality — the reflected type tag (R2b.2) is **invisible to value semantics**, so two
/// enum values with equal name/variant/data are equal regardless of their tags (the tree-walker twin
/// of the VM comparing `Payload::Enum`, not the node tag).
impl PartialEq for EnumValue {
    fn eq(&self, other: &EnumValue) -> bool {
        self.enum_name == other.enum_name
            && self.variant == other.variant
            && self.data == other.data
    }
}

impl EnumValue {
    pub fn display(&self) -> String {
        // The built-in `Result`/`Option` print with their bare surface constructors
        // (`Ok(x)`, `none`), matching how they are written; user enums keep `Type.Variant`.
        let head = if self.is_builtin_result_or_option() {
            self.variant.clone()
        } else {
            // Display strips a qualified identity to its short name; the identity keyed on for
            // dispatch/`is`/`as` stays qualified.
            format!(
                "{}.{}",
                noeta_ast::short_type_name(&self.enum_name),
                self.variant
            )
        };
        if self.data.is_empty() {
            head
        } else {
            let parts: Vec<String> = self.data.iter().map(Value::display).collect();
            format!("{head}({})", parts.join(", "))
        }
    }

    fn is_builtin_result_or_option(&self) -> bool {
        self.enum_name == "Result" || self.enum_name == "Option"
    }
}

/// The definition of a struct or class type, registered as a [`Value::Type`].
///
/// Structs and classes share one representation: a struct is just a class with no
/// methods. `new`/`draft`/etc. are ordinary entries in `methods` (associated functions
/// returning the type); the distinction between an associated function and an instance
/// method is made at the call site (a type receiver vs. an instance receiver), not here.
pub struct TypeDef {
    name: String,
    fields: Vec<FieldSpec>,
    pub(crate) methods: HashMap<String, Rc<Closure>>,
    /// The class's `destruct` block lowered to a parameterless Core-IR [`noeta_ir::Func`], if any —
    /// run by the runtime when the last reference to an instance drops (not directly callable),
    /// with the instance's fields and `self` bound into its scope. Shared so an `ObjectValue` can
    /// reach it.
    pub(crate) destructor: Option<Rc<noeta_ir::Func>>,
    /// Whether this came from a `struct X {...}` struct (vs. a `class`). Cosmetic in M0.
    is_struct: bool,
    /// Whether `==` on this type is **structural** (field-wise) rather than **reference identity**
    /// (object-model slice 2): true for a value `struct`/opaque, or a `class` that is `Equatable`
    /// (derives it or hand-`impl`s `eq`); false only for a plain `class` (`==` → identity). Mirrors
    /// the VM `Shape::structural_eq`, computed from the same inputs so both backends agree — even on
    /// a *nested* class field, where the method-dispatch path is unavailable.
    structural_eq: bool,
    /// Whether values of this type may **key a `Map` / member a `Set`** (P-PKEY): a key-capable
    /// `@packed` struct. A `Cell` because capability settles by fixpoint — a later declaration
    /// can complete a forward-referenced nested chain, and the interpreter re-stamps every
    /// settled type's (`Rc`-shared) def then. Mirrors the VM's `Shape::key_capable`, computed
    /// from the same shared `noeta_ast::key_capable_packed`.
    key_capable: std::cell::Cell<bool>,
    /// Whether the type `@derive(Comparable)`s without a hand-written `compare`: its instances
    /// get structural field-wise ordering for `< <= > >=`.
    derives_comparable: bool,
    /// Whether the type `@derive(Serialize<Json>)`s without a hand-written `to_json`: `o.to_json()`
    /// synthesizes a structural JSON serializer.
    derives_tojson: bool,
    /// An *opaque* stub introduced by a `use` import: its real field set is unknown until
    /// module loading lands (M1), so its all-fields literal accepts whatever fields are
    /// given (no unknown-field or full-init checks) and `..` spread copies the whole base.
    opaque: bool,
    /// Each field carrying a default (`x: T = expr`), as a parameterless [`noeta_ir::Thunk`] run in
    /// the type's **definition (global) scope** when a literal omits the field (object-model
    /// slice 5). Keyed by field name; only defaulted fields appear. Empty for an all-mandatory type
    /// and for opaque imports. Mirrors the VM's `(type, field) → default thunk` table, so a missing
    /// field is filled identically in both backends.
    field_defaults: Vec<(String, noeta_ir::Thunk)>,
}

impl TypeDef {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The slot index of `name` in this type's declared field list, or `None` — a linear scan of the
    /// (small) field list, used to locate a packed field by name in [`crate::value::PackedList::field`].
    pub(crate) fn slot_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

impl std::fmt::Debug for TypeDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Methods hold `Rc<Closure>` whose captured scope can be cyclic; never recurse.
        f.debug_struct("TypeDef")
            .field("name", &self.name)
            .field("is_struct", &self.is_struct)
            .field("opaque", &self.opaque)
            .finish_non_exhaustive()
    }
}

/// One declared field of a [`TypeDef`]: its name. (Field mutability is a *static* concern — the
/// checker enforces `mut`-field assignment, E0033 — so the runtime tree-walker records only the name.)
struct FieldSpec {
    name: String,
}

/// A struct or class instance: its type and the values of its fields. A value `struct` is
/// effectively immutable (a `..` structural update or `x.f = v` produces a new object via the
/// copy-on-write path); a reference `class` mutates **in place** through the `RefCell`, so the
/// change is visible through every alias sharing the `Rc` (object-model slice 2b). The `RefCell`
/// is what lets a *shared* (`strong_count > 1`) class instance be mutated at all — a bare
/// `Rc<BTreeMap>` cannot. Borrows are always released promptly (snapshot-then-release) so a method
/// or destructor that re-enters and mutates the same instance never hits a borrow conflict.
pub struct ObjectValue {
    pub(crate) def: Rc<TypeDef>,
    /// The field values in **slot order**, parallel to `def.fields` (`slots[i]` is the value of
    /// `def.fields[i]`). This mirrors the VM's `Payload::Object { shape, slots }` — the layout
    /// groundwork P-PACK Phase 1 needs — replacing the former name-keyed `BTreeMap`: a `Vec` index
    /// is a cache-friendly array slot, not a tree node, and the slot↔name map lives once on the
    /// shared `def`, not per instance. An opaque `use`-import is constructed with a per-literal `def`
    /// whose fields are the literal's keys in sorted order (matching the VM's opaque shape), so even
    /// dynamic-field imports fit the uniform slot model.
    pub(crate) slots: RefCell<Vec<Value>>,
    /// A monotonic per-run **creation sequence** (object-model slice 2c): the instance's allocation
    /// age. The cycle reaper finalizes reclaimed members in reverse-creation order (newest-first) by
    /// this key, matching the VM's `ObjHeader::seq` so cyclic `destruct` order agrees across backends.
    seq: u64,
    /// The checker-resolved reflected type (runtime type-argument reflection, R2), `Some` only for a
    /// **generic** instantiation (`Box<int>` → `Struct("Box", [Int])`) so `type_of` recovers the type
    /// arguments after a `dyn` launder; `None` for a non-generic type (recovered head-only from the
    /// shape) and every non-literal-constructed instance. Invisible to value semantics — `PartialEq`
    /// compares only `def`/`slots` — the tree-walker twin of the VM's node tag. An object's type is
    /// invariant under field mutation, so (unlike the collection tags) it is never cleared.
    reflect: Option<Rc<TypeRepr>>,
}

thread_local! {
    /// Monotonic object-creation counter for [`ObjectValue::seq`] (object-model slice 2c).
    static OBJECT_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl ObjectValue {
    /// Build an instance from its **slot-ordered** field values (parallel to `def.fields`), routing
    /// through the leak-oracle counter (paired with [`Drop`]). All `ObjectValue` construction goes
    /// through here so the live count is exact. The caller guarantees `slots.len() == def.fields.len()`.
    fn new(def: Rc<TypeDef>, slots: Vec<Value>) -> ObjectValue {
        ObjectValue::new_reflected(def, slots, None)
    }

    /// As [`ObjectValue::new`], but carrying the reflected type tag (R2). Used only at object-literal
    /// construction for a generic instantiation; every other construction path uses [`ObjectValue::new`]
    /// (untagged → head-only reflection).
    fn new_reflected(
        def: Rc<TypeDef>,
        slots: Vec<Value>,
        reflect: Option<Rc<TypeRepr>>,
    ) -> ObjectValue {
        debug_assert_eq!(
            slots.len(),
            def.fields.len(),
            "slot count must match the shape"
        );
        leak::inc();
        let seq = OBJECT_SEQ.with(|c| {
            let s = c.get();
            c.set(s.wrapping_add(1));
            s
        });
        ObjectValue {
            def,
            slots: RefCell::new(slots),
            seq,
            reflect,
        }
    }

    /// The slot index of `name` in this object's shape, or `None` if it has no such field. A linear
    /// scan of the (small) declared field list — the tree-walker analogue of `Shape::slot_of`.
    fn slot_of(&self, name: &str) -> Option<usize> {
        self.def.fields.iter().position(|f| f.name == name)
    }

    /// The field's current value (cloned — a refcount bump for heap values), or `None` if absent.
    /// Releases the borrow before returning, so the caller may freely re-enter the instance.
    fn field(&self, name: &str) -> Option<Value> {
        self.slot_of(name).map(|i| self.slots.borrow()[i].clone())
    }

    /// Whether the instance currently has `name` among its fields.
    fn has_field_value(&self, name: &str) -> bool {
        self.slot_of(name).is_some()
    }

    /// A cloned snapshot of the slot vector (each value's refcount bumped) — used by the
    /// copy-on-write (`struct`) update path and by traversals that must not hold a borrow across
    /// re-entry. Slot-ordered, parallel to `def.fields`.
    fn fields_snapshot(&self) -> Vec<Value> {
        self.slots.borrow().clone()
    }

    /// Overwrite the slot named `name` **in place** (reference-`class` mutation, or a uniquely-owned
    /// `struct` update), returning the displaced value so the caller can destroy it. The field must
    /// exist (callers validate first); a non-field name is a no-op returning `None`.
    fn set_field_value(&self, name: &str, value: Value) -> Option<Value> {
        match self.slot_of(name) {
            Some(i) => Some(std::mem::replace(&mut self.slots.borrow_mut()[i], value)),
            None => None,
        }
    }

    /// `Type { field: value, ... }`, fields in slot (declared, or sorted for an opaque import) order.
    pub fn display(&self) -> String {
        let slots = self.slots.borrow();
        let parts: Vec<String> = self
            .def
            .fields
            .iter()
            .zip(slots.iter())
            .map(|(f, value)| format!("{}: {}", f.name, value.repr()))
            .collect();
        // Display strips a qualified identity to its short name; the identity keyed on for
        // dispatch/`is`/`as` stays qualified.
        format!(
            "{} {{{}}}",
            noeta_ast::short_type_name(&self.def.name),
            parts.join(", ")
        )
    }
}

impl Drop for ObjectValue {
    fn drop(&mut self) {
        leak::dec();
    }
}

impl std::fmt::Debug for ObjectValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectValue({})", self.display())
    }
}

impl PartialEq for ObjectValue {
    fn eq(&self, other: &ObjectValue) -> bool {
        // Structural equality: same type name, same field layout, and slot-wise equal values (the
        // value-kind comparison; a reference class's identity `==` is decided in `ops::values_equal`
        // before reaching here). The field-name check guards the opaque case, where two imports can
        // share a name but carry different field sets.
        self.def.name == other.def.name
            && self.def.fields.len() == other.def.fields.len()
            && self
                .def
                .fields
                .iter()
                .zip(&other.def.fields)
                .all(|(a, b)| a.name == b.name)
            && *self.slots.borrow() == *other.slots.borrow()
    }
}

/// A user function value: its parameter names, its body, and the lexical scope it was
/// defined in (captured for closures and recursion).
pub struct Closure {
    params: Vec<String>,
    /// Each parameter's default-value thunk, parallel to `params` (`None` for a required
    /// parameter). A call that omits a trailing argument evaluates the matching default in the
    /// closure's `captured` (definition/global) scope — never seeing other parameters or fields,
    /// matching the VM's globals-only default thunks.
    defaults: Vec<Option<noeta_ir::Thunk>>,
    body: Rc<noeta_ir::Func>,
    pub(crate) captured: Rc<Scope>,
    /// The declared name (`"f"`, `"Type.method"`) for the abort traceback — the eval twin of the
    /// VM's `Chunk::name`. `None` for an anonymous closure value.
    name: Option<String>,
}

impl Closure {
    /// Build a closure, routing through the leak-oracle counter (paired with [`Drop`]). All
    /// `Closure` construction goes through here so the live count is exact. Capturing a scope is the
    /// one way the tree-walker can tie a reference cycle (`scope → vars → closure → captured → scope`,
    /// e.g. a self-recursive nested `fn`), which `Rc` alone cannot reclaim — so the captured scope is
    /// recorded as a cycle-collection candidate (Phase 6.3, [`register_captured_scope`]).
    fn new(
        params: Vec<String>,
        defaults: Vec<Option<noeta_ir::Thunk>>,
        body: Rc<noeta_ir::Func>,
        captured: Rc<Scope>,
        name: Option<String>,
    ) -> Closure {
        leak::inc();
        register_captured_scope(&captured);
        Closure {
            params,
            defaults,
            body,
            captured,
            name,
        }
    }
}

thread_local! {
    /// Every scope a closure has captured this run, held **weakly** so membership never keeps a
    /// scope alive. The Phase-6.3 eval cycle reaper ([`Interpreter::reap_captured_scope_cycles`])
    /// walks it at clean exit: a captured scope still live after global teardown is reachable only
    /// through a capture cycle (the tree-walker's analogue of the VM's mark-sweep over the heap
    /// registry), so breaking its bindings lets `Rc` cascade-free it.
    static CAPTURED_SCOPES: RefCell<Vec<Weak<Scope>>> = const { RefCell::new(Vec::new()) };
}

/// Record a captured scope as a candidate cycle root (a weak reference, so it imposes no ownership).
///
/// A `Weak` keeps the scope's `Rc` *control block* allocated even after the scope itself is freed, so
/// the buffer is **self-pruned**: when it has doubled past 64, dead handles (whose scope is already
/// gone) are dropped, freeing those headers. Without this a long-lived session (the REPL drives the
/// per-batch path, which never reaches the exit reaper) or a closure-heavy loop would pin one header
/// per closure ever created. Pruning at power-of-two lengths makes it amortized O(1) and bounds the
/// buffer to ~2× the live captured-scope set.
fn register_captured_scope(scope: &Rc<Scope>) {
    CAPTURED_SCOPES.with(|r| {
        let mut reg = r.borrow_mut();
        if reg.len() >= 64 && reg.len().is_power_of_two() {
            reg.retain(|w| w.strong_count() > 0);
        }
        reg.push(Rc::downgrade(scope));
    });
}

thread_local! {
    /// Every reference-`class` instance whose `mut` field has been **assigned in place** this run,
    /// held weakly (object-model slice 2c). A class cycle (`a.next = b; b.next = a`) can only be
    /// *closed* by such an in-place field assignment — construction cannot reference a
    /// not-yet-created object — so the cycle members are a subset of these. The exit reaper
    /// ([`Interpreter::reap_object_cycles`]) walks it after global teardown: any survivor is reachable
    /// only through a reference cycle `Rc` cannot reclaim, so draining its fields lets the rest
    /// cascade-free. (The VM analogue is the heap-registry mark-sweep in `noeta-gc`.)
    static MUTATED_OBJECTS: RefCell<Vec<Weak<ObjectValue>>> = const { RefCell::new(Vec::new()) };
}

/// Record a class instance whose field was just mutated as a candidate cycle member (a weak
/// reference — no ownership). Self-pruned at power-of-two lengths past 64, exactly as
/// [`register_captured_scope`].
fn register_mutated_object(obj: &Rc<ObjectValue>) {
    MUTATED_OBJECTS.with(|r| {
        let mut reg = r.borrow_mut();
        if reg.len() >= 64 && reg.len().is_power_of_two() {
            reg.retain(|w| w.strong_count() > 0);
        }
        reg.push(Rc::downgrade(obj));
    });
}

/// The still-live captured-scope candidates (upgraded, deduplicated by pointer) — the safepoint
/// collector's seed set. **Peeked, not drained**: live and deferred entries must stay registered
/// for the exit reapers; entries whose target the collection frees fail their next upgrade and are
/// pruned by [`prune_cycle_registries`].
pub(crate) fn captured_scope_candidates() -> Vec<Rc<Scope>> {
    CAPTURED_SCOPES.with(|r| {
        let mut seen = std::collections::HashSet::new();
        r.borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|s| seen.insert(Rc::as_ptr(s) as usize))
            .collect()
    })
}

/// The still-live mutated-object candidates — see [`captured_scope_candidates`].
pub(crate) fn mutated_object_candidates() -> Vec<Rc<ObjectValue>> {
    MUTATED_OBJECTS.with(|r| {
        let mut seen = std::collections::HashSet::new();
        r.borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|o| seen.insert(Rc::as_ptr(o) as usize))
            .collect()
    })
}

/// Drop dead weak entries from both candidate registries (after a safepoint collection freed
/// their targets), bounding the registries to the live candidate set.
pub(crate) fn prune_cycle_registries() {
    CAPTURED_SCOPES.with(|r| r.borrow_mut().retain(|w| w.strong_count() > 0));
    MUTATED_OBJECTS.with(|r| r.borrow_mut().retain(|w| w.strong_count() > 0));
}

impl Drop for Closure {
    fn drop(&mut self) {
        leak::dec();
    }
}

impl std::fmt::Debug for Closure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately shallow: the captured scope can form reference cycles, so we
        // never recurse into it.
        f.debug_struct("Closure")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

/// A name binding and whether it may be reassigned.
struct Binding {
    pub(crate) value: Value,
    mutable: bool,
}

/// A lexical scope: its own bindings plus a link to the enclosing scope. Reference-
/// counted with non-atomic `Rc` (shared-nothing per isolate, per the design). Cyclic
/// captures (a function holding the scope that captures it) form an `Rc` cycle that
/// refcounting alone cannot break; the migration's Phase-6 exit-time reaper reclaims
/// them by clearing the bindings of any captured scope still live after global teardown,
/// so heap residency reaches 0 on this backend (the leak oracle's gate).
struct Scope {
    pub(crate) vars: RefCell<HashMap<String, Binding>>,
    /// Binding names in declaration order, so the runtime can destroy them in reverse
    /// declaration order at scope exit — the deterministic destruction order the spec wants.
    pub(crate) order: RefCell<Vec<String>>,
    pub(crate) parent: Option<Rc<Scope>>,
    /// The SEALED-fn frontier: `Some(allow)` on a named fn's call scope. An outward write walk
    /// (`assign`/`assign_force`/`take_mut`) crossing this scope may continue to the parent only
    /// for names in `allow` (the `use (…)` captures) — any other name reports `NotFound`, so the
    /// caller declares a fresh local, exactly as the checker typed it. Reads (`lookup`) are not
    /// gated: the checker already rejected unlisted reads, and statics resolve through the chain.
    seal: Option<HashSet<String>>,
}

/// The outcome of trying to reassign an existing binding through the scope chain. `Assigned`
/// carries the *displaced* value, so the caller can run its destructor if it was the last
/// reference.
enum AssignOutcome {
    Assigned(Value),
    Immutable,
    NotFound,
}

impl Drop for Scope {
    fn drop(&mut self) {
        // The leak-oracle counterpart of construction (always paired, so a scope reclaimed by `Rc`
        // drops the count; one kept alive by a capture cycle never does — which is the leak we want
        // the oracle to report). See [`crate::leak`].
        leak::dec();
        // Clear any drop-audit poison keyed to this scope's address before it can be reused.
        drop_audit::on_scope_drop(self as *const Scope as usize);
    }
}

impl Scope {
    fn global() -> Rc<Scope> {
        leak::inc();
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            parent: None,
            seal: None,
        })
    }

    fn child(parent: &Rc<Scope>) -> Rc<Scope> {
        leak::inc();
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            parent: Some(Rc::clone(parent)),
            seal: None,
        })
    }

    /// A call scope carrying a sealed fn's write frontier (see the `seal` field).
    fn sealed_child(parent: &Rc<Scope>, allow: &[String]) -> Rc<Scope> {
        leak::inc();
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            parent: Some(Rc::clone(parent)),
            seal: Some(allow.iter().cloned().collect()),
        })
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        if let Some(binding) = self.vars.borrow().get(name) {
            // The drop audit (Phase 3.x) flags a read of a binding whose value was dropped here and
            // not since rebound — a use-after-drop, i.e. a static death that preceded this real use.
            drop_audit::on_read(self as *const Scope as usize, name);
            return Some(binding.value.clone());
        }
        self.parent.as_ref().and_then(|parent| parent.lookup(name))
    }

    fn declare(&self, name: String, value: Value, mutable: bool) {
        // A (re)binding makes any prior drop of this name in this scope legitimate again.
        drop_audit::on_bind(self as *const Scope as usize, &name);
        let mut vars = self.vars.borrow_mut();
        if vars
            .insert(name.clone(), Binding { value, mutable })
            .is_none()
        {
            self.order.borrow_mut().push(name);
        }
    }

    /// Reassign an existing binding, searching outward through the chain, returning the
    /// displaced value on success.
    fn assign(&self, name: &str, value: Value) -> AssignOutcome {
        if let Some(binding) = self.vars.borrow_mut().get_mut(name) {
            return if binding.mutable {
                // A reassignment rebinds the name: clear any drop poison for it in this scope.
                drop_audit::on_bind(self as *const Scope as usize, name);
                let old = std::mem::replace(&mut binding.value, value);
                AssignOutcome::Assigned(old)
            } else {
                AssignOutcome::Immutable
            };
        }
        if let Some(allow) = &self.seal
            && !allow.contains(name)
            && !name.starts_with('$')
        {
            // The sealed frontier: an unlisted name never reaches the surrounding scope.
            // (`$`-prefixed names are the lowering's synthesized state variables — seal-exempt.)
            return AssignOutcome::NotFound;
        }
        match &self.parent {
            Some(parent) => parent.assign(name, value),
            None => AssignOutcome::NotFound,
        }
    }

    /// Reassign an existing binding **ignoring** its mutability — used for a field-set `x.f = v`
    /// (object-model slice 2b′), where the checker has already enforced the kind-aware `mut` rule (a
    /// `struct` field-set on an immutable binding is a static E0006; a `class` field-set mutates the
    /// shared instance in place, and the rebind merely restores `x` to that same instance).
    fn assign_force(&self, name: &str, value: Value) -> AssignOutcome {
        if let Some(binding) = self.vars.borrow_mut().get_mut(name) {
            drop_audit::on_bind(self as *const Scope as usize, name);
            let old = std::mem::replace(&mut binding.value, value);
            return AssignOutcome::Assigned(old);
        }
        if let Some(allow) = &self.seal
            && !allow.contains(name)
            && !name.starts_with('$')
        {
            return AssignOutcome::NotFound;
        }
        match &self.parent {
            Some(parent) => parent.assign_force(name, value),
            None => AssignOutcome::NotFound,
        }
    }

    /// Take the value out of an existing **mutable** binding (replacing it with `Unit`),
    /// searching outward through the chain like [`Scope::assign`]. Returns the displaced value, or
    /// `None` if the nearest binding for `name` is immutable or absent (in which case the caller
    /// must fall back to the ordinary path, which reports the right error).
    ///
    /// Used by the copy-on-write self-append fast path (`acc ~= [x]`): dropping the scope's
    /// reference *before* the right-hand side is evaluated lets a uniquely-owned list be appended
    /// in place instead of copied, turning the O(n²) accumulator loop into O(n).
    fn take_mut(&self, name: &str) -> Option<Value> {
        if let Some(binding) = self.vars.borrow_mut().get_mut(name) {
            return binding
                .mutable
                .then(|| std::mem::replace(&mut binding.value, Value::Unit));
        }
        if let Some(allow) = &self.seal
            && !allow.contains(name)
            && !name.starts_with('$')
        {
            // Sealed frontier: the COW fast path must not reach an unlisted surrounding binding.
            return None;
        }
        self.parent
            .as_ref()
            .and_then(|parent| parent.take_mut(name))
    }

    /// **Plainly** release the value of the nearest binding for `name` — replace it with `Unit`,
    /// dropping the old value's `Rc` with **no** `destruct` block (that is `destroy_value`'s job,
    /// reserved for globals until Phase 4). This is the IR `DropVar` operation (Phase 3): it
    /// reclaims a function-local's value at its last use instead of at scope teardown. Searches
    /// outward like [`Scope::assign`]; a no-op if `name` is not bound (the drop pass only targets
    /// function-locals, never globals or captures, so the nearest binding is the intended local).
    /// The binding itself stays (holding `Unit`), so scope-exit drain finds nothing more to free.
    fn release_binding(&self, name: &str) {
        if let Some(binding) = self.vars.borrow_mut().get_mut(name) {
            binding.value = Value::Unit;
            // Poison this binding for the drop audit: a subsequent read before a rebind is a
            // use-after-drop (a static death that preceded the real last use).
            drop_audit::on_drop(self as *const Scope as usize, name);
            return;
        }
        if let Some(parent) = &self.parent {
            parent.release_binding(name);
        }
    }

    /// Like [`Scope::release_binding`], but **returns** the displaced value (leaving `Unit` in the
    /// slot) so the caller can run its `destruct` block if it holds the last reference — the
    /// Phase-4 destructor-firing drop. Returns `None` if `name` is unbound. The audit poison and
    /// outward search match `release_binding`; only the disposal differs (the caller owns the value
    /// and decides whether a destructor fires, rather than the `Rc` dropping silently here).
    fn take_for_drop(&self, name: &str) -> Option<Value> {
        if let Some(binding) = self.vars.borrow_mut().get_mut(name) {
            let value = std::mem::replace(&mut binding.value, Value::Unit);
            drop_audit::on_drop(self as *const Scope as usize, name);
            return Some(value);
        }
        self.parent
            .as_ref()
            .and_then(|parent| parent.take_for_drop(name))
    }

    /// Remove a binding **entirely**, returning its value so the caller can run its destructor
    /// (the REPL `:drop`/`:free` meta-command). Unlike [`Scope::take_for_drop`], which leaves `Unit`
    /// in the slot, the name is fully unbound afterward, so a later reference to it is an unknown
    /// name. Searches outward through the chain like the other binding operations.
    fn remove(&self, name: &str) -> Option<Value> {
        if let Some(binding) = self.vars.borrow_mut().remove(name) {
            self.order.borrow_mut().retain(|n| n != name);
            drop_audit::on_drop(self as *const Scope as usize, name);
            return Some(binding.value);
        }
        self.parent.as_ref().and_then(|parent| parent.remove(name))
    }

    /// The names bound directly in this scope, in declaration order (the REPL `:bindings` command
    /// lists the persistent global scope's names).
    fn names(&self) -> Vec<String> {
        self.order.borrow().clone()
    }

    /// Remove and return this scope's bindings in **reverse** declaration order, for
    /// deterministic destruction at scope exit.
    fn drain_reverse(&self) -> Vec<Value> {
        let order = std::mem::take(&mut *self.order.borrow_mut());
        let mut vars = self.vars.borrow_mut();
        order
            .into_iter()
            .rev()
            .filter_map(|name| vars.remove(&name).map(|b| b.value))
            .collect()
    }
}

/// Why evaluation is unwinding the Rust call stack. Either a fatal abort (a diagnostic
/// has been recorded and the whole program stops) or an early function return triggered
/// by `?` short-circuiting on an `Err`/`none`, caught at the enclosing call boundary.
enum Unwind {
    Abort,
    Return(Value),
}

type Eval<T> = Result<T, Unwind>;

/// Whether a statement fell through normally or returned from the enclosing function.
enum Flow {
    Normal,
    Return(Value),
    /// `break` — unwinds to the innermost loop, which stops. Never escapes a loop (the checker
    /// rejects `break`/`continue` outside a loop, so it always meets one).
    Break,
    /// `continue` — unwinds to the innermost loop, which proceeds to its next iteration.
    Continue,
}

/// A spawned task in a structured-concurrency scope (Track A.3b): its future (an `async fn` state
/// machine) and its completion result once driven to `Ready` (`None` while still pending). A handle
/// referencing this task reads `result`; the scheduler polls `future` and fills `result`.
struct Task {
    future: Value,
    result: Option<Value>,
    /// Set when the task is **cancelled** (Track A.8) — e.g. a `race` loser. Cancelled tasks are never
    /// polled again and count as done for the join; the tree-walker mirror of the VM's flag.
    cancelled: bool,
    /// Set while this task's future is **being polled** (its step is executing). A nested
    /// `poll_all_scopes_round` — a `concurrent` join *inside* this task's own body — must skip it:
    /// re-entering a mid-execution state machine re-runs its current segment (infinite recursion).
    /// The tree-walker mirror of the VM's flag.
    polling: bool,
    /// The task's **saved task-local context** (native-otel T5a): a snapshot of the spawner's
    /// `ctx_current` at `spawn`, swapped in around each poll of this task's step — the tree-walker
    /// mirror of the VM's field.
    context: Vec<u64>,
    /// The channels this task holds a **producer hold** on (isolates I.4c auto-close): the indices of
    /// every `Sender<T>` it captured. Decremented when the task's future is reclaimed (on completion
    /// or at scope end), auto-closing a channel when its last producer is gone. The VM's `Task.holds`
    /// mirror. Emptied once decremented so completion and scope-end never double-count.
    holds: Vec<usize>,
}

/// One traced future (native-otel T5c) — the tree-walker mirror of the VM's entry. `future` is a
/// cloned [`Value::Future`] (the `Rc` keeps it alive; identity = `Rc::ptr_eq`); `context` is the
/// stack its polls run under; `span` is ended when it completes.
struct TracedFuture {
    future: Value,
    context: Vec<u64>,
    span: u64,
}

/// Traced-future identity: the same step future (`Rc` pointer equality). Only [`Value::Future`]
/// is ever registered, so other flavors never match.
fn traced_same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Future(x), Value::Future(y)) => std::rc::Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// A bounded channel's scheduler-owned state (isolates I.1): a FIFO queue of buffered messages, its
/// capacity, and whether it has been closed (all senders done). Endpoints (`Sender`/`Receiver`) are
/// just indices into the interpreter's `channels` table — the queue is never shared heap memory.
/// Mirrors the VM's `Channel`; both backends run the identical FIFO + block-on-full/empty logic, so
/// the differential holds by construction.
struct Channel {
    /// Each queued message also carries the sender's trace context (native-otel T5d, `None` when
    /// telemetry is off or no span was active) — the automatic-propagation envelope, mirroring the
    /// VM's `Channel::Local`.
    buffer: std::collections::VecDeque<(Value, Option<noeta_stdlib::TraceContext>)>,
    capacity: usize,
    closed: bool,
    /// Live **producer holds** (isolates I.4c auto-close): spawned tasks/isolates that captured a
    /// `Sender` for this channel. Born at 0 (the `channel()` split does not count); when it returns
    /// to 0 after being positive the channel auto-closes. The tree-walker mirror of the VM
    /// `Channel::Local::producers`.
    producers: u32,
}

/// The channel indices of every `Sender<T>` reachable from a spawned future's captures (isolates
/// I.4c auto-close): a cycle-safe walk of the tree-walker value graph. It follows a closure's
/// **immediate captured-scope bindings** (never its parent-scope chain up to the globals), so it
/// collects the same producer holds the VM's `gc_children`-based walk does (whose closures likewise
/// carry only explicit captures) and both backends agree.
pub(crate) fn collect_producer_channels(root: &Value) -> Vec<usize> {
    fn walk(v: &Value, out: &mut Vec<usize>, seen: &mut HashSet<*const Scope>) {
        match v {
            Value::Sender(id) => out.push(id.index()),
            Value::Future(inner) => walk(inner, out, seen),
            Value::BoundMethod(recv, _) => walk(recv, out, seen),
            Value::Function(closure) => {
                let scope: &Rc<Scope> = &closure.captured;
                if seen.insert(Rc::as_ptr(scope)) {
                    for binding in scope.vars.borrow().values() {
                        walk(&binding.value, out, seen);
                    }
                }
            }
            Value::Tuple(items) | Value::Set(items, _) => {
                items.iter().for_each(|it| walk(it, out, seen));
            }
            Value::List(ListRepr::Boxed { items, .. }) => {
                items.iter().for_each(|it| walk(it, out, seen));
            }
            Value::Map(entries, _) => entries.values().for_each(|it| walk(it, out, seen)),
            Value::Enum(e) => e.data.iter().for_each(|it| walk(it, out, seen)),
            Value::Object(o) => o.slots.borrow().iter().for_each(|it| walk(it, out, seen)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(root, &mut out, &mut seen);
    out
}

/// One program's worth of evaluation state.
struct Interpreter {
    stdout: String,
    /// The program's standard-error accumulator — `std.io`'s `err`/`errln` push here through
    /// [`noeta_ext_abi::NativeCtx::write_stderr`], the stderr twin of `stdout`. Observable output,
    /// drained into the [`RunResult`]/[`SessionOutput`] and compared by the differential oracle.
    stderr: String,
    diagnostics: Vec<Diagnostic>,
    /// A deliberate `os.exit(code)` (stdlib-gaps): the requested exit code, set when the
    /// distinguished `ErrorKind::Exit` unwinds. Not a diagnostic — the run halts cleanly
    /// (stdout kept, nothing reported) and the run's exit code is this value.
    requested_exit: Option<i32>,
    /// Every declared `@packed` struct's field-type names (P-PKEY,
    /// `noeta_ast::packed_named_fields`) — the input to the key-capability fixpoint below.
    /// Accumulated across declarations (a session declares incrementally).
    packed_fields: std::collections::HashMap<String, Vec<Option<String>>>,
    /// The **key-capable** packed structs (P-PKEY, `noeta_ast::key_capable_packed`): types whose
    /// values may key a `Map` / member a `Set`. Recomputed whenever a packed struct declares, and
    /// consulted at key-*use* time — which necessarily follows every involved declaration, so a
    /// forward-referenced nested chain is settled before it can be observed. Mirrors the VM's
    /// `Shape::key_capable`, computed from the same inputs by the same shared fixpoint.
    key_capable_packed: std::collections::HashSet<String>,
    scope: Rc<Scope>,
    /// The root (global) scope, held so a field-default thunk can be run in the type's **definition
    /// scope** (object-model slice 5) — types are top-level, so their defaults resolve globals only,
    /// never the construction site's locals. A clone of the scope `scope` is initialized to.
    globals: Rc<Scope>,
    /// All host-coupled effects (filesystem, seeded PRNG, logical clock) behind the M2.1
    /// [`noeta_stdlib::Host`] seam. Conformance constructs a deterministic
    /// [`noeta_stdlib::SandboxHost`] so file IO, PRNG, and clock stay isolated and identical to
    /// the VM by construction; a real host (later M2 slices) swaps in without touching this struct.
    host: Box<dyn noeta_stdlib::Host>,
    /// The async executor (Track A.2): the clock + pending-timer set that `sleep(ms)` and
    /// drive-to-completion `.await` consult, behind the [`noeta_stdlib::Executor`] seam. Conformance
    /// and the differential get a deterministic [`noeta_stdlib::SandboxExecutor`] (logical time, fresh
    /// per run, identical to the VM's by construction); the CLI swaps in a real wall-clock executor
    /// (Track A.4) without touching this struct — the same discipline as `host`.
    executor: Box<dyn noeta_stdlib::Executor>,
    /// The structured-concurrency scope stack (Track A.3b): one entry per open `concurrent { }` block,
    /// each a list of the tasks `spawn`ed in it. `spawn` appends; `.await` inside the block and the
    /// join at `}` drive the tasks round-robin. A handle references a task by its `(scope, task)`
    /// position here. Mirrors the VM's `scopes` field; both round-robin identically, so the differential
    /// holds by construction.
    ///
    /// A closed scope is **tombstoned** (its task list drained and `scope_closed[i]` set), not removed,
    /// so scope indices stay stable for handles (Track A.7): a split `concurrent { }` may close while a
    /// sibling task's scope is still open above it, i.e. out of structured-stack order, so popping the
    /// top would corrupt the sibling. Trailing tombstones are trimmed on close (the common LIFO case), so
    /// the Vec stays bounded by the concurrently-open high-water mark.
    scopes: Vec<Vec<Task>>,
    /// Whether each `scopes` slot is a closed tombstone (Track A.7). Parallel to `scopes`; the only extra
    /// state a stable-index scope stack needs. Mirrors the VM's `scope_closed`.
    scope_closed: Vec<bool>,
    /// The **current strand's task-local context** (native-otel T5a): an opaque `u64` stack
    /// extensions read through `NativeCtx::context_*` (telemetry's active-span stack is the first
    /// client). Belongs to whichever strand is executing — the main strand (root) by default; the
    /// scheduler swaps a task's saved context in around each poll of its step, and a `spawn`
    /// snapshots it into the child. The tree-walker mirror of the VM's `ctx_current`.
    ctx_current: Vec<u64>,
    /// **Traced futures** (native-otel T5c) — the future-completion hook, the tree-walker mirror
    /// of the VM's table. Entries are `Value` clones (`Rc`-owned, so teardown is automatic when
    /// the interpreter drops); almost always empty (the `poll_once` fast path is `is_empty()`).
    traced_futures: Vec<TracedFuture>,
    /// The channel table (isolates I.1): every `channel::<T>(cap)` appends a [`Channel`]; endpoints
    /// reference one by index. Never cleared during a run (indices stay stable), like `scopes` it
    /// mirrors the VM exactly. `channel_progress` counts successful queue operations (a `send` push, a
    /// `recv` pop, or a `close`) so the scheduler distinguishes real channel progress from a stalled
    /// round — a channel op that unblocks a sibling is progress even when no task *completes*.
    channels: Vec<Channel>,
    channel_progress: u64,
    /// The extensions' retained-value arena (higher-order-abi H4) — the tree-walker twin of the
    /// VM's `ext_arena`. Entries are `Rc` clones, so ownership is automatic (dropping the
    /// interpreter drops whatever the program never released, mirroring the VM's teardown
    /// release); freed indices are reused via `ext_arena_free`.
    ext_arena: Vec<Option<Value>>,
    ext_arena_free: Vec<u32>,
    /// Per-run extension Rust state (`NativeCtx::state`, H4), keyed by the extension's own key.
    ext_state: Vec<(&'static str, noeta_stdlib::ExtState)>,
    /// The shared reflection artifact (attribute manifest + type registry), built from the program
    /// by the *same* `noeta_ast::reflect::build` the VM uses — so `attributes_of` materializes
    /// identical values in both backends. Populated at the start of `run`.
    reflection: noeta_ast::reflect::ReflectionInfo,
    /// The concrete static type the checker resolved for each `type_of(value)` site (keyed by the
    /// `Expr::TypeOf` span), harvested via `noeta_check::resolve_type_of_sites` from the *same*
    /// program the VM harvests — so both backends bake identical full-fidelity `Type` constants
    /// (`type_of` fidelity A, P2.3). A site absent here uses the runtime head-constructor path.
    type_of_sites: std::collections::HashMap<noeta_span::Span, noeta_ast::reflect::TypeRepr>,
    /// The `@derive(Deserialize<Json>)` decode registry (L2.2 DI), keyed by type name — the
    /// tree-walker twin of the VM's `deserialize_recipes`. Lifted from the checker's sites at
    /// `run_ir` start; `Rvalue::DecodeTyped` (`json.decode_typed(name, text)`) looks a runtime type
    /// name up here to decode a JSON body into that type. Empty on every run with no such derive.
    deserialize_recipes: std::collections::HashMap<String, noeta_stdlib::TypeRecipe>,
    /// Every `@packed` struct's flat layout by (qualified) type name (native type-declaration
    /// unification, Slice E2), lifted from the checker's sites at `run_ir` start. The from-scratch
    /// producer [`NativeCtx::make_packed`](crate::native_ctx) resolves a produced `List<packed>`'s
    /// element schema by name here — the tree-walker twin of the VM's interned `packed_schemas`
    /// by-name scan. Empty on the checkerless REPL session path (no `@packed` layout is known there).
    packed_type_layouts: std::collections::HashMap<String, noeta_ast::reflect::PackedLayout>,
    /// The program-wide **type-argument table** (poly-values F2b) — the concrete instantiations of
    /// forwarding generics, lifted from the IR `Program` at `run_ir` start. A dynamic
    /// call-site-typed site resolves its per-instantiation recipe/name through the hidden slot's
    /// index into this table; identical to the VM's copy by construction.
    type_args: Vec<noeta_stdlib::TypeArgInfo>,
    /// The live **call-site shadow stack**: one `(callee name, call-site span)` per function/method
    /// activation currently on the Rust call stack, pushed at each call boundary and popped on the
    /// way out (abort included). Only read when an abort snapshots [`Self::abort_trace`], so it
    /// costs a push/pop per call — fine for the reference interpreter, whose mandate is clarity and
    /// exactness, not speed (the VM gets the same information for free from its saved resume pcs).
    call_sites: Vec<(Option<String>, Span)>,
    /// The abort traceback, innermost frame first — the tree-walker twin of the VM's. Snapshotted
    /// from `call_sites` at the moment the **first** abort's diagnostic is recorded (later teardown
    /// aborts do not overwrite it), so unwinding and swallowed destructor aborts can never leave a
    /// stale trace.
    abort_trace: Vec<noeta_backend::TraceFrame>,
    /// The extension **registry** this interpreter resolves native names against (instance-registry
    /// IR3) — the tree-walker twin of the VM's `registry` field. `None` (the default on every
    /// ordinary run) falls back to the process-global default registry through [`Interpreter::reg`],
    /// so the differential is unchanged; an embedding host that assembled its own extension set
    /// threads its `Registry` in, and both backends then resolve the same names by construction.
    registry: Option<&'static noeta_stdlib::registry::Registry>,
}

impl Interpreter {
    /// The extension registry this interpreter resolves native names against (instance-registry
    /// IR3) — the tree-walker twin of `Vm::reg`. Falls back to the process-global default when
    /// unset, keeping every ordinary run (and the differential) unchanged. Returns `&'static`.
    fn reg(&self) -> &'static noeta_stdlib::registry::Registry {
        self.registry
            .unwrap_or_else(noeta_stdlib::registry::default_seeded)
    }

    fn new() -> Interpreter {
        Interpreter::with_host(Box::new(noeta_stdlib::SandboxHost::new()))
    }

    /// Build an interpreter against a caller-provided [`noeta_stdlib::Host`] (M2.3), keeping the
    /// default deterministic executor. `new` uses the deterministic sandbox (what the differential
    /// needs); the CLI/REPL pass a real host here.
    fn with_host(host: Box<dyn noeta_stdlib::Host>) -> Interpreter {
        Interpreter::with_host_and_executor(host, Box::new(noeta_stdlib::SandboxExecutor::new()))
    }

    /// Build an interpreter against caller-provided host *and* executor (Track A.4). The CLI pairs a
    /// real host with a real wall-clock executor; the differential always uses the sandbox pair.
    fn with_host_and_executor(
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
    ) -> Interpreter {
        let global = Scope::global();
        for &builtin in Builtin::PRELUDE {
            global.declare(builtin.name().to_string(), Value::Builtin(builtin), false);
        }
        // `none` is the `Option` absence value — a binding, not a function (it takes no
        // parens), so it is registered here rather than as a `Builtin`.
        global.declare(
            "none".to_string(),
            builtin_enum("Option", "none", vec![]),
            false,
        );
        // The built-in `Ordering` enum is namable like any other, so `Ordering.Less` can be
        // constructed directly (not only received from `.compare()`); its variants carry no data
        // and build the same `EnumValue` `compare` returns.
        global.declare(
            "Ordering".to_string(),
            Value::EnumType(Rc::new(EnumDef {
                name: "Ordering".to_string(),
                variants: ["Less", "Equal", "Greater"]
                    .into_iter()
                    .map(|name| VariantInfo {
                        name: name.to_string(),
                        field_names: Vec::new(),
                    })
                    .collect(),
                derives_comparable: false,
                derives_tojson: false,
                methods: HashMap::new(),
            })),
            false,
        );
        Interpreter {
            stdout: String::new(),
            stderr: String::new(),
            diagnostics: Vec::new(),
            requested_exit: None,
            packed_fields: std::collections::HashMap::new(),
            key_capable_packed: std::collections::HashSet::new(),
            globals: Rc::clone(&global),
            scope: global,
            host,
            executor,
            scopes: Vec::new(),
            scope_closed: Vec::new(),
            ctx_current: Vec::new(),
            traced_futures: Vec::new(),
            channels: Vec::new(),
            channel_progress: 0,
            ext_arena: Vec::new(),
            ext_arena_free: Vec::new(),
            ext_state: Vec::new(),
            reflection: noeta_ast::reflect::ReflectionInfo::default(),
            type_of_sites: std::collections::HashMap::new(),
            deserialize_recipes: std::collections::HashMap::new(),
            packed_type_layouts: std::collections::HashMap::new(),
            type_args: Vec::new(),
            call_sites: Vec::new(),
            abort_trace: Vec::new(),
            registry: None,
        }
    }

    /// Destroy the global scope's bindings in reverse declaration order.
    fn destroy_globals(&mut self) {
        for value in self.scope.drain_reverse() {
            self.destroy_value(value);
        }
    }

    /// Reap reference cycles the tree-walker tied through closure capture (Phase 6.3) — the eval
    /// analogue of the VM's backup mark-sweep. Run **once at clean exit, after [`destroy_globals`]**:
    /// at that point every legitimately-live binding has been torn down, so any captured scope still
    /// alive is reachable only through a `scope ↔ closure` capture cycle that `Rc` cannot break.
    ///
    /// Each such scope's bindings are **drained and destroyed** (Phase-6 destructor-on-collect): a
    /// captured destructor-bearing value held only by the dead cycle fires its `__destruct` at its last
    /// reference, exactly as the VM runs destructors on the values its collector reclaims. (Object
    /// cycles cannot form under value semantics, so a binding is never itself a cycle member with a
    /// destructor; the destructor-bearing values are the *captured* leaves, as on the VM side.) Draining
    /// also breaks the cycle, so `Rc` reclaims the closures and the scope. The upgraded handle keeps the
    /// scope alive across its own drain; intra-cycle order is best-effort (spec §6), matching the VM.
    fn reap_captured_scope_cycles(&mut self) {
        let candidates = CAPTURED_SCOPES.with(|r| std::mem::take(&mut *r.borrow_mut()));
        for weak in candidates {
            if let Some(scope) = weak.upgrade() {
                let values: Vec<Value> = scope
                    .vars
                    .borrow_mut()
                    .drain()
                    .map(|(_, binding)| binding.value)
                    .collect();
                scope.order.borrow_mut().clear();
                for value in values {
                    self.destroy_value(value);
                }
            }
        }
    }

    /// Reap reference-`class` cycles the program tied through `mut` fields (object-model slice 2c —
    /// `a.next = b; b.next = a`), which refcounting alone cannot reclaim. Run once at clean exit,
    /// **after** global teardown and the scope reaper: any object in [`MUTATED_OBJECTS`] still live is
    /// reachable only through such a cycle. Done in two passes so it is robust to objects sharing a
    /// cycle: first **drain every survivor's fields** (breaking every cycle at once, leaving acyclic
    /// chains), then **destroy the drained values** — now plain `Rc` cascade-frees, firing each
    /// member's `destruct` at its true last reference. The tree-walker analogue of the VM's
    /// post-teardown `collect_trace`.
    fn reap_object_cycles(&mut self) {
        let candidates = MUTATED_OBJECTS.with(|r| std::mem::take(&mut *r.borrow_mut()));
        // The live, de-duplicated survivors (an object may have been registered on several field
        // assignments). After teardown these are exactly the reachable cycle members.
        let mut survivors: Vec<Rc<ObjectValue>> = Vec::new();
        for weak in &candidates {
            if let Some(obj) = weak.upgrade()
                && !survivors.iter().any(|s| Rc::ptr_eq(s, &obj))
            {
                survivors.push(obj);
            }
        }
        // Reverse-creation order (newest-first) so cyclic `destruct` order is deterministic and
        // matches the VM (which sorts its trace garbage by the same `seq`).
        survivors.sort_by_key(|o| std::cmp::Reverse(o.seq));
        // Pass 1 — run each member's `destruct` once, with its fields (still intact) and `self` in
        // scope, exactly as `destroy_object` would. (Both backends destruct every cycle member
        // before freeing any.)
        for obj in &survivors {
            if let Some(body) = obj.def.destructor.clone() {
                let scope = Scope::child(&self.scope);
                // Only `self` is in scope (EX.1 — member access is explicit: `self.field`).
                scope.declare("self".to_string(), Value::Object(obj.clone()), false);
                let saved = std::mem::replace(&mut self.scope, scope);
                let _ = self.exec_ir_fn_body(&body);
                self.scope = saved;
            }
        }
        // Pass 2 — break every cycle by draining all survivors' fields, then release the drained
        // values. Destructors already ran in pass 1, so the drained field values are dropped
        // directly (a plain `Rc` release that fires no further `destruct`); with every back-edge
        // gone the members' refcounts reach zero and their allocations free (`leak::dec`).
        let mut drained: Vec<Value> = Vec::new();
        for obj in &survivors {
            let slots = std::mem::take(&mut *obj.slots.borrow_mut());
            drained.extend(slots);
        }
        drop(survivors);
        drop(drained);
    }

    /// Fire the destructors of the current scope's live values as a **panic/abort unwinds through
    /// it** (spec §6, Phase 4.2c-ii). Drains the scope in reverse-construction order and runs each
    /// `destruct` at its last reference — the same `drain_reverse` + `destroy_value` as global
    /// teardown, but applied to a function/block scope the abort is abandoning. Called at every
    /// scope boundary on `Unwind::Abort`, so as the abort climbs the call stack each frame's locals
    /// are destroyed innermost-first; the VM's abort teardown walks the matching per-frame list.
    fn fire_aborted_scope(&mut self) {
        for value in self.scope.drain_reverse() {
            self.destroy_value(value);
        }
    }

    /// Destroy `value` if it holds the last reference to a destructor-bearing structure, running
    /// the observable `destruct` blocks in spec order. Mirrors the VM's `release_value`:
    /// container-before-contained (spec §4) — an aggregate runs its own `destruct` first (its
    /// fields still live), then releases its fields / payloads / elements in declared (object),
    /// positional (enum), or iteration (list/set/map) order, each recursively firing its own
    /// `destruct` at *its* last reference. A non-aggregate, or an aliased aggregate (refcount > 1),
    /// simply lets its `Rc` drop here — memory is reclaimed, no destructor fires (deferred to the
    /// final reference, §2). Closure-captured values are out of §4 scope (Phase 6 owns capture).
    /// Register one producer hold on channel `cid` (isolates I.4c): a spawned task/isolate captured a
    /// `Sender` for it. The tree-walker mirror of the VM's `add_producer_hold`.
    pub(crate) fn add_producer_hold(&mut self, cid: usize) {
        self.channels[cid].producers += 1;
    }

    /// End one producer hold on channel `cid` (isolates I.4c): its task completed or was reclaimed.
    /// Auto-closes the channel when its last producer is gone, marking channel progress so a parked
    /// receiver re-polls and observes the close. The VM's `end_producer_hold` mirror.
    pub(crate) fn end_producer_hold(&mut self, cid: usize) {
        if noeta_stdlib::channel::producer_left(&mut self.channels[cid].producers) {
            self.channels[cid].closed = true;
            self.channel_progress += 1;
        }
    }

    /// Release the producer holds a task recorded, if not already released (isolates I.4c) — on the
    /// task's completion, or at scope end for a task that never completed. Empties the list so the two
    /// paths never double-count. The VM's `release_task_holds` mirror.
    pub(crate) fn release_task_holds(&mut self, holds: &mut Vec<usize>) {
        for cid in std::mem::take(holds) {
            self.end_producer_hold(cid);
        }
    }

    fn destroy_value(&mut self, value: Value) {
        match value {
            Value::Object(obj) => self.destroy_object(obj),
            Value::Enum(e) => self.destroy_enum(e),
            // A boxed list / set / tuple releases its elements through the shared `Rc<Vec<Value>>`.
            Value::List(ListRepr::Boxed { items, .. })
            | Value::Set(items, _)
            | Value::Tuple(items) => self.destroy_sequence(items),
            // A packed list (P-PACK 2.3) holds only primitive words — no heap elements, no
            // destructors — so its `Rc<Vec<u64>>` simply drops here, reclaiming the buffer.
            Value::List(ListRepr::Packed(_)) => {}
            Value::Map(entries, _) => self.destroy_map(entries),
            // A closure/future/generator that reaches its last reference destroys the values it
            // captured — a destructor-bearing local that outlives its defining scope inside a returned
            // closure, an async fn, or a generator (the VM's twin: `release_value` recursing into the
            // closure/future/iterator's cells). Container-before-contained, deferring to the true last
            // reference of each capture.
            Value::Function(c) => self.destroy_closure(c),
            // A bound handle owns its captured receiver — destroy it with the handle.
            Value::BoundMethod(recv, _) => self.destroy_value(*recv),
            Value::Future(inner) => self.destroy_boxed(inner),
            Value::Iter(state) => self.destroy_iter(state),
            // Scalars/types/handles bear no destructor; their `Rc`/value drops here, reclaiming memory.
            _ => {}
        }
    }

    /// Destroy a closure at its last reference: run the destructors of the values it captured. A
    /// closure captures its whole definition scope by `Rc<Scope>`; a captured value dies with the
    /// closure only when the closure holds the scope's **last** reference (else the scope — and its
    /// bindings — outlive this closure, shared with another closure or a live frame). Walks the
    /// captured scope chain, destroying each level this closure uniquely holds, in **reverse
    /// declaration order** (`Scope::order`) — the spec's deterministic destruction order, matching the
    /// VM's cell walk. `destroy_value` on each binding defers a still-aliased capture to its own last
    /// reference. A capture *cycle* (a closure holding the scope that captures it) keeps a strong
    /// count > 1, so it is not reclaimed here — the Phase-6 exit reaper still breaks those.
    fn destroy_closure(&mut self, closure: Rc<Closure>) {
        let Ok(closure) = Rc::try_unwrap(closure) else {
            return; // aliased — destruction defers to the last reference.
        };
        // `Scope`/`Closure` have a `Drop` (leak bookkeeping), so fields cannot be moved out; clone the
        // captured-scope handle, then release the closure's own reference so the strong count reflects
        // only outside holders.
        let mut scope = closure.captured.clone();
        drop(closure);
        loop {
            // The closure must hold the scope's sole reference for its bindings to die now.
            if Rc::strong_count(&scope) != 1 {
                break; // shared — its bindings outlive this closure.
            }
            // Take the bindings out before running any destructor (which may re-enter and allocate);
            // `Scope::drop` only does leak bookkeeping, so an emptied scope reclaims cleanly.
            let order = scope.order.take();
            let mut vars = scope.vars.take();
            for name in order.iter().rev() {
                if let Some(binding) = vars.remove(name) {
                    self.destroy_value(binding.value);
                }
            }
            // Ascend to the parent this scope uniquely held (clone the handle, drop this scope).
            let parent = scope.parent.clone();
            drop(scope);
            match parent {
                Some(p) => scope = p,
                None => break,
            }
        }
    }

    /// Destroy a boxed single value (an async `Future`'s wrapped thunk/step closure) at its last
    /// reference, recursing so a captured destructor-bearing local runs.
    fn destroy_boxed(&mut self, inner: Rc<Value>) {
        if let Ok(value) = Rc::try_unwrap(inner) {
            self.destroy_value(value);
        }
    }

    /// Destroy an iterator/generator at its last reference: a generator (`IterState::Gen`) owns a step
    /// closure whose cells hold the generator body's locals, so recurse into it.
    fn destroy_iter(&mut self, state: Rc<RefCell<IterState>>) {
        if let Ok(state) = Rc::try_unwrap(state)
            && let IterState::Gen { step } = state.into_inner()
        {
            self.destroy_value(step);
        }
    }

    /// Destroy a struct/class instance: its own `destruct` (if any) first, then its fields in
    /// declared order (spec §4). Only acts at the last reference; a destructor that resurrects
    /// `self` (raising the count) leaves the box shared, so the field walk is skipped.
    fn destroy_object(&mut self, obj: Rc<ObjectValue>) {
        if Rc::strong_count(&obj) != 1 {
            return; // aliased — this reference drops, destruction defers to the last (§2).
        }
        // 1. The container's own `destruct`, run like a parameterless method with the instance's
        //    fields and `self` in scope. It runs for its effects; its control flow/errors are not
        //    part of any expression's value, so they are swallowed at this boundary.
        if let Some(body) = obj.def.destructor.clone() {
            let scope = Scope::child(&self.scope);
            // Only `self` is in scope (EX.1 — member access is explicit: `self.field`).
            scope.declare("self".to_string(), Value::Object(obj.clone()), false);
            let saved = std::mem::replace(&mut self.scope, scope);
            let _ = self.exec_ir_fn_body(&body);
            self.scope = saved;
        }
        // 2. Then release each field, container-before-contained. We hold the last reference, so
        //    take ownership of the box to move its fields out; `try_unwrap` fails iff the
        //    destructor resurrected `self`, in which case the fields are not force-destroyed.
        if let Ok(mut object) = Rc::try_unwrap(obj) {
            // Slot order is declared order (records/classes) or sorted-key order (opaque imports) —
            // either way the slots already iterate in the spec's §4 release order.
            let slots = std::mem::take(&mut object.slots).into_inner();
            drop(object); // the emptied `ObjectValue` drops here (its `leak::dec` balances `new`).
            for field in slots {
                self.destroy_value(field);
            }
        }
    }

    /// Destroy an enum value's payloads in positional order (enums carry no own `destruct`; only
    /// classes do). Only at the last reference.
    fn destroy_enum(&mut self, e: Rc<EnumValue>) {
        if let Ok(ev) = Rc::try_unwrap(e) {
            for field in ev.data {
                self.destroy_value(field);
            }
        }
    }

    /// Destroy a list's or set's elements in iteration order. Only at the last reference; an
    /// aliased collection's `Rc` simply drops.
    fn destroy_sequence(&mut self, items: Rc<Vec<Value>>) {
        if let Ok(items) = Rc::try_unwrap(items) {
            for item in items {
                self.destroy_value(item);
            }
        }
    }

    /// Destroy a map's values in sorted-key order (its keys are strings — no destructors). Only at
    /// the last reference.
    fn destroy_map(&mut self, entries: Rc<BTreeMap<noeta_stdlib::MapKey, Value>>) {
        if let Ok(entries) = Rc::try_unwrap(entries) {
            for (_, value) in entries {
                self.destroy_value(value);
            }
        }
    }

    /// Materialize the elements a `for` loop iterates over: a list/set in canonical order, a
    /// map's values in key order, or a user object's `Iterable` (`iter`) list. Shared by the
    /// AST walker's `exec_for` and the Core-IR interpreter so both agree by construction.
    /// Whether a value is a user object exposing a `next` member — a declared method, or a field
    /// (whose value the drain calls through the ordinary member-call path; a non-callable one
    /// raises the indirect-call error there). The gate for `next`-driven user iteration,
    /// mirrored by the VM's shape-based gate.
    fn has_user_next(v: &Value) -> bool {
        matches!(
            v,
            Value::Object(o) if o.def.methods.contains_key("next") || o.field("next").is_some()
        )
    }

    fn iter_elements(&mut self, iterable: Value, span: Span) -> Eval<Vec<Value>> {
        match &iterable {
            Value::List(repr) => Ok((*repr.to_rc_vec()).clone()),
            // A set iterates in its canonical (sorted) order — deterministic, like the VM.
            Value::Set(items, _) => Ok((**items).clone()),
            // Iterating a map yields its values, in deterministic key order.
            Value::Map(entries, _) => Ok(entries.values().cloned().collect()),
            // A user object lights up the `Iterable` trait: `for x in o` iterates the list its
            // `iter` method returns — or, composing with the member-handle iterator below, the
            // `next`-driven user iterator object it returns.
            Value::Object(object) if object.def.methods.contains_key("iter") => {
                match self.call_method(iterable.clone(), "iter", Vec::new(), span)? {
                    Value::List(repr) => Ok((*repr.to_rc_vec()).clone()),
                    other if Self::has_user_next(&other) => self.drain_next_object(other, span),
                    other => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`iter` must return a list, found {}", other.type_name()),
                    )),
                }
            }
            // The **member-handle iterator** (coroutines Track-I trigger): a user object with no
            // `iter` but a callable `next` member — a method, or (through the member-call
            // fallback) a closure-valued field — drives iteration directly: `next()` until
            // `none`, each `some(x)` contributing an element. Eager like the `Iterable` list
            // path (user iteration snapshots; lazy streaming remains built-in `Iterator<T>`'s).
            Value::Object(_) if Self::has_user_next(&iterable) => {
                self.drain_next_object(iterable.clone(), span)
            }
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("cannot iterate over {}", other.type_name()),
            )),
        }
    }

    /// Drain a `next`-driven user iterator object into its element list: call the object's `next`
    /// member — dispatched through the ordinary member-call path, so a method or a closure-valued
    /// field both work — until it returns `none`; each `some(x)` contributes `x`. A step that is
    /// not a built-in option is E0007, identically in both backends.
    fn drain_next_object(&mut self, obj: Value, span: Span) -> Eval<Vec<Value>> {
        let mut elements = Vec::new();
        loop {
            let step = self.call_method(obj.clone(), "next", Vec::new(), span)?;
            let payload = match &step {
                Value::Enum(e) if e.enum_name == "Option" && e.variant == "some" => {
                    e.data.first().cloned().unwrap_or(Value::Unit)
                }
                Value::Enum(e) if e.enum_name == "Option" && e.variant == "none" => {
                    return Ok(elements);
                }
                other => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "iterator `next` must return an option, found {}",
                            other.type_name()
                        ),
                    ));
                }
            };
            elements.push(payload);
        }
    }

    fn bind_for_pattern(
        &mut self,
        scope: &Rc<Scope>,
        pattern: &ForPattern,
        element: Value,
        span: Span,
    ) -> Eval<()> {
        match pattern {
            ForPattern::Single { name, .. } => {
                scope.declare(name.clone(), element, false);
                Ok(())
            }
            // `for (a, b, …) in …` destructures each iterated **tuple** element positionally
            // (object-model slice 4b). An element of the wrong kind/arity is a runtime error.
            ForPattern::Tuple { names, .. } => match &element {
                Value::Tuple(items) if items.len() == names.len() => {
                    for ((name, _), item) in names.iter().zip(items.iter()) {
                        scope.declare(name.clone(), item.clone(), false);
                    }
                    Ok(())
                }
                other => Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "destructuring `({})` expects a {}-tuple, found {}",
                        names
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        names.len(),
                        other.type_name()
                    ),
                )),
            },
        }
    }

    /// Register the names imported by a `use` declaration. Real module loading is M1; in
    /// M0 each imported name resolves to an *opaque stub type* so references and all-fields
    /// literals (`User { name: ... }`) work even though the type's real shape is unknown.
    fn declare_use(&mut self, path: &[String], names: &[noeta_ast::UseName]) {
        // `use std.{json, ...}` binds each recognized name to its Ring 2 native module; a selective
        // member import (`use std.math.sqrt`) binds each member to a `(module, func)` module-function
        // value; other imports (and unrecognized `std` names) fall back to the opaque-stub binding.
        // Classification is `Registry::classify_use` — the ONE source of truth the checker, IDE,
        // and bytecode compiler consume (this used to be a third hand-rolled copy of the rules).
        let reg = self.reg();
        for imported in names {
            use noeta_stdlib::registry::UseKind;
            let value = match reg.classify_use(path, &imported.name) {
                // A plain (`use std.{json}`) or nested (`use std.http.client`) module import: the
                // module *value* carries the root-qualified identity; the bound name stays the
                // last segment.
                UseKind::Module(qualified) => Value::NativeModule(qualified),
                UseKind::MemberFn { module, func } => Value::ModuleFn(module, func),
                // A native enum (`use shade.Hue`, native-extensibility S1b): bind the imported short
                // name to a real `EnumType`, so `Hue.Red` / `Hue.Labeled(x)` construct exactly like a
                // `.noe` enum's `EnumType`. Variants (and hence their declaration indices) come from
                // the registry keyed by qualified identity — the same source the bytecode compiler
                // and the checker read; the `EnumDef` carries the **short** name the runtime value
                // and every match pattern compare against (S1 identity note). A payload variant's
                // field names are positional placeholders (only the count is load-bearing).
                UseKind::ExtEnum(qualified) if reg.find_enum_qualified(&qualified).is_some() => {
                    let en = reg.find_enum_qualified(&qualified).unwrap();
                    Value::EnumType(Rc::new(EnumDef {
                        name: en.name.to_string(),
                        variants: en
                            .variants
                            .iter()
                            .map(|v| VariantInfo {
                                name: v.name.to_string(),
                                field_names: (0..v.fields.len()).map(|n| format!("_{n}")).collect(),
                            })
                            .collect(),
                        derives_comparable: false,
                        derives_tojson: false,
                        methods: HashMap::new(),
                    }))
                }
                // A native **fielded type** (`use geo.Handle` / `use geo.Point`, native-extensibility
                // S2 + fielded unification): bind the imported short name to a **constructible**
                // `TypeDef` — fields in registry order, non-opaque — so `Handle { … }` / `Point { … }`
                // constructs a real `Object` exactly like a `.noe` class/struct. The `FieldedKind`
                // selects the semantics: a class is a reference type (`is_struct = false`,
                // `structural_eq = false` → identity); a struct is a value type (`is_struct = true`,
                // `structural_eq = true` → structural `==`, copy-on-assign). The registry (keyed by
                // qualified identity) is the single source of fields, and the def carries the **short**
                // name a value stamps; matches the VM's `ext_fielded_type_info` seed.
                UseKind::ExtClass(qualified) | UseKind::ExtStruct(qualified)
                    if reg.resolve_fielded(&qualified).is_some() =>
                {
                    let cl = reg.resolve_fielded(&qualified).unwrap();
                    let is_struct = cl.kind == noeta_stdlib::FieldedKind::Struct;
                    Value::Type(Rc::new(TypeDef {
                        name: cl.name.to_string(),
                        fields: cl
                            .fields
                            .iter()
                            .map(|f| FieldSpec {
                                name: f.name.to_string(),
                            })
                            .collect(),
                        methods: HashMap::new(),
                        destructor: None,
                        is_struct,
                        structural_eq: is_struct,
                        key_capable: std::cell::Cell::new(false),
                        derives_comparable: false,
                        derives_tojson: false,
                        opaque: false,
                        field_defaults: Vec::new(),
                    }))
                }
                _ => {
                    Value::Type(Rc::new(TypeDef {
                        name: imported.name.clone(),
                        fields: Vec::new(),
                        methods: HashMap::new(),
                        destructor: None,
                        is_struct: false,
                        // An opaque import has no known fields; treat `==` structurally (matches the
                        // VM's `ShapeKind::Opaque` default), not as class identity.
                        structural_eq: true,
                        key_capable: std::cell::Cell::new(false),
                        derives_comparable: false,
                        derives_tojson: false,
                        opaque: true,
                        field_defaults: Vec::new(),
                    }))
                }
            };
            self.scope.declare(imported.name.clone(), value, false);
        }
    }

    /// Construct a struct/class instance from an all-fields object literal. This is the
    /// full-initialization choke point: every declared field must end up set (by a named
    /// initializer or the `..` spread), or it is a [`DiagnosticCode::MissingField`] error.
    /// Materialize the `#[type_name(...)]` attributes from the manifest into a `List<Attributed<T>>`
    /// — each a real `T` struct (built from its stored args) paired with its target. Builds fresh
    /// `TypeDef`s from the shared reflection info; the VM builds the matching shapes the same way, so
    /// the materialized values agree across backends by construction.
    fn materialize_attributes(&self, type_name: &str) -> Value {
        let shape = noeta_ast::reflect::attribute_shape(type_name, &self.reflection);
        let fields = shape.fields;
        let attr_def = Rc::new(fresh_type_def(type_name, &fields, shape.is_struct));
        let attributed_def = Rc::new(fresh_type_def(
            "Attributed",
            &["target".to_string(), "value".to_string()],
            true,
        ));
        let items: Vec<Value> = self
            .reflection
            .manifest
            .iter()
            .filter(|a| a.name == type_name)
            .map(|a| {
                let values = noeta_ast::reflect::materialize_args(a, &fields, &shape.defaults);
                // `attr_def.fields` is `fields` (same order), so the materialized values are already
                // in slot order; `attributed_def` is `{ target, value }` in that order.
                let t_slots: Vec<Value> = values
                    .iter()
                    .map(|v| attr_value_to_eval(v, &self.reflection))
                    .collect();
                let t_value = Value::Object(Rc::new(ObjectValue::new(attr_def.clone(), t_slots)));
                let a_slots = vec![Value::Str(a.target.clone()), t_value];
                Value::Object(Rc::new(ObjectValue::new(attributed_def.clone(), a_slots)))
            })
            .collect();
        Value::list(items)
    }

    /// Materialize the `(declaration, Role)` index from the reflection info into a
    /// `List<RoleBinding>` — each `{ target: string, role: Role }`. Builds fresh `TypeDef`s/enum
    /// values; the VM builds the matching shapes the same way, so the values agree by construction.
    /// (P2.7.)
    fn materialize_roles(&self, role_enum: Option<&str>) -> Value {
        let binding_def = Rc::new(fresh_type_def(
            "RoleBinding",
            &["target".to_string(), "role".to_string()],
            true,
        ));
        let items: Vec<Value> = self
            .reflection
            .roles
            .iter()
            // `roles_of::<E>()` keeps only bindings of enum `E`; bare `roles_of()` keeps all.
            .filter(|r| role_enum.is_none_or(|e| r.enum_name == e))
            .map(|r| {
                // `binding_def` is `{ target, role }` — build the slots in that order.
                let slots = vec![
                    Value::Str(r.target.clone()),
                    builtin_enum(&r.enum_name, &r.variant, Vec::new()),
                ];
                Value::Object(Rc::new(ObjectValue::new(binding_def.clone(), slots)))
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a callable's declared parameter list from the reflection info into a
    /// `List<ParamInfo>` — each `{ name: string, type: Type, optional: bool, attrs: List<dyn> }`.
    /// `type` is the prelude `Type` ADT value built from the parameter's declared type (the same
    /// `build_type_value` `type_of` uses), `optional` reports whether the parameter declared a
    /// default, and `attrs` holds the parameter's `#[...]` attribute instances. Builds a fresh
    /// `TypeDef`; the VM builds the matching shape the same way, so the values agree by
    /// construction. An unknown target yields an empty list.
    ///
    /// `attrs` is **joined from the attribute manifest**, not carried in the parameter record: the
    /// rows are exactly the ones `attributes_of::<T>()` returns for the same parameter, reached
    /// through the shared `param_attributes_for` key. So the two query surfaces are two renderings
    /// of one table, and a parameter attribute cannot be visible through one and missing from the
    /// other.
    fn materialize_params(&self, target: &str) -> Value {
        let info_def = Rc::new(fresh_type_def(
            noeta_ast::reflect::PARAM_INFO,
            &[
                "name".to_string(),
                "type".to_string(),
                "optional".to_string(),
                "attrs".to_string(),
            ],
            true,
        ));
        let items: Vec<Value> = self
            .reflection
            .params_for(target)
            .iter()
            .map(|p| {
                // `info_def` is `{ name, type, optional, attrs }` — build the slots in that order.
                let slots = vec![
                    Value::Str(p.name.clone()),
                    build_type_value(&p.ty),
                    Value::Bool(p.optional),
                    self.materialize_param_attrs(target, &p.name),
                ];
                Value::Object(Rc::new(ObjectValue::new(info_def.clone(), slots)))
            })
            .collect();
        Value::list(items)
    }

    /// One parameter's `#[...]` attributes, materialized into a `List<dyn>` of attribute-struct
    /// instances. Each instance is built exactly as `attributes_of` builds it — same
    /// `attribute_shape`, same `materialize_args` field resolution — so the value a consumer reads
    /// off `ParamInfo.attrs` is indistinguishable from the one it would read off an `Attributed`.
    fn materialize_param_attrs(&self, callable: &str, param: &str) -> Value {
        let items: Vec<Value> = self
            .reflection
            .param_attributes_for(callable, param)
            .into_iter()
            .map(|a| {
                let shape = noeta_ast::reflect::attribute_shape(&a.name, &self.reflection);
                let slots: Vec<Value> =
                    noeta_ast::reflect::materialize_args(a, &shape.fields, &shape.defaults)
                        .iter()
                        .map(|v| attr_value_to_eval(v, &self.reflection))
                        .collect();
                let def = Rc::new(fresh_type_def(&a.name, &shape.fields, shape.is_struct));
                Value::Object(Rc::new(ObjectValue::new(def, slots)))
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a struct/class instance's fields into a `List<FieldEntry>` (`{ name, value }`,
    /// declaration order) — the value-level reflection `fields_of` (derive layer 3). Any other
    /// value yields the empty list. Builds a fresh `TypeDef`; the VM builds the matching shape
    /// the same way, so the values agree by construction.
    fn materialize_fields(&self, value: &Value) -> Value {
        let entry_def = Rc::new(fresh_type_def(
            noeta_ast::reflect::FIELD_ENTRY,
            &["name".to_string(), "value".to_string()],
            true,
        ));
        let items: Vec<Value> = match value {
            Value::Object(obj) => obj
                .def
                .fields
                .iter()
                .zip(obj.slots.borrow().iter())
                .map(|(field, field_value)| {
                    let slots = vec![Value::Str(field.name.clone()), field_value.clone()];
                    Value::Object(Rc::new(ObjectValue::new(entry_def.clone(), slots)))
                })
                .collect(),
            _ => Vec::new(),
        };
        Value::list(items)
    }

    /// Build a struct/class instance from already-evaluated field values and an optional
    /// already-evaluated `..` spread base. The full-initialization choke point, shared by the
    /// AST walker's [`Self::eval_object`] and the Core-IR interpreter so both agree by
    /// construction. The unknown-field and missing-field checks here are also enforced by the
    /// type checker, so for a check-clean program they are defensive (and never fire on the
    /// differential corpus); they keep the runtime honest for opaque/imported edges.
    fn construct_object(
        &mut self,
        type_name: &str,
        type_name_span: Span,
        field_values: Vec<(String, Span, Value)>,
        spread: Option<(Value, Span)>,
        reflect: Option<Rc<TypeRepr>>,
        span: Span,
    ) -> Eval<Value> {
        let def = match self.scope.lookup(type_name) {
            Some(Value::Type(def)) => def,
            Some(other) => {
                return Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    type_name_span,
                    format!(
                        "`{}` is a {}, not a record or class type",
                        type_name,
                        other.type_name()
                    ),
                ));
            }
            None => {
                return Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    type_name_span,
                    format!("cannot find type `{type_name}` in this scope"),
                ));
            }
        };

        // An opaque `use`-import has no statically-known field set, so it cannot use the shared
        // `def`'s slot layout: collect its fields name-keyed (spread base + named overrides), then
        // build a fresh per-literal `def` whose slots are those names in sorted-key order — exactly
        // the VM's per-literal opaque shape, so both backends lay the object out identically.
        if def.opaque {
            let mut fields: BTreeMap<String, Value> = BTreeMap::new();
            if let Some((base, spread_span)) = spread {
                match base {
                    Value::Object(base) => {
                        let slots = base.slots.borrow();
                        for (spec, value) in base.def.fields.iter().zip(slots.iter()) {
                            fields.insert(spec.name.clone(), value.clone());
                        }
                    }
                    other => {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            spread_span,
                            format!("spread `..` expects an object, found {}", other.type_name()),
                        ));
                    }
                }
            }
            for (name, _name_span, value) in field_values {
                fields.insert(name, value);
            }
            let names: Vec<String> = fields.keys().cloned().collect();
            let lit_def = Rc::new(fresh_opaque_def(def.name(), &names));
            let slots: Vec<Value> = fields.into_values().collect();
            return Ok(Value::Object(Rc::new(ObjectValue::new(lit_def, slots))));
        }

        // Declared struct/class: build the slot vector directly in `def.fields` order. `..base`
        // fills unnamed slots first; named initializers override; any slot still empty is filled
        // from its per-field default (slice 5), and a slot with neither is the full-initialization
        // violation (E0009). The unknown-field and missing-field checks mirror the type checker.
        let mut slots: Vec<Option<Value>> = vec![None; def.fields.len()];

        if let Some((base, spread_span)) = spread {
            match base {
                Value::Object(base) => {
                    let base_slots = base.slots.borrow();
                    for (i, spec) in def.fields.iter().enumerate() {
                        if let Some(j) = base.slot_of(&spec.name) {
                            slots[i] = Some(base_slots[j].clone());
                        }
                    }
                }
                other => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        spread_span,
                        format!("spread `..` expects an object, found {}", other.type_name()),
                    ));
                }
            }
        }

        for (name, name_span, value) in field_values {
            let Some(i) = def.fields.iter().position(|f| f.name == name) else {
                return Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    name_span,
                    format!("type `{}` has no field `{}`", def.name(), name),
                ));
            };
            slots[i] = Some(value);
        }

        let mut missing: Vec<String> = Vec::new();
        for (i, spec) in def.fields.iter().enumerate() {
            if slots[i].is_some() {
                continue;
            }
            if let Some((_, thunk)) = def.field_defaults.iter().find(|(n, _)| n == &spec.name) {
                slots[i] = Some(self.run_field_default(thunk)?);
            } else {
                missing.push(spec.name.clone());
            }
        }
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(self.runtime_error(
                DiagnosticCode::MissingField,
                span,
                format!(
                    "missing field(s) {list} in `{}` literal — every field must be set",
                    def.name()
                ),
            ));
        }

        let slots: Vec<Value> = slots.into_iter().map(Option::unwrap).collect();
        Ok(Value::Object(Rc::new(ObjectValue::new_reflected(
            Rc::clone(&def),
            slots,
            reflect,
        ))))
    }

    /// Resolve a checker [`PackedLayout`](noeta_ast::reflect::PackedLayout) to the eval-side
    /// [`PackedSchema`] — binding each field to the concrete [`TypeDef`] it materializes to (so a
    /// packed element unpacks into the same `Value::Object` the boxed path would build), recursing
    /// into nested packed structs. The schema's field order follows `def.fields` (the slot/
    /// materialization order); each field's kind is read from the layout by name. Returns `None` if a
    /// The packed field kinds of a `@packed` struct named `name`, in declared order — the element
    /// bundle methods' width source (scalar-unification slice 3). Resolved from the recorded per-type
    /// field type names (`packed_fields`), so it covers every declared `@packed` struct, not only those
    /// materialized into a packed list. `None` if `name` is not a `@packed` struct or any field is not
    /// a simple numeric/`bool` primitive (such a type never binds a `vec` bundle).
    pub(crate) fn packed_field_kinds(&self, name: &str) -> Option<Vec<noeta_stdlib::PackedField>> {
        let fields = self.packed_fields.get(name)?;
        fields
            .iter()
            .map(|f| f.as_deref().and_then(parse_packed_field))
            .collect()
    }

    /// The **constructible `TypeDef`** for a `@packed` element type named `name` — the scope binding
    /// for a source struct (bound by its unqualified name), or a registry-built def for a native
    /// `@packed` struct (native type-declaration unification, Slice E2). A native struct's **qualified**
    /// layout name (`geo.Pt`) is never scope-bound — a `use geo.Pt` binds only the short name `Pt` — so
    /// packed-schema resolution for a native element (a `List<Pt>` literal, or the from-scratch
    /// `make_packed` producer) falls through to the registry, building the same value-`TypeDef` a
    /// native import binds (fields in registry order, structural `==`), which the VM's interned shape
    /// mirrors. `None` if the name resolves to neither a scope type nor a native fielded *struct*.
    fn packed_type_def(&self, name: &str) -> Option<Rc<TypeDef>> {
        if let Some(Value::Type(def)) = self.scope.lookup(name) {
            return Some(def);
        }
        let cl = self.reg().resolve_fielded(name)?;
        if cl.kind != noeta_stdlib::FieldedKind::Struct {
            return None;
        }
        Some(Rc::new(TypeDef {
            name: cl.name.to_string(),
            fields: cl
                .fields
                .iter()
                .map(|f| FieldSpec {
                    name: f.name.to_string(),
                })
                .collect(),
            methods: HashMap::new(),
            destructor: None,
            is_struct: true,
            structural_eq: true,
            key_capable: std::cell::Cell::new(false),
            derives_comparable: false,
            derives_tojson: false,
            opaque: false,
            field_defaults: Vec::new(),
        }))
    }

    /// (nested) type name is not a struct in scope, the field sets disagree, or the layout is empty.
    fn resolve_packed_schema(
        &self,
        layout: &noeta_ast::reflect::PackedLayout,
    ) -> Option<Rc<PackedSchema>> {
        use noeta_ast::reflect::PackedKind;

        // A bare-scalar element (`List<i32>`/`List<f32>`) has no nominal type — no scope lookup, no
        // `def`; its single field's kind is the whole element. Build the one-slot schema directly.
        if layout.is_scalar() {
            let kind = scalar_slot_kind(&layout.fields[0].kind)?;
            let byte_size = layout.byte_size();
            if byte_size == 0 {
                return None;
            }
            return Some(Rc::new(PackedSchema {
                def: None,
                fields: vec![PackedSlot { kind }],
                byte_size,
                column: false,
            }));
        }

        // Scope for a source struct (bound by its unqualified name); the registry for a native
        // `@packed` struct, whose **qualified** layout name is never scope-bound (an import binds only
        // its short name). Registry-awareness lets the from-scratch producer (`make_packed`, Slice E2)
        // — and a native `List<Pt>` literal — resolve a native element's schema and pack it flat.
        let def = self.packed_type_def(&layout.type_name)?;
        if def.fields.len() != layout.fields.len() {
            return None;
        }
        let mut fields = Vec::with_capacity(def.fields.len());
        for spec in &def.fields {
            let layout_field = layout.fields.iter().find(|f| f.name == spec.name)?;
            let kind = match &layout_field.kind {
                PackedKind::Int => SlotKind::Int,
                PackedKind::Float => SlotKind::Float,
                PackedKind::F32 => SlotKind::F32,
                PackedKind::F64 => SlotKind::F64,
                PackedKind::IntN { bits, signed } => SlotKind::IntN {
                    bits: *bits,
                    signed: *signed,
                },
                PackedKind::Bool => SlotKind::Bool,
                PackedKind::Struct(inner) => SlotKind::Struct(self.resolve_packed_schema(inner)?),
            };
            fields.push(PackedSlot { kind });
        }
        let byte_size = layout.byte_size();
        if byte_size == 0 {
            return None; // a zero-field packed struct has no recoverable element count — stay boxed.
        }
        Some(Rc::new(PackedSchema {
            def: Some(def),
            fields,
            byte_size,
            column: layout.column,
        }))
    }

    /// Build an enum value from a type, a variant name, and its argument values.
    fn make_variant(
        &mut self,
        def: &Rc<EnumDef>,
        variant: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        let Some(info) = def.variant(variant) else {
            return Err(self.runtime_error(
                DiagnosticCode::UnknownName,
                span,
                format!("enum `{}` has no variant `{variant}`", def.name()),
            ));
        };
        if args.len() != info.field_names.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "variant `{}.{variant}` takes {} field(s) but {} were supplied",
                    def.name(),
                    info.field_names.len(),
                    args.len()
                ),
            ));
        }
        Ok(Value::Enum(Rc::new(EnumValue {
            enum_name: def.name().to_string(),
            variant: variant.to_string(),
            data: args,
            variant_index: def.variant_index(variant),
            reflect: None,
        })))
    }

    /// `MyEnum.try_from(s)` → `?MyEnum` (`some(case)` if `s` names a payload-free case, else `none`)
    /// and `MyEnum.from(s)` → `MyEnum` (the case, or a panic if `s` names none) — the PHP `tryFrom`/
    /// `from` pair, matched by case **name**. A payload-carrying variant is not name-constructible
    /// (no payload to supply), so it never matches. The VM's `Op::EnumFromStr` mirrors this exactly.
    fn enum_from_string(
        &mut self,
        def: &Rc<EnumDef>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        if args.len() != 1 {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("`{}.{method}` takes one string argument", def.name()),
            ));
        }
        let Value::Str(key) = &args[0] else {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}.{method}` expects a string, found {}",
                    def.name(),
                    args[0].type_name()
                ),
            ));
        };
        let key = key.clone();
        // Match only a payload-free case of this name (a payload variant cannot be built from a name).
        let matched = def.variant(&key).is_some_and(|v| v.field_names.is_empty());
        if matched {
            let value = Value::Enum(Rc::new(EnumValue {
                enum_name: def.name().to_string(),
                variant_index: def.variant_index(&key),
                variant: key,
                data: vec![],
                reflect: None,
            }));
            if method == "from" {
                Ok(value)
            } else {
                Ok(builtin_enum("Option", "some", vec![value]))
            }
        } else if method == "from" {
            Err(self.runtime_error(
                DiagnosticCode::Panic,
                span,
                format!("panic: `{}` has no case `{key}`", def.name()),
            ))
        } else {
            Ok(builtin_enum("Option", "none", vec![]))
        }
    }

    /// Apply the binding rules: `mut` declares/overwrites a mutable binding in the
    /// current scope; a bare `name = expr` reassigns an existing (mutable) binding if
    /// one is in scope, errors on an immutable one, and otherwise introduces a new
    /// immutable binding locally.
    fn bind(&mut self, mut_decl: bool, name: &str, name_span: Span, value: Value) -> Eval<()> {
        self.bind_inner(mut_decl, false, name, name_span, value)
    }

    /// As [`Self::bind`], but for the reassignment wrapping a field-set `x.f = v` (object-model
    /// slice 2b′): the immutable-binding check is skipped, because the checker already enforced the
    /// kind-aware `mut` rule statically — a `struct` field-set on an immutable `x` is a static
    /// E0006, and a `class` field-set mutates in place (this rebind restores `x` to that instance).
    fn bind_field_assign(&mut self, name: &str, name_span: Span, value: Value) -> Eval<()> {
        self.bind_inner(false, true, name, name_span, value)
    }

    fn bind_inner(
        &mut self,
        mut_decl: bool,
        field_assign: bool,
        name: &str,
        name_span: Span,
        value: Value,
    ) -> Eval<()> {
        if mut_decl {
            self.scope.declare(name.to_string(), value, true);
            return Ok(());
        }
        let outcome = if field_assign {
            self.scope.assign_force(name, value.clone())
        } else {
            self.scope.assign(name, value.clone())
        };
        match outcome {
            // Reassignment drops the displaced value, running its destructor if it was the
            // last reference (the deterministic destruction the spec requires).
            AssignOutcome::Assigned(old) => {
                self.destroy_value(old);
                Ok(())
            }
            AssignOutcome::NotFound => {
                self.scope.declare(name.to_string(), value, false);
                Ok(())
            }
            AssignOutcome::Immutable => {
                self.record_abort_trace(name_span);
                self.diagnostics.push(
                    Diagnostic::error(
                        DiagnosticCode::ImmutableAssignment,
                        name_span,
                        format!("cannot assign to `{name}`, which is immutable"),
                    )
                    .with_help(format!(
                        "declare it with `mut {name} = ...` to allow reassignment"
                    )),
                );
                Err(Unwind::Abort)
            }
        }
    }

    /// `?` for the Core-IR interpreter: like [`eval_try`](Self::eval_try), but on the **error** path
    /// it first runs `on_error` — the drop pass's statically-computed reclamation of the frame locals
    /// this early return abandons (Phase 4.2c) — so a `?` propagation destroys them exactly as an
    /// explicit `return` would, before the value unwinds to the caller.
    fn eval_try_ir(
        &mut self,
        value: Value,
        on_error: &[noeta_ir::TryDrop],
        span: Span,
    ) -> Eval<Value> {
        match try_branch(&value) {
            Some(TryBranch::Success(inner)) => Ok(inner),
            Some(TryBranch::Empty) => {
                for drop in on_error {
                    if drop.relevant {
                        if let Some(v) = self.scope.take_for_drop(&drop.name) {
                            self.destroy_value(v);
                        }
                    } else {
                        self.scope.release_binding(&drop.name);
                    }
                }
                Err(Unwind::Return(value))
            }
            None => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`?` expects a `Result` or `Option`, found {}",
                    value.type_name()
                ),
            )),
        }
    }

    /// Dispatch a built-in method (`.count()`, `.enumerate()`) on a receiver value, or
    /// construct an enum variant (`OrderError.NegativePrice(2)`).
    /// Call a method whose arguments may skip a defaulted parameter, per the mask lowering carried
    /// on the call. Only a user method or associated function can be masked; the built-in and
    /// native paths declare no defaults, so a mask never reaches them.
    fn call_method_masked(
        &mut self,
        receiver: Value,
        name: &str,
        args: Vec<Value>,
        span: Span,
        supplied: Option<u64>,
    ) -> Eval<Value> {
        if supplied.is_some() {
            if let Value::Object(object) = &receiver
                && let Some(method) = object.def.methods.get(name)
            {
                let (object, method) = (Rc::clone(object), Rc::clone(method));
                return self.call_method_on_masked(&object, &method, args, span, supplied);
            }
            if let Value::Type(def) = &receiver
                && let Some(method) = def.methods.get(name)
            {
                return self.call_closure_masked(&Rc::clone(method), args, span, supplied);
            }
            if let Value::EnumType(def) = &receiver
                && def.variant(name).is_none()
                && let Some(method) = def.method(name)
            {
                return self.call_closure_masked(&Rc::clone(method), args, span, supplied);
            }
            if let Value::Enum(e) = &receiver
                && let Some(Value::EnumType(def)) = self.scope.lookup(&e.enum_name)
                && let Some(method) = def.method(name)
            {
                let method = Rc::clone(method);
                return self.call_enum_method_masked(receiver, &method, args, span, supplied);
            }
        }
        self.call_method(receiver, name, args, span)
    }

    fn call_method(
        &mut self,
        receiver: Value,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        if let Value::EnumType(def) = &receiver {
            // `MyEnum.try_from(s)` / `MyEnum.from(s)` — construct a payload-free case from its name
            // string (the PHP `tryFrom`/`from` pair). Intercepted before variant construction so the
            // built-in names cannot be shadowed by a same-named variant lookup.
            if name == "try_from" || name == "from" {
                return self.enum_from_string(&Rc::clone(def), name, args, span);
            }
            // An associated function `Enum.f(...)` (the unified body, object-model slice 3): resolved
            // when the name is a method rather than a variant. Variant construction still wins for a
            // variant name (uppercase by convention, so the two never collide).
            if def.variant(name).is_none()
                && let Some(method) = def.method(name)
            {
                return self.call_closure(&Rc::clone(method), args, span);
            }
            return self.make_variant(&Rc::clone(def), name, args, span);
        }
        // `json.parse(...)` — a Ring 2 native module function call.
        if let Value::NativeModule(module) = &receiver {
            return self.call_native_module(module, name, &args, span);
        }
        // `Order.new(...)` — an associated function (no instance); call it directly.
        if let Value::Type(def) = &receiver {
            return match def.methods.get(name) {
                Some(method) => self.call_closure(&Rc::clone(method), args, span),
                None => Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("type `{}` has no associated function `{name}`", def.name()),
                )),
            };
        }
        // `order.total()` — an instance method; the instance's fields are in scope.
        if let Value::Object(object) = &receiver {
            // `o.to_json()` on a type that `@derive(Serialize<Json>)` (so has no hand-written `to_json`)
            // synthesizes a structural JSON string.
            if name == "to_json" && args.is_empty() && object.def.derives_tojson {
                return Ok(Value::Str(value_to_json(&receiver)));
            }
            if let Some(method) = object.def.methods.get(name) {
                return self.call_method_on(&Rc::clone(object), &Rc::clone(method), args, span);
            }
            // A native class's instance method (native-extensibility S3 / Pass 2a): no hoisted
            // `.noe` method by this name, but the object's shape names a registered native class
            // that declares it — route to the class's native `dispatch` (the Object-arm twin of the
            // extern-method seam). A user class always resolves through the method table above, so
            // only a genuine native class reaches this branch.
            if self
                .reg()
                .find_class_method(object.def.name(), name)
                .is_some()
            {
                return self.call_native_class_method(&receiver, name, args, span);
            }
            // The runtime member-call fallback (the field-access-then-call desugar's `dyn`
            // path): no method `name`, but the object HAS a field `name` — `obj.f(args)`
            // means `(obj.f)(args)`, so call the field's value. The same order the checker
            // pins statically (a method wins, the field is consulted only on a miss), and the
            // same route the lowered `Field` + `Call` takes — a non-callable field value
            // raises the indirect-call E0007 ("`X` is not callable"), identically in both
            // backends. A type with neither stays the runtime E0005.
            return match object.field(name) {
                Some(value) => self.call(value, args, span),
                None => Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("type `{}` has no method `{name}`", object.def.name()),
                )),
            };
        }
        // `status.label()` — an enum instance method (the unified body, object-model slice 3). The
        // enum value carries no method table of its own (a bare name/variant/data triple), so the
        // `EnumDef` is resolved from the definition (global) scope by the enum's name. An unknown
        // method falls through to the built-in paths below (e.g. the primitive `compare`).
        if let Value::Enum(e) = &receiver
            && let Some(Value::EnumType(def)) = self.scope.lookup(&e.enum_name)
            && let Some(method) = def.method(name)
        {
            return self.call_enum_method(receiver.clone(), &Rc::clone(method), args, span);
        }
        // `color.value()` on a native **backed** enum (native-extensibility S1): a native enum has
        // no `EnumType` in scope, so its backing constant is resolved from the registry by the
        // value's (short) enum name + variant. The checker types this as the backing scalar.
        if let Value::Enum(e) = &receiver
            && name == "value"
            && args.is_empty()
            && let Some(en) = self.reg().resolve_enum(&e.enum_name)
            && let Some((_, variant)) = en.variant(&e.variant)
        {
            return Ok(match variant.value {
                noeta_stdlib::VariantValue::Str(s) => Value::Str(s.to_string()),
                noeta_stdlib::VariantValue::Int(n) => Value::Int(n),
                noeta_stdlib::VariantValue::None => Value::Unit,
            });
        }
        // `e.to_json()` on an enum that `@derive(Serialize<Json>)`s (so has no hand-written
        // `to_json`): the variant rendering `json.stringify` produces — the enum twin of the
        // object arm below (and of the VM's enum method-dispatch gate).
        if let Value::Enum(e) = &receiver
            && name == "to_json"
            && args.is_empty()
            && let Some(Value::EnumType(def)) = self.scope.lookup(&e.enum_name)
            && def.derives_tojson
        {
            return Ok(Value::Str(value_to_json(&receiver)));
        }
        // A **native enum**'s instance method (native-extensibility S1 / Slice B): no `.noe`
        // `EnumDef` method by this name and not the built-in `value()`/`to_json` accessor, but the
        // value's (short) enum name resolves to a registered native enum that declares it — route to
        // the enum's native `dispatch`. The enum twin of the Object arm's `find_class_method` →
        // `call_native_class_method` fall-through above.
        if let Value::Enum(e) = &receiver
            && self.reg().find_enum_method(&e.enum_name, name).is_some()
        {
            return self.call_native_enum_method(&receiver, name, args, span);
        }
        // `x.compare(y)` — the `Ordering` of two primitives. This is the value a `Comparable`
        // impl returns (typically by delegating to a field's `compare`); it lights up nothing on
        // its own, but `Comparable` dispatch reads the variant to derive `< <= > >=`.
        if name == "compare" {
            if args.len() != 1 {
                return Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "method `compare` takes 1 argument but {} were supplied",
                        args.len()
                    ),
                ));
            }
            return match compare_primitive(&receiver, &args[0]) {
                Some(ordering) => Ok(builtin_enum(
                    "Ordering",
                    noeta_ast::ordering_variant(ordering),
                    Vec::new(),
                )),
                None => Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "cannot compare {} and {}",
                        receiver.type_name(),
                        args[0].type_name()
                    ),
                )),
            };
        }
        // Ring 1 string methods (`upper`/`split`/`replace`/...) — their semantics, arity, and
        // argument typing live once in `noeta-stdlib`, shared with the VM, so the two backends
        // cannot drift. `Unknown` falls through to the collection methods below.
        if let Value::Str(s) = &receiver {
            let projected: Vec<_> = args.iter().map(project_arg).collect();
            match noeta_stdlib::string_method(s, name, &projected) {
                noeta_stdlib::Dispatch::Done(output) => return Ok(output_to_value(output)),
                noeta_stdlib::Dispatch::Err(error) => {
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                noeta_stdlib::Dispatch::Unknown => {}
            }
        }
        // Bit-manipulation methods on `int` (P-BITS Tier B4) — the popcount-class intrinsics,
        // delegating to the shared `int_method` so the backends agree by construction. `rotate_*`
        // take an `int` amount; the rest take none.
        if let Value::Int(recv) = &receiver
            && let Some(method) = noeta_stdlib::IntMethod::from_name(name)
        {
            let recv = *recv;
            self.expect_std_arity(name, &args, method.arity(), span)?;
            let arg = match args.first() {
                Some(Value::Int(n)) => *n,
                Some(other) => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`int.{name}` expects an integer argument, found {}",
                            other.type_name()
                        ),
                    ));
                }
                None => 0,
            };
            return Ok(Value::Int(noeta_stdlib::int_method(recv, method, arg)));
        }
        // Cross-domain numeric conversions (S0): `int→float/f32`, `float/f32→int`, `float↔f32`. The
        // `IntMethod` branch above already handled `int→int` (`to_i32`…) and returned; an integer
        // receiver reaches here only for the two float destinations (`to_float`/`to_f32`), a
        // `float`/`f32` receiver for any. Delegates to the shared `num_convert` so both backends agree.
        if let Some(src) = numeric_scalar(&receiver)
            && let Some(dest) = noeta_stdlib::NumConvert::from_name(name)
        {
            self.expect_std_arity(name, &args, 0, span)?;
            return Ok(match noeta_stdlib::num_convert(src, dest) {
                noeta_stdlib::NumScalar::Int(i) => Value::Int(i),
                noeta_stdlib::NumScalar::F64(f) => Value::Float(f),
                noeta_stdlib::NumScalar::F32(f) => Value::F32(f),
            });
        }
        // Ring 1 list methods (reverse/contains/join) — Value-specific, so implemented per
        // backend, but the method set is the shared `ListMethod` enum: a non-exhaustive `match`
        // here would not compile, so the VM cannot omit a method this backend offers.
        if let Value::List(repr) = &receiver
            && let Some(method) = noeta_stdlib::ListMethod::from_name(name)
        {
            let items = repr.to_rc_vec();
            // The source list's reflected element type (R1) — carried onto a `to_set` result so a
            // `Set<T>` recovers its element type (sets have no literal of their own).
            let list_reflect = repr.reflect();
            return self.call_list_method(method, &items, list_reflect, name, &args, span);
        }
        // Ring 1 set methods (contains/union/intersection).
        if let Value::Set(items, _) = &receiver
            && let Some(method) = noeta_stdlib::SetMethod::from_name(name)
        {
            return self.call_set_method(method, items, name, &args, span);
        }
        // Extern-type methods (extern-types X1): every registry-contributed type routes through
        // its registered `ExtType`'s one shared dispatch. Mirrors the VM's `call_extern_method`.
        if let Value::Extern(cell) = &receiver {
            let cell = Rc::clone(cell);
            return self.call_extern_method(&cell, name, &args, span);
        }
        // Channel endpoint methods (isolates I.1): `tx.send(v)`/`tx.close()` on a sender, `rx.recv()`
        // on a receiver. `send`/`recv` return leaf futures (enqueue/dequeue when polled); `close` is
        // synchronous. Endpoint validity was checked statically, so an unknown method here is a
        // genuine miss (falls through to the unknown-method error below).
        if let Value::Sender(id) = &receiver {
            let id = *id;
            match name {
                "send" => {
                    self.expect_std_arity(name, &args, 1, span)?;
                    let value = args.into_iter().next().unwrap();
                    return Ok(Value::ChannelSend(
                        id,
                        Rc::new(value),
                        Rc::new(std::cell::Cell::new(
                            noeta_stdlib::channel::SendPhase::Fresh,
                        )),
                    ));
                }
                "close" => {
                    self.expect_std_arity(name, &args, 0, span)?;
                    self.channels[id.index()].closed = true;
                    self.channel_progress += 1;
                    return Ok(Value::Unit);
                }
                _ => {}
            }
        }
        if let Value::Receiver(id) = &receiver
            && name == "recv"
        {
            self.expect_std_arity(name, &args, 0, span)?;
            return Ok(Value::ChannelRecv(*id));
        }
        // Task-handle cancellation methods (Track A.8): `h.cancel()` marks the task cancelled
        // exactly as a `race` loser (idempotent; a no-op on a completed task or a bare future),
        // `h.join()` drives it and reports the typed `Result<T, Cancelled>` outcome. Offered on any
        // `Future<T>`, since a spawn/isolate handle is itself a `Future<T>`. The VM's mirror.
        if matches!(receiver, Value::Handle(..) | Value::Future(_)) {
            match name {
                "cancel" => {
                    self.expect_std_arity(name, &args, 0, span)?;
                    self.cancel_task(&receiver);
                    return Ok(Value::Unit);
                }
                "join" => {
                    self.expect_std_arity(name, &args, 0, span)?;
                    return self.join_task(receiver, span);
                }
                _ => {}
            }
        }
        // (The reactive handle methods lived here until higher-order-abi H5 — `Signal`/
        // `Computed`/`Effect` are registry extern types now, dispatched through the ctx
        // seam like any other. Mirrors the VM.)
        // Iterator methods (next/collect) — the shared `IterMethod` enum, like the file handle above.
        if let Value::Iter(state) = &receiver
            && let Some(method) = noeta_stdlib::IterMethod::from_name(name)
        {
            let state = Rc::clone(state);
            return self.call_iter_method(method, &state, name, &args, span);
        }
        // `iter()` on a built-in collection (Track I.1a) → a lazy iterator. A set/map first becomes a
        // list of its elements / values (the iteration order `for` uses); a list shares its backing.
        // Guarded to built-in collections so it does not shadow a user object's own `iter` method.
        if name == "iter" && matches!(receiver, Value::List(_) | Value::Set(..) | Value::Map(..)) {
            self.expect_std_arity(name, &args, 0, span)?;
            let list = match &receiver {
                Value::List(_) => receiver.clone(),
                Value::Set(items, _) => Value::list_rc(Rc::clone(items)),
                Value::Map(entries, _) => Value::list(entries.values().cloned().collect()),
                _ => unreachable!("guarded to list/set/map above"),
            };
            return Ok(Value::Iter(Rc::new(RefCell::new(IterState::List {
                list,
                cursor: 0,
            }))));
        }
        // Ring 1 map methods (keys/values/has).
        if let Value::Map(entries, _) = &receiver
            && let Some(method) = noeta_stdlib::MapMethod::from_name(name)
        {
            return self.call_map_method(method, entries, name, &args, span);
        }
        // `list.to_bytes()` — serialize a `List<@packed>` to its raw flat buffer (P-PACK 4.4). A
        // boxed list has no canonical serialized form, so it is a type error (surfaced, not silent).
        if name == "to_bytes"
            && let Value::List(repr) = &receiver
        {
            self.expect_std_arity(name, &args, 0, span)?;
            return match repr.packed_raw_bytes() {
                Some(buf) => Ok(Value::Bytes(Rc::new(buf))),
                None => Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "`to_bytes` expects a packed list (a `List` of `@packed` structs)".to_string(),
                )),
            };
        }
        // Buffer-direct list reductions (packed-reductions arc): `sum`/`product`/`min`/`max` on a
        // numeric list, `any`/`all`/`count` on a `List<bool>`. A packed scalar list folds its raw
        // byte buffer in a tight kernel; a boxed list folds element-wise — one shared body in
        // `noeta-stdlib`, so both representations and both backends agree. `sum` intercepts here
        // (superseding the old materializing `Builtin::Sum`) so the packed fast path and the
        // width-wrapping result type apply.
        if let Value::List(repr) = &receiver
            && args.is_empty()
            && (noeta_stdlib::NumReduce::from_name(name).is_some()
                || noeta_stdlib::BoolReduce::from_name(name).is_some())
        {
            let repr = repr.clone();
            return self.call_list_reduction(&repr, name, span);
        }
        // `checked_sum()` (array-ops arc): the opt-in overflow-reporting reduction, beside the folds.
        if let Value::List(repr) = &receiver
            && name == "checked_sum"
            && args.is_empty()
        {
            let repr = repr.clone();
            return self.call_list_checked_sum(&repr, span);
        }
        // Element-wise array-programming methods (array-ops arc): `scale`/`abs`/`neg`/`clamp` produce
        // a new list; one shared kernel with the operators, so packed and boxed paths agree.
        if let Value::List(repr) = &receiver
            && noeta_stdlib::is_bulk_method(name)
        {
            let repr = repr.clone();
            return self.call_list_bulk_method(&repr, name, &args, span);
        }
        // Eager collection methods that reuse the prelude builtin impls (prelude-redesign P1):
        // `xs.map(f)` / `xs.filter(f)` on a list. Routed through `call_builtin` with the receiver as
        // the first argument, so the method form and the (legacy) free-function form `map(xs, f)`
        // share exactly one implementation. A user object's own `map`/`filter` method wins — it is
        // dispatched earlier, before this built-in fallback.
        if let Value::List(_) = &receiver
            && let Some(builtin) = match name {
                "map" if args.len() == 1 => Some(Builtin::Map),
                "filter" if args.len() == 1 => Some(Builtin::Filter),
                _ => None,
            }
        {
            let mut builtin_args = Vec::with_capacity(args.len() + 1);
            builtin_args.push(receiver);
            builtin_args.extend(args);
            return self.call_builtin(builtin, builtin_args, span);
        }
        let arity_ok = args.is_empty();
        // `len()` is the length of a collection (P1.3 — `count` is iterator-only, a consuming
        // terminal; a collection `count` is an unknown method like any other).
        let result = match (name, &receiver) {
            ("len", Value::List(items)) if arity_ok => Some(Value::Int(items.len() as i64)),
            ("len", Value::Set(items, _)) if arity_ok => Some(Value::Int(items.len() as i64)),
            ("len", Value::Map(entries, _)) if arity_ok => Some(Value::Int(entries.len() as i64)),
            ("len", Value::Str(s)) if arity_ok => Some(Value::Int(s.chars().count() as i64)),
            ("len", Value::Bytes(b)) if arity_ok => Some(Value::Int(b.len() as i64)),
            // Lowercase hex rendering of a `bytes` buffer (crypto arc C1) — the shared helper,
            // so both backends print digests identically.
            ("to_hex", Value::Bytes(b)) if arity_ok => {
                Some(Value::Str(noeta_stdlib::bytes_to_hex(b)))
            }
            // UTF-8 decode — the inverse of `string.to_bytes()`; invalid UTF-8 is `none`.
            ("decode", Value::Bytes(b)) if arity_ok => Some(optional_to_value(
                noeta_stdlib::bytes_decode_utf8(b).map(Value::Str),
            )),
            // `.enumerate()` yields a list of `(index, value)` **tuples** (object-model slice 4b —
            // tuples are the positional-pair type), destructured by a `for (i, x) in …` pattern.
            ("enumerate", Value::List(items)) if arity_ok => {
                let pairs = items
                    .to_rc_vec()
                    .iter()
                    .enumerate()
                    .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                    .collect();
                Some(Value::list(pairs))
            }
            _ => None,
        };
        match result {
            Some(value) => Ok(value),
            // The "takes no arguments" error applies only to the *known* zero-arg methods
            // (`count`/`enumerate`) called with arguments — not to an unknown method name, which is
            // always `UnknownName` regardless of arity. (Without this guard, `xs.map(f)` — `map` is a
            // free function, not a method — reported `TypeMismatch` here while the VM reported
            // `UnknownName`; the guard makes both backends agree.)
            None if !arity_ok && (name == "len" || name == "enumerate") => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("method `{name}` takes no arguments"),
            )),
            None => Err(self.runtime_error(
                DiagnosticCode::UnknownName,
                span,
                format!("no method `{name}` on {}", receiver.type_name()),
            )),
        }
    }

    /// `invoke(recv, name, args)` / `invoke(name, args)` — fallible by-name dispatch (P2.6). Reuses
    /// the same name-keyed method tables as `call_method` (or, receiver-less, the global scope), but
    /// **pre-checks** name resolution and arity so a miss is a runtime `Result.Err` rather than a
    /// recorded diagnostic. A panic *inside* the invoked body still aborts (the `?` propagation
    /// below), so only the by-name *resolution* is caught. The VM's `Op::Invoke` mirrors this
    /// exactly, building identical `Ok`/`Err` values.
    fn invoke_dynamic(
        &mut self,
        receiver: Option<Value>,
        name_val: Value,
        args_val: Value,
        span: Span,
    ) -> Eval<Value> {
        let Value::Str(method) = &name_val else {
            return Ok(invoke_err(format!(
                "invoke name must be a string, found {}",
                name_val.type_name()
            )));
        };
        let Value::List(items) = &args_val else {
            return Ok(invoke_err(format!(
                "invoke args must be a list, found {}",
                args_val.type_name()
            )));
        };
        let args: Vec<Value> = (*items.to_rc_vec()).clone();
        // No receiver: the free-function form. `method` names a **top-level function** — the same
        // string `params_of` takes for a free fn — so resolution is a lookup in the global scope
        // and nowhere else. Deliberately NOT `self.scope`: the VM reads a global slot, so consulting
        // the local chain here would let `invoke("g", …)` find a local `g` in one backend and miss
        // it in the other. Calling through `call_closure` also means the callee gets its ordinary
        // sealed child scope, exactly as a direct `g(...)` would.
        let Some(receiver) = receiver else {
            let Some(Value::Function(closure)) = self.globals.lookup(method) else {
                return Ok(invoke_err(free_fn_miss_message(method)));
            };
            let required = required_count(&closure.defaults);
            if args.len() < required || args.len() > closure.params.len() {
                return Ok(invoke_err(arity_message(
                    "function",
                    required,
                    closure.params.len(),
                    args.len(),
                )));
            }
            let result = self.call_closure(&closure, args, span)?;
            return Ok(builtin_enum("Result", "Ok", vec![result]));
        };
        // A reflection `Type` value (e.g. a stored attribute type-ref) dispatches like the type
        // handle it names: resolve it to the type and fall through to the `Value::Type` arm.
        let receiver = match reflection_type_name(&receiver) {
            Some(name) => match self.scope.lookup(&name) {
                Some(found @ Value::Type(_)) => found,
                _ => {
                    return Ok(invoke_err(format!(
                        "type `{name}` is not a constructible type"
                    )));
                }
            },
            None => receiver,
        };
        match &receiver {
            // A type handle → an associated function (no receiver).
            Value::Type(def) => {
                let Some(closure) = def.methods.get(method) else {
                    return Ok(invoke_err(format!(
                        "type `{}` has no associated function `{method}`",
                        def.name()
                    )));
                };
                let closure = Rc::clone(closure);
                let required = required_count(&closure.defaults);
                if args.len() < required || args.len() > closure.params.len() {
                    return Ok(invoke_err(arity_message(
                        "associated function",
                        required,
                        closure.params.len(),
                        args.len(),
                    )));
                }
                let result = self.call_closure(&closure, args, span)?;
                Ok(builtin_enum("Result", "Ok", vec![result]))
            }
            // A value → an instance method (the instance's fields are in scope).
            Value::Object(object) => {
                let Some(method_closure) = object.def.methods.get(method) else {
                    return Ok(invoke_err(format!(
                        "type `{}` has no method `{method}`",
                        object.def.name()
                    )));
                };
                let object = Rc::clone(object);
                let method_closure = Rc::clone(method_closure);
                let required = required_count(&method_closure.defaults);
                if args.len() < required || args.len() > method_closure.params.len() {
                    return Ok(invoke_err(arity_message(
                        "method",
                        required,
                        method_closure.params.len(),
                        args.len(),
                    )));
                }
                let result = self.call_method_on(&object, &method_closure, args, span)?;
                Ok(builtin_enum("Result", "Ok", vec![result]))
            }
            _ => Ok(invoke_err(format!(
                "cannot invoke on a value of type `{}`",
                receiver.type_name()
            ))),
        }
    }

    /// A Ring 1 list method (`reverse`/`contains`/`join`). Mirrors the VM's `call_list_method`;
    /// arity/type misuse is reported through the shared `noeta-stdlib` error builders so both
    /// backends produce identical diagnostics.
    #[allow(clippy::too_many_arguments)]
    fn call_list_method(
        &mut self,
        method: noeta_stdlib::ListMethod,
        items: &[Value],
        list_reflect: Option<Rc<TypeRepr>>,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match method {
            noeta_stdlib::ListMethod::Reverse => {
                self.expect_std_arity(name, args, 0, span)?;
                let mut reversed = items.to_vec();
                reversed.reverse();
                Ok(Value::list(reversed))
            }
            noeta_stdlib::ListMethod::Contains => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(Value::Bool(items.iter().any(|item| *item == args[0])))
            }
            noeta_stdlib::ListMethod::Join => {
                self.expect_std_arity_range(name, args, 0, 1, span)?;
                let separator = match args.first() {
                    Some(arg) => self.expect_std_string(name, arg, span)?.to_string(),
                    None => String::new(),
                };
                let joined = items
                    .iter()
                    .map(Value::display)
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::Str(joined))
            }
            noeta_stdlib::ListMethod::Sorted => {
                self.expect_std_arity(name, args, 0, span)?;
                // Every element must be mutually orderable with the first (homogeneous numbers
                // or strings — or derived-`Comparable` structs/enums, which order structurally
                // via `compare_field`); otherwise there is no total order to sort by. A stable
                // sort then keeps equal elements in input order, matching the VM exactly.
                if items
                    .iter()
                    .any(|item| compare_field(&items[0], item).is_none())
                {
                    let error = noeta_stdlib::unorderable_error(name);
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                let mut sorted = items.to_vec();
                sorted.sort_by(|a, b| compare_field(a, b).unwrap_or(std::cmp::Ordering::Equal));
                Ok(Value::list(sorted))
            }
            noeta_stdlib::ListMethod::Slice => {
                self.expect_std_arity_range(name, args, 1, 2, span)?;
                let start = self.expect_std_int(name, &args[0], span)?;
                let len = items.len();
                let end = self.expect_std_opt_int(name, args, 1, len as i64, span)?;
                if start < 0 || end < start || end as usize > len {
                    let error = noeta_stdlib::slice_bounds_error(start, end, len);
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                let slice = items[start as usize..end as usize].to_vec();
                Ok(Value::list(slice))
            }
            noeta_stdlib::ListMethod::First => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(match items.first() {
                    Some(value) => builtin_enum("Option", "some", vec![value.clone()]),
                    None => builtin_enum("Option", "none", Vec::new()),
                })
            }
            noeta_stdlib::ListMethod::Last => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(match items.last() {
                    Some(value) => builtin_enum("Option", "some", vec![value.clone()]),
                    None => builtin_enum("Option", "none", Vec::new()),
                })
            }
            noeta_stdlib::ListMethod::ToSet => {
                self.expect_std_arity(name, args, 0, span)?;
                match canonical_set(items) {
                    // Carry the element type from the source list's `List<T>` tag onto the resulting
                    // `Set<T>` (R1 set tags); mirrors the VM's `set_tag_from_list`.
                    Some(canonical) => {
                        let set_tag = match list_reflect.as_deref() {
                            Some(TypeRepr::List(elem)) => {
                                Some(Rc::new(TypeRepr::Set(elem.clone())))
                            }
                            _ => None,
                        };
                        Ok(Value::set_value_tagged(Rc::new(canonical), set_tag))
                    }
                    None => {
                        let error = noeta_stdlib::unorderable_error(name);
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            noeta_stdlib::ListMethod::Set => {
                self.expect_std_arity(name, args, 2, span)?;
                let i = self.expect_std_int(name, &args[0], span)?;
                if i < 0 || i as usize >= items.len() {
                    return Err(self.runtime_error(
                        DiagnosticCode::IndexOutOfBounds,
                        span,
                        format!("index {i} out of bounds for list of length {}", items.len()),
                    ));
                }
                let mut new = items.to_vec();
                new[i as usize] = args[1].clone();
                Ok(Value::list(new))
            }
        }
    }

    /// A Ring 1 set method (`contains`/`union`/`intersection`). Mirrors the VM's
    /// `call_set_method`. The receiver `items` are already canonical (sorted, de-duplicated).
    fn call_set_method(
        &mut self,
        method: noeta_stdlib::SetMethod,
        items: &[Value],
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match method {
            noeta_stdlib::SetMethod::Contains => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(Value::Bool(items.iter().any(|item| *item == args[0])))
            }
            noeta_stdlib::SetMethod::Union => {
                self.expect_std_arity(name, args, 1, span)?;
                let other = self.expect_std_set(name, &args[0], span)?;
                let mut combined = items.to_vec();
                combined.extend(other.iter().cloned());
                // Both operands are valid sets, so every element is orderable.
                let canonical = canonical_set(&combined).expect("set elements are orderable");
                Ok(Value::set_value(Rc::new(canonical)))
            }
            noeta_stdlib::SetMethod::Intersection => {
                self.expect_std_arity(name, args, 1, span)?;
                let other = self.expect_std_set(name, &args[0], span)?;
                // `items` is already canonical, so filtering it preserves sorted, de-duplicated order.
                let kept: Vec<Value> = items
                    .iter()
                    .filter(|item| other.iter().any(|o| *item == o))
                    .cloned()
                    .collect();
                Ok(Value::set_value(Rc::new(kept)))
            }
            noeta_stdlib::SetMethod::Add => {
                self.expect_std_arity(name, args, 1, span)?;
                let mut combined = items.to_vec();
                combined.push(args[0].clone());
                // The new element must be orderable with the rest (a homogeneous set).
                match canonical_set(&combined) {
                    Some(canonical) => Ok(Value::set_value(Rc::new(canonical))),
                    None => {
                        let error = noeta_stdlib::unorderable_error(name);
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            noeta_stdlib::SetMethod::Remove => {
                self.expect_std_arity(name, args, 1, span)?;
                // `items` is already canonical; filtering preserves sorted, de-duplicated order.
                let kept: Vec<Value> = items
                    .iter()
                    .filter(|item| **item != args[0])
                    .cloned()
                    .collect();
                Ok(Value::set_value(Rc::new(kept)))
            }
        }
    }

    /// Read a set argument for a set method, raising the shared `noeta-stdlib` type error.
    fn expect_std_set<'a>(
        &mut self,
        name: &str,
        value: &'a Value,
        span: Span,
    ) -> Eval<&'a Rc<Vec<Value>>> {
        match value {
            Value::Set(items, _) => Ok(items),
            _ => {
                let error = noeta_stdlib::type_error(name, "set");
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Dispatch a Ring 2 native module function call (`json.parse(...)`). Mirrors the VM's
    /// `call_native_module`.
    fn call_native_module(
        &mut self,
        module: &str,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        // A virtual-module function (`reactive.signal(...)`, prelude-redesign P2) is a builtin —
        // it needs the executor/reactive graph the plain registry seam cannot reach — so a
        // qualified call intercepts here, ahead of registry dispatch, exactly like `fs.*_async`.
        // Per-**function**, not per-module (higher-order-abi H0): a module migrates onto the ctx
        // seam name by name (`task.sleep` is registered, `task.all` still virtual), so unmatched
        // names fall through to the registry arms and the shared unknown-function error below.
        // (The virtual-module intercept died with higher-order-abi H5: `task` migrated at H0/H2,
        // `http.serve` at H3, and `reactive` — the last virtual module — at H5. Every std module
        // now dispatches through the registry arms below. Mirrors the VM.)
        // A function registered in the native-extension registry dispatches through the shared
        // seam: project arguments onto `NativeValue`, run the one shared dispatch body (host
        // threaded in), and materialize the `NativeOut` result (the result shape supplied from the
        // function's `RetTy`). Routing is per-function so a partially-migrated module (`vec`, whose
        // bulk `*_all` kernels stay per-backend) falls through for its unmigrated functions.
        let name = module;
        // Bound once (instance-registry IR3): `reg` is `&'static`, so it outlives the `&mut self`
        // host borrow below and every native lookup routes through this interpreter's registry.
        let reg = self.reg();
        if let Some(sig) = reg.find_function(name, func) {
            // A reflective module (`json`) marshals its arguments deeply (the recursive value tree
            // `json.stringify` introspects); every other module uses the cheap shallow projection.
            let deep = reg.find_module(name).is_some_and(|m| m.deep_marshal);
            let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
                args.iter().map(value_to_native_deep).collect()
            } else {
                args.iter().map(|a| marshal_native_arg(a, reg)).collect()
            };
            return match reg.dispatch(name, func, &mut *self.host, &nargs) {
                // Async WORK (extern-types X5): ticket the descriptor on the executor and hand
                // back the leaf async-IO future — mirrors the VM.
                Ok(noeta_stdlib::NativeOut::Spawn(spawn)) => {
                    let id = self.executor.spawn_ext(&mut *self.host, spawn.0);
                    Ok(Value::AsyncIo(id))
                }
                Ok(out) => Ok(materialize_ext(out, sig.ret, args)),
                Err(error) => Err(self.std_dispatch_error(error, span)),
            };
        }
        // A registered **higher-order** function (higher-order-abi H0) dispatches through the
        // `NativeCtx` seam: opaque slots + backend re-entry instead of marshalled values. Checked
        // after the plain table — plain functions vastly outnumber ctx ones, and the two name
        // sets are disjoint, so order is behavior-neutral and keeps the common path lean.
        // (The last per-backend intercept — `vec`'s bulk `*_all` kernels — died with the N3.4
        // raw-buffer seam: they are ordinary ctx functions now, reached right here.)
        if reg.find_ctx_function(module, func).is_some() {
            return self.call_ctx_function(module, func, args, span);
        }
        let error = noeta_stdlib::no_function_error(name, func);
        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
    }

    /// Dispatch a method on an extern-type receiver (extern-types X1) through its registered
    /// [`noeta_stdlib::ExtType`]'s shared dispatch — project the arguments, run the one shared
    /// body (host threaded in, receiver borrowed mutably), materialize the result. Mirrors the
    /// VM's `call_extern_method`, so the two backends agree by construction.
    fn call_extern_method(
        &mut self,
        cell: &Rc<RefCell<noeta_stdlib::ExternBox>>,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        // A type's **higher-order** methods (higher-order-abi H4) route through the ctx seam —
        // they call closures back and reach the retained arena, which the plain by-value
        // dispatch below cannot. Name sets are disjoint, so routing is per-method.
        let identity = cell.borrow().type_identity();
        // Bound once (IR3): `&'static`, so it survives the `&mut self` host borrow below.
        let reg = self.reg();
        if reg.find_type_ctx_method(identity, name).is_some() {
            let recv = Value::Extern(Rc::clone(cell));
            return self.call_ctx_type_method(identity, recv, name, args, span);
        }
        // A type declaring `deep_marshal` (the metrics instruments' `*_with(_, attrs)`) projects a
        // container argument to a full `NativeValue` tree; every other type uses the shallow
        // projection — mirrors the VM's `call_extern_method`.
        let deep = reg
            .find_type_qualified(identity)
            .is_some_and(|t| t.deep_marshal);
        let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
            args.iter().map(value_to_native_deep).collect()
        } else {
            args.iter().map(|a| marshal_native_arg(a, reg)).collect()
        };
        // `cell` is an independent `Rc`, so borrowing it and `self.host` at once is fine (the
        // FileHandle discipline).
        let result = reg.dispatch_method(&mut **cell.borrow_mut(), name, &mut *self.host, &nargs);
        match result {
            // Async WORK from an extern-type method (e.g. `Process.wait_async`, process-signals
            // arc): ticket the descriptor on the executor and hand back the async-IO future —
            // mirrors the module-function path in `call_std_function` and the VM.
            Ok(noeta_stdlib::NativeOut::Spawn(spawn)) => {
                let id = self.executor.spawn_ext(&mut *self.host, spawn.0);
                Ok(Value::AsyncIo(id))
            }
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.runtime_error(std_error_code(error.kind), span, error.message)),
        }
    }

    /// Dispatch a **native class**'s instance method (native-extensibility S3 / Pass 2a) — the
    /// [`ExtClass`] analogue of [`Self::call_extern_method`]. The receiver is a class-kind object;
    /// it crosses to the native `dispatch` as the whole instance marshalled to a
    /// [`NativeValue::Instance`] (its fields by name), the same shape a class value takes arg-IN, so
    /// the method reads a field off it. Host threaded in, result materialized — mirrors the VM's
    /// `call_native_class_method`, so the two backends agree by construction.
    fn call_native_class_method(
        &mut self,
        recv: &Value,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        // Bound once (IR3): `&'static`, so it survives the `&mut self` host borrow below.
        let reg = self.reg();
        // Resolve over BOTH classes and structs (fielded unification) — a value-struct method
        // dispatches through the same seam; only in-place mutation (`InstanceUpdate`) is class-only.
        let class = match recv {
            Value::Object(obj) => reg.resolve_fielded(obj.def.name()),
            _ => None,
        };
        let Some(class) = class else {
            return Err(self.runtime_error(
                DiagnosticCode::UnknownName,
                span,
                format!("no native fielded-type method `{name}`"),
            ));
        };
        let recv_native = marshal_native_arg(recv, reg);
        let nargs: Vec<noeta_stdlib::NativeValue> =
            args.iter().map(|a| marshal_native_arg(a, reg)).collect();
        match (class.dispatch)(&recv_native, name, &mut *self.host, &nargs) {
            // A **struct** (value type) has no in-place mutation: reject an `InstanceUpdate` from a
            // struct dispatch as a runtime error rather than silently mutating a value. Mirrors the
            // VM's guard, so both backends agree.
            Ok(noeta_stdlib::NativeOut::InstanceUpdate { .. })
                if class.kind == noeta_stdlib::FieldedKind::Struct =>
            {
                Err(self.runtime_error(
                    DiagnosticCode::ImmutableField,
                    span,
                    format!(
                        "native struct method `{name}` returned an in-place mutation, but a struct \
                         `{}` is a value type — return a new value instead",
                        class.name
                    ),
                ))
            }
            // Boundary 1: an in-place instance mutation (class only). Apply each write to the LIVE
            // receiver's slot (in-place, so aliases see it; the displaced value drops → its
            // destructor fires), then materialize the method's own `ret`. Mirrors the VM's
            // `call_native_class_method`.
            Ok(noeta_stdlib::NativeOut::InstanceUpdate { writes, ret }) => {
                let obj = match recv {
                    Value::Object(obj) => obj,
                    // Unreachable: `class` resolved from this receiver's shape above.
                    _ => unreachable!("a native class method's receiver is a class object"),
                };
                for (field, value) in writes {
                    // A write must target a declared `mut` field — the ABI mirrors the source-level
                    // E0022-family rule; an unknown or non-`mut` field is a runtime error.
                    match class.fields.iter().find(|f| f.name == field) {
                        Some(spec) if spec.is_mut => {}
                        Some(_) => {
                            return Err(self.runtime_error(
                                DiagnosticCode::ImmutableField,
                                span,
                                format!(
                                    "native method `{name}` cannot write immutable field `{field}` \
                                     of class `{}`",
                                    class.name
                                ),
                            ));
                        }
                        None => {
                            return Err(self.runtime_error(
                                DiagnosticCode::UnknownName,
                                span,
                                format!(
                                    "native method `{name}` writes unknown field `{field}` of \
                                     class `{}`",
                                    class.name
                                ),
                            ));
                        }
                    }
                    // In-place overwrite; the displaced old value drops here (its `Drop`/destructor
                    // fires), so swapping a native-state handle releases the prior resource.
                    let _old = obj.set_field_value(&field, materialize_native(value));
                }
                Ok(materialize_native(*ret))
            }
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.runtime_error(std_error_code(error.kind), span, error.message)),
        }
    }

    /// Dispatch a **native enum**'s instance method (native-extensibility S1 / Slice B) — the
    /// [`ExtEnum`] analogue of [`Self::call_native_class_method`], reusing the shared
    /// [`NativeMethodDispatch`] seam. The receiver is an enum value; it crosses to the native
    /// `dispatch` as a [`NativeValue::Variant`] (its case + declaration index + positional payload),
    /// the same shape an enum value takes arg-IN, so the method reads its payload off it. Host
    /// threaded in, result materialized — mirrors the VM's `call_native_enum_method`, so the two
    /// backends agree by construction. An enum is an **immutable value type**: a dispatch returning
    /// [`NativeOut::InstanceUpdate`] is a runtime error, exactly as it is for a value struct.
    fn call_native_enum_method(
        &mut self,
        recv: &Value,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        // Bound once (IR3): `&'static`, so it survives the `&mut self` host borrow below.
        let reg = self.reg();
        let en = match recv {
            Value::Enum(e) => reg.resolve_enum(&e.enum_name),
            _ => None,
        };
        let Some(en) = en else {
            return Err(self.runtime_error(
                DiagnosticCode::UnknownName,
                span,
                format!("no native enum method `{name}`"),
            ));
        };
        let recv_native = marshal_native_arg(recv, reg);
        let nargs: Vec<noeta_stdlib::NativeValue> =
            args.iter().map(|a| marshal_native_arg(a, reg)).collect();
        match (en.dispatch)(&recv_native, name, &mut *self.host, &nargs) {
            // An enum is a value type — it has no in-place mutation. Reject an `InstanceUpdate` from
            // an enum dispatch as a runtime error rather than silently mutating a value, mirroring
            // the struct guard in `call_native_class_method` (and the VM's enum guard).
            Ok(noeta_stdlib::NativeOut::InstanceUpdate { .. }) => Err(self.runtime_error(
                DiagnosticCode::ImmutableField,
                span,
                format!(
                    "native enum method `{name}` returned an in-place mutation, but an enum `{}` is \
                     an immutable value type — return a new value instead",
                    en.name
                ),
            )),
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.runtime_error(std_error_code(error.kind), span, error.message)),
        }
    }

    /// Dispatch an iterator method (Track I). `next`/`collect`/`count` consume the cursor;
    /// `take`/`drop`/`chain` wrap this iterator in a new adapter. Mirrors the VM's `call_iter_method`.
    fn call_iter_method(
        &mut self,
        method: noeta_stdlib::IterMethod,
        state: &Rc<RefCell<IterState>>,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        use noeta_stdlib::IterMethod as M;
        // This iterator as a value, to use as an adapter's source.
        let source = || Value::Iter(Rc::clone(state));
        let wrap = |inner| Value::Iter(Rc::new(RefCell::new(inner)));
        match method {
            M::Next => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(match self.iter_advance(state, span)? {
                    Some(v) => builtin_enum("Option", "some", vec![v]),
                    None => builtin_enum("Option", "none", Vec::new()),
                })
            }
            M::Collect => {
                self.expect_std_arity(name, args, 0, span)?;
                let mut out = Vec::new();
                while let Some(v) = self.iter_advance(state, span)? {
                    out.push(v);
                }
                Ok(Value::list(out))
            }
            M::Count => {
                self.expect_std_arity(name, args, 0, span)?;
                let mut n = 0i64;
                while self.iter_advance(state, span)?.is_some() {
                    n += 1;
                }
                Ok(Value::Int(n))
            }
            M::Take | M::Drop => {
                self.expect_std_arity(name, args, 1, span)?;
                let n = self.expect_std_int(name, &args[0], span)?.max(0) as usize;
                Ok(wrap(if method == M::Take {
                    IterState::Take {
                        source: source(),
                        remaining: n,
                    }
                } else {
                    IterState::Drop {
                        source: source(),
                        pending: n,
                    }
                }))
            }
            M::Chain => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(wrap(IterState::Chain {
                    first: source(),
                    second: args[0].clone(),
                }))
            }
            M::Enumerate => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(wrap(IterState::Enumerate {
                    source: source(),
                    index: 0,
                }))
            }
            M::Zip => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(wrap(IterState::Zip {
                    a: source(),
                    b: args[0].clone(),
                }))
            }
            M::Map => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(wrap(IterState::Map {
                    source: source(),
                    func: args[0].clone(),
                }))
            }
            M::Filter => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(wrap(IterState::Filter {
                    source: source(),
                    pred: args[0].clone(),
                }))
            }
            M::Sum => {
                self.expect_std_arity(name, args, 0, span)?;
                // A directly list-backed iterator (`xs.iter().sum()`, the canonical form) delegates to
                // the eager list reduction over its remaining elements — so a packed narrow-width list
                // folds its buffer and width-wraps *identically* to `xs.sum()` (no divergence). An
                // adapter chain (`take`/`map`/…) falls through to the generic fold below, where the
                // element type is already a 64-bit `int`/`float`, so no width-wrapping is at stake.
                let direct = match &*state.borrow() {
                    IterState::List {
                        list: Value::List(repr),
                        cursor,
                    } => Some((repr.clone(), *cursor)),
                    _ => None,
                };
                if let Some((repr, cursor)) = direct {
                    if let IterState::List { cursor, .. } = &mut *state.borrow_mut() {
                        *cursor = repr.len(); // drain
                    }
                    return self.call_list_reduction_from(&repr, "sum", cursor, span);
                }
                // A narrow-width source (`xs.iter().take(k)` over a `List<i32>`, …): the generic fold
                // accumulates at 64 bits, so mask the integer total back to the element width at the
                // end — the same wrap `xs.sum()` applies — so a narrow-typed iterator reduction agrees
                // (array-ops arc). Traced through the width-preserving adapters only.
                let narrow = iter_narrow_bits(&Value::Iter(Rc::clone(state)));
                let mut int_total: i64 = 0;
                let mut float_total: f64 = 0.0;
                let mut any_float = false;
                while let Some(e) = self.iter_advance(state, span)? {
                    match e {
                        Value::Int(i) => int_total = int_total.wrapping_add(i),
                        Value::Float(f) => {
                            any_float = true;
                            float_total += f;
                        }
                        other => {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!(
                                    "`sum` expects numeric elements, found {}",
                                    other.type_name()
                                ),
                            ));
                        }
                    }
                }
                Ok(if any_float {
                    Value::Float(float_total + int_total as f64)
                } else if let Some((signed, bits)) = narrow {
                    Value::Int(noeta_stdlib::mask_to_width(int_total, signed, bits))
                } else {
                    Value::Int(int_total)
                })
            }
        }
    }

    /// A Ring 1 map method (`keys`/`values`/`has`). Mirrors the VM's `call_map_method`.
    fn call_map_method(
        &mut self,
        method: noeta_stdlib::MapMethod,
        entries: &BTreeMap<noeta_stdlib::MapKey, Value>,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match method {
            noeta_stdlib::MapMethod::Keys => {
                self.expect_std_arity(name, args, 0, span)?;
                // A string key becomes a fresh string value; an extern key a fresh extern value.
                let keys = entries
                    .keys()
                    .map(|k| match k {
                        noeta_stdlib::MapKey::Str(s) => Value::Str(s.as_str().to_owned()),
                        noeta_stdlib::MapKey::Int(i) => Value::Int(*i),
                        noeta_stdlib::MapKey::Extern(e) => {
                            Value::Extern(Rc::new(RefCell::new(e.clone())))
                        }
                        // P-PKEY: rebuild the packed struct value from the content snapshot.
                        noeta_stdlib::MapKey::Packed(p) => {
                            self.packed_key_value(&p.type_name, &p.fields)
                        }
                    })
                    .collect();
                Ok(Value::list(keys))
            }
            noeta_stdlib::MapMethod::Values => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(Value::list(entries.values().cloned().collect()))
            }
            noeta_stdlib::MapMethod::Has => {
                self.expect_std_arity(name, args, 1, span)?;
                let key = self.expect_map_key(name, &args[0], span)?;
                Ok(Value::Bool(entries.contains_key(&key)))
            }
            noeta_stdlib::MapMethod::Set => {
                self.expect_std_arity(name, args, 2, span)?;
                let key = self.expect_map_key(name, &args[0], span)?;
                let mut new = entries.clone();
                new.insert(key, args[1].clone());
                Ok(Value::map_value(Rc::new(new)))
            }
            noeta_stdlib::MapMethod::Remove => {
                self.expect_std_arity(name, args, 1, span)?;
                let key = self.expect_map_key(name, &args[0], span)?;
                let mut new = entries.clone();
                new.remove(&key);
                Ok(Value::map_value(Rc::new(new)))
            }
            noeta_stdlib::MapMethod::GetOr => {
                self.expect_std_arity(name, args, 2, span)?;
                let key = self.expect_map_key(name, &args[0], span)?;
                Ok(entries
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| args[1].clone()))
            }
            noeta_stdlib::MapMethod::Get => {
                self.expect_std_arity(name, args, 1, span)?;
                let key = self.expect_map_key(name, &args[0], span)?;
                Ok(match entries.get(&key) {
                    Some(value) => builtin_enum("Option", "some", vec![value.clone()]),
                    None => builtin_enum("Option", "none", Vec::new()),
                })
            }
        }
    }

    /// Read a map-key argument (string or key-capable extern), raising the shared map-key error
    /// Rebuild a packed key's struct value (P-PKEY) — the `keys()` direction, mirroring the
    /// VM's `packed_key_value`. The key's content snapshot carries the field values; the
    /// `TypeDef` comes from the global scope by name (types are top-level, and a key of type
    /// `T` implies `T` is declared).
    fn packed_key_value(&self, type_name: &str, fields: &[noeta_stdlib::PackedKeyField]) -> Value {
        let Some(Value::Type(def)) = self.globals.lookup(type_name) else {
            panic!("packed key type `{type_name}` must be declared");
        };
        let slots = fields
            .iter()
            .map(|f| match f {
                noeta_stdlib::PackedKeyField::Int(i) => Value::Int(*i),
                noeta_stdlib::PackedKeyField::Bool(b) => Value::Bool(*b),
                noeta_stdlib::PackedKeyField::Struct(name, inner) => {
                    self.packed_key_value(name, inner)
                }
            })
            .collect();
        Value::Object(Rc::new(ObjectValue::new(def, slots)))
    }

    /// otherwise. Mirrors the VM's `map_update_key`/`map_probe` gate.
    fn expect_map_key(
        &mut self,
        _name: &str,
        value: &Value,
        span: Span,
    ) -> Eval<noeta_stdlib::MapKey> {
        match value_map_key(value) {
            Some(key) => Ok(key),
            None => {
                let error = noeta_stdlib::map_key::map_key_error(value.type_name());
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Enforce a collection method's arity, raising the shared `noeta-stdlib` arity error.
    fn expect_std_arity(
        &mut self,
        name: &str,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Eval<()> {
        if args.len() == expected {
            Ok(())
        } else {
            let error = noeta_stdlib::arity_error(name, expected, args.len());
            Err(self.runtime_error(std_error_code(error.kind), span, error.message))
        }
    }

    /// Accept `min..=max` arguments — a collection method with a trailing-optional parameter
    /// (`slice(start, end?)`, `join(sep?)`). Mirrors the VM's `stdlib_arity_range`.
    fn expect_std_arity_range(
        &mut self,
        name: &str,
        args: &[Value],
        min: usize,
        max: usize,
        span: Span,
    ) -> Eval<()> {
        if (min..=max).contains(&args.len()) {
            Ok(())
        } else {
            let error = noeta_stdlib::arity_error(name, max, args.len());
            Err(self.runtime_error(std_error_code(error.kind), span, error.message))
        }
    }

    /// Read an **optional** int argument at `index`, falling back to `default` when absent — the
    /// trailing-optional-parameter reader (`slice`'s `end?`). Mirrors the VM's `stdlib_opt_int`.
    fn expect_std_opt_int(
        &mut self,
        name: &str,
        args: &[Value],
        index: usize,
        default: i64,
        span: Span,
    ) -> Eval<i64> {
        match args.get(index) {
            None => Ok(default),
            Some(value) => self.expect_std_int(name, value, span),
        }
    }

    /// Read a string argument for a collection method, raising the shared `noeta-stdlib` type error.
    fn expect_std_string<'a>(&mut self, name: &str, value: &'a Value, span: Span) -> Eval<&'a str> {
        match value {
            Value::Str(s) => Ok(s),
            _ => {
                let error = noeta_stdlib::type_error(name, "string");
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Read an int argument for a collection method, raising the shared `noeta-stdlib` type error.
    fn expect_std_int(&mut self, name: &str, value: &Value, span: Span) -> Eval<i64> {
        match value {
            Value::Int(i) => Ok(*i),
            _ => {
                let error = noeta_stdlib::type_error(name, "int");
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Render a value for `echo` / string interpolation. A user object that implements the
    /// `Display` trait (defines `to_string`) renders through that method; every other value
    /// uses the structural `Value::display`. This is the one place `to_string` is consulted, so
    /// the same object renders identically whether echoed or interpolated.
    fn display_value(&mut self, value: &Value, span: Span) -> Eval<String> {
        if let Value::Object(object) = value
            && object.def.methods.contains_key("to_string")
        {
            let rendered = self.call_method(value.clone(), "to_string", Vec::new(), span)?;
            return Ok(rendered.display());
        }
        // An enum value with an `impl Display { to_string }` renders through it too (object-model
        // slice 3) — resolved through the `EnumDef` in scope, since the value carries no table.
        if let Value::Enum(e) = value
            && let Some(Value::EnumType(def)) = self.scope.lookup(&e.enum_name)
            && def.method("to_string").is_some()
        {
            let rendered = self.call_method(value.clone(), "to_string", Vec::new(), span)?;
            return Ok(rendered.display());
        }
        Ok(value.display())
    }

    /// Positional tuple projection `receiver.N` (object-model slice 4), shared by both eval
    /// backends. The index is in range by construction (the checker verified it against the tuple's
    /// arity); a non-tuple receiver or an out-of-range index is a runtime error for robustness.
    fn tuple_index(&mut self, receiver: Value, index: u32, span: Span) -> Eval<Value> {
        match &receiver {
            Value::Tuple(items) => match items.get(index as usize) {
                Some(value) => Ok(value.clone()),
                None => Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "tuple index `{index}` is out of range for a {}-tuple",
                        items.len()
                    ),
                )),
            },
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "cannot apply tuple index `.{index}` to {}",
                    other.type_name()
                ),
            )),
        }
    }

    /// Evaluate `receiver[index]`. A user object dispatches through its `Index` trait
    /// (`receiver.get(index)`); a built-in list addresses an element by integer position
    /// (bounds-checked). Any other receiver is not indexable.
    fn eval_index(&mut self, receiver: Value, index: Value, span: Span) -> Eval<Value> {
        match &receiver {
            // `o[i]` on a user object lights up the `Index` trait: dispatch to `get`. An object
            // without an `Index` impl has no `get` method, so this reports the missing method.
            Value::Object(_) => self.call_method(receiver, "get", vec![index], span),
            Value::List(items) => {
                let Value::Int(i) = index else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("list index must be an int, found {}", index.type_name()),
                    ));
                };
                if i < 0 || i as usize >= items.len() {
                    return Err(self.runtime_error(
                        DiagnosticCode::IndexOutOfBounds,
                        span,
                        format!("index {i} out of bounds for list of length {}", items.len()),
                    ));
                }
                Ok(items.get(i as usize).expect("bounds checked above"))
            }
            // `m[k]` on a map looks the value up by its key — a string, or a key-capable
            // extern value (extern-types X4); a missing key is `E0018`.
            Value::Map(entries, _) => {
                let Some(key) = value_map_key(&index) else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("map index must be a string, found {}", index.type_name()),
                    ));
                };
                match entries.get(&key) {
                    Some(value) => Ok(value.clone()),
                    None => Err(self.runtime_error(
                        DiagnosticCode::KeyNotFound,
                        span,
                        format!("map has no key {}", key.render()),
                    )),
                }
            }
            // `s[i]` on a string addresses a single character by position (bounds-checked),
            // counting by Unicode scalar values to match `len`.
            Value::Str(s) => {
                let Value::Int(i) = index else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("string index must be an int, found {}", index.type_name()),
                    ));
                };
                let count = s.chars().count();
                if i < 0 || i as usize >= count {
                    return Err(self.runtime_error(
                        DiagnosticCode::IndexOutOfBounds,
                        span,
                        format!("index {i} out of bounds for string of length {count}"),
                    ));
                }
                Ok(Value::Str(s.chars().nth(i as usize).unwrap().to_string()))
            }
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("cannot index a value of type {}", other.type_name()),
            )),
        }
    }

    /// Call a value whose arguments may skip a defaulted parameter (`f(1, c: 9)`), per the
    /// supplied-mask the checker recorded and lowering carried. Only a user closure can be masked
    /// — nothing else has defaulted parameters — so every other callee ignores it, and a mask
    /// reaching one is a lowering bug rather than a user error.
    fn call_masked(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        span: Span,
        supplied: Option<u64>,
    ) -> Eval<Value> {
        match (&callee, supplied) {
            (Value::Function(closure), Some(_)) => {
                let closure = Rc::clone(closure);
                self.call_closure_masked(&closure, args, span, supplied)
            }
            _ => self.call(callee, args, span),
        }
    }

    fn call(&mut self, callee: Value, args: Vec<Value>, span: Span) -> Eval<Value> {
        match callee {
            Value::Builtin(builtin) => self.call_builtin(builtin, args, span),
            // A selectively-imported module function (`use std.math.sqrt`) called by its bare name —
            // dispatched exactly like the `math.sqrt(...)` member call.
            Value::ModuleFn(module, func) => self.call_native_module(&module, &func, &args, span),
            // An unbound method handle (`Type.method` as a value). An associated handle dispatches
            // on the named type (`ty.method(args)`); an instance handle takes its first argument as
            // the receiver (`recv.method(rest)`). Both route through the ordinary (total) method call.
            Value::MethodHandle(ty, method, associated) => {
                if associated {
                    match self.scope.lookup(&ty) {
                        Some(type_value) => self.call_method(type_value, &method, args, span),
                        None => Err(self.runtime_error(
                            DiagnosticCode::UnknownName,
                            span,
                            format!("cannot find type `{ty}` for method handle `{ty}.{method}`"),
                        )),
                    }
                } else {
                    let mut args = args;
                    if args.is_empty() {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!("method handle `{ty}.{method}` needs a receiver argument"),
                        ));
                    }
                    let recv = args.remove(0);
                    self.call_method(recv, &method, args, span)
                }
            }
            // A bound handle (`f = x.method`, EX.2b): dispatch the method on the captured receiver.
            Value::BoundMethod(recv, method) => self.call_method(*recv, &method, args, span),
            Value::Function(closure) => self.call_closure(&closure, args, span),
            other => {
                // The **`Callable` protocol**: an object (or enum value) invoked as a value —
                // `obj(args)` dispatches to its `call` METHOD, the protocol's required method.
                // Structural at runtime like the other protocol dispatches (`iter`, `to_string`):
                // the method table is what is consulted, and `impl Callable { fn call(...) }` is
                // the validated way to populate it. Deliberately method-only — a closure-valued
                // FIELD named `call` does not make the object invocable (that is member-call
                // territory: `obj.call(args)` reaches it) — so both backends gate identically.
                // An **extern** value participates in the protocol too (http arc H10): a
                // registered `call` method makes it invocable, routed through ordinary extern
                // method dispatch. Mirrors the VM, so both backends gate identically. (The
                // field-vs-method rule described above governs the `Value::Object` arm below.)
                if let Value::Extern(cell) = &other
                    && self
                        .reg()
                        .find_type_method_sig(cell.borrow().type_identity(), "call")
                        .is_some()
                {
                    return self.call_method(other, "call", args, span);
                }
                if matches!(&other, Value::Object(o) if o.def.methods.contains_key("call")) {
                    return self.call_method(other, "call", args, span);
                }
                if let Value::Enum(e) = &other
                    && let Some(Value::EnumType(def)) = self.scope.lookup(&e.enum_name)
                    && def.method("call").is_some()
                {
                    return self.call_method(other, "call", args, span);
                }
                Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("{} is not callable", other.type_name()),
                ))
            }
        }
    }

    /// Bind a call's arguments, and every parameter it left out, into `call_scope`.
    ///
    /// The one statement of the argument→parameter rule on this backend: an argument binds the
    /// parameter it was bound to at check time (its own position without a mask, the `i`-th
    /// supplied parameter with one), and every parameter the call did not fill takes its default,
    /// evaluated in the callee's captured (definition/global) scope — never seeing the call's other
    /// arguments. `supplied` is indexed over the callee's DECLARED parameters; a receiver, where
    /// there is one, binds as `self` and takes no parameter slot.
    fn bind_call_scope(
        &mut self,
        callee: &Rc<Closure>,
        args: Vec<Value>,
        supplied: Option<u64>,
        call_scope: &Scope,
    ) -> Eval<()> {
        let n_args = args.len();
        for (i, arg) in args.into_iter().enumerate() {
            let p = noeta_bytecode::param_of_arg(i, supplied);
            call_scope.declare(callee.params[p].clone(), arg, false);
        }
        for i in 0..callee.params.len() {
            if noeta_bytecode::is_param_filled(i, n_args, supplied) {
                continue;
            }
            let value = self.eval_default(callee, i)?;
            call_scope.declare(callee.params[i].clone(), value, false);
        }
        Ok(())
    }

    fn call_closure(&mut self, closure: &Rc<Closure>, args: Vec<Value>, span: Span) -> Eval<Value> {
        self.call_closure_masked(closure, args, span, None)
    }

    fn call_closure_masked(
        &mut self,
        closure: &Rc<Closure>,
        args: Vec<Value>,
        span: Span,
        supplied: Option<u64>,
    ) -> Eval<Value> {
        let required = required_count(&closure.defaults);
        if args.len() < required || args.len() > closure.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("function", required, closure.params.len(), args.len()),
            ));
        }
        // Safepoint-GC poll at the call boundary (memory-management 6.x) — with the loop
        // polls, this bounds a recursion-driven cycle builder too.
        cycles::poll_safepoint();
        let call_scope = match &closure.body.captures {
            Some(allow) => Scope::sealed_child(&closure.captured, allow),
            None => Scope::child(&closure.captured),
        };
        self.bind_call_scope(closure, args, supplied, &call_scope)?;
        // Shadow the call for the abort traceback: (callee name, call-site span). Popped on every
        // exit — an abort's trace is snapshotted deeper, at the diagnostic (see `record_abort_trace`).
        self.call_sites.push((closure.name.clone(), span));
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = self.exec_ir_fn_body(&closure.body);
        self.scope = saved;
        self.call_sites.pop();
        catch_return(result)
    }

    /// Call an instance method: `self` binds to the whole object; fields are NOT in scope
    /// (member access is explicit — prelude-redesign EX.1). Parameters bind after `self`.
    fn call_method_on(
        &mut self,
        object: &Rc<ObjectValue>,
        method: &Rc<Closure>,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        self.call_method_on_masked(object, method, args, span, None)
    }

    fn call_method_on_masked(
        &mut self,
        object: &Rc<ObjectValue>,
        method: &Rc<Closure>,
        args: Vec<Value>,
        span: Span,
        supplied: Option<u64>,
    ) -> Eval<Value> {
        let required = required_count(&method.defaults);
        if args.len() < required || args.len() > method.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("method", required, method.params.len(), args.len()),
            ));
        }
        // Safepoint-GC poll at the call boundary — see `call_closure`.
        cycles::poll_safepoint();
        let call_scope = match &method.body.captures {
            Some(allow) => Scope::sealed_child(&method.captured, allow),
            None => Scope::child(&method.captured),
        };
        // Bind only `self` — fields are **not** snapshotted into the scope. A bare field read
        // resolves live off `self` (see `eval_ir_atom`), mirroring the VM (which loads fields off the
        // receiver register, never a copy), so a field mutated mid-method — including through an alias
        // — is observed by a later bare read. A bare *write* `n = v` therefore declares a local (the
        // name is not in scope); mutating a field is the explicit `self.f = v`.
        call_scope.declare("self".to_string(), Value::Object(Rc::clone(object)), false);
        self.bind_call_scope(method, args, supplied, &call_scope)?;
        // Shadowed for the abort traceback, exactly as `call_closure` (popped on every exit).
        self.call_sites.push((method.name.clone(), span));
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = self.exec_ir_fn_body(&method.body);
        self.scope = saved;
        self.call_sites.pop();
        catch_return(result)
    }

    /// Call an enum instance method (object-model slice 3): `self` binds to the whole enum value
    /// (there is no implicit per-field scope — an enum's variants carry different data, so a method
    /// reaches its payload by `match`ing on `self`). Otherwise identical to [`Self::call_method_on`]:
    /// parameters bind after `self`, omitted trailing ones take their defaults in the captured scope.
    fn call_enum_method(
        &mut self,
        receiver: Value,
        method: &Rc<Closure>,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        self.call_enum_method_masked(receiver, method, args, span, None)
    }

    fn call_enum_method_masked(
        &mut self,
        receiver: Value,
        method: &Rc<Closure>,
        args: Vec<Value>,
        span: Span,
        supplied: Option<u64>,
    ) -> Eval<Value> {
        let required = required_count(&method.defaults);
        if args.len() < required || args.len() > method.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("method", required, method.params.len(), args.len()),
            ));
        }
        let call_scope = match &method.body.captures {
            Some(allow) => Scope::sealed_child(&method.captured, allow),
            None => Scope::child(&method.captured),
        };
        call_scope.declare("self".to_string(), receiver, false);
        self.bind_call_scope(method, args, supplied, &call_scope)?;
        // Shadowed for the abort traceback, exactly as `call_closure` (popped on every exit).
        self.call_sites.push((method.name.clone(), span));
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = self.exec_ir_fn_body(&method.body);
        self.scope = saved;
        self.call_sites.pop();
        catch_return(result)
    }

    /// Evaluate parameter `i`'s default value in `closure`'s captured (definition/global) scope.
    /// Isolating it there is what keeps a default from reaching the call's other arguments, the
    /// receiver's fields, or `self` — so the tree-walker and the VM (whose default thunks run
    /// against globals only) compute the same value.
    fn eval_default(&mut self, closure: &Rc<Closure>, i: usize) -> Eval<Value> {
        let default = closure.defaults[i]
            .as_ref()
            .expect("an omitted argument must correspond to a defaulted parameter");
        let scope = Scope::child(&closure.captured);
        let saved = std::mem::replace(&mut self.scope, scope);
        let result = self.exec_ir_thunk(default);
        self.scope = saved;
        result
    }

    /// Run a field's default-value thunk (object-model slice 5) to its value, in the type's
    /// **definition scope** — a fresh child of the global scope, so the default resolves globals
    /// only and never sees the construction site's locals, `self`, or sibling fields. Mirrors
    /// [`Self::eval_default`] for parameters, but rooted at `globals` (types are always top-level)
    /// rather than a closure's captured scope.
    fn run_field_default(&mut self, thunk: &noeta_ir::Thunk) -> Eval<Value> {
        let scope = Scope::child(&self.globals);
        let saved = std::mem::replace(&mut self.scope, scope);
        let result = self.exec_ir_thunk(thunk);
        self.scope = saved;
        result
    }

    fn call_builtin(&mut self, builtin: Builtin, args: Vec<Value>, span: Span) -> Eval<Value> {
        match builtin {
            Builtin::Len => {
                self.expect_arity(builtin, &args, 1, span)?;
                match &args[0] {
                    Value::List(items) => Ok(Value::Int(items.len() as i64)),
                    Value::Set(items, _) => Ok(Value::Int(items.len() as i64)),
                    Value::Map(entries, _) => Ok(Value::Int(entries.len() as i64)),
                    Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                    // A user object lights up the `Length` trait: `len(o)` dispatches to its
                    // `len` method (an object without a `Length` impl has no `len` method).
                    Value::Object(object) if object.def.methods.contains_key("len") => {
                        let receiver = args[0].clone();
                        self.call_method(receiver, "len", Vec::new(), span)
                    }
                    other => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`len` expects a list, map, or string, found {}",
                            other.type_name()
                        ),
                    )),
                }
            }
            Builtin::Map => {
                self.expect_arity(builtin, &args, 2, span)?;
                let items = self.expect_list(&args[0], "map", span)?;
                let function = args[1].clone();
                let mut result = Vec::with_capacity(items.len());
                for item in items.iter() {
                    result.push(self.call(function.clone(), vec![item.clone()], span)?);
                }
                Ok(Value::list(result))
            }
            Builtin::Filter => {
                self.expect_arity(builtin, &args, 2, span)?;
                let items = self.expect_list(&args[0], "filter", span)?;
                let function = args[1].clone();
                let mut result = Vec::new();
                for item in items.iter() {
                    match self.call(function.clone(), vec![item.clone()], span)? {
                        Value::Bool(true) => result.push(item.clone()),
                        Value::Bool(false) => {}
                        other => {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!(
                                    "`filter` predicate must return a bool, found {}",
                                    other.type_name()
                                ),
                            ));
                        }
                    }
                }
                Ok(Value::list(result))
            }
            Builtin::Sum => {
                self.expect_arity(builtin, &args, 1, span)?;
                let items = self.expect_list(&args[0], "sum", span)?;
                self.sum_list(&items, span)
            }
            // `Ok(x)` wraps a value; `Ok()` is the void success used by `Result<void, _>`.
            Builtin::MakeOk => {
                let data = match args.len() {
                    0 => Vec::new(),
                    1 => args,
                    n => {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!("`Ok` takes 0 or 1 argument(s) but {n} were supplied"),
                        ));
                    }
                };
                Ok(builtin_enum("Result", "Ok", data))
            }
            Builtin::MakeErr => {
                self.expect_arity(builtin, &args, 1, span)?;
                Ok(builtin_enum("Result", "Err", args))
            }
            Builtin::MakeSome => {
                self.expect_arity(builtin, &args, 1, span)?;
                Ok(builtin_enum("Option", "some", args))
            }
            // `panic(msg)` is the unrecoverable path: record an `E0010` and unwind. There
            // is no `catch`; it stops the program with a nonzero exit.
            Builtin::Panic => {
                self.expect_arity(builtin, &args, 1, span)?;
                let message = args[0].display();
                Err(self.runtime_error(DiagnosticCode::Panic, span, format!("panic: {message}")))
            }
            // `assert(cond)` / `assert(cond, msg)` — the test runner's failure signal. A false
            // condition aborts with the same `Panic` diagnostic `panic` raises (the runner reads a
            // nonzero exit as a failed test); a true condition yields unit and falls through. The
            // condition must be `bool` (no general truthiness), so a non-bool is a `TypeMismatch` —
            // both checked identically here and in the VM so the differential agrees.
            Builtin::Assert => {
                if args.len() != 1 && args.len() != 2 {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`assert` expects 1 or 2 arguments, found {}", args.len()),
                    ));
                }
                let cond = match &args[0] {
                    Value::Bool(b) => *b,
                    other => {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!("`assert` expects a bool, found {}", other.display()),
                        ));
                    }
                };
                if cond {
                    Ok(Value::Unit)
                } else {
                    let message = match args.get(1) {
                        Some(msg) => format!("assertion failed: {}", msg.display()),
                        None => "assertion failed".to_string(),
                    };
                    Err(self.runtime_error(DiagnosticCode::Panic, span, message))
                }
            } // (The whole `Builtin` orchestration family — `task` at higher-order-abi H0/H2,
              // `http.serve` at H3, `signal`/`computed`/`effect` at H5 — migrated to the
              // registry's `NativeCtx` dispatch: `noeta-stdlib/src/{task,serve,reactive}.rs`;
              // the drive loops there are shared with the VM.)
        }
    }

    /// Poll a future once (Track A.3 — the tree-walker twin of the VM's `Vm::poll_once`). A leaf timer
    /// is ready once the executor clock reaches its deadline, else it registers the deadline and reports
    /// `None` (pending). A step future's poll runs the state machine to its next suspend: the step
    /// returns the raw completion value (ready) or the pending sentinel (`None`). A non-future passes
    /// through as ready (totality for the uncheck­ed property test).
    ///
    /// The thin outer layer is the **traced-future hook** (native-otel T5c), the VM's mirrored:
    /// a future registered via `NativeCtx::trace_future` polls under its own saved context and,
    /// on completion or abort, has its telemetry span ended here.
    fn poll_once(&mut self, future: &Value, span: Span) -> Eval<Option<Value>> {
        if self.traced_futures.is_empty() {
            return self.poll_once_inner(future, span);
        }
        let Some(idx) = self
            .traced_futures
            .iter()
            .position(|t| traced_same(&t.future, future))
        else {
            return self.poll_once_inner(future, span);
        };
        let ctx = std::mem::take(&mut self.traced_futures[idx].context);
        let saved = std::mem::replace(&mut self.ctx_current, ctx);
        let polled = self.poll_once_inner(future, span);
        let ctx = std::mem::replace(&mut self.ctx_current, saved);
        // Re-find by identity: a nested poll may have completed another traced future
        // (`swap_remove` moves entries), so `idx` cannot be trusted across the poll.
        if let Some(idx) = self
            .traced_futures
            .iter()
            .position(|t| traced_same(&t.future, future))
        {
            match &polled {
                Ok(None) => self.traced_futures[idx].context = ctx,
                Ok(Some(_)) | Err(_) => {
                    let traced = self.traced_futures.swap_remove(idx);
                    if polled.is_err() {
                        self.host.tel_span_set_status(
                            traced.span,
                            noeta_stdlib::SpanStatus::Error("span body aborted".into()),
                        );
                    }
                    self.host.tel_span_end(traced.span);
                }
            }
        }
        polled
    }

    /// The sender's trace context to ride an outbound channel message (native-otel T5d) — the
    /// VM's helper, mirrored: `None` (one bool test) when telemetry is off or no span is active.
    fn outbound_trace_context(&mut self) -> Option<noeta_stdlib::TraceContext> {
        if !self.host.tel_enabled() {
            return None;
        }
        let top = *self.ctx_current.last()?;
        Some(self.host.tel_span_context(top))
    }

    /// Seed the receiving strand's context from a dequeued message's (native-otel T5d) — the VM's
    /// helper, mirrored: only when the strand is at top level (empty, or exactly one remote seed,
    /// which is replaced and released); real active spans are never hijacked.
    fn seed_context_from_message(&mut self, context: Option<noeta_stdlib::TraceContext>) {
        let at_top = match self.ctx_current.as_slice() {
            [] => true,
            [only] => self.host.tel_is_remote(*only),
            _ => false,
        };
        if !at_top {
            return;
        }
        if let [old] = self.ctx_current.as_slice() {
            let old = *old;
            self.host.tel_release_remote(old);
            self.ctx_current.clear();
        }
        if let Some(ctx) = context {
            let seed = self.host.tel_intern_remote(ctx);
            self.ctx_current.push(seed);
        }
    }

    fn poll_once_inner(&mut self, future: &Value, span: Span) -> Eval<Option<Value>> {
        match future {
            Value::Timer(deadline) => {
                let deadline = *deadline;
                if self.executor.now() >= deadline {
                    Ok(Some(Value::Unit))
                } else {
                    self.executor.register_timer(deadline);
                    Ok(None)
                }
            }
            Value::Future(step) => {
                let result = self.call((**step).clone(), vec![Value::Unit], span)?;
                if matches!(result, Value::Pending) {
                    Ok(None)
                } else {
                    Ok(Some(result))
                }
            }
            // A task handle (Track A.3b): ready iff its task has a stored result — polling a handle only
            // *reads* the task (the scheduler polls the task itself). A stale handle (its scope popped)
            // reads as ready-unit, defensively (structured use awaits within the scope).
            Value::Handle(si, ti) => {
                let (si, ti) = (*si, *ti);
                match self.scopes.get(si.index()).and_then(|s| s.get(ti.index())) {
                    Some(task) => Ok(task.result.clone()),
                    None => Ok(Some(Value::Unit)),
                }
            }
            // A leaf async-IO future (Track A.4c/A.10): ask the executor whether the request completed.
            // Ready → the outcome as a value (read → `string`, write/append → unit); an IO failure
            // aborts (E0021) at the `.await`, matching the synchronous `fs.*`; pending → `None` (the
            // sandbox always resolves on the first poll).
            Value::AsyncIo(id) => {
                let id = *id;
                match self.executor.poll_ext(id) {
                    // Ready → materialize the descriptor's `NativeOut` exactly like a
                    // synchronous dispatch result (extern-types X5); an IO failure aborts
                    // (E0021) at the `.await`, matching the synchronous `fs.*`.
                    Some(Ok(out)) => Ok(Some(materialize_native(out))),
                    Some(Err(error)) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                    None => Ok(None),
                }
            }
            // `tx.send(v)` (isolates I.1 + I.4c rendezvous): the shared `channel` policy decides the
            // action from the channel's scalar state and this send's rendezvous phase (carried on the
            // future's `Rc<Cell>`, so it persists across the same awaited future's re-polls).
            Value::ChannelSend(id, value, phase_cell) => {
                use noeta_stdlib::channel::{SendAction, SendPhase};
                let id = *id;
                let phase = phase_cell.get();
                let chan = &self.channels[id.index()];
                let action = noeta_stdlib::channel::poll_send(
                    chan.capacity,
                    chan.buffer.len(),
                    chan.closed,
                    phase,
                );
                match action {
                    // Sending on a closed channel is a bug (E0010) — the receiver would never see it.
                    SendAction::Closed => Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        "cannot send on a closed channel".to_string(),
                    )),
                    // Buffered deliver (complete now) or rendezvous deposit (park until taken): the
                    // message enters the one queue either way.
                    SendAction::DeliverBuffered | SendAction::Deposit => {
                        let value = (**value).clone();
                        // The sender's trace context rides the message (T5d) — the VM's envelope,
                        // mirrored.
                        let context = self.outbound_trace_context();
                        self.channels[id.index()].buffer.push_back((value, context));
                        self.channel_progress += 1;
                        if action == SendAction::Deposit {
                            // A rendezvous send parks, recording that its message is now in the
                            // handoff, and completes only once a receiver takes it.
                            phase_cell.set(SendPhase::Deposited);
                            Ok(None)
                        } else {
                            Ok(Some(Value::Unit))
                        }
                    }
                    // Rendezvous: the deposited message has been taken — complete.
                    SendAction::Complete => {
                        self.channel_progress += 1;
                        Ok(Some(Value::Unit))
                    }
                    SendAction::Park => Ok(None),
                }
            }
            // `rx.recv()` (isolates I.1): dequeue the next message (ready → `some(v)`), yield `none`
            // once the channel is closed and drained, else suspend (pending) on an empty open buffer.
            Value::ChannelRecv(id) => {
                let id = *id;
                let chan = &self.channels[id.index()];
                match noeta_stdlib::channel::poll_recv(chan.buffer.len(), chan.closed) {
                    noeta_stdlib::channel::RecvAction::Deliver => {
                        let (value, context) = self.channels[id.index()]
                            .buffer
                            .pop_front()
                            .expect("non-empty");
                        // Seed the receiving strand from the message's context (T5d).
                        self.seed_context_from_message(context);
                        self.channel_progress += 1;
                        Ok(Some(builtin_enum("Option", "some", vec![value])))
                    }
                    noeta_stdlib::channel::RecvAction::ClosedEmpty => {
                        Ok(Some(builtin_enum("Option", "none", vec![])))
                    }
                    noeta_stdlib::channel::RecvAction::Park => Ok(None),
                }
            }
            other => Ok(Some(other.clone())),
        }
    }

    /// Poll every not-yet-complete task in **every open scope** once (Track A.7 — nested-`concurrent`
    /// interleaving), storing any `Ready` results; returns whether any task completed this round.
    /// Polling across all scope levels (not just the innermost) lets an outer scope's spawned siblings
    /// make progress while an inner `concurrent` block is joined. Re-reads the scope/task counts each
    /// step so mid-round `spawn`s are polled the same round; a nested `concurrent` in a task body pushes
    /// and pops its own scope within that task's poll (balanced), so the stack is stable between polls.
    fn poll_all_scopes_round(&mut self, span: Span) -> Eval<bool> {
        let mut completed = false;
        let mut si = 0;
        while si < self.scopes.len() {
            let mut ti = 0;
            while ti < self.scopes[si].len() {
                let task = &self.scopes[si][ti];
                // Skip a task whose step is *currently executing* (`polling`): a nested round — a
                // `concurrent` join inside that task's own body — must not re-enter its
                // mid-execution state machine (that re-runs the current segment: infinite
                // recursion). The VM's guard, mirrored.
                if task.result.is_none() && !task.cancelled && !task.polling {
                    let future = task.future.clone();
                    self.scopes[si][ti].polling = true;
                    // Swap the task's own context in for the duration of its poll (T5a) — the
                    // VM's swap discipline, mirrored: paired swaps nest across re-entrant rounds,
                    // and the `polling` guard keeps each task's pair balanced.
                    let ctx = std::mem::take(&mut self.scopes[si][ti].context);
                    let saved = std::mem::replace(&mut self.ctx_current, ctx);
                    let polled = self.poll_once(&future, span);
                    self.scopes[si][ti].context = std::mem::replace(&mut self.ctx_current, saved);
                    self.scopes[si][ti].polling = false;
                    if let Some(value) = polled? {
                        self.scopes[si][ti].result = Some(value);
                        completed = true;
                        // The task is done, so its **producer holds** end now — auto-closing any
                        // channel whose last producer just completed, while the scope is still open,
                        // so a sibling receiver drains then observes `none` (isolates I.4c). The
                        // future is left for `ScopeEnd` to reclaim, so captured-local destructors
                        // still fire at the join (unchanged, both backends agree); only the producer
                        // accounting resolves eagerly here. The VM's mirror.
                        let mut holds = std::mem::take(&mut self.scopes[si][ti].holds);
                        self.release_task_holds(&mut holds);
                    }
                }
                ti += 1;
            }
            si += 1;
        }
        Ok(completed)
    }

    /// Cancel the task a handle references (Track A.8) — a `race` loser. Already-completed tasks keep
    /// their result; otherwise the task is marked cancelled so it is never polled again and counts as
    /// done for the join. The tree-walker mirror of the VM's `cancel_task`.
    fn cancel_task(&mut self, handle: &Value) {
        if let Value::Handle(si, ti) = handle
            && let Some(task) = self
                .scopes
                .get_mut(si.index())
                .and_then(|s| s.get_mut(ti.index()))
            && task.result.is_none()
        {
            task.cancelled = true;
        }
    }

    /// Open a structured-concurrency scope and return its (stable) index (Track A.7). Appends a fresh
    /// slot, so the new scope is the innermost; a subsequent `spawn` in the same straight-line segment
    /// lands in it. The tree-walker mirror of the VM's scope open.
    fn open_scope(&mut self) -> usize {
        self.scopes.push(Vec::new());
        self.scope_closed.push(false);
        self.scopes.len() - 1
    }

    /// The innermost still-open scope index (Track A.7) — the highest non-tombstoned slot. Used by
    /// `spawn` and the synchronous join/close (a split `concurrent { }` closes by *captured* index).
    /// Panics only for a `spawn`/join with no open scope, which is E0041 at check.
    fn innermost_open(&self) -> usize {
        self.scope_closed
            .iter()
            .rposition(|closed| !closed)
            .expect("an open concurrency scope")
    }

    /// Close the (already-drained) scope at index `si` (Track A.7): release each task's producer holds,
    /// future, and result (destructor-aware, mirroring `Stmt::ScopeEnd`), tombstone the slot, then trim
    /// trailing tombstones so the Vec stays bounded (the common LIFO case reclaims immediately). Closing
    /// by index — not popping the top — keeps sibling scopes that are still open above it intact. The
    /// tree-walker mirror of the VM's `close_scope`.
    fn close_scope(&mut self, si: usize) {
        let scope = std::mem::take(&mut self.scopes[si]);
        for mut task in scope {
            self.release_task_holds(&mut task.holds);
            self.destroy_value(task.future);
            if let Some(result) = task.result {
                self.destroy_value(result);
            }
        }
        self.scope_closed[si] = true;
        while self.scope_closed.last() == Some(&true) {
            self.scopes.pop();
            self.scope_closed.pop();
        }
    }

    /// Join the innermost scope (Track A.3b): drive tasks round-robin until the innermost scope's tasks
    /// all complete. Each round polls **all** open scopes (A.7) so an outer scope's siblings interleave
    /// with the inner join; the loop exits on the innermost scope alone. On a round where nothing
    /// completed, advance the logical clock; a pending scope with no timer to advance is a deterministic
    /// deadlock.
    fn join_scope(&mut self, span: Span) -> Eval<()> {
        let si = self.innermost_open();
        loop {
            let before = self.channel_progress;
            let progressed = self.poll_all_scopes_round(span)?;
            if self.scopes[si]
                .iter()
                .all(|t| t.result.is_some() || t.cancelled)
            {
                return Ok(());
            }
            // A channel op (a `send` unblocked, a `recv` drained) is progress even when no task
            // completed this round — otherwise a producer/consumer pair would look deadlocked.
            let progressed = progressed || self.channel_progress != before;
            if !progressed && self.executor.advance().is_none() {
                return Err(self.runtime_error(
                    DiagnosticCode::Panic,
                    span,
                    "async deadlock: a `concurrent` task is stuck with no pending timers"
                        .to_string(),
                ));
            }
        }
    }

    /// Drive an awaited future to completion via the executor (Track A.2/A.3 — a `.await` in inlined
    /// context: the top level or a `concurrent` block body). Polls the target; each iteration also
    /// drives every open `concurrent` scope's sibling tasks a round (A.7 — across all scope levels) so
    /// they interleave; advances the logical clock when nothing progresses; deadlocks if nothing can
    /// advance.
    fn drive_future(&mut self, future: Value, span: Span) -> Eval<Value> {
        // `.await` on a cancelled task is a **loud error** (Track A.8, E0056) — a cancelled task
        // never produces a value. Cancel-aware code uses `h.join()` (the same drive, cancelled
        // outcome reported) instead. The VM's `drive_future` mirror.
        match self.drive_future_outcome(future, span)? {
            Some(value) => Ok(value),
            None => Err(self.runtime_error(
                DiagnosticCode::AwaitCancelled,
                span,
                "cannot await a cancelled task; use `.join()` to observe the cancelled outcome"
                    .to_string(),
            )),
        }
    }

    /// The shared drive loop behind `.await` ([`Self::drive_future`]) and `h.join()`
    /// ([`Self::join_task`]) (Track A.8): drive the target to completion, interleaving open scopes
    /// each round. `Some(value)` on completion, `None` when the target is a task **handle whose task
    /// was cancelled** (never polled again, never gets a result). The tree-walker mirror of the VM's
    /// `drive_future_outcome`.
    fn drive_future_outcome(&mut self, future: Value, span: Span) -> Eval<Option<Value>> {
        loop {
            let before = self.channel_progress;
            if let Some(value) = self.poll_once(&future, span)? {
                return Ok(Some(value));
            }
            // A cancelled handle never becomes ready — report the cancelled outcome rather than
            // spinning to a deadlock. Checked after the poll so a sibling's cancel this round shows.
            if self.handle_cancelled(&future) {
                return Ok(None);
            }
            // Interleave: run every open scope's tasks one round (so awaiting a handle — or a `sleep` —
            // inside a `concurrent` block lets siblings at all levels make progress).
            let progressed = if self.scopes.is_empty() {
                false
            } else {
                self.poll_all_scopes_round(span)?
            };
            // A channel op during any poll this iteration is progress (see `join_scope`).
            let progressed = progressed || self.channel_progress != before;
            if !progressed && self.executor.advance().is_none() {
                return Err(self.runtime_error(
                    DiagnosticCode::Panic,
                    span,
                    "async deadlock: awaited a pending future with no pending timers".to_string(),
                ));
            }
        }
    }

    /// Drive a task handle for `h.join()` (Track A.8) and report its outcome as a typed
    /// `Result<T, Cancelled>`: `Ok(value)` once the task completes, `Err(Cancelled)` if it was
    /// cancelled. The cancel-aware counterpart to `.await` (which raises E0056 on a cancelled task).
    /// The tree-walker mirror of the VM's `join_task`.
    fn join_task(&mut self, future: Value, span: Span) -> Eval<Value> {
        match self.drive_future_outcome(future, span)? {
            Some(value) => Ok(builtin_enum("Result", "Ok", vec![value])),
            None => Ok(builtin_enum(
                "Result",
                "Err",
                vec![builtin_enum("Cancelled", "Cancelled", Vec::new())],
            )),
        }
    }

    /// Whether `future` is a task **handle** whose task has been cancelled (Track A.8) — the terminal
    /// state after `h.cancel()` (or a `race` loser). The tree-walker mirror of the VM's
    /// `handle_cancelled`.
    fn handle_cancelled(&self, future: &Value) -> bool {
        if let Value::Handle(si, ti) = future {
            return self
                .scopes
                .get(si.index())
                .and_then(|s| s.get(ti.index()))
                .is_some_and(|task| task.result.is_none() && task.cancelled);
        }
        false
    }

    fn expect_arity(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Eval<()> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` takes {expected} argument(s) but {} were supplied",
                    builtin.name(),
                    args.len()
                ),
            ))
        }
    }

    fn expect_list(&mut self, value: &Value, who: &str, span: Span) -> Eval<Rc<Vec<Value>>> {
        match value {
            Value::List(repr) => Ok(repr.to_rc_vec()),
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("`{who}` expects a list, found {}", other.type_name()),
            )),
        }
    }

    /// Buffer-direct list reductions (packed-reductions arc): `sum`/`product`/`min`/`max` (numeric)
    /// and `any`/`all`/`count` (`List<bool>`). A packed scalar list folds its raw byte buffer through
    /// the shared kernel; a boxed (or packed-struct) list folds its scalar elements — one body in
    /// `noeta-stdlib`, so the packed and boxed paths and the two backends all agree. `sum`/`product`
    /// wrap at the element width; `min`/`max` return `?T` (`none` for an empty list).
    fn call_list_reduction(&mut self, list: &ListRepr, method: &str, span: Span) -> Eval<Value> {
        self.call_list_reduction_from(list, method, 0, span)
    }

    /// [`call_list_reduction`](Self::call_list_reduction) over the elements at or after `from` — the
    /// form `iter().sum()` delegates to, so a directly list-backed iterator's `sum` folds the same
    /// buffer (and thus width-wraps identically) as `xs.sum()`.
    fn call_list_reduction_from(
        &mut self,
        list: &ListRepr,
        method: &str,
        from: usize,
        span: Span,
    ) -> Eval<Value> {
        if let Some(op) = noeta_stdlib::NumReduce::from_name(method) {
            // Packed scalar fast path (a single-field packed element is a contiguous native-width
            // buffer, so `[from..]` is a byte sub-slice); otherwise fold the materialized scalars.
            let folded = match list {
                ListRepr::Packed(p) if p.seam_view().fields.len() == 1 => {
                    let view = p.seam_view();
                    noeta_stdlib::reduce_num_packed(
                        op,
                        &view.fields[0],
                        &p.raw()[from * view.byte_size..],
                    )
                }
                _ => noeta_stdlib::reduce_num_scalars(
                    op,
                    self.list_scalars(list, method, from, span)?,
                ),
            };
            let folded =
                folded.map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?;
            return Ok(match op {
                noeta_stdlib::NumReduce::Min | noeta_stdlib::NumReduce::Max => match folded {
                    Some(rn) => builtin_enum("Option", "some", vec![rednum_to_value(rn)]),
                    None => builtin_enum("Option", "none", Vec::new()),
                },
                // `sum`/`product` always yield a value (identity for the empty list).
                _ => rednum_to_value(folded.expect("sum/product fold to a value")),
            });
        }
        let op = noeta_stdlib::BoolReduce::from_name(method)
            .expect("the caller gates this to a reduction method name");
        let folded = match list {
            ListRepr::Packed(p) if p.seam_view().fields.len() == 1 => {
                noeta_stdlib::reduce_bool_packed(op, &p.raw()[from * p.seam_view().byte_size..])
            }
            _ => {
                noeta_stdlib::reduce_bool_scalars(op, self.list_scalars(list, method, from, span)?)
                    .map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?
            }
        };
        Ok(match folded {
            noeta_stdlib::RedBool::Bool(b) => Value::Bool(b),
            noeta_stdlib::RedBool::Int(i) => Value::Int(i),
        })
    }

    /// Materialize a list's elements at or after `from` as primitive [`Scalar`](noeta_stdlib::Scalar)s
    /// for the boxed reduction fallback, erroring on a non-scalar element (an object/string — a
    /// struct-packed or heterogeneous list a reduction cannot fold).
    fn list_scalars(
        &mut self,
        list: &ListRepr,
        method: &str,
        from: usize,
        span: Span,
    ) -> Eval<std::vec::IntoIter<noeta_stdlib::Scalar>> {
        let items = list.to_rc_vec();
        let mut scalars = Vec::with_capacity(items.len().saturating_sub(from));
        for item in items.iter().skip(from) {
            match value_to_scalar(item) {
                Some(s) => scalars.push(s),
                None => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`{method}` expects a list of numbers, found an element of type {}",
                            item.type_name()
                        ),
                    ));
                }
            }
        }
        Ok(scalars.into_iter())
    }

    fn sum_list(&mut self, items: &[Value], span: Span) -> Eval<Value> {
        let mut int_total: i64 = 0;
        let mut float_total: f64 = 0.0;
        let mut any_float = false;
        for item in items {
            match item {
                Value::Int(i) => int_total = int_total.wrapping_add(*i),
                Value::Float(f) => {
                    any_float = true;
                    float_total += f;
                }
                other => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`sum` expects numeric elements, found {}",
                            other.type_name()
                        ),
                    ));
                }
            }
        }
        if any_float {
            Ok(Value::Float(float_total + int_total as f64))
        } else {
            Ok(Value::Int(int_total))
        }
    }

    /// Apply a **non-short-circuiting** binary operator to already-evaluated operands,
    /// including operator-trait dispatch on user objects. Extracted from [`Self::eval_binary`]
    /// so the Core-IR interpreter (whose operands are pre-evaluated atoms) shares the exact
    /// same semantics — the two backends agree by construction. `&&`/`||` never reach here;
    /// they short-circuit in `eval_binary` / the IR's `Logical` statement.
    fn apply_binary_op(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
        span: Span,
    ) -> Eval<Value> {
        // Operator-trait dispatch on a user object: an arithmetic/concat operator (`Add` for `+`,
        // …) calls the matching method and uses its result directly; `==`/`!=` dispatch to the
        // `Equatable` `eq` method (`!=` negating the bool); and `< <= > >=` dispatch to the
        // `Comparable` `compare` method, mapping the returned `Ordering` variant to this operator's
        // bool.
        if let Value::Object(object) = &left {
            if let Some(method_name) = op.overload_method()
                && let Some(method) = object.def.methods.get(method_name)
            {
                return self.call_method_on(
                    &Rc::clone(object),
                    &Rc::clone(method),
                    vec![right],
                    span,
                );
            }
            if let Some(negate) = op.equatable_negation()
                && let Some(method) = object.def.methods.get("eq")
            {
                let result =
                    self.call_method_on(&Rc::clone(object), &Rc::clone(method), vec![right], span)?;
                return Ok(match result {
                    Value::Bool(b) if negate => Value::Bool(!b),
                    other => other,
                });
            }
            if let Some(method_name) = op.comparable_method()
                && let Some(method) = object.def.methods.get(method_name)
            {
                let result =
                    self.call_method_on(&Rc::clone(object), &Rc::clone(method), vec![right], span)?;
                // `compare` returns an `Ordering`; map its variant to this operator's bool.
                return Ok(match &result {
                    Value::Enum(e) if e.enum_name == "Ordering" => {
                        Value::Bool(op.ordering_satisfies(&e.variant))
                    }
                    _ => result,
                });
            }
            // Derived structural comparison: `@derive(Comparable)` without a hand-written
            // `compare` gives `< <= > >=` field-wise ordering.
            if op.comparable_method().is_some() && object.def.derives_comparable {
                return match object_structural_compare(object, &right) {
                    Some(ordering) => Ok(Value::Bool(
                        op.ordering_satisfies(noeta_ast::ordering_variant(ordering)),
                    )),
                    None => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "cannot compare {} and {}",
                            left.type_name(),
                            right.type_name()
                        ),
                    )),
                };
            }
        }
        // Operator-trait dispatch on an enum value (object-model slice 3): identical to the object
        // path, but the method table is reached through the `EnumDef` resolved from the definition
        // (global) scope by the enum's name (the value itself carries no table). An enum's in-body
        // `impl` blocks are uniform with a class's — no per-kind restriction — so `impl Add`,
        // `impl Equatable`, `impl Comparable`, … all light up their operator here. A case without a
        // hand-written method falls through to the built-in structural behaviour below.
        if let Value::Enum(e) = &left
            && let Some(Value::EnumType(def)) = self.scope.lookup(&e.enum_name)
        {
            if let Some(method_name) = op.overload_method()
                && let Some(method) = def.method(method_name)
            {
                return self.call_enum_method(left.clone(), &Rc::clone(method), vec![right], span);
            }
            if let Some(negate) = op.equatable_negation()
                && let Some(method) = def.method("eq")
            {
                let result =
                    self.call_enum_method(left.clone(), &Rc::clone(method), vec![right], span)?;
                return Ok(match result {
                    Value::Bool(b) if negate => Value::Bool(!b),
                    other => other,
                });
            }
            if let Some(method_name) = op.comparable_method()
                && let Some(method) = def.method(method_name)
            {
                let result =
                    self.call_enum_method(left.clone(), &Rc::clone(method), vec![right], span)?;
                return Ok(match &result {
                    Value::Enum(o) if o.enum_name == "Ordering" => {
                        Value::Bool(op.ordering_satisfies(&o.variant))
                    }
                    _ => result,
                });
            }
            // Derived structural comparison on an enum: `@derive(Comparable)` without a
            // hand-written `compare` orders by variant declaration index, then payload fields —
            // the enum twin of the object arm above (and of the VM's `enum_structural_compare`).
            if op.comparable_method().is_some() && def.derives_comparable {
                return match enum_structural_compare(e, &right) {
                    Some(ordering) => Ok(Value::Bool(
                        op.ordering_satisfies(noeta_ast::ordering_variant(ordering)),
                    )),
                    None => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "cannot compare {} and {}",
                            left.type_name(),
                            right.type_name()
                        ),
                    )),
                };
            }
        }
        // Element-wise array-programming ops (array-ops arc): `+`/`-`/`*` on two lists of the same
        // numeric element type fold element-wise into a new list (`~` is concat, so the operator is
        // free). Packed operands fold their buffers, boxed operands their scalars — one shared
        // `noeta-stdlib` kernel, so both representations and both backends agree; ints wrap at width.
        if let (Value::List(l), Value::List(r)) = (&left, &right)
            && let Some(bop) = elem_bin_op(op)
        {
            let (l, r) = (l.clone(), r.clone());
            return self.call_list_elementwise(bop, &l, &r, span);
        }
        match ops::apply_binary(op, &left, &right) {
            Ok(value) => Ok(value),
            Err(error) => Err(self.runtime_error(error.code, span, error.text)),
        }
    }

    /// Element-wise `+`/`-`/`*` over two numeric lists (array-ops arc). A length mismatch is a runtime
    /// error (E0007). Two packed scalar buffers of the same field fold directly (the result shares the
    /// operand's packed schema); otherwise both sides materialize to scalars and fold — the boxed
    /// fallback, matching the packed path for a given list type.
    fn call_list_elementwise(
        &mut self,
        op: noeta_stdlib::ElemBinOp,
        left: &ListRepr,
        right: &ListRepr,
        span: Span,
    ) -> Eval<Value> {
        if left.len() != right.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                noeta_stdlib::length_mismatch(op).message,
            ));
        }
        // Packed fast path: two single-field scalar buffers of the same element kind.
        if let (ListRepr::Packed(pa), ListRepr::Packed(pb)) = (left, right) {
            let (va, vb) = (pa.seam_view(), pb.seam_view());
            if va.fields.len() == 1 && vb.fields.len() == 1 && va.fields[0] == vb.fields[0] {
                let bytes = noeta_stdlib::zip_num_packed(op, &va.fields[0], pa.raw(), pb.raw())
                    .map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?;
                return Ok(Value::List(ListRepr::Packed(pa.like(bytes))));
            }
        }
        // Boxed fallback: fold the materialized scalars.
        let a: Vec<_> = self.list_scalars(left, op.symbol(), 0, span)?.collect();
        let b: Vec<_> = self.list_scalars(right, op.symbol(), 0, span)?.collect();
        let out = noeta_stdlib::zip_num_scalars(op, &a, &b)
            .map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?;
        Ok(Value::list(out.into_iter().map(scalar_to_value).collect()))
    }

    /// The bulk array-programming **methods** (array-ops arc): `scale(s)`, `abs()`, `neg()`,
    /// `clamp(lo, hi)` — each producing a new list of the operand's numeric element type. A packed
    /// list folds its buffer; a boxed list its scalars — the shared `noeta-stdlib` kernel, so the two
    /// representations (and both backends) agree.
    fn call_list_bulk_method(
        &mut self,
        list: &ListRepr,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        let arg_scalar = |this: &mut Self, i: usize| -> Eval<noeta_stdlib::Scalar> {
            match args.get(i).and_then(value_to_scalar) {
                Some(s) => Ok(s),
                None => Err(this.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("`{method}` expects a numeric argument"),
                )),
            }
        };
        // Packed fast path: a single-field scalar buffer.
        if let ListRepr::Packed(p) = list {
            let view = p.seam_view();
            if view.fields.len() == 1 {
                let field = &view.fields[0];
                let bytes = match method {
                    "scale" => noeta_stdlib::scale_num_packed(field, p.raw(), arg_scalar(self, 0)?),
                    "clamp" => noeta_stdlib::clamp_num_packed(
                        field,
                        p.raw(),
                        arg_scalar(self, 0)?,
                        arg_scalar(self, 1)?,
                    ),
                    _ => {
                        let op = noeta_stdlib::ElemMap::from_name(method)
                            .expect("the caller gates this to a bulk method name");
                        noeta_stdlib::map_num_packed(op, field, p.raw())
                    }
                }
                .map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?;
                return Ok(Value::List(ListRepr::Packed(p.like(bytes))));
            }
        }
        // Boxed fallback.
        let a: Vec<_> = self.list_scalars(list, method, 0, span)?.collect();
        let out = match method {
            "scale" => noeta_stdlib::scale_num_scalars(&a, arg_scalar(self, 0)?),
            "clamp" => {
                noeta_stdlib::clamp_num_scalars(&a, arg_scalar(self, 0)?, arg_scalar(self, 1)?)
            }
            _ => {
                let op = noeta_stdlib::ElemMap::from_name(method)
                    .expect("the caller gates this to a bulk method name");
                noeta_stdlib::map_num_scalars(op, &a)
            }
        }
        .map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?;
        Ok(Value::list(out.into_iter().map(scalar_to_value).collect()))
    }

    /// `checked_sum()` (array-ops arc): the opt-in overflow-reporting sum — `none` on integer
    /// overflow, `some(total)` otherwise. A packed buffer folds directly; a boxed list its scalars.
    fn call_list_checked_sum(&mut self, list: &ListRepr, span: Span) -> Eval<Value> {
        let folded = match list {
            ListRepr::Packed(p) if p.seam_view().fields.len() == 1 => {
                let view = p.seam_view();
                noeta_stdlib::checked_sum_packed(&view.fields[0], p.raw())
            }
            _ => noeta_stdlib::checked_sum_scalars(self.list_scalars(
                list,
                "checked_sum",
                0,
                span,
            )?),
        }
        .map_err(|e| self.runtime_error(std_error_code(e.kind), span, e.message))?;
        Ok(match folded {
            Some(rn) => builtin_enum("Option", "some", vec![rednum_to_value(rn)]),
            None => builtin_enum("Option", "none", Vec::new()),
        })
    }

    /// A sign-dependent fixed-width integer op (Tier W3): `/ % < <= > >=` where the operand width
    /// and signedness matter. Operands are erased ints (no object/enum dispatch), so this goes
    /// straight to the shared `ops::apply_binary_wide` — the VM's `Op::WideInt` twin.
    fn apply_binary_wide_op(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
        signed: bool,
        bits: u8,
        span: Span,
    ) -> Eval<Value> {
        match ops::apply_binary_wide(op, &left, &right, signed, bits) {
            Ok(value) => Ok(value),
            Err(error) => Err(self.runtime_error(error.code, span, error.text)),
        }
    }

    fn eval_unary(&mut self, op: UnaryOp, value: Value, span: Span) -> Eval<Value> {
        match ops::apply_unary(op, &value) {
            Ok(value) => Ok(value),
            Err(error) => Err(self.runtime_error(error.code, span, error.text)),
        }
    }

    /// Convert a native-dispatch [`noeta_stdlib::StdError`] into the abort sentinel. The
    /// distinguished `Exit` kind (`os.exit(code)`, stdlib-gaps) is NOT a diagnostic: it records
    /// the requested code and aborts cleanly — nothing is reported, stdout is kept.
    fn std_dispatch_error(&mut self, error: noeta_stdlib::StdError, span: Span) -> Unwind {
        if let noeta_stdlib::ErrorKind::Exit(code) = error.kind {
            self.requested_exit = Some(code);
            return Unwind::Abort;
        }
        self.runtime_error(std_error_code(error.kind), span, error.message)
    }

    /// Record a runtime diagnostic and produce the abort sentinel.
    fn runtime_error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Unwind {
        self.record_abort_trace(span);
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Unwind::Abort
    }

    /// Snapshot the abort traceback from the call-site shadow stack, innermost frame first — the
    /// tree-walker twin of the VM's frame-stack walk. Taken at the moment the abort's diagnostic is
    /// recorded (`span` is the failing location), pairing each shadowed activation's *name* with the
    /// location it is paused at: the innermost function at the failing span, each caller at the call
    /// site it entered its callee from, and the top level as `main`. First abort wins: a later abort
    /// (e.g. from teardown) never overwrites the trace of the one actually unwinding the program.
    fn record_abort_trace(&mut self, span: Span) {
        if !self.abort_trace.is_empty() {
            return;
        }
        let mut at = span;
        for (name, call_site) in self.call_sites.iter().rev() {
            self.abort_trace.push(noeta_backend::TraceFrame {
                name: name.clone(),
                span: Some(at),
            });
            at = *call_site;
        }
        self.abort_trace.push(noeta_backend::TraceFrame {
            name: Some("main".to_string()),
            span: Some(at),
        });
    }
}

/// Concatenate for the copy-on-write self-append fast path, **consuming** `left` (which the caller
/// has taken out of its scope slot, so it may be uniquely owned). A uniquely-owned list is extended
/// in place — O(1) amortized; a shared list (refcount > 1, e.g. an alias `b = acc`) is copied,
/// preserving immutable semantics; a non-list pairing falls back to display concatenation, byte-for
/// -byte identical to [`ops::apply_binary`]'s `~`. The result is observably indistinguishable from
/// the ordinary path — only the cost differs.
fn cow_concat(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::List(a), Value::List(b)) => {
            // The in-place fast path consumes the left list's unique `Rc`. A boxed list hands its
            // `Rc` straight through (preserving the O(1)-amortized extend when uniquely owned); a
            // packed list (P-PACK 2.3) has no specialized concat yet, so it materializes to a fresh,
            // uniquely-owned boxed vector — correct, just not flat. Either way the result is boxed.
            let mut a = match a {
                ListRepr::Boxed { items: rc, .. } => rc,
                ListRepr::Packed(_) => a.to_rc_vec(),
            };
            let b = b.to_rc_vec();
            if let Some(items) = Rc::get_mut(&mut a) {
                items.extend(b.iter().cloned());
                Value::list_rc(a)
            } else {
                let mut items = (*a).clone();
                items.extend(b.iter().cloned());
                Value::list(items)
            }
        }
        (left, right) => Value::Str(format!("{}{}", left.display(), right.display())),
    }
}

/// Read a numeric receiver (`int`/erased `IntN`, `float`, or `f32`) as the shared
/// [`noeta_stdlib::NumScalar`] for the conversion tower (S0); `None` for any non-numeric value.
fn numeric_scalar(value: &Value) -> Option<noeta_stdlib::NumScalar> {
    match value {
        Value::Int(i) => Some(noeta_stdlib::NumScalar::Int(*i)),
        Value::Float(f) => Some(noeta_stdlib::NumScalar::F64(*f)),
        Value::F32(f) => Some(noeta_stdlib::NumScalar::F32(*f)),
        _ => None,
    }
}

/// Construct a built-in `Result`/`Option`/`Ordering` value (`Ok`/`Err`/`some`/`none`, or
/// `Ordering.Less`/`Equal`/`Greater`). These reuse the ordinary [`EnumValue`] representation, so
/// they participate in `match` and equality like any enum; only `Result`/`Option`'s display and
/// the `?`/`??` operators treat them specially.
fn builtin_enum(enum_name: &str, variant: &str, data: Vec<Value>) -> Value {
    // The built-in enums' defined variant order (`none < some`, `Ok < Err`,
    // `Less < Equal < Greater`) — must agree with the VM's shape indices.
    let variant_index = match variant {
        "none" | "Ok" | "Less" => 0,
        "some" | "Err" | "Equal" => 1,
        _ => 2,
    };
    Value::Enum(Rc::new(EnumValue {
        enum_name: enum_name.to_string(),
        variant: variant.to_string(),
        data,
        variant_index,
        reflect: None,
    }))
}

/// The `Err` payload of a `Result::Err` value (validation arc): `Some(payload)` when `value` is
/// `Result::Err(e)`, else `None` (an `Ok`, or any non-`Result`). Used by `materialize_recipe` to
/// read a `Validate::validate` result.
pub(crate) fn result_err_payload(value: &Value) -> Option<Value> {
    let Value::Enum(e) = value else {
        return None;
    };
    match (e.enum_name.as_str(), e.variant.as_str()) {
        ("Result", "Err") => Some(e.data.first().cloned().unwrap_or(Value::Unit)),
        _ => None,
    }
}

/// Wrap a path-carrying [`noeta_stdlib::json::JsonError`] as an extern `Value` — the `Err` payload
/// of a validation-rejecting recipe door (validation arc).
pub(crate) fn json_error_value(error: noeta_stdlib::json::JsonError) -> Value {
    Value::Extern(Rc::new(RefCell::new(noeta_stdlib::ExternBox::new(error))))
}

/// One snapshot of an [`IterState`]'s shape (its child values cloned, its counters copied). Read
/// under a *short* `RefCell` borrow so the driver can recurse into a source — or run a `map`/`filter`
/// closure — with **no** borrow held, mirroring the VM's [`noeta_value::Value::iter_next_apply`]: a
/// user closure that re-enters the same iterator must not find it already borrowed (which would panic).
enum IterShape {
    List,
    Take { source: Value, remaining: usize },
    Drop { source: Value, pending: usize },
    Chain { first: Value, second: Value },
    Enumerate { source: Value, index: usize },
    Zip { a: Value, b: Value },
    Map { source: Value, func: Value },
    Filter { source: Value, pred: Value },
    Gen { step: Value },
}

impl Interpreter {
    /// Advance an iterator value one step, or `None` at end (Track I). `map`/`filter` adapters run a
    /// user closure, so stepping is fallible.
    fn iter_value_next(&mut self, it: &Value, span: Span) -> Eval<Option<Value>> {
        match it {
            Value::Iter(state) => self.iter_advance(state, span),
            _ => Ok(None),
        }
    }

    /// Read an iterator's next element and advance (Track I). Shared by `next` (one step), `collect`
    /// (drain), `count`, and `sum`. The base cursors a `Value::List` (`ListRepr::get` materializes a
    /// packed element on demand); adapters delegate to their source(s). The shape is snapshotted under
    /// a short borrow ([`IterShape`]) so no borrow is held across a recursive pull or a closure call.
    fn iter_advance(&mut self, state: &Rc<RefCell<IterState>>, span: Span) -> Eval<Option<Value>> {
        loop {
            let shape = match &*state.borrow() {
                IterState::List { .. } => IterShape::List,
                IterState::Take { source, remaining } => IterShape::Take {
                    source: source.clone(),
                    remaining: *remaining,
                },
                IterState::Drop { source, pending } => IterShape::Drop {
                    source: source.clone(),
                    pending: *pending,
                },
                IterState::Chain { first, second } => IterShape::Chain {
                    first: first.clone(),
                    second: second.clone(),
                },
                IterState::Enumerate { source, index } => IterShape::Enumerate {
                    source: source.clone(),
                    index: *index,
                },
                IterState::Zip { a, b } => IterShape::Zip {
                    a: a.clone(),
                    b: b.clone(),
                },
                IterState::Map { source, func } => IterShape::Map {
                    source: source.clone(),
                    func: func.clone(),
                },
                IterState::Filter { source, pred } => IterShape::Filter {
                    source: source.clone(),
                    pred: pred.clone(),
                },
                IterState::Gen { step } => IterShape::Gen { step: step.clone() },
            };
            match shape {
                // No recursion, no user code: read and advance the cursor under one short borrow.
                IterShape::List => {
                    let mut st = state.borrow_mut();
                    let IterState::List { list, cursor } = &mut *st else {
                        unreachable!("shape matched List")
                    };
                    let elem = match list {
                        Value::List(repr) => repr.get(*cursor),
                        _ => None,
                    };
                    if elem.is_some() {
                        *cursor += 1;
                    }
                    return Ok(elem);
                }
                IterShape::Take { source, remaining } => {
                    if remaining == 0 {
                        return Ok(None);
                    }
                    let elem = self.iter_value_next(&source, span)?;
                    if elem.is_some()
                        && let IterState::Take { remaining, .. } = &mut *state.borrow_mut()
                    {
                        *remaining -= 1;
                    }
                    return Ok(elem);
                }
                IterShape::Drop { source, pending } => {
                    if pending > 0 {
                        match self.iter_value_next(&source, span)? {
                            Some(_) => {
                                // The skipped element is dropped (Rc auto-frees).
                                if let IterState::Drop { pending, .. } = &mut *state.borrow_mut() {
                                    *pending -= 1;
                                }
                                continue;
                            }
                            None => {
                                if let IterState::Drop { pending, .. } = &mut *state.borrow_mut() {
                                    *pending = 0;
                                }
                                return Ok(None);
                            }
                        }
                    }
                    return self.iter_value_next(&source, span);
                }
                IterShape::Chain { first, second } => {
                    if let Some(e) = self.iter_value_next(&first, span)? {
                        return Ok(Some(e));
                    }
                    return self.iter_value_next(&second, span);
                }
                IterShape::Enumerate { source, index } => {
                    let Some(elem) = self.iter_value_next(&source, span)? else {
                        return Ok(None);
                    };
                    let tuple = Value::Tuple(Rc::new(vec![Value::Int(index as i64), elem]));
                    if let IterState::Enumerate { index, .. } = &mut *state.borrow_mut() {
                        *index += 1;
                    }
                    return Ok(Some(tuple));
                }
                IterShape::Zip { a, b } => {
                    // Pull from both; the shorter source ends the zip (a leftover element is dropped).
                    let Some(ea) = self.iter_value_next(&a, span)? else {
                        return Ok(None);
                    };
                    let Some(eb) = self.iter_value_next(&b, span)? else {
                        return Ok(None);
                    };
                    return Ok(Some(Value::Tuple(Rc::new(vec![ea, eb]))));
                }
                IterShape::Map { source, func } => {
                    let Some(elem) = self.iter_value_next(&source, span)? else {
                        return Ok(None);
                    };
                    return Ok(Some(self.call(func, vec![elem], span)?));
                }
                // A generator (Track G): run the step closure (one resume arg, here unit) and
                // interpret its returned `?T` — `some(x)` → element, `none`/other → end.
                IterShape::Gen { step } => {
                    let opt = self.call(step, vec![Value::Unit], span)?;
                    return Ok(match opt {
                        Value::Enum(e) if e.variant == "some" => e.data.first().cloned(),
                        _ => None,
                    });
                }
                IterShape::Filter { source, pred } => loop {
                    let Some(elem) = self.iter_value_next(&source, span)? else {
                        return Ok(None);
                    };
                    match self.call(pred.clone(), vec![elem.clone()], span)? {
                        Value::Bool(true) => return Ok(Some(elem)),
                        Value::Bool(false) => {} // try the next source element
                        other => {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!(
                                    "`filter` predicate must return a bool, found {}",
                                    other.type_name()
                                ),
                            ));
                        }
                    }
                },
            }
        }
    }
}

/// The `Result.Err(msg)` returned when a by-name `invoke` cannot resolve (unknown name, wrong
/// arity, non-string name, non-list args, or a non-invokable receiver). The VM builds the same
/// value from `Op::Invoke`'s baked `err_shape`.
fn invoke_err(message: String) -> Value {
    builtin_enum("Result", "Err", vec![Value::Str(message)])
}

/// Classify a runtime value into its **head-constructor** [`TypeRepr`] (`type_of`, fidelity B).
/// Generics are erased at runtime, so a container's element/argument types collapse to `Dyn`.
/// Mirrors the VM's `vm_type_repr` exactly so both backends reflect identical `Type` values.
/// Map a bare-scalar element's checker [`PackedKind`](noeta_ast::reflect::PackedKind) to the eval
/// [`SlotKind`] (packed-widths bare-scalar arc). A scalar element is a single primitive — a nested
/// `Struct` is not a scalar, so it returns `None` (the caller stays boxed) defensively.
/// Parse a `@packed` field's declared type name into the seam's [`noeta_stdlib::PackedField`] kind
/// (scalar-unification slice 3) — the width source for the element bundle methods, keyed off the
/// per-type field-name index. Only the numeric primitives (and `bool`) are recognised; anything else
/// (a nested struct name, a generic) yields `None`, which bars the type from a `vec` bundle.
fn parse_packed_field(name: &str) -> Option<noeta_stdlib::PackedField> {
    use noeta_stdlib::PackedField as PF;
    Some(match name {
        "int" => PF::Int,
        "float" => PF::Float,
        "f32" => PF::F32,
        "f64" => PF::F64,
        "bool" => PF::Bool,
        "i8" => PF::IntN {
            bits: 8,
            signed: true,
        },
        "i16" => PF::IntN {
            bits: 16,
            signed: true,
        },
        "i32" => PF::IntN {
            bits: 32,
            signed: true,
        },
        "i64" => PF::IntN {
            bits: 64,
            signed: true,
        },
        "u8" => PF::IntN {
            bits: 8,
            signed: false,
        },
        "u16" => PF::IntN {
            bits: 16,
            signed: false,
        },
        "u32" => PF::IntN {
            bits: 32,
            signed: false,
        },
        "u64" => PF::IntN {
            bits: 64,
            signed: false,
        },
        _ => return None,
    })
}

fn scalar_slot_kind(kind: &noeta_ast::reflect::PackedKind) -> Option<SlotKind> {
    use noeta_ast::reflect::PackedKind;
    Some(match kind {
        PackedKind::Int => SlotKind::Int,
        PackedKind::Float => SlotKind::Float,
        PackedKind::F32 => SlotKind::F32,
        PackedKind::F64 => SlotKind::F64,
        PackedKind::IntN { bits, signed } => SlotKind::IntN {
            bits: *bits,
            signed: *signed,
        },
        PackedKind::Bool => SlotKind::Bool,
        PackedKind::Struct(_) => return None,
    })
}

fn eval_type_repr(value: &Value) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    let dyn_ = || Box::new(TypeRepr::Dyn);
    match value {
        Value::Bool(_) => TypeRepr::Bool,
        Value::Int(_) => TypeRepr::Int,
        Value::Float(_) => TypeRepr::Float,
        Value::F32(_) => TypeRepr::F32,
        Value::Str(_) => TypeRepr::Str,
        Value::Bytes(_) => TypeRepr::Bytes,
        Value::Unit => TypeRepr::Unit,
        // A list carrying a reflected type tag (R1 — a tagged literal, preserved through pure
        // aliasing) reports that precise element type; an untagged/packed list falls back to the
        // head-only `List(Dyn)`. Mirrors the VM's `vm_type_repr` tag consultation.
        // A bare-scalar packed list (`List<i32>`/`List<f32>`) recovers its element width from its
        // schema — a laundered value still reflects `List<i32>`, not `List<dyn>` (slice-1 identity).
        Value::List(repr) => repr
            .reflect()
            .map(|r| (*r).clone())
            .or_else(|| repr.scalar_elem_repr().map(|e| TypeRepr::List(Box::new(e))))
            .unwrap_or_else(|| TypeRepr::List(dyn_())),
        // A tuple has no reflection descriptor (like a union) — it erases to the dynamic top.
        Value::Tuple(_) => TypeRepr::Dyn,
        // A set carrying a reflected type tag (R1) reports its precise `Set(T)` type (carried from
        // the source list through `to_set`); an untagged/derived set falls back to head-only.
        Value::Set(_, reflect) => reflect
            .as_ref()
            .map(|r| (**r).clone())
            .unwrap_or_else(|| TypeRepr::Set(dyn_())),
        // A map carrying a reflected type tag (R1) reports its precise `Map(K, V)` type; an
        // untagged/derived map falls back to the head-only `Map(dyn, dyn)`. Mirrors `vm_type_repr`.
        Value::Map(_, reflect) => reflect
            .as_ref()
            .map(|r| (**r).clone())
            .unwrap_or_else(|| TypeRepr::Map(dyn_(), dyn_())),
        Value::Function(_)
        | Value::Builtin(_)
        | Value::ModuleFn(..)
        | Value::MethodHandle(..)
        | Value::BoundMethod(..) => TypeRepr::Fn(Vec::new(), dyn_()),
        // A generic enum instance carrying a reflected type tag (R2b.2) reports its precise type;
        // otherwise the head-only classification (`Option`/`Result` by name, else the enum name with
        // empty args). Mirrors `vm_type_repr`'s node-tag consultation.
        Value::Enum(e) => match &e.reflect {
            Some(repr) => (**repr).clone(),
            None => match e.enum_name.as_str() {
                "Option" => TypeRepr::Option(dyn_()),
                "Result" => TypeRepr::Result(dyn_(), dyn_()),
                other => TypeRepr::Enum(other.to_string(), Vec::new()),
            },
        },
        // A generic struct/class instance carrying a reflected type tag (R2) reports its precise type
        // (with type arguments); a non-generic/untagged instance falls back to the head-only shape name
        // with empty args. Mirrors `vm_type_repr`'s node-tag consultation.
        Value::Object(o) if o.def.is_struct => o
            .reflect
            .as_ref()
            .map(|r| (**r).clone())
            .unwrap_or_else(|| TypeRepr::Struct(o.def.name().to_string(), Vec::new())),
        Value::Object(o) => o
            .reflect
            .as_ref()
            .map(|r| (**r).clone())
            .unwrap_or_else(|| TypeRepr::Class(o.def.name().to_string(), Vec::new())),
        // An extern-type value reflects as its registered nominal type under its qualified
        // identity (`std.id.Uuid`), mirroring the checker's `Type::Named` for it.
        Value::Extern(e) => TypeRepr::Named(e.borrow().type_identity().to_string(), Vec::new()),
        // A type value, module, iterator, or enum-type has no nameable lattice type → top.
        Value::EnumType(_)
        | Value::Type(_)
        | Value::NativeModule(_)
        | Value::Iter(_)
        | Value::Future(_)
        | Value::Timer(_)
        | Value::Pending
        | Value::Handle(..)
        | Value::AsyncIo(_)
        | Value::Sender(_)
        | Value::Receiver(_)
        | Value::ChannelSend(..)
        | Value::ChannelRecv(_) => TypeRepr::Dyn,
    }
}

/// Build the prelude `Type` enum value from a [`TypeRepr`], recursively. Reuses the ordinary
/// [`EnumValue`] representation (enum name `Type`), so the value participates in `match` like any
/// enum and is structurally identical to the VM's `build_type_value`.
fn build_type_value(repr: &noeta_ast::reflect::TypeRepr) -> Value {
    use noeta_ast::reflect::{TYPE_ENUM, TypeRepr};
    let list = |items: Vec<Value>| Value::list(items);
    let data: Vec<Value> = match repr {
        TypeRepr::Int
        | TypeRepr::Float
        | TypeRepr::F32
        | TypeRepr::F64
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Bytes
        | TypeRepr::Unit
        | TypeRepr::Dyn => Vec::new(),
        // `Type.IntN(bits: int, signed: bool)` — the width descriptor.
        TypeRepr::IntN { signed, bits } => {
            vec![Value::Int(i64::from(*bits)), Value::Bool(*signed)]
        }
        TypeRepr::List(t) | TypeRepr::Set(t) | TypeRepr::Option(t) => {
            vec![build_type_value(t)]
        }
        TypeRepr::Map(k, v) | TypeRepr::Result(k, v) => {
            vec![build_type_value(k), build_type_value(v)]
        }
        TypeRepr::Enum(name, args)
        | TypeRepr::Struct(name, args)
        | TypeRepr::Class(name, args)
        | TypeRepr::Named(name, args) => vec![
            Value::Str(name.clone()),
            list(args.iter().map(build_type_value).collect()),
        ],
        TypeRepr::DynTrait(name) => vec![Value::Str(name.clone())],
        TypeRepr::Fn(params, ret) => vec![
            list(params.iter().map(build_type_value).collect()),
            build_type_value(ret),
        ],
        TypeRepr::Union(members) => {
            vec![list(members.iter().map(build_type_value).collect())]
        }
    };
    builtin_enum(TYPE_ENUM, repr.variant_name(), data)
}

/// A minimal `TypeDef` for a reflection-materialized struct (no methods, derives, or destructor) —
/// the tree-walker counterpart to the VM's fresh `Shape`. Both carry only name + field names.
fn fresh_type_def(name: &str, fields: &[String], is_struct: bool) -> TypeDef {
    TypeDef {
        name: name.to_string(),
        fields: fields
            .iter()
            .map(|f| FieldSpec { name: f.clone() })
            .collect(),
        methods: HashMap::new(),
        destructor: None,
        is_struct,
        // No derive context here; a struct compares structurally, a class by identity.
        structural_eq: is_struct,
        // Reflection-materialized: no packed context (mirrors the VM's fresh `Shape` default).
        key_capable: std::cell::Cell::new(false),
        derives_comparable: false,
        derives_tojson: false,
        opaque: false,
        field_defaults: Vec::new(),
    }
}

/// A per-literal `TypeDef` for an opaque `use`-import instance: its `fields` are the literal's keys
/// in the caller-supplied (sorted) order, so the object lays out on the uniform slot model. `==` is
/// structural (matching the VM's `ShapeKind::Opaque`); it carries no methods, derives, or destructor.
fn fresh_opaque_def(name: &str, fields: &[String]) -> TypeDef {
    TypeDef {
        opaque: true,
        structural_eq: true,
        ..fresh_type_def(name, fields, false)
    }
}

/// If `value` is a reflection `Type` value naming a nominal type (`Type.Named`/`Struct`/`Class`/
/// `Enum`, whose first payload is the type's name), return that name — so a stored type reference
/// can be used as an `invoke` receiver. Mirrors the VM's `reflection_type_name`.
fn reflection_type_name(value: &Value) -> Option<String> {
    let Value::Enum(ev) = value else {
        return None;
    };
    if ev.enum_name == noeta_ast::reflect::TYPE_ENUM
        && matches!(ev.variant.as_str(), "Named" | "Struct" | "Class" | "Enum")
        && let Some(Value::Str(name)) = ev.data.first()
    {
        return Some(name.clone());
    }
    None
}

/// Convert a manifest attribute-argument literal tree to a tree-walker value, recursing through the
/// collection and nominal literals. A type reference materializes as the reflection `Type` ADT
/// classified by the named type's *kind* (`Type.Struct`/`Enum`/`Class`, or `Type.Named` for an
/// unknown-kind name) via the shared [`reflect::ReflectionInfo::type_ref_repr`]; a set is
/// canonicalized exactly like the runtime `to_set` (sorted/deduped when orderable, else insertion
/// order). The VM's `attr_value_to_vm` builds the matching values the same way, so the materialized
/// attribute agrees across the differential by construction.
fn attr_value_to_eval(
    value: &noeta_ast::AttrValue,
    reflection: &noeta_ast::reflect::ReflectionInfo,
) -> Value {
    use noeta_ast::AttrValue as A;
    let recur = |v: &A| attr_value_to_eval(v, reflection);
    match value {
        A::Str(s) => Value::Str(s.clone()),
        A::Int(n) => Value::Int(*n),
        A::Float(f) => Value::Float(*f),
        A::Bool(b) => Value::Bool(*b),
        A::List(items) => Value::list(items.iter().map(recur).collect()),
        A::Set(items) => {
            let vals: Vec<Value> = items.iter().map(recur).collect();
            Value::set_value(Rc::new(canonical_set(&vals).unwrap_or(vals)))
        }
        A::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                map.insert(noeta_stdlib::MapKey::from(k.as_str()), recur(v));
            }
            Value::map_value(Rc::new(map))
        }
        A::Enum {
            enum_name,
            variant,
            args,
        } => builtin_enum(enum_name, variant, args.iter().map(recur).collect()),
        A::Struct { type_name, fields } => {
            let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
            let def = Rc::new(fresh_type_def(type_name, &names, true));
            // `def.fields` is `names` (same order), so the recursed values are already slot-ordered.
            let slots: Vec<Value> = fields.iter().map(|(_, v)| recur(v)).collect();
            Value::Object(Rc::new(ObjectValue::new(def, slots)))
        }
        A::TypeRef { name, args } => build_type_value(&reflection.type_ref_repr(name, args)),
    }
}

/// Whether a runtime value matches a narrowing target type `ty` (`x.as<T>()`). Generics are
/// erased, so only the **head constructor** is tested — `List<int>` checks "is a list", trusting
/// the element type from the annotation. Keyed on the same canonical kind names the VM uses
/// (`Value::type_name` and the enum/object shape name), so both backends decide identically.
fn runtime_matches(value: &Value, ty: &TypeRef) -> bool {
    match ty {
        // A union target matches if the value matches any member (`x.as<int | string>()`).
        TypeRef::Union { members, .. } => members.iter().any(|m| runtime_matches(value, m)),
        // `?T` is `Option<T>`: matches any `Option` value (its payload is not re-checked).
        TypeRef::Optional { .. } => {
            matches!(value, Value::Enum(e) if e.enum_name == "Option")
        }
        // Narrowing to a trait object matches any value (the permissive over-approximation, matching
        // the VM's `NarrowTarget::Dyn`); a precise implementor test is future work.
        TypeRef::DynTrait { .. } => true,
        // A tuple target matches any tuple value — head-constructor only, arity/elements erased
        // (object-model slice 4), exactly like `List` ignoring its element type.
        TypeRef::Tuple { .. } => matches!(value, Value::Tuple(_)),
        // A function type is erased to a head-constructor "is callable" test (params/return dropped),
        // matching the VM's `NarrowTarget::Fn` (type_name `"function"`).
        TypeRef::Fn { .. } => matches!(
            value,
            Value::Function(_)
                | Value::Builtin(_)
                | Value::ModuleFn(..)
                | Value::MethodHandle(..)
                | Value::BoundMethod(..)
        ),
        TypeRef::Named { name, args, .. } => {
            // The built-in heads, exhaustive over `BuiltinTy` so this and the VM's `narrow_head` —
            // the two halves of the differential — cannot drift apart. `None` means the name has no
            // built-in head and falls through to the nominal match below.
            let builtin_ok = noeta_ast::BuiltinTy::from_name_any(name).and_then(|b| {
                use noeta_ast::BuiltinTy;
                Some(match b {
                    BuiltinTy::Int => matches!(value, Value::Int(_)),
                    // Subtype edge `F32 <: float`: a plain `float` OR a reified `f32` matches `float`.
                    BuiltinTy::Float => matches!(value, Value::Float(_) | Value::F32(_)),
                    // The `f32` head matches only a reified `f32` — a plain `float` is not a subtype.
                    BuiltinTy::F32 => matches!(value, Value::F32(_)),
                    BuiltinTy::Bool => matches!(value, Value::Bool(_)),
                    BuiltinTy::Str => matches!(value, Value::Str(_)),
                    BuiltinTy::Bytes => matches!(value, Value::Bytes(_)),
                    BuiltinTy::Unit => matches!(value, Value::Unit),
                    // Narrowing to the open top is a no-op: every value is a `dyn`.
                    BuiltinTy::Dyn => true,
                    BuiltinTy::List => matches!(value, Value::List(_)),
                    BuiltinTy::Map => matches!(value, Value::Map(..)),
                    BuiltinTy::Set => matches!(value, Value::Set(..)),
                    // Abstract kind-types match any value of that declaration kind (structs and
                    // classes are both `Object`s, told apart by `TypeDef::is_struct`).
                    BuiltinTy::KindEnum => matches!(value, Value::Enum(_)),
                    BuiltinTy::KindStruct => matches!(value, Value::Object(o) if o.def.is_struct),
                    BuiltinTy::KindClass => matches!(value, Value::Object(o) if !o.def.is_struct),
                    // `Option`/`Result` are enums whose shape name *is* the type name, so they fall
                    // to the nominal path like a user enum.
                    BuiltinTy::Option | BuiltinTy::Result => return None,
                    // The erased widths carry no runtime tag on a scalar, so they never match a
                    // scalar (the checker warns). `f32` alone is reified, handled above. See the VM's
                    // `narrow_head`.
                    BuiltinTy::F64 | BuiltinTy::IntN { .. } => return None,
                })
            });
            let head_ok = match builtin_ok {
                Some(ok) => ok,
                // A nominal target: a user record/class/enum, `Option`/`Result`, or an extern type.
                None => match value {
                    Value::Object(object) => object.def.name() == name,
                    Value::Enum(enum_value) => &enum_value.enum_name == name,
                    // An extern value matches by its qualified identity (`std.id.Uuid`) — the
                    // target an imported native type lowers to, compared directly against the
                    // identity the value itself carries — so it never matches a same-short-named
                    // user type nor another namespace's same-short-named extern type (mirrors
                    // the VM's `narrow_matches`).
                    Value::Extern(e) => e.borrow().type_identity() == name.as_str(),
                    _ => false,
                },
            };
            // A parametrized target (`x is List<int>`, R3) additionally checks its type arguments
            // against the value's reflected tag; a bare name stays head-only (the widening `x is List`
            // and the untagged fallback). An untagged value's `eval_type_repr` yields `dyn` arguments,
            // so the check passes head-only — mirroring the VM's `NarrowTarget::Generic` by construction.
            if head_ok && !args.is_empty() {
                let target: Vec<noeta_ast::reflect::TypeRepr> = args
                    .iter()
                    .map(noeta_ast::reflect::typeref_to_repr_arg)
                    .collect();
                noeta_ast::reflect::narrow_args_match(&target, &eval_type_repr(value))
            } else {
                head_ok
            }
        }
    }
}

/// Extract the owned [`noeta_stdlib::MapKey`] a map operation keys by: a string, a key-capable
/// extern value (a boxed snapshot — extern-types X4), or a key-capable `@packed` struct (a
/// content snapshot — P-PKEY, gated by the interpreter's `key_capable_packed` fixpoint, the same
/// shared computation the VM bakes into `Shape::key_capable`). `None` for anything else; the
/// caller raises the shared map-key error. Mirrors the VM's key extraction.
pub(crate) fn value_map_key(value: &Value) -> Option<noeta_stdlib::MapKey> {
    match value {
        Value::Str(s) => Some(noeta_stdlib::MapKey::from(s.as_str())),
        // P-PKEY S4: ints key maps (`float` stays excluded — NaN).
        Value::Int(i) => Some(noeta_stdlib::MapKey::Int(*i)),
        Value::Extern(e) if noeta_stdlib::map_key::extern_key_capable(&**e.borrow()) => {
            Some(noeta_stdlib::MapKey::Extern(e.borrow().clone()))
        }
        Value::Object(o) if o.def.key_capable.get() => Some(noeta_stdlib::MapKey::packed(
            o.def.name(),
            packed_key_fields(o)?,
        )),
        _ => None,
    }
}

/// The [`value_map_key`] field walk (P-PKEY): the object's slots in declaration order as plain
/// [`noeta_stdlib::PackedKeyField`] data. `None` on a slot the capability contract excludes —
/// defensive (a key-capable type's slots are ints/bools/nested-capable by construction), so the
/// caller falls back to the ordinary key error rather than corrupting a map. Mirrors
/// `Value::packed_key_fields` on the VM side.
fn packed_key_fields(object: &ObjectValue) -> Option<Vec<noeta_stdlib::PackedKeyField>> {
    object
        .slots
        .borrow()
        .iter()
        .map(|v| match v {
            Value::Int(i) => Some(noeta_stdlib::PackedKeyField::Int(*i)),
            Value::Bool(b) => Some(noeta_stdlib::PackedKeyField::Bool(*b)),
            Value::Object(o) if o.def.key_capable.get() => {
                Some(noeta_stdlib::PackedKeyField::Struct(
                    o.def.name().into(),
                    packed_key_fields(o)?.into_boxed_slice(),
                ))
            }
            _ => None,
        })
        .collect()
}

/// Project a tree-walker `Value` onto the native-extension registry's argument view. One of the
/// two functions (with [`materialize_native`]) that form the backend's half of the value seam;
/// every migrated module call goes through these rather than a per-function `read_*`. The
/// scalar/host modules use only the scalar and string shapes; richer shapes are added as the
/// modules that need them migrate. Mirrors the VM-side projection.
fn marshal_native_arg(
    value: &Value,
    reg: &'static noeta_stdlib::registry::Registry,
) -> noeta_stdlib::NativeValue {
    use noeta_stdlib::{NativeValue, Scalar};
    match value {
        Value::Int(n) => NativeValue::Scalar(Scalar::Int(*n)),
        Value::Float(f) => NativeValue::Scalar(Scalar::Float(*f)),
        Value::F32(f) => NativeValue::Scalar(Scalar::F32(*f)),
        Value::Bool(b) => NativeValue::Scalar(Scalar::Bool(*b)),
        Value::Str(s) => NativeValue::Str(s.clone()),
        Value::Bytes(b) => NativeValue::Bytes((**b).clone()),
        // An extern-type argument crosses by value (`clone_box`); extern producers are host/IO
        // shaped, never a hot path. Mirrors the VM-side projection.
        Value::Extern(e) => NativeValue::Extern(e.borrow().clone()),
        // A class instance crossing INTO a dispatch (native-extensibility S2): the full instance
        // (class name + `(field, value)` pairs in slot order, each marshalled), so a native fn can
        // receive a native class value a program constructed. Mirrors the VM's projection. This arm
        // is UNCHANGED (`!is_struct && !opaque`) so a user `.noe` class marshals here exactly as
        // before; a native value-struct is handled by the next arm.
        Value::Object(obj) if !obj.def.is_struct && !obj.def.opaque => {
            let slots = obj.slots.borrow();
            let fields = obj
                .def
                .fields
                .iter()
                .map(|f| f.name.clone())
                .zip(slots.iter().map(|s| marshal_native_arg(s, reg)))
                .collect();
            NativeValue::Instance {
                class: obj.def.name().to_string(),
                fields,
            }
        }
        // A **native value-struct** crossing INTO a dispatch (fielded unification): a struct-kind
        // object whose type name resolves to a registered native fielded type. It marshals as the
        // full `Instance`, so a native method/fn receives it exactly like a class. The registry gate
        // keeps this SEPARATE from a user value-struct (a `Vec3` is `is_struct` too but does NOT
        // resolve in the registry, so it falls through to the all-scalar `Object` arm below — no
        // Vec3 regression).
        Value::Object(obj)
            if obj.def.is_struct && reg.resolve_fielded(obj.def.name()).is_some() =>
        {
            let slots = obj.slots.borrow();
            let fields = obj
                .def
                .fields
                .iter()
                .map(|f| f.name.clone())
                .zip(slots.iter().map(|s| marshal_native_arg(s, reg)))
                .collect();
            NativeValue::Instance {
                class: obj.def.name().to_string(),
                fields,
            }
        }
        // An object with all-scalar fields (e.g. a `Vec3`) projects to its field scalars in slot
        // order; anything with a non-scalar field is opaque (a dispatch that wanted an object will
        // report the type error). Mirrors the prior `read_vec3`.
        Value::Object(obj) => {
            let slots = obj.slots.borrow();
            match slots
                .iter()
                .map(value_to_scalar)
                .collect::<Option<Vec<Scalar>>>()
            {
                Some(fields) => NativeValue::Object {
                    type_name: value.type_name(),
                    fields,
                },
                None => NativeValue::Opaque(value.type_name()),
            }
        }
        // A native enum value crossing INTO a dispatch (native-extensibility S1): the full variant
        // (name + case + declaration index + payload), so a native fn receives a real variant, not
        // the lossy `Opaque` the fallback would give. Mirrors the VM's projection.
        Value::Enum(e) => NativeValue::Variant {
            enum_name: e.enum_name.clone(),
            variant: e.variant.clone(),
            variant_index: e.variant_index as u32,
            fields: e.data.iter().map(|v| marshal_native_arg(v, reg)).collect(),
        },
        other => NativeValue::Opaque(other.type_name()),
    }
}

/// Project a primitive tree-walker value onto a [`noeta_stdlib::Scalar`], or `None` if not primitive.
/// Lift a numeric reduction result (packed-reductions arc) into a tree-walker `Value`. An integer
/// (`int`/`IntN`, erased) becomes `Value::Int`; the float widths keep their runtime tag.
/// The narrow integer element width (`signed`, `bits < 64`) of a list, or `None` for a boxed / wide /
/// non-integer list — for masking a narrow-typed iterator reduction (array-ops arc).
fn list_narrow_bits(v: &Value) -> Option<(bool, u8)> {
    if let Value::List(ListRepr::Packed(p)) = v {
        let view = p.seam_view();
        if view.fields.len() == 1
            && let noeta_stdlib::PackedField::IntN { bits, signed } = view.fields[0]
            && bits < 64
        {
            return Some((signed, bits));
        }
    }
    None
}

/// Trace an iterator's narrow integer element width back through the **width-preserving** adapters
/// (`take`/`drop`/`chain`/`filter`) to its backing list (array-ops arc), so `xs.iter().take(k).sum()`
/// wraps at the same width `xs.sum()` does. `map`/`enumerate`/`zip` change the element type, so they
/// stop the trace (the fold then stays at 64 bits — the element is already a full `int`).
fn iter_narrow_bits(v: &Value) -> Option<(bool, u8)> {
    match v {
        Value::List(_) => list_narrow_bits(v),
        Value::Iter(state) => match &*state.borrow() {
            IterState::List { list, .. } => list_narrow_bits(list),
            IterState::Take { source, .. }
            | IterState::Drop { source, .. }
            | IterState::Filter { source, .. } => iter_narrow_bits(source),
            IterState::Chain { first, .. } => iter_narrow_bits(first),
            _ => None,
        },
        _ => None,
    }
}

/// Map an arithmetic operator to its element-wise list op (array-ops arc): `+`/`-`/`*` fold two
/// lists element-wise; `/`/`%` (and every non-arithmetic operator) have no list form (`None`).
fn elem_bin_op(op: BinaryOp) -> Option<noeta_stdlib::ElemBinOp> {
    Some(match op {
        BinaryOp::Add => noeta_stdlib::ElemBinOp::Add,
        BinaryOp::Sub => noeta_stdlib::ElemBinOp::Sub,
        BinaryOp::Mul => noeta_stdlib::ElemBinOp::Mul,
        _ => return None,
    })
}

fn rednum_to_value(rn: noeta_stdlib::RedNum) -> Value {
    match rn {
        noeta_stdlib::RedNum::Int(i) => Value::Int(i),
        noeta_stdlib::RedNum::Float(f) => Value::Float(f),
        noeta_stdlib::RedNum::F32(f) => Value::F32(f),
    }
}

pub(crate) fn value_to_scalar(value: &Value) -> Option<noeta_stdlib::Scalar> {
    use noeta_stdlib::Scalar;
    Some(match value {
        Value::Int(n) => Scalar::Int(*n),
        Value::Float(f) => Scalar::Float(*f),
        Value::F32(f) => Scalar::F32(*f),
        Value::Bool(b) => Scalar::Bool(*b),
        _ => return None,
    })
}

pub(crate) fn scalar_to_value(scalar: noeta_stdlib::Scalar) -> Value {
    use noeta_stdlib::Scalar;
    match scalar {
        Scalar::Int(n) => Value::Int(n),
        Scalar::Float(f) => Value::Float(f),
        Scalar::F32(f) => Value::F32(f),
        Scalar::Bool(b) => Value::Bool(b),
    }
}

/// Lift a native-extension result back into a tree-walker `Value`, supplying the result *shape* for
/// an object result from the function's [`RetTy`] (the same shape as the named argument — e.g.
/// `vec.add(v, w)` builds a value shaped like `v`).
fn materialize_ext(
    out: noeta_stdlib::NativeOut,
    ret: noeta_stdlib::RetTy,
    args: &[Value],
) -> Value {
    use noeta_stdlib::{NativeOut, RetTy};
    match out {
        NativeOut::Object(fields) => {
            let i = match ret {
                RetTy::SameAsArg(i) => i,
                _ => 0,
            };
            let Value::Object(obj) = &args[i] else {
                unreachable!("an object result is shaped like an object argument")
            };
            Value::Object(Rc::new(ObjectValue::new(
                Rc::clone(&obj.def),
                fields.into_iter().map(scalar_to_value).collect(),
            )))
        }
        other => materialize_native(other),
    }
}

/// Lift a native-extension [`noeta_stdlib::NativeOut`] result back into a tree-walker `Value`.
pub(crate) fn materialize_native(out: noeta_stdlib::NativeOut) -> Value {
    use noeta_stdlib::{NativeOut, Scalar};
    match out {
        NativeOut::Scalar(Scalar::Int(n)) => Value::Int(n),
        NativeOut::Scalar(Scalar::Float(f)) => Value::Float(f),
        NativeOut::Scalar(Scalar::F32(f)) => Value::F32(f),
        NativeOut::Scalar(Scalar::Bool(b)) => Value::Bool(b),
        NativeOut::Str(s) => Value::Str(s),
        NativeOut::Bytes(b) => Value::Bytes(Rc::new(b)),
        NativeOut::Unit => Value::Unit,
        NativeOut::List(items) => Value::list(items.into_iter().map(materialize_native).collect()),
        // A typed bulk-primitive vector (N3.4: a packed reduction's result) converts in one pass.
        NativeOut::Scalars(v) => {
            use noeta_stdlib::ScalarVec;
            Value::list(match v {
                ScalarVec::Int(xs) => xs.into_iter().map(Value::Int).collect(),
                ScalarVec::Float(xs) => xs.into_iter().map(Value::Float).collect(),
                ScalarVec::F32(xs) => xs.into_iter().map(Value::F32).collect(),
                ScalarVec::Bool(xs) => xs.into_iter().map(Value::Bool).collect(),
            })
        }
        // A dynamic `json.parse` object → a string-keyed map (entries arrive in key order).
        NativeOut::Map(entries) => Value::map_value(Rc::new(
            entries
                .into_iter()
                .map(|(k, v)| (noeta_stdlib::MapKey::from(k), materialize_native(v)))
                .collect(),
        )),
        // An extern-type value: host the box in the shared cell (extern-types X1).
        NativeOut::Extern(e) => Value::Extern(Rc::new(RefCell::new(e))),
        // Object results carry no shape, so they are built by `materialize_ext` (which has the
        // function's `RetTy` + arguments) and never reach here.
        NativeOut::Object(_) => {
            unreachable!("object results are materialized by `materialize_ext`")
        }
        // Option results from ordinary dispatch (`id.parse`, extern-type methods like
        // `timestamp_ms` — extern-types X2).
        NativeOut::None => builtin_enum("Option", "none", Vec::new()),
        NativeOut::Some(inner) => builtin_enum("Option", "some", vec![materialize_native(*inner)]),
        NativeOut::Ok(inner) => builtin_enum("Result", "Ok", vec![materialize_native(*inner)]),
        NativeOut::Err(inner) => builtin_enum("Result", "Err", vec![materialize_native(*inner)]),
        // A native-declared enum value (native-extensibility S1): a REAL `EnumValue` carrying the
        // enum's short name + variant + declaration index, so a `match` over it is exhaustive and it
        // is differential-identical to the VM's interned enum shape. The payload materializes
        // recursively (a payload-carrying variant nests).
        NativeOut::Variant {
            enum_name,
            variant,
            variant_index,
            fields,
        } => Value::Enum(Rc::new(EnumValue {
            enum_name,
            variant,
            data: fields.into_iter().map(materialize_native).collect(),
            variant_index: variant_index as usize,
            reflect: None,
        })),
        // A native-declared **fielded-type** instance (native-extensibility S2, unified): a REAL
        // `Object` whose `TypeDef` kind comes from the carried `FieldedKind`. A `Class` gets a class
        // `TypeDef` (`structural_eq = false` → `==` is identity, `is_struct = false`; aliases + cycle
        // participation; its extern-handle field's `Drop` is the destructor). A `Struct` gets a value
        // `TypeDef` (`is_struct = true`, `structural_eq = true` → structural `==`, value semantics).
        // Fields materialize recursively in declared slot order. Differential-identical to the VM's
        // shape kind, and interchangeable with a source-constructed instance.
        NativeOut::Instance {
            class,
            fields,
            kind,
        } => {
            let is_struct = matches!(kind, noeta_stdlib::FieldedKind::Struct);
            let field_specs = fields
                .iter()
                .map(|(n, _)| FieldSpec { name: n.clone() })
                .collect();
            let slots: Vec<Value> = fields
                .into_iter()
                .map(|(_, out)| materialize_native(out))
                .collect();
            let def = Rc::new(TypeDef {
                name: class,
                fields: field_specs,
                methods: HashMap::new(),
                destructor: None,
                is_struct,
                structural_eq: is_struct,
                key_capable: std::cell::Cell::new(false),
                derives_comparable: false,
                derives_tojson: false,
                opaque: false,
                field_defaults: Vec::new(),
            });
            Value::Object(Rc::new(ObjectValue::new(def, slots)))
        }
        // An in-place instance mutation (boundary 1) has no receiver here to write into — the
        // class-method call site (`call_native_class_method`) intercepts it, applies the write-set,
        // and materializes `ret` there. Reaching this generic path means a non-class dispatch returned
        // it, which has no `self` to mutate, so the writes are a no-op and only `ret` materializes.
        NativeOut::InstanceUpdate { ret, .. } => materialize_native(*ret),
        // The typed `json.parse::<T>` results that name their own types are built by the typed-call
        // path (`materialize_recipe`, which has the interpreter's type registry), not here; async
        // work is ticketed at the dispatch return (extern-types X5), never materialized.
        NativeOut::Struct { .. } | NativeOut::Spawn(_) => {
            unreachable!("recipe/spawn results never reach materialize_native")
        }
    }
}

/// Project a tree-walker `Value` onto the backend-agnostic [`noeta_stdlib::Arg`] the shared
/// stdlib dispatch reads. Only the primitive shapes the stdlib introspects are distinguished;
/// everything else collapses to `Other`. Mirrors the VM-side projection so both backends feed
/// the shared surface identically.
fn project_arg(value: &Value) -> noeta_stdlib::Arg<'_> {
    match value {
        Value::Str(s) => noeta_stdlib::Arg::Str(s),
        Value::Int(i) => noeta_stdlib::Arg::Int(*i),
        Value::Float(f) => noeta_stdlib::Arg::Float(*f),
        Value::Bool(b) => noeta_stdlib::Arg::Bool(*b),
        _ => noeta_stdlib::Arg::Other,
    }
}

/// Lift a shared stdlib [`noeta_stdlib::Output`] back into a tree-walker `Value`.
fn output_to_value(output: noeta_stdlib::Output) -> Value {
    match output {
        noeta_stdlib::Output::Str(s) => Value::Str(s),
        noeta_stdlib::Output::Bool(b) => Value::Bool(b),
        noeta_stdlib::Output::Int(i) => Value::Int(i),
        noeta_stdlib::Output::Float(f) => Value::Float(f),
        noeta_stdlib::Output::StrList(items) => {
            Value::list(items.into_iter().map(Value::Str).collect())
        }
        noeta_stdlib::Output::Bytes(data) => Value::Bytes(Rc::new(data)),
        // Optional shapes — the shared dispatch reports presence; the backend builds its own
        // `some(...)`/`none` enum value.
        noeta_stdlib::Output::OptStr(opt) => optional_to_value(opt.map(Value::Str)),
        noeta_stdlib::Output::OptInt(opt) => optional_to_value(opt.map(Value::Int)),
        noeta_stdlib::Output::OptFloat(opt) => optional_to_value(opt.map(Value::Float)),
    }
}

/// Wrap an already-lifted optional payload into the built-in `Option` enum value.
fn optional_to_value(opt: Option<Value>) -> Value {
    match opt {
        Some(value) => builtin_enum("Option", "some", vec![value]),
        None => builtin_enum("Option", "none", Vec::new()),
    }
}

/// Map a stdlib misuse kind onto a diagnostic code: arity/argument-type mistakes are a
/// `TypeMismatch`; an out-of-range index/range is an `IndexOutOfBounds`.
fn std_error_code(kind: noeta_stdlib::ErrorKind) -> DiagnosticCode {
    match kind {
        noeta_stdlib::ErrorKind::Arity | noeta_stdlib::ErrorKind::ArgType => {
            DiagnosticCode::TypeMismatch
        }
        noeta_stdlib::ErrorKind::Bounds => DiagnosticCode::IndexOutOfBounds,
        noeta_stdlib::ErrorKind::UnknownName => DiagnosticCode::UnknownName,
        noeta_stdlib::ErrorKind::Io => DiagnosticCode::IoError,
        // Intercepted upstream (`std_dispatch_error`) — defensive mapping only.
        noeta_stdlib::ErrorKind::Exit(_) => DiagnosticCode::Panic,
        noeta_stdlib::ErrorKind::Panic => DiagnosticCode::Panic,
        noeta_stdlib::ErrorKind::ReactiveCycle => DiagnosticCode::ReactiveCycle,
    }
}

/// Field-wise (declared order) ordering of two same-type objects, the behavior synthesized by
/// `@derive(Comparable)`. Compares fields lexicographically via [`compare_primitive`]. Returns
/// `None` if `right` is not an object of the same type, or any field is non-primitive — the caller
/// turns that into a runtime type error. Mirrors `noeta_value::structural_compare` (VM side).
fn object_structural_compare(left: &ObjectValue, right: &Value) -> Option<std::cmp::Ordering> {
    let Value::Object(rb) = right else {
        return None;
    };
    if left.def.name != rb.def.name {
        return None;
    }
    let la = left.slots.borrow();
    let lb = rb.slots.borrow();
    for (i, f) in left.def.fields.iter().enumerate() {
        let a = &la[i];
        let b = &lb[rb.slot_of(&f.name)?];
        match compare_field(a, b)? {
            std::cmp::Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// Compare one field of two structurally-compared objects: a nested object recurses (matching the
/// VM's `compare_field`), a nested enum pair orders by variant index then payload (how an
/// `?int`/enum field inside a derived struct orders), anything else goes through
/// [`compare_primitive`]. `None` is an incomparable pairing, which the caller turns into a
/// runtime type error.
fn compare_field(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match a {
        Value::Object(la) => object_structural_compare(la, b),
        Value::Enum(la) => enum_structural_compare(la, b),
        _ => compare_primitive(a, b),
    }
}

/// Two same-enum values order by variant **declaration index**, then payload slots
/// lexicographically — the enum half of derived `Comparable`. Mirrors the VM's
/// `enum_structural_compare` (`noeta_value::ops`). `None` if `right` is not the same enum.
fn enum_structural_compare(left: &EnumValue, right: &Value) -> Option<std::cmp::Ordering> {
    let Value::Enum(rb) = right else {
        return None;
    };
    if left.enum_name != rb.enum_name {
        return None;
    }
    match left.variant_index.cmp(&rb.variant_index) {
        std::cmp::Ordering::Equal => {}
        other => return Some(other),
    }
    if left.data.len() != rb.data.len() {
        return None;
    }
    for (a, b) in left.data.iter().zip(rb.data.iter()) {
        match compare_field(a, b)? {
            std::cmp::Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// The JSON encoding synthesized by `@derive(Serialize<Json>)`. The byte-for-byte mirror of the VM's
/// Serialize a tree-walker value to JSON (`json.stringify` and `@derive(Serialize<Json>)`). Marshals
/// the value into the neutral [`noeta_stdlib::NativeValue`] tree (see [`value_to_native_deep`]) and
/// runs the shared [`noeta_stdlib::json::stringify`], so the VM — driving the same walk over its own
/// marshalled tree — produces byte-identical output by construction.
fn value_to_json(value: &Value) -> String {
    noeta_stdlib::json::stringify(&value_to_native_deep(value))
}

/// Deeply marshal a tree-walker value into the neutral [`noeta_stdlib::NativeValue`] tree the shared
/// JSON serializer consumes. Numbers become scalars; strings, enum variants, and the opaque
/// length/`<fn>`/`<module …>` summaries become [`NativeValue::Str`]; lists/tuples/sets become a
/// [`NativeValue::List`]; maps and objects a [`NativeValue::Map`] (objects in declared field order).
/// Mirrors the VM's [`noeta_value::Value::to_native_deep`] so both backends agree — per-
/// representation glue, mirrored by design (see `plans/backend-mirror.md`); divergence is caught
/// by `std/json_encoder_one_engine.noe` and the differential.
fn value_to_native_deep(value: &Value) -> noeta_stdlib::NativeValue {
    use noeta_stdlib::{NativeValue, Scalar};
    match value {
        Value::Unit => NativeValue::Unit,
        Value::Bool(b) => NativeValue::Scalar(Scalar::Bool(*b)),
        Value::Int(n) => NativeValue::Scalar(Scalar::Int(*n)),
        Value::Float(f) => NativeValue::Scalar(Scalar::Float(*f)),
        Value::F32(f) => NativeValue::Scalar(Scalar::F32(*f)),
        Value::Str(s) => NativeValue::Str(s.clone()),
        // A byte buffer has no JSON representation (it is the binary alternative): a length summary.
        Value::Bytes(b) => NativeValue::Str(format!("<{} bytes>", b.len())),
        // Lists, tuples, and sets all serialize as a JSON array.
        Value::List(repr) => {
            NativeValue::List(repr.to_rc_vec().iter().map(value_to_native_deep).collect())
        }
        Value::Tuple(items) | Value::Set(items, _) => {
            NativeValue::List(items.iter().map(value_to_native_deep).collect())
        }
        // An extern key marshals as its canonical display form (JSON keys are strings).
        Value::Map(entries, _) => NativeValue::Map(
            entries
                .iter()
                .map(|(k, v)| (k.as_native_str(), value_to_native_deep(v)))
                .collect(),
        ),
        Value::Object(object) => {
            // Declared field order (records/classes) or sorted-key order (opaque imports). Recursion
            // is on field *values* (distinct objects), so holding this borrow is safe.
            let slots = object.slots.borrow();
            NativeValue::Map(
                object
                    .def
                    .fields
                    .iter()
                    .zip(slots.iter())
                    .map(|(f, v)| (f.name.clone(), value_to_native_deep(v)))
                    .collect(),
            )
        }
        // An `Option` marshals **through** its payload (mirrors the VM's `to_native_deep`): the
        // JSON-null convention and what a native consumer (a SQL bind parameter, `json.stringify`)
        // means by an optional — `some(x)` is `x`, `none` is null/unit. Otherwise an `Option` would
        // flatten to its variant *name* (`"some"`), a silently wrong bound value / serialization.
        Value::Enum(e) if e.enum_name == "Option" => match e.variant.as_str() {
            "some" => e
                .data
                .first()
                .map(value_to_native_deep)
                .unwrap_or(NativeValue::Unit),
            _ => NativeValue::Unit,
        },
        // Any other enum marshals to its variant name (the tag).
        Value::Enum(e) => NativeValue::Str(e.variant.clone()),
        Value::Function(_)
        | Value::Builtin(_)
        | Value::ModuleFn(..)
        | Value::MethodHandle(..)
        | Value::BoundMethod(..) => NativeValue::Str("<fn>".to_string()),
        Value::NativeModule(module) => NativeValue::Str(format!("<module {module}>")),
        // An extern-type value marshals as itself; the shared serializer renders its display
        // form as a JSON string (a `Uuid` is its canonical string).
        Value::Extern(e) => NativeValue::Extern(e.borrow().clone()),
        // An iterator has no JSON analog — its opaque display form, like the VM.
        Value::Iter(_) => NativeValue::Str("<iterator>".to_string()),
        Value::Future(_)
        | Value::Timer(_)
        | Value::Handle(..)
        | Value::AsyncIo(_)
        | Value::ChannelSend(..)
        | Value::ChannelRecv(_) => NativeValue::Str("<future>".to_string()),
        Value::Sender(_) => NativeValue::Str("<sender>".to_string()),
        Value::Receiver(_) => NativeValue::Str("<receiver>".to_string()),
        Value::Pending => NativeValue::Str("<pending>".to_string()),
        // An enum/struct *type* value has no JSON analog; its quoted display form, like the VM.
        Value::EnumType(_) | Value::Type(_) => NativeValue::Str(value.display()),
    }
}

/// The total order of two primitives for `x.compare(y)`: integers compare exactly, strings
/// lexically, and any other numeric pairing as `f64`. Returns `None` when the operands are not
/// comparable (different non-numeric kinds, or a `NaN` float).
pub(crate) fn compare_primitive(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        // `false < true` — bool is checker-declared `Comparable`, so derived structural compare
        // and `.compare()` order it. Mirrors the VM's `compare_primitive`.
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        // Extern-type values order through their contract (extern-types X1) — a total order per
        // key-capable kind; `None` for unordered kinds. Mirrors the VM's `compare_primitive`.
        (Value::Extern(a), Value::Extern(b)) => a.borrow().cmp_value(&**b.borrow()),
        // P-PKEY: two key-capable `@packed` structs order by content — (type name, then
        // field-wise slot order), exactly `MapKey::Packed`'s order. Mirrors the VM's
        // `packed_primitive_cmp`.
        (Value::Object(a), Value::Object(b))
            if a.def.key_capable.get() && b.def.key_capable.get() =>
        {
            let by_name = a.def.name().cmp(b.def.name());
            if by_name != std::cmp::Ordering::Equal {
                return Some(by_name);
            }
            let (xs, ys) = (a.slots.borrow(), b.slots.borrow());
            for (x, y) in xs.iter().zip(ys.iter()) {
                let ord = match (x, y) {
                    (Value::Bool(p), Value::Bool(q)) => p.cmp(q),
                    (Value::Int(p), Value::Int(q)) => p.cmp(q),
                    _ => compare_primitive(x, y)?,
                };
                if ord != std::cmp::Ordering::Equal {
                    return Some(ord);
                }
            }
            Some(std::cmp::Ordering::Equal)
        }
        _ => {
            // `f32` widens to f64 like any other numeric pairing — mirrors the VM's
            // `compare_primitive` (`noeta_value::ops`), which orders f32 the same way.
            let num = |v: &Value| match v {
                Value::Int(i) => Some(*i as f64),
                Value::Float(f) => Some(*f),
                Value::F32(f) => Some(f64::from(*f)),
                _ => None,
            };
            num(left)?.partial_cmp(&num(right)?)
        }
    }
}

/// The ordering that admits a value into a **set** (canonicalization, `add`/`remove`, `to_set`):
/// the total structural [`compare_field`] ordering, except a `class` instance (either side) is
/// refused — a set stores its elements sorted, and a reference type could be mutated *after*
/// insertion, silently breaking the canonical-order invariant. Value kinds (primitives, structs,
/// enums) are snapshots. Mirrors the VM's `noeta_value::set_order`.
fn set_order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    let is_class = |v: &Value| matches!(v, Value::Object(o) if !o.def.is_struct);
    if is_class(a) || is_class(b) {
        return None;
    }
    // Primitives, externs, and key-capable `@packed` structs order through `compare_primitive`
    // first — preserving P-PKEY's content order exactly (including its cross-type by-name order
    // over capable packed structs); any other value kind (a plain struct, an enum) falls back to
    // the total structural ordering. Mirrors the VM's `set_order`.
    compare_primitive(a, b).or_else(|| compare_field(a, b))
}

/// Build a set's canonical form from `items`: every element must be mutually orderable (an
/// orderable primitive, or a derived-`Comparable`-style value kind — see [`set_order`]); the
/// result is sorted and de-duplicated. Returns `None` if any element is non-orderable or of a
/// different kind, so the caller raises the shared unorderable error. Mirrors the VM's
/// `canonical_set` so both backends build identical sets.
fn canonical_set(items: &[Value]) -> Option<Vec<Value>> {
    if items.is_empty() {
        return Some(Vec::new());
    }
    if items
        .iter()
        .any(|item| set_order(&items[0], item).is_none())
    {
        return None;
    }
    let mut canonical = items.to_vec();
    canonical.sort_by(|a, b| set_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
    canonical.dedup_by(|a, b| set_order(a, b) == Some(std::cmp::Ordering::Equal));
    Some(canonical)
}

/// At a function-call boundary, turn a `?`-induced early return into the call's value;
/// pass every other outcome (normal value or fatal abort) through unchanged.
fn catch_return(result: Eval<Value>) -> Eval<Value> {
    match result {
        Err(Unwind::Return(value)) => Ok(value),
        other => other,
    }
}

/// The number of required (non-defaulted) parameters: the leading run with no default value.
/// `defaults` is parallel to a closure's parameter list, so this is the lowest legal argument count.
fn required_count(defaults: &[Option<noeta_ir::Thunk>]) -> usize {
    defaults
        .iter()
        .position(Option::is_some)
        .unwrap_or(defaults.len())
}

/// The arity-mismatch message, worded identically to the VM's (so the differential matches). `kind`
/// is `"function"` or `"method"`; the range form appears only when some parameters are defaulted.
fn arity_message(kind: &str, required: usize, total: usize, supplied: usize) -> String {
    if required == total {
        format!("this {kind} takes {total} argument(s) but {supplied} were supplied")
    } else {
        format!(
            "this {kind} takes between {required} and {total} argument(s) but {supplied} were supplied"
        )
    }
}

/// The message for a free-function `invoke(name, args)` that resolved to nothing callable, worded
/// identically to the VM's `free_fn_miss_message` (so the differential matches).
///
/// **One message for every kind of miss** — unbound, bound to a non-function, or naming a type — and
/// that uniformity is load-bearing rather than lazy. The two backends index the top-level namespace
/// with different structures: the tree-walker's global scope holds types and functions together,
/// while the VM's global slot table holds only value bindings (a type name is not a global there at
/// all). Reporting *why* the lookup failed would therefore report different things in each backend
/// for the same program. What both can always agree on is that no top-level function of this name
/// was found.
///
/// The qualified-name hint needs no namespace knowledge — it is a property of the string — so it
/// stays identical in both backends by construction.
fn free_fn_miss_message(name: &str) -> String {
    if name.contains('.') {
        format!(
            "no top-level function `{name}`; a qualified name dispatches through the three-argument \
             `invoke(recv, name, args)`"
        )
    } else {
        format!("no top-level function `{name}`")
    }
}

/// How a value behaves under `?`/`??`: the unwrapped success payload, or the empty case.
enum TryBranch {
    /// `Ok(x)` / `some(x)` — unwrap to `x` (or `unit` for the void `Ok()`).
    Success(Value),
    /// `Err(_)` / `none` — short-circuit (`?`) or take the fallback (`??`).
    Empty,
}

/// Classify a value for the `?`/`??` operators. Only the built-in `Result`/`Option`
/// enums qualify; anything else returns `None` (a type error at the operator's span).
fn try_branch(value: &Value) -> Option<TryBranch> {
    let Value::Enum(e) = value else {
        return None;
    };
    match (e.enum_name.as_str(), e.variant.as_str()) {
        ("Result", "Ok") | ("Option", "some") => Some(TryBranch::Success(
            e.data.first().cloned().unwrap_or(Value::Unit),
        )),
        ("Result", "Err") | ("Option", "none") => Some(TryBranch::Empty),
        _ => None,
    }
}

/// Try to match `pattern` against `value`. Returns the bindings it introduces on
/// success, or `None` if the pattern does not match.
fn match_pattern(pattern: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
    match pattern {
        Pattern::Wildcard { .. } => Some(Vec::new()),
        Pattern::Binding { name, .. } => Some(vec![(name.clone(), value.clone())]),
        Pattern::Int {
            value: expected, ..
        } => match value {
            Value::Int(actual) if actual == expected => Some(Vec::new()),
            _ => None,
        },
        Pattern::Str {
            value: expected, ..
        } => match value {
            Value::Str(actual) if actual == expected => Some(Vec::new()),
            _ => None,
        },
        Pattern::Bool {
            value: expected, ..
        } => match value {
            Value::Bool(actual) if actual == expected => Some(Vec::new()),
            _ => None,
        },
        Pattern::Variant {
            type_name,
            variant,
            bindings,
            ..
        } => {
            let Value::Enum(enum_value) = value else {
                return None;
            };
            if let Some(type_name) = type_name
                && &enum_value.enum_name != type_name
            {
                return None;
            }
            if &enum_value.variant != variant || bindings.len() != enum_value.data.len() {
                return None;
            }
            let mut all = Vec::new();
            for (sub, data) in bindings.iter().zip(&enum_value.data) {
                all.extend(match_pattern(sub, data)?);
            }
            Some(all)
        }
        // `is T` matches on the head constructor (same erased test as `x.as<T>()`), binding
        // nothing — the narrowed value is referred to by the scrutinee's own name.
        Pattern::IsType { ty, .. } => runtime_matches(value, ty).then(Vec::new),
        // A tuple pattern `(p, q, …)` matches a tuple of the same arity, destructuring each position
        // against its sub-pattern (object-model slice 4b); refutable on kind, arity, and elements.
        Pattern::Tuple { elements, .. } => {
            let Value::Tuple(items) = value else {
                return None;
            };
            if items.len() != elements.len() {
                return None;
            }
            let mut all = Vec::new();
            for (sub, item) in elements.iter().zip(items.iter()) {
                all.extend(match_pattern(sub, item)?);
            }
            Some(all)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn run(text: &str) -> RunResult {
        IrRefBackend::new().run(&program_of(text))
    }

    #[test]
    fn drop_audit_flags_a_read_after_drop_and_not_a_legitimate_one() {
        // Positive control for the Phase-3.x use-after-drop audit: the mechanism must actually fire
        // on a synthetic static-before-dynamic death, so the corpus test asserting zero cannot pass
        // vacuously. A read after `release_binding` is a violation; a read after a rebind is not.
        let scope = Scope::global();

        // (1) declare → drop → read  ⇒ one violation (the drop preceded this real use).
        drop_audit::begin();
        scope.declare("x".into(), Value::Int(1), false);
        scope.release_binding("x");
        let _ = scope.lookup("x");
        assert_eq!(drop_audit::end(), 1, "a read after a drop must be flagged");

        // (2) declare → drop → re-declare → read  ⇒ no violation (the rebind clears the poison).
        drop_audit::begin();
        scope.declare("y".into(), Value::Int(1), false);
        scope.release_binding("y");
        scope.declare("y".into(), Value::Int(2), false);
        let _ = scope.lookup("y");
        assert_eq!(
            drop_audit::end(),
            0,
            "a read after a rebind must not be flagged"
        );

        // (3) declare → read (no drop)  ⇒ no violation.
        drop_audit::begin();
        scope.declare("z".into(), Value::Int(1), false);
        let _ = scope.lookup("z");
        assert_eq!(drop_audit::end(), 0, "an ordinary read must not be flagged");
    }

    #[test]
    fn leak_counter_round_trips_on_an_acyclic_program() {
        // No top-level functions ⇒ no capture cycle ⇒ every scope, struct, and list reclaims via
        // `Rc`. The leak oracle's per-program residency delta is zero. Measured as a delta because
        // a prior leaking test on this thread may have left a positive baseline (thread-local).
        let before = live_count();
        let r = run("struct P { x: int } a = P { x: 1 }; b = [a, a]; echo a.x;");
        assert_eq!(r.stdout, "1\n");
        assert_eq!(live_count(), before, "acyclic program must reclaim fully");
    }

    #[test]
    fn leak_counter_reclaims_a_top_level_function() {
        // A top-level `fn` captures the global scope, which holds the function — a would-be `Rc`
        // cycle. `destroy_globals` drains the global bindings at program end, dropping the closure
        // and breaking the cycle, so the tree-walker *does* reclaim it: residency returns to
        // baseline. (Capture cycles rooted below the global scope are the residual Phase 6 closes;
        // the corpus-wide oracle measures whether any remain.)
        let before = live_count();
        run("fn f(): int { return 1; } echo f();");
        assert_eq!(
            live_count(),
            before,
            "top-level functions reclaim via destroy_globals"
        );
    }

    fn program_of(text: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "test.noe", text);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse errors: {:?}",
            parsed.diagnostics
        );
        parsed.program
    }

    #[test]
    fn cow_self_append_preserves_aliases() {
        // The copy-on-write `~=` fast path may mutate in place only when uniquely owned. An alias
        // taken before the appends must still observe the original list; an explicit `acc = acc ~ acc`
        // must double (the self-reference guard sends it to the copy path, never vacating a slot the
        // RHS reads). Locks the P-COW correctness invariant at the crate level.
        let out = run(
            "mut acc = [1, 2];\nb = acc;\nacc ~= [3];\nacc ~= [4];\necho acc;\necho b;\nmut twice = [5];\ntwice = twice ~ twice;\necho twice;\n",
        );
        assert_eq!(out.stdout, "[1, 2, 3, 4]\n[1, 2]\n[5, 5]\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn operator_trait_overloads_plus() {
        // `a + b` on a class implementing `Add` dispatches to its `add` method (M1.8); the VM
        // reproduces this identically (see noeta-vm), guarded by the differential oracle.
        let out = run(
            "class Money {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): Money { return Money { amount: a, currency: c }; }\n  impl Add {\n    fn add(other: Money): Money { return Money { amount: self.amount + other.amount, currency: self.currency }; }\n  }\n}\na = Money.new(5, \"USD\");\nb = Money.new(3, \"USD\");\nt = a + b;\necho t.amount;\necho t.currency;\n",
        );
        assert_eq!(out.stdout, "8\nUSD\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn equatable_overrides_equality_and_negates_for_ne() {
        // `impl Equatable` routes `==`/`!=` to `eq` (here ignoring `tag`); `!=` negates. The VM
        // reproduces this identically (see noeta-vm), guarded by the differential oracle.
        let out = run(
            "class M {\n  amount: int\n  tag: int\n  fn new(a: int, t: int): M { return M { amount: a, tag: t }; }\n  impl Equatable {\n    fn eq(other: M): bool { return self.amount == other.amount; }\n  }\n}\na = M.new(5, 1);\nb = M.new(5, 2);\necho a == b;\necho a != b;\necho a == M.new(9, 1);\n",
        );
        assert_eq!(out.stdout, "true\nfalse\nfalse\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn comparable_overloads_ordering_operators() {
        // `impl Comparable` routes `< <= > >=` to `compare` (delegating to the built-in primitive
        // `.compare()`); the returned `Ordering` is mapped to each operator's bool.
        let out = run(
            "class M {\n  amount: int\n  fn new(a: int): M { return M { amount: a }; }\n  impl Comparable {\n    fn compare(other: M): Ordering { return self.amount.compare(other.amount); }\n  }\n}\na = M.new(5);\nb = M.new(8);\necho a < b;\necho a > b;\necho a <= b;\necho a >= b;\n",
        );
        assert_eq!(out.stdout, "true\nfalse\ntrue\nfalse\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn enum_body_methods_and_impls_dispatch() {
        // Object-model slice 3: an enum's unified body on the tree-walker. An instance method takes
        // the whole value as `self`; an associated function is called on the bare type name; `${}`
        // routes to `impl Display`; `==` routes to `impl Equatable`. The VM reproduces this (see
        // noeta-vm), guarded by the differential oracle.
        let out = run(
            "enum Color {\n  Red;\n  Green;\n  fn label(): string { return match self { Color.Red => \"r\", Color.Green => \"g\" }; }\n  fn first(): Color { return Color.Red; }\n  impl Display { fn to_string(): string { return \"<${self.label()}>\"; } }\n  impl Equatable { fn eq(other: Color): bool { return true; } }\n}\necho Color.Green.label();\necho Color.first();\necho Color.Red == Color.Green;\necho Color.Red != Color.Green;\n",
        );
        assert_eq!(out.stdout, "g\n<r>\ntrue\nfalse\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn primitive_compare_yields_ordering() {
        let out = run(
            "echo 1.compare(2);\necho 5.compare(5);\necho 9.compare(2);\necho \"a\".compare(\"b\");\n",
        );
        assert_eq!(
            out.stdout,
            "Ordering.Less\nOrdering.Equal\nOrdering.Greater\nOrdering.Less\n"
        );
    }

    #[test]
    fn derive_comparable_orders_fields_lexicographically() {
        // `@derive(Comparable)` synthesizes structural ordering: compare `x`, then `y`.
        let out = run(
            "@derive(Comparable)\nclass P {\n  x: int\n  y: int\n  fn new(x: int, y: int): P { return P { x: x, y: y }; }\n}\na = P.new(1, 2);\nb = P.new(1, 5);\nc = P.new(1, 2);\necho a < b;\necho a > b;\necho a <= c;\necho a >= c;\n",
        );
        assert_eq!(out.stdout, "true\nfalse\ntrue\ntrue\n");
    }

    #[test]
    fn comparison_on_non_comparable_object_still_errors() {
        // Without `@derive(Comparable)` or an `impl`, an object has no order: `<` is an error.
        let out = run(
            "class P {\n  x: int\n  fn new(x: int): P { return P { x: x }; }\n}\necho P.new(1) < P.new(2);\n",
        );
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.diagnostics[0].code, DiagnosticCode::TypeMismatch);
    }

    #[test]
    fn session_persists_state_and_prints_trailing_expression() {
        let mut session = Session::new();
        // A binding in one entry is visible in the next (persistent scope).
        session.eval(&program_of("x = 5;"));
        let out = session.eval(&program_of("echo x;"));
        assert_eq!(out.stdout, "5\n");
        assert_eq!(out.value, None);
        // A trailing bare expression is returned as a value to print, not discarded.
        let out = session.eval(&program_of("x + 10;"));
        assert_eq!(out.value.as_deref(), Some("15"));
        // `next_id()` continuity persists across entries.
        assert_eq!(
            session
                .eval(&program_of("use std.id.{next_id}; next_id();"))
                .value
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            session.eval(&program_of("next_id();")).value.as_deref(),
            Some("2")
        );
    }

    #[test]
    fn session_persists_declarations_across_entries() {
        // A `fn`/`class` declared in one entry is callable in a later one — the IR path registers
        // declarations into the persistent global scope, exactly as the AST walker did.
        let mut session = Session::new();
        session.eval(&program_of("fn double(n: int): int { return n * 2; }"));
        session.eval(&program_of("class Box { v: int }"));
        let out = session.eval(&program_of("double(Box { v: 21 }.v);"));
        assert_eq!(out.value.as_deref(), Some("42"));
    }

    #[test]
    fn session_runs_destructors_at_last_use_via_the_ir() {
        // The REPL now executes on the Core IR, so a destructor-bearing value created and dropped
        // inside a called function fires its `destruct` at last use during the entry — the canonical
        // last-use semantics `lang run` has, which the superseded AST-walk session never produced.
        let mut session = Session::new();
        session.eval(&program_of(
            "class Res { id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; } }",
        ));
        let out = session.eval(&program_of(
            "fn use_it(): void { mut r = Res.new(1); echo \"using\"; }\nuse_it();",
        ));
        assert_eq!(out.stdout, "using\ndrop 1\n");
    }

    #[test]
    fn session_meta_commands_drop_type_bindings_reset() {
        let mut session = Session::new();
        session.eval(&program_of(
            "class Res { id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; } }",
        ));
        session.eval(&program_of("mut r = Res.new(7);"));
        session.eval(&program_of("x = 42;"));

        // `:bindings` lists only user names (no prelude built-ins, no sentinel).
        let mut names = session.binding_names();
        names.sort();
        assert_eq!(
            names,
            vec!["Res".to_string(), "r".to_string(), "x".to_string()]
        );

        // `:type` reports runtime types — the declared name for an object, the kind otherwise.
        assert_eq!(
            session.type_of(&program_of("r;")).value.as_deref(),
            Some("Res")
        );
        assert_eq!(
            session.type_of(&program_of("x + 1;")).value.as_deref(),
            Some("int")
        );
        assert_eq!(
            session.type_of(&program_of("[1, 2, 3];")).value.as_deref(),
            Some("list")
        );

        // `:drop r` runs the destructor now and unbinds the name.
        let (found, out) = session.drop_binding("r");
        assert!(found);
        assert_eq!(out.stdout, "drop 7\n");
        assert!(!session.binding_names().contains(&"r".to_string()));
        // Dropping an unknown name is a no-op miss.
        assert!(!session.drop_binding("nope").0);

        // `:reset` clears every user binding back to the bare prelude.
        session.reset();
        assert!(session.binding_names().is_empty());
    }

    #[test]
    fn session_type_then_drop_still_fires_the_destructor() {
        // Regression: evaluating a value for `:type` (or echoing a trailing expression) must not
        // leave a lingering reference in the sentinel binding — otherwise an immediately following
        // `:drop` of the same object would see refcount > 1 and skip its destructor.
        let mut session = Session::new();
        session.eval(&program_of(
            "class Res { id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; } }",
        ));
        session.eval(&program_of("mut a = Res.new(1);"));
        // Type it (and, separately, echo it) — both go through the sentinel.
        assert_eq!(
            session.type_of(&program_of("a;")).value.as_deref(),
            Some("Res")
        );
        assert_eq!(
            session.eval(&program_of("a;")).value.as_deref(),
            Some("Res {id: 1}")
        );
        // The destructor still fires on the very next drop.
        let (found, out) = session.drop_binding("a");
        assert!(found);
        assert_eq!(out.stdout, "drop 1\n");
    }

    #[test]
    fn session_trailing_value_is_not_stale_across_entries() {
        // A trailing bare expression is captured via a reserved sentinel binding; an entry with no
        // trailing expression must report `value: None`, never the prior entry's captured value.
        let mut session = Session::new();
        assert_eq!(
            session.eval(&program_of("1 + 2;")).value.as_deref(),
            Some("3")
        );
        let out = session.eval(&program_of("echo 9;"));
        assert_eq!(out.stdout, "9\n");
        assert_eq!(out.value, None);
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(run("echo 1 + 2 * 3;").stdout, "7\n");
        assert_eq!(run("echo (1 + 2) * 3;").stdout, "9\n");
        assert_eq!(run("echo 10 / 4;").stdout, "2\n");
    }

    #[test]
    fn concatenation_stringifies() {
        assert_eq!(
            run("echo \"users/\" ~ 42 ~ \"/profile\";").stdout,
            "users/42/profile\n"
        );
    }

    #[test]
    fn if_else_chain() {
        let src = "fn classify(n) { if n == 0 { return \"zero\"; } else if n == 1 { return \"one\"; } else { return \"many\"; } } echo classify(0); echo classify(1); echo classify(7);";
        assert_eq!(run(src).stdout, "zero\none\nmany\n");
    }

    #[test]
    fn for_loop_sums_a_list() {
        assert_eq!(
            run("mut t = 0; for n in [1, 2, 3, 4] { t = t + n; } echo t;").stdout,
            "10\n"
        );
    }

    #[test]
    fn for_loop_destructures_enumerate() {
        assert_eq!(
            run("for (i, x) in [\"a\", \"b\"].enumerate() { echo i ~ \":\" ~ x; }").stdout,
            "0:a\n1:b\n"
        );
    }

    #[test]
    fn tuple_destructuring_binding() {
        // Object-model slice 4b: `(a, b, …) = expr` unpacks a tuple positionally on the tree-walker.
        assert_eq!(
            run("(a, b) = (1, \"two\");\n(x, y, z) = (3, 4, 5);\necho a ~ \" \" ~ b;\necho x ~ \" \" ~ y ~ \" \" ~ z;").stdout,
            "1 two\n3 4 5\n"
        );
    }

    #[test]
    fn list_and_map_literals_and_len() {
        assert_eq!(run("echo [1, 2, 3];").stdout, "[1, 2, 3]\n");
        assert_eq!(run("echo [1, 2, 3].len();").stdout, "3\n");
        assert_eq!(run("echo {\"a\": 1, \"b\": 2}.len();").stdout, "2\n");
    }

    #[test]
    fn map_filter_sum_pipeline() {
        // Method-chain form since P1.2 — the free `map`/`filter`/`sum` left the prelude.
        let src = "echo [1, 2, 3, 4].filter(fn(n) => n % 2 == 0).map(fn(n) => n * 10).sum();";
        assert_eq!(run(src).stdout, "60\n");
    }

    #[test]
    fn recursion_with_if() {
        assert_eq!(
            run("fn fact(n) { if n <= 1 { return 1; } return n * fact(n - 1); } echo fact(5);")
                .stdout,
            "120\n"
        );
    }

    #[test]
    fn string_interpolation() {
        assert_eq!(
            run("name = \"Niro\"; echo \"Hello ${name}\";").stdout,
            "Hello Niro\n"
        );
        assert_eq!(run("echo \"sum is ${1 + 2 * 3}\";").stdout, "sum is 7\n");
        assert_eq!(
            run("id = 1; echo \"Order #${id} ready\";").stdout,
            "Order #1 ready\n"
        );
    }

    #[test]
    fn interpolation_escapes_and_literal_braces() {
        assert_eq!(run("echo \"a\\tb\";").stdout, "a\tb\n");
        // Bare braces are literal now (no `{{`/`}}` escaping); only `${` triggers interpolation.
        assert_eq!(run("echo \"{literal}\";").stdout, "{literal}\n");
        // A literal `${` is escaped as `\${`.
        assert_eq!(run("echo \"\\${x}\";").stdout, "${x}\n");
    }

    #[test]
    fn single_quoted_strings_are_raw() {
        // No interpolation, and bare ${}, braces, $ are literal.
        assert_eq!(
            run("name = \"Niro\"; echo '${name} {x} $y';").stdout,
            "${name} {x} $y\n"
        );
        // `\t` is not an escape in a raw string — backslash and t are both literal.
        assert_eq!(run("echo 'a\\tb';").stdout, "a\\tb\n");
        // The only escapes are `\'` (a quote) and `\\` (a backslash).
        assert_eq!(run("echo 'it\\'s';").stdout, "it's\n");
        assert_eq!(run("echo 'back\\\\slash';").stdout, "back\\slash\n");
    }

    #[test]
    fn backtick_templates_interpolate_and_dedent() {
        // Dedent strips the common indentation and the leading/trailing blank line; `${}`
        // interpolates with the right names (a regression guard — dedent must not corrupt holes).
        let src = "name = \"Ada\";\necho `\n    Hi ${name}\n    bye\n`;";
        assert_eq!(run(src).stdout, "Hi Ada\nbye\n");
        // A single-line template behaves like a normal interpolated string.
        assert_eq!(run("x = \"A\"; echo `v=${x}`;").stdout, "v=A\n");
    }

    #[test]
    fn set_literal_desugars_to_to_set() {
        // `#{...}` is `[...].to_set()`: sorted, de-duplicated, `#{}` empty.
        assert_eq!(run("echo #{3, 1, 2, 1};").stdout, "{1, 2, 3}\n");
        assert_eq!(run("echo #{};").stdout, "{}\n");
        assert_eq!(run("echo #{1, 2} == [2, 1, 2].to_set();").stdout, "true\n");
    }

    #[test]
    fn plain_enum_and_match() {
        let src = "enum Color { Red; Green; Blue; } c = Color.Green; echo match c { Color.Red => \"r\", Color.Green => \"g\", Color.Blue => \"b\" };";
        assert_eq!(run(src).stdout, "g\n");
    }

    #[test]
    fn algebraic_enum_binds_data() {
        let src = "enum E { Empty; Code(n: int); } x = E.Code(42); echo match x { E.Empty => \"empty\", E.Code(n) => \"code ${n}\" };";
        assert_eq!(run(src).stdout, "code 42\n");
    }

    #[test]
    fn match_wildcard_and_literals() {
        let src = "fn name(n) { return match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" }; } echo name(0); echo name(5);";
        assert_eq!(run(src).stdout, "zero\nmany\n");
    }

    #[test]
    fn enum_equality() {
        let src = "enum S { A; B; } echo S.A == S.A; echo S.A == S.B;";
        assert_eq!(run(src).stdout, "true\nfalse\n");
    }

    #[test]
    fn unmatched_value_is_a_runtime_error() {
        let result = run("enum E { A; B; } echo match E.B { E.A => 1 };");
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn immutable_binding_reports_error() {
        let result = run("name = \"a\"; name = \"b\";");
        assert_eq!(
            result.diagnostics[0].code,
            DiagnosticCode::ImmutableAssignment
        );
    }

    #[test]
    fn functions_and_calls() {
        assert_eq!(
            run("fn add(a, b) { return a + b; } echo add(2, 3);").stdout,
            "5\n"
        );
    }

    #[test]
    fn closures_capture_environment() {
        assert_eq!(
            run("base = 100; addbase = fn(x) => x + base; echo addbase(5);").stdout,
            "105\n"
        );
    }

    #[test]
    fn functions_can_call_other_functions() {
        // Forward references through the shared global scope work; true self-recursion
        // is exercised once `if` (control flow) lands in Slice 3.
        assert_eq!(
            run("fn dbl(n) { return n * 2; } fn quad(n) { return dbl(dbl(n)); } echo quad(3);")
                .stdout,
            "12\n"
        );
    }

    #[test]
    fn pipeline_threads_value_as_first_argument() {
        assert_eq!(
            run("fn inc(n) { return n + 1; } echo 5 |> inc |> inc;").stdout,
            "7\n"
        );
        assert_eq!(
            run("fn add(a, b) { return a + b; } echo 5 |> add(10);").stdout,
            "15\n"
        );
    }

    #[test]
    fn next_id_is_deterministic() {
        assert_eq!(
            run("use std.id.{next_id}; echo next_id(); echo next_id();").stdout,
            "1\n2\n"
        );
    }

    #[test]
    fn calling_a_non_function_is_an_error() {
        let result = run("x = 5; echo x(1);");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::TypeMismatch);
    }

    #[test]
    fn arity_mismatch_is_an_error() {
        let result = run("fn one(a) { return a; } echo one(1, 2);");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::TypeMismatch);
    }

    #[test]
    fn record_literal_and_field_access() {
        let src = "struct Item { price: float qty: int } a = Item { price: 2.5, qty: 4 }; echo a.price; echo a.price * a.qty;";
        assert_eq!(run(src).stdout, "2.5\n10.0\n");
    }

    #[test]
    fn records_compare_structurally() {
        let src = "struct P { x: int y: int } a = P { x: 1, y: 2 }; b = P { x: 1, y: 2 }; c = P { x: 1, y: 9 }; echo a == b; echo a == c;";
        assert_eq!(run(src).stdout, "true\nfalse\n");
    }

    #[test]
    fn class_constructor_and_instance_method() {
        let src = "class Box { v: int fn new(v: int): Box { return Box { v: v }; } fn doubled(): int { return self.v * 2; } } b = Box.new(21); echo b.doubled(); echo b.v;";
        assert_eq!(run(src).stdout, "42\n21\n");
    }

    #[test]
    fn method_takes_arguments_alongside_fields() {
        let src = "class Counter { base: int fn new(base: int): Counter { return Counter { base: base }; } fn plus(n: int): int { return self.base + n; } } c = Counter.new(10); echo c.plus(5);";
        assert_eq!(run(src).stdout, "15\n");
    }

    #[test]
    fn structural_update_overrides_one_field() {
        let src = "class M { amount: int currency: string fn new(a: int, c: string): M { return M { amount: a, currency: c }; } } a = M.new(500, \"USD\"); b = M { amount: 300, ...a }; echo b.amount; echo b.currency; echo a.amount;";
        assert_eq!(run(src).stdout, "300\nUSD\n500\n");
    }

    #[test]
    fn missing_field_is_an_error() {
        let result = run("class P { x: int y: int } p = P { x: 1 };");
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::MissingField);
    }

    #[test]
    fn destructors_run_in_reverse_declaration_order_at_program_end() {
        // A `destruct` block runs when the last reference to an instance drops; top-level
        // bindings are destroyed at program end in reverse declaration order.
        let src = "class R { name: string fn new(name: string): R { return R { name: name }; } destruct { echo \"close ${self.name}\"; } } a = R.new(\"a\"); b = R.new(\"b\"); echo \"body\";";
        assert_eq!(run(src).stdout, "body\nclose b\nclose a\n");
    }

    #[test]
    fn reassignment_destroys_the_displaced_instance() {
        let src = "class R { name: string fn new(name: string): R { return R { name: name }; } destruct { echo \"close ${self.name}\"; } } mut x = R.new(\"first\"); x = R.new(\"second\"); echo \"mid\";";
        assert_eq!(run(src).stdout, "close first\nmid\nclose second\n");
    }

    #[test]
    fn unknown_field_in_literal_is_an_error() {
        let result = run("struct R { a: int } r = R { a: 1, b: 2 };");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::UnknownName);
    }

    #[test]
    fn object_displays_as_a_literal() {
        let src = "struct Pt { x: int y: int } echo Pt { x: 1, y: 2 };";
        assert_eq!(run(src).stdout, "Pt {x: 1, y: 2}\n");
    }

    #[test]
    fn result_constructors_display_bare() {
        assert_eq!(run("echo Ok(5);").stdout, "Ok(5)\n");
        assert_eq!(run("echo Err(\"boom\");").stdout, "Err(boom)\n");
        assert_eq!(run("echo some(3);").stdout, "some(3)\n");
        assert_eq!(run("echo none;").stdout, "none\n");
        // The void success carries no payload.
        assert_eq!(run("echo Ok();").stdout, "Ok\n");
    }

    #[test]
    fn question_propagates_err_from_enclosing_fn() {
        // `validate` returns Err; `?` re-returns it from `run_it`, so the trailing
        // `Ok("done")` never executes and the caller sees the original error.
        let src = "fn validate(): int { return Err(\"empty\"); } \
                   fn run_it(): int { validate()?; return Ok(\"done\"); } \
                   echo run_it();";
        assert_eq!(run(src).stdout, "Err(empty)\n");
    }

    #[test]
    fn question_unwraps_ok_and_some() {
        let src = "fn ok_val(): int { return Ok(41); } \
                   fn use_it(): int { x = ok_val()?; return Ok(x + 1); } \
                   echo use_it();";
        assert_eq!(run(src).stdout, "Ok(42)\n");
    }

    #[test]
    fn question_propagates_none() {
        let src = "fn lookup(): int { return none; } \
                   fn first(): int { v = lookup()?; return some(v); } \
                   echo first();";
        assert_eq!(run(src).stdout, "none\n");
    }

    #[test]
    fn coalesce_supplies_a_default() {
        assert_eq!(run("echo none ?? 99;").stdout, "99\n");
        assert_eq!(run("echo some(7) ?? 99;").stdout, "7\n");
        assert_eq!(run("echo Err(\"x\") ?? 0;").stdout, "0\n");
        assert_eq!(run("echo Ok(5) ?? 0;").stdout, "5\n");
    }

    #[test]
    fn coalesce_round_trip_through_option() {
        let src = "fn find(b): int { if b { return some(10); } return none; } \
                   echo find(true) ?? -1; echo find(false) ?? -1;";
        assert_eq!(run(src).stdout, "10\n-1\n");
    }

    #[test]
    fn question_on_a_non_result_is_an_error() {
        let result = run("fn f(): int { x = 5?; return Ok(x); } echo f();");
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::TypeMismatch);
    }

    #[test]
    fn panic_aborts_with_nonzero_exit() {
        let result = run("echo \"before\"; panic(\"boom\"); echo \"after\";");
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::Panic);
        // stdout up to the panic is preserved; nothing after runs.
        assert_eq!(result.stdout, "before\n");
    }

    #[test]
    fn namespace_is_a_noop_and_use_imports_resolve() {
        // `namespace` runs without effect; the `use`d name `User` resolves to a stub type
        // that can be constructed and read, proving import resolution works.
        let src = "namespace App.Orders; use App.Models.User; \
                   u = User { name: \"Ada\", id: 1 }; echo u.name; echo u.id;";
        assert_eq!(run(src).stdout, "Ada\n1\n");
    }

    #[test]
    fn grouped_use_imports_each_name() {
        let src = "use App.Billing.{Invoice, Receipt}; \
                   i = Invoice { number: 42 }; r = Receipt { paid: true }; \
                   echo i.number; echo r.paid;";
        assert_eq!(run(src).stdout, "42\ntrue\n");
    }

    #[test]
    fn imported_stub_displays_its_actual_fields() {
        let src = "use M.User; echo User { name: \"Ada\" };";
        assert_eq!(run(src).stdout, "User {name: \"Ada\"}\n");
    }

    #[test]
    fn named_call_arguments_bind_positionally() {
        // `E.Bad(index: 5)` — the `index:` label is surface sugar in M0; the value binds by
        // position, so the variant's single field gets `5`.
        let src = "enum E { Bad(index: int); } x = E.Bad(index: 5); \
                   echo match x { E.Bad(i) => \"bad ${i}\" };";
        assert_eq!(run(src).stdout, "bad 5\n");
    }

    #[test]
    fn result_and_option_participate_in_match() {
        let src = "fn run_it(b): int { if b { return Ok(1); } return Err(\"no\"); } \
                   echo match run_it(true) { Ok(n) => \"ok ${n}\", Err(e) => \"err ${e}\" }; \
                   echo match run_it(false) { Ok(n) => \"ok ${n}\", Err(e) => \"err ${e}\" };";
        assert_eq!(run(src).stdout, "ok 1\nerr no\n");
    }
}
