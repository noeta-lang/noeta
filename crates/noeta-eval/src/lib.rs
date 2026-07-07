//! The evaluator: an AST → a [`RunResult`].
//!
//! Crucially, evaluation runs behind the [`Backend`] trait and returns a *structured*
//! [`RunResult`] — it never writes to `stdout` or calls `process::exit` directly. That
//! is what makes the M0 tree-walker a clean differential oracle: in M1 the bytecode VM
//! becomes a second [`Backend`] and the two are run against the same programs and their
//! `RunResult`s compared. Build the seam now; retrofitting it later is the trap.
//!
//! M0 scope grows one vertical slice at a time.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::{Rc, Weak};

use noeta_ast::reflect::TypeRepr;
use noeta_ast::{BinaryOp, ForPattern, Pattern, Program, Stmt, TypeRef, UnaryOp};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

pub mod drop_audit;
mod ids;
mod ir;
mod leak;
mod ops;
mod value;

pub(crate) use ids::{ChannelId, ScopeId, TaskId};
pub use leak::live_count;
pub use value::{IterState, ListRepr, Value};
use value::{PackedList, PackedSchema, PackedSlot, SlotKind};

// The `Backend`/`RunResult` seam moved into its own crate in M1 so the tree-walker and the
// bytecode VM are siblings (neither depends on the other). Re-exported here so existing
// `noeta_eval::{Backend, RunResult}` users keep working.
pub use noeta_backend::{Backend, RunResult};

/// The M0 tree-walking interpreter, exposed as a [`Backend`].
/// (The id-source seed field left with `IdGen` — sequential ids are host-owned now, so the
/// deterministic counter is the sandbox host's, not this backend's.)
#[derive(Debug, Clone)]
pub struct TreeWalkBackend {}

impl TreeWalkBackend {
    pub fn new() -> TreeWalkBackend {
        TreeWalkBackend {}
    }

    // The AST-walk entry points (`run_with_host`, `run_with_sites`, `run_with_host_sites`) were
    // retired in migration Phase 7: `lang run` executes on the Core IR via
    // [`TreeWalkBackend::run_ir_with_host`], and the reference oracle lowers unconditionally, so
    // nothing drove the AST-walk host/sites wrappers any longer. The plain [`Backend::run`] path
    // remains for the perf benches and property tests (an AST-walk baseline, not an oracle), and
    // `Interpreter::run_with_sites` stays as the shared executor the IR interpreter reuses.
}

impl Default for TreeWalkBackend {
    fn default() -> TreeWalkBackend {
        TreeWalkBackend::new()
    }
}

impl Backend for TreeWalkBackend {
    /// Execute a program through the canonical **Core-IR interpreter** — the same path `lang run`
    /// and the conformance reference take. (The AST tree-walker this crate began as was retired:
    /// it was neither a production path nor the differential oracle — the oracle is VM-vs-IR — so it
    /// was pure duplication. Lowering is total over the parsed language, so this never fails.)
    fn run(&self, program: &Program) -> RunResult {
        // Apply the same IR passes the production paths do (`lang run` in `noeta-cli`, the conformance
        // reference): precise-RC drop insertion (with destructor relevance) and reuse-token threading.
        // Without them `reuse` is never set, so e.g. a list self-append `acc ~= [i]` copies the whole
        // accumulator each step — O(n²) instead of the O(n) in-place path the real run takes. A single
        // `check_all` yields both the `type_of` sites and the relevance, so the checker runs once.
        let checked = noeta_check::check_all(program);
        // Lower with the checker's site maps: packed-list literals stream into a flat buffer
        // (P-PACK 2.5) and `list[i].field` reads fuse to `Rvalue::IndexField` (P-PACK 2.5+). Both are
        // carried inline on the IR, so `run_ir` needs no map.
        let ir = noeta_ir::lower_with_sites(
            program,
            noeta_ir::LoweringSites {
                packed_list_sites: &checked.packed_list_sites,
                index_field_sites: &checked.index_field_sites,
                ext_call_sites: &checked.ext_call_sites,
                for_stream_sites: &checked.for_stream_sites,
                width_sites: &checked.width_sites,
                construction_sites: &checked.construction_sites,
                handle_sites: &checked.handle_sites,
                bound_handle_sites: &checked.bound_handle_sites,
                f32_literal_sites: &checked.f32_literal_sites,
            },
        )
        .expect("Core-IR lowering is total over the parsed language");
        let ir =
            noeta_ir_passes::insert_drops(&ir, Some(&relevance_of(&checked.destructor_relevance)));
        let ir = noeta_ir_passes::thread_reuse(&ir);
        self.run_ir(program, &ir, checked.type_of_sites)
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

/// A persistent evaluation session — the REPL backend. Unlike [`TreeWalkBackend::run`],
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
        self.interp
            .reflection
            .accumulate(noeta_ast::reflect::build(&lowerable));
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

/// The reserved binding name a trailing bare REPL expression is rewritten into, so its value is
/// captured in the persistent scope. Contains a NUL so it can never collide with a user identifier
/// and never appears in displayed output.
const REPL_VALUE: &str = "\0repl-value";

/// If `program`'s final statement is a bare expression, return a copy with that statement rewritten
/// to `mut <REPL_VALUE> = <expr>;` (so the IR path captures its value) and `true`; otherwise return
/// the program unchanged and `false`. Only the trailing statement is touched — earlier bare
/// expressions stay discarded statements.
fn rewrite_trailing_expr(program: &Program) -> (Program, bool) {
    match program.stmts.last() {
        Some(Stmt::Expr { expr, span }) => {
            let mut stmts = program.stmts.clone();
            *stmts.last_mut().expect("non-empty: matched last") = Stmt::Binding {
                mut_decl: true,
                name: REPL_VALUE.to_string(),
                name_span: *span,
                ty: None,
                value: expr.clone(),
                span: *span,
            };
            (
                Program {
                    stmts,
                    span: program.span,
                },
                true,
            )
        }
        _ => (program.clone(), false),
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
    /// `sleep(ms)` — a leaf timer future (Track A.2) ready once the executor clock reaches
    /// `now + ms`. The first future that can report `Pending`.
    Sleep,
    /// `all(list)` — await every future concurrently, returning their results as a `List<T>` in
    /// order (Track A.9).
    All,
    /// `race(list)` — await concurrently, returning the first result and cancelling the losers
    /// (Track A.9 + cooperative cancellation A.8).
    Race,
    /// `map_bounded(items, n, f)` — apply async `f` to each item, at most `n` concurrently, results
    /// as a `List<B>` in item order (Track A.9).
    MapBounded,
    /// `signal(v)` — create a reactive cell holding `v` (reactivity S1); the tree-walker twin of the
    /// VM's `Builtin::Signal`. Returns a `Signal<T>` handle read/updated via the interpreter's graph.
    Signal,
    /// `computed(fn)` — create a lazy, memoized derivation (reactivity S3); the tree-walker twin of the
    /// VM's `Builtin::Computed`. Returns a `Computed<T>` whose `.get()` recomputes only when a
    /// dependency it read has changed.
    Computed,
    /// `effect(fn)` — register a side effect (reactivity S2); the tree-walker twin of the VM's
    /// `Builtin::Effect`. Runs `fn` immediately, tracks the signals it reads, and reruns on change.
    Effect,
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
            Builtin::Sleep => "sleep",
            Builtin::All => "all",
            Builtin::Race => "race",
            Builtin::MapBounded => "map_bounded",
            Builtin::Signal => "signal",
            Builtin::Computed => "computed",
            Builtin::Effect => "effect",
        }
    }

    /// The builtin behind a **virtual-module** function name (`use std.reactive.{signal}` →
    /// `Builtin::Signal`, prelude-redesign P2) — the names `registry::VIRTUAL_MODULES` exports.
    /// The bytecode compiler resolves the same names through its own `Builtin::from_name`.
    fn from_virtual_name(name: &str) -> Option<Builtin> {
        match name {
            "signal" => Some(Builtin::Signal),
            "computed" => Some(Builtin::Computed),
            "effect" => Some(Builtin::Effect),
            "sleep" => Some(Builtin::Sleep),
            "all" => Some(Builtin::All),
            "race" => Some(Builtin::Race),
            "map_bounded" => Some(Builtin::MapBounded),
            _ => None,
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
    /// Inherent + `impl`-block methods (the unified body, object-model slice 3), compiled to
    /// closures capturing the definition (global) scope — exactly like a struct/class's `methods`.
    /// An instance call `value.m(...)` and an associated call `Enum.f(...)` both resolve here; the
    /// distinction (a value receiver vs. a bare type-name receiver) is made at the call site.
    methods: HashMap<String, Rc<Closure>>,
}

impl EnumDef {
    pub fn name(&self) -> &str {
        &self.name
    }

    fn variant(&self, name: &str) -> Option<&VariantInfo> {
        self.variants.iter().find(|v| v.name == name)
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
    data: Vec<Value>,
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
            format!("{}.{}", self.enum_name, self.variant)
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
    methods: HashMap<String, Rc<Closure>>,
    /// The class's `destruct` block lowered to a parameterless Core-IR [`noeta_ir::Func`], if any —
    /// run by the runtime when the last reference to an instance drops (not directly callable),
    /// with the instance's fields and `self` bound into its scope. Shared so an `ObjectValue` can
    /// reach it.
    destructor: Option<Rc<noeta_ir::Func>>,
    /// Whether this came from a `struct X {...}` struct (vs. a `class`). Cosmetic in M0.
    is_struct: bool,
    /// Whether `==` on this type is **structural** (field-wise) rather than **reference identity**
    /// (object-model slice 2): true for a value `struct`/opaque, or a `class` that is `Equatable`
    /// (derives it or hand-`impl`s `eq`); false only for a plain `class` (`==` → identity). Mirrors
    /// the VM `Shape::structural_eq`, computed from the same inputs so both backends agree — even on
    /// a *nested* class field, where the method-dispatch path is unavailable.
    structural_eq: bool,
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
    def: Rc<TypeDef>,
    /// The field values in **slot order**, parallel to `def.fields` (`slots[i]` is the value of
    /// `def.fields[i]`). This mirrors the VM's `Payload::Object { shape, slots }` — the layout
    /// groundwork P-PACK Phase 1 needs — replacing the former name-keyed `BTreeMap`: a `Vec` index
    /// is a cache-friendly array slot, not a tree node, and the slot↔name map lives once on the
    /// shared `def`, not per instance. An opaque `use`-import is constructed with a per-literal `def`
    /// whose fields are the literal's keys in sorted order (matching the VM's opaque shape), so even
    /// dynamic-field imports fit the uniform slot model.
    slots: RefCell<Vec<Value>>,
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
        format!("{} {{{}}}", self.def.name, parts.join(", "))
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
    captured: Rc<Scope>,
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
    value: Value,
    mutable: bool,
}

/// A lexical scope: its own bindings plus a link to the enclosing scope. Reference-
/// counted with non-atomic `Rc` (shared-nothing per isolate, per the design). Cyclic
/// captures (a function holding the scope that captures it) form an `Rc` cycle that
/// refcounting alone cannot break; the migration's Phase-6 exit-time reaper reclaims
/// them by clearing the bindings of any captured scope still live after global teardown,
/// so heap residency reaches 0 on this backend (the leak oracle's gate).
struct Scope {
    vars: RefCell<HashMap<String, Binding>>,
    /// Binding names in declaration order, so the runtime can destroy them in reverse
    /// declaration order at scope exit — the deterministic destruction order the spec wants.
    order: RefCell<Vec<String>>,
    parent: Option<Rc<Scope>>,
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
        })
    }

    fn child(parent: &Rc<Scope>) -> Rc<Scope> {
        leak::inc();
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            parent: Some(Rc::clone(parent)),
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
}

/// A bounded channel's scheduler-owned state (isolates I.1): a FIFO queue of buffered messages, its
/// capacity, and whether it has been closed (all senders done). Endpoints (`Sender`/`Receiver`) are
/// just indices into the interpreter's `channels` table — the queue is never shared heap memory.
/// Mirrors the VM's `Channel`; both backends run the identical FIFO + block-on-full/empty logic, so
/// the differential holds by construction.
struct Channel {
    buffer: std::collections::VecDeque<Value>,
    capacity: usize,
    closed: bool,
}

/// One program's worth of evaluation state.
struct Interpreter {
    stdout: String,
    diagnostics: Vec<Diagnostic>,
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
    scopes: Vec<Vec<Task>>,
    /// The channel table (isolates I.1): every `channel::<T>(cap)` appends a [`Channel`]; endpoints
    /// reference one by index. Never cleared during a run (indices stay stable), like `scopes` it
    /// mirrors the VM exactly. `channel_progress` counts successful queue operations (a `send` push, a
    /// `recv` pop, or a `close`) so the scheduler distinguishes real channel progress from a stalled
    /// round — a channel op that unblocks a sibling is progress even when no task *completes*.
    channels: Vec<Channel>,
    channel_progress: u64,
    /// The reactive graph (reactivity S1): the tree-walker twin of the VM's `reactive` field. Every
    /// `signal(v)`/`computed(fn)`/`effect(fn)` allocates a node here; a `Reactive` handle references
    /// one by [`NodeId`]. Held behind `Rc` so a flush can borrow the graph and the interpreter
    /// independently (the graph's methods are `&self`, interior-mutable). The stored values are plain
    /// `Value`s — `Rc`-shared, so the graph's clones/drops are refcount-correct for free, no wrapper
    /// needed (unlike the VM's `GcVal`). Cleared at program end so held values drop.
    reactive: std::rc::Rc<noeta_reactive::ReactiveGraph<Value>>,
    /// The shared reflection artifact (attribute manifest + type registry), built from the program
    /// by the *same* `noeta_ast::reflect::build` the VM uses — so `attributes_of` materializes
    /// identical values in both backends. Populated at the start of `run`.
    reflection: noeta_ast::reflect::ReflectionInfo,
    /// The concrete static type the checker resolved for each `type_of(value)` site (keyed by the
    /// `Expr::TypeOf` span), harvested via `noeta_check::resolve_type_of_sites` from the *same*
    /// program the VM harvests — so both backends bake identical full-fidelity `Type` constants
    /// (`type_of` fidelity A, P2.3). A site absent here uses the runtime head-constructor path.
    type_of_sites: std::collections::HashMap<noeta_span::Span, noeta_ast::reflect::TypeRepr>,
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
}

impl Interpreter {
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
                methods: HashMap::new(),
            })),
            false,
        );
        Interpreter {
            stdout: String::new(),
            diagnostics: Vec::new(),
            globals: Rc::clone(&global),
            scope: global,
            host,
            executor,
            scopes: Vec::new(),
            channels: Vec::new(),
            channel_progress: 0,
            reactive: std::rc::Rc::new(noeta_reactive::ReactiveGraph::new()),
            reflection: noeta_ast::reflect::ReflectionInfo::default(),
            type_of_sites: std::collections::HashMap::new(),
            call_sites: Vec::new(),
            abort_trace: Vec::new(),
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
    fn iter_elements(&mut self, iterable: Value, span: Span) -> Eval<Vec<Value>> {
        match &iterable {
            Value::List(repr) => Ok((*repr.to_rc_vec()).clone()),
            // A set iterates in its canonical (sorted) order — deterministic, like the VM.
            Value::Set(items, _) => Ok((**items).clone()),
            // Iterating a map yields its values, in deterministic key order.
            Value::Map(entries, _) => Ok(entries.values().cloned().collect()),
            // A user object lights up the `Iterable` trait: `for x in o` iterates the list its
            // `iter` method returns.
            Value::Object(object) if object.def.methods.contains_key("iter") => {
                match self.call_method(iterable.clone(), "iter", Vec::new(), span)? {
                    Value::List(repr) => Ok((*repr.to_rc_vec()).clone()),
                    other => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`iter` must return a list, found {}", other.type_name()),
                    )),
                }
            }
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("cannot iterate over {}", other.type_name()),
            )),
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
        let is_std = path == ["std"];
        let selective_module = (path.len() == 2 && path[0] == "std")
            .then(|| path[1].as_str())
            .filter(|m| {
                noeta_stdlib::registry::find_module(m).is_some()
                    || noeta_stdlib::registry::is_virtual_module(m)
            });
        for imported in names {
            let value = if is_std
                && (noeta_stdlib::registry::find_module(&imported.name).is_some()
                    || noeta_stdlib::registry::is_virtual_module(&imported.name))
            {
                Value::NativeModule(imported.name.clone())
            } else if let Some(module) = selective_module
                && noeta_stdlib::registry::virtual_module_function(module, &imported.name)
            {
                // A virtual-module member (`use std.reactive.{signal}`, P2a): the function IS a
                // builtin, so bind the first-class builtin value — the old prelude binding, gated.
                Value::Builtin(
                    Builtin::from_virtual_name(&imported.name)
                        .expect("every virtual-module function is a named builtin"),
                )
            } else if let Some(module) = selective_module
                && noeta_stdlib::registry::is_module_function(module, &imported.name)
            {
                Value::ModuleFn(module.to_string(), imported.name.clone())
            } else {
                Value::Type(Rc::new(TypeDef {
                    name: imported.name.clone(),
                    fields: Vec::new(),
                    methods: HashMap::new(),
                    destructor: None,
                    is_struct: false,
                    // An opaque import has no known fields; treat `==` structurally (matches the
                    // VM's `ShapeKind::Opaque` default), not as class identity.
                    structural_eq: true,
                    derives_comparable: false,
                    derives_tojson: false,
                    opaque: true,
                    field_defaults: Vec::new(),
                }))
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
    fn materialize_roles(&self) -> Value {
        let binding_def = Rc::new(fresh_type_def(
            "RoleBinding",
            &["target".to_string(), "role".to_string()],
            true,
        ));
        let items: Vec<Value> = self
            .reflection
            .roles
            .iter()
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
    /// (nested) type name is not a struct in scope, the field sets disagree, or the layout is empty.
    fn resolve_packed_schema(
        &self,
        layout: &noeta_ast::reflect::PackedLayout,
    ) -> Option<Rc<PackedSchema>> {
        use noeta_ast::reflect::PackedKind;

        let def = match self.scope.lookup(&layout.type_name) {
            Some(Value::Type(def)) => def,
            _ => return None,
        };
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
            def,
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
            return match object.def.methods.get(name) {
                Some(method) => {
                    self.call_method_on(&Rc::clone(object), &Rc::clone(method), args, span)
                }
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
                    return Ok(Value::ChannelSend(id, Rc::new(value)));
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
        // Reactive handle methods (reactivity S1/S2/S3): `signal.get()`/`.set(v)`/`.update(fn)`,
        // `computed.get()`, and `effect.dispose()` — the tree-walker twin of the VM's dispatch. Each
        // method is guarded by the node's `kind` (a `signal` is not disposable, a `computed` is
        // read-only, an `effect` is not readable); an invalid pair falls through to the generic
        // no-method runtime error below, exactly as any other unknown method on a built-in type.
        // `get` on a `computed` recomputes a dirty body via `read_reactive`; on a `signal` it is a
        // plain read (the callback never fires).
        if let Value::Reactive(kind, node) = &receiver {
            use noeta_reactive::NodeKind;
            let kind = *kind;
            let node = *node;
            match (kind, name) {
                (NodeKind::Signal | NodeKind::Computed, "get") => {
                    self.expect_std_arity(name, &args, 0, span)?;
                    return self.read_reactive(node, span);
                }
                (NodeKind::Signal, "set") => {
                    self.expect_std_arity(name, &args, 1, span)?;
                    let value = args.into_iter().next().unwrap();
                    self.reactive.set(node, value);
                    // Coalesce: a set inside a running effect body only enqueues — the ongoing flush
                    // picks it up. Only a top-level set drives a fresh flush. (reactivity S4)
                    if !self.reactive.is_flushing() {
                        self.drive_flush(span)?;
                    }
                    return Ok(Value::Unit);
                }
                (NodeKind::Signal, "update") => {
                    self.expect_std_arity(name, &args, 1, span)?;
                    // Read-modify-write: read the current value, call the updater with it, store the
                    // result, then flush (coalescing inside a running flush, like `set`).
                    let f = args.into_iter().next().unwrap();
                    let current = self.read_reactive(node, span)?;
                    let updated = self.call(f, vec![current], span)?;
                    self.reactive.set(node, updated);
                    if !self.reactive.is_flushing() {
                        self.drive_flush(span)?;
                    }
                    return Ok(Value::Unit);
                }
                (NodeKind::Effect, "dispose") => {
                    self.expect_std_arity(name, &args, 0, span)?;
                    self.reactive.dispose(node);
                    return Ok(Value::Unit);
                }
                _ => {}
            }
        }
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
        // Eager collection methods that reuse the prelude builtin impls (prelude-redesign P1):
        // `xs.map(f)` / `xs.filter(f)` / `xs.sum()` on a list. Routed through `call_builtin` with the
        // receiver as the first argument, so the method form and the (legacy) free-function form
        // `map(xs, f)` share exactly one implementation. A user object's own `map`/`filter`/`sum`
        // method wins — it is dispatched earlier, before this built-in fallback.
        if let Value::List(_) = &receiver
            && let Some(builtin) = match name {
                "map" if args.len() == 1 => Some(Builtin::Map),
                "filter" if args.len() == 1 => Some(Builtin::Filter),
                "sum" if args.is_empty() => Some(Builtin::Sum),
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

    /// `invoke(recv, name, args)` — fallible by-name dispatch (P2.6). Reuses the same name-keyed
    /// method tables as `call_method`, but **pre-checks** name resolution and arity so a miss is a
    /// runtime `Result.Err` rather than a recorded diagnostic. A panic *inside* the invoked body
    /// still aborts (the `?` propagation below), so only the by-name *resolution* is caught. The
    /// VM's `Op::Invoke` mirrors this exactly, building identical `Ok`/`Err` values.
    fn invoke_dynamic(
        &mut self,
        receiver: Value,
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
                self.expect_std_arity(name, args, 1, span)?;
                let separator = self.expect_std_string(name, &args[0], span)?.to_string();
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
                // or strings); otherwise there is no total order to sort by. A stable sort then
                // keeps equal elements in input order, matching the VM exactly.
                if items
                    .iter()
                    .any(|item| compare_primitive(&items[0], item).is_none())
                {
                    let error = noeta_stdlib::unorderable_error(name);
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                let mut sorted = items.to_vec();
                sorted.sort_by(|a, b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
                Ok(Value::list(sorted))
            }
            noeta_stdlib::ListMethod::Slice => {
                self.expect_std_arity(name, args, 2, span)?;
                let start = self.expect_std_int(name, &args[0], span)?;
                let end = self.expect_std_int(name, &args[1], span)?;
                let len = items.len();
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
        // A virtual module's functions (`reactive.signal(...)`, prelude-redesign P2) are builtins —
        // they need the executor/reactive graph the registry seam cannot reach — so a qualified call
        // intercepts here, ahead of registry dispatch, exactly like `fs.*_async`. Mirrors the VM.
        if noeta_stdlib::registry::is_virtual_module(module) {
            let Some(builtin) = noeta_stdlib::registry::virtual_module_function(module, func)
                .then(|| Builtin::from_virtual_name(func))
                .flatten()
            else {
                return Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("module `{module}` has no function `{func}`"),
                ));
            };
            return self.call_builtin(builtin, args.to_vec(), span);
        }
        // A function registered in the native-extension registry dispatches through the shared
        // seam: project arguments onto `NativeValue`, run the one shared dispatch body (host
        // threaded in), and materialize the `NativeOut` result (the result shape supplied from the
        // function's `RetTy`). Routing is per-function so a partially-migrated module (`vec`, whose
        // bulk `*_all` kernels stay per-backend) falls through for its unmigrated functions.
        let name = module;
        if let Some(sig) = noeta_stdlib::registry::find_function(name, func) {
            // A reflective module (`json`) marshals its arguments deeply (the recursive value tree
            // `json.stringify` introspects); every other module uses the cheap shallow projection.
            let deep = noeta_stdlib::registry::find_module(name).is_some_and(|m| m.deep_marshal);
            let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
                args.iter().map(value_to_native_deep).collect()
            } else {
                args.iter().map(marshal_native_arg).collect()
            };
            return match noeta_stdlib::registry::dispatch(name, func, &mut *self.host, &nargs) {
                // Async WORK (extern-types X5): ticket the descriptor on the executor and hand
                // back the leaf async-IO future — mirrors the VM.
                Ok(noeta_stdlib::NativeOut::Spawn(spawn)) => {
                    let id = self.executor.spawn_ext(&mut *self.host, spawn.0);
                    Ok(Value::AsyncIo(id))
                }
                Ok(out) => Ok(materialize_ext(out, sig.ret, args)),
                Err(error) => {
                    Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                }
            };
        }
        // `vec`'s bulk `*_all` kernels are the only unmigrated native functions and stay per-backend;
        // every other reachable name is registered, so anything else here is an unknown function.
        if module == "vec" {
            return self.call_vec(func, args, span);
        }
        let error = noeta_stdlib::no_function_error(name, func);
        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
    }

    /// The `vec` 3D-math module (P-PACK Phase 4.1): scalar Vec3 ops over structural 3-`f32` objects.
    /// The arithmetic lives in `noeta_stdlib::vec3`, so both backends compute identically; this is glue
    /// — read the components, dispatch, rebuild a same-shape result (mirrors the VM's `call_vec`).
    /// The `vec` module's **bulk** kernels over `List<Vec3<f32>>` (P-PACK 4.2). The scalar ops
    /// (`add`/`dot`/…) migrated to the shared native-extension dispatch; these stay per-backend
    /// (a packed-layout specialization, not a value-seam concern). Packed inputs take the flat
    /// autovectorized buffer path; a boxed/demoted operand falls back to a scalar loop.
    fn call_vec(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        use noeta_stdlib::vec3;
        match func {
            "add_all" | "sub_all" => {
                self.expect_std_arity(func, args, 2, span)?;
                // `add`/`sub` are element-wise over the flat `f32` array, so they are layout-agnostic:
                // the same `*_buffers` kernel on two column buffers yields the correct column result
                // (P-SIMD C3). Handle either layout uniformly — both operands must share it.
                if let (Some((schema, a)), Some((_, b))) =
                    (args[0].packed_vec3_any(), args[1].packed_vec3_any())
                {
                    if a.len() != b.len() {
                        return Err(self.vec_len_error(func, span));
                    }
                    let out = if func == "add_all" {
                        vec3::add_buffers(&a, &b)
                    } else {
                        vec3::sub_buffers(&a, &b)
                    };
                    return Ok(Value::packed_list_from(schema, out));
                }
                self.vec_bulk_binary_scalar(func, &args[0], &args[1], span)
            }
            "scale_all" => {
                self.expect_std_arity(func, args, 2, span)?;
                let s = self.read_scalar_f32(func, &args[1], span)?;
                // Layout-agnostic like `add`/`sub` — `scale_buffer` on column bytes is a column result.
                if let Some((schema, a)) = args[0].packed_vec3_any() {
                    return Ok(Value::packed_list_from(schema, vec3::scale_buffer(&a, s)));
                }
                self.vec_map_scalar(func, &args[0], span, |c| vec3::scale(c, s))
            }
            "dot_all" => {
                self.expect_std_arity(func, args, 2, span)?;
                // Column fast path (P-SIMD C3): read the three contiguous columns directly (no decode)
                // — `col_dot` autovectorizes and is bit-identical to the AoS reduction.
                if let (Some((_, a)), Some((_, b))) =
                    (args[0].packed_vec3_columns(), args[1].packed_vec3_columns())
                {
                    if a.len() != b.len() {
                        return Err(self.vec_len_error(func, span));
                    }
                    return Ok(f32_list(&vec3::col_dot(&a, &b)));
                }
                if let (Some((_, a)), Some((_, b))) =
                    (args[0].packed_vec3_data(), args[1].packed_vec3_data())
                {
                    if a.len() != b.len() {
                        return Err(self.vec_len_error(func, span));
                    }
                    return Ok(f32_list(&vec3::dot_buffers(&a, &b)));
                }
                self.vec_bulk_dot_scalar(func, &args[0], &args[1], span)
            }
            "length_all" => {
                self.expect_std_arity(func, args, 1, span)?;
                if let Some((_, a)) = args[0].packed_vec3_columns() {
                    return Ok(f32_list(&vec3::col_length(&a)));
                }
                if let Some((_, a)) = args[0].packed_vec3_data() {
                    return Ok(f32_list(&vec3::length_buffer(&a)));
                }
                let elems = self.expect_list(&args[0], func, span)?;
                let mut scalars = Vec::with_capacity(elems.len());
                for e in elems.iter() {
                    scalars.push(vec3::length(self.read_vec3(func, e, span)?));
                }
                Ok(f32_list(&scalars))
            }
            _ => {
                let error = noeta_stdlib::no_function_error("vec", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    fn vec_len_error(&mut self, func: &str, span: Span) -> Unwind {
        self.runtime_error(
            DiagnosticCode::TypeMismatch,
            span,
            format!("`vec.{func}` expects two lists of equal length"),
        )
    }

    /// Scalar fallback for `add_all`/`sub_all` on boxed/demoted operands.
    fn vec_bulk_binary_scalar(
        &mut self,
        func: &str,
        xs: &Value,
        ys: &Value,
        span: Span,
    ) -> Eval<Value> {
        use noeta_stdlib::vec3;
        let xe = self.expect_list(xs, func, span)?;
        let ye = self.expect_list(ys, func, span)?;
        if xe.len() != ye.len() {
            return Err(self.vec_len_error(func, span));
        }
        let mut out = Vec::with_capacity(xe.len());
        for (x, y) in xe.iter().zip(ye.iter()) {
            let a = self.read_vec3(func, x, span)?;
            let b = self.read_vec3(func, y, span)?;
            let c = if func == "add_all" {
                vec3::add(a, b)
            } else {
                vec3::sub(a, b)
            };
            out.push(build_vec3(x, c));
        }
        Ok(Value::list(out))
    }

    /// Scalar fallback for `dot_all`.
    fn vec_bulk_dot_scalar(
        &mut self,
        func: &str,
        xs: &Value,
        ys: &Value,
        span: Span,
    ) -> Eval<Value> {
        use noeta_stdlib::vec3;
        let xe = self.expect_list(xs, func, span)?;
        let ye = self.expect_list(ys, func, span)?;
        if xe.len() != ye.len() {
            return Err(self.vec_len_error(func, span));
        }
        let mut scalars = Vec::with_capacity(xe.len());
        for (x, y) in xe.iter().zip(ye.iter()) {
            let a = self.read_vec3(func, x, span)?;
            let b = self.read_vec3(func, y, span)?;
            scalars.push(vec3::dot(a, b));
        }
        Ok(f32_list(&scalars))
    }

    /// Map a component-wise unary op over a `List<Vec3>` — the `scale_all` scalar fallback.
    fn vec_map_scalar(
        &mut self,
        func: &str,
        list: &Value,
        span: Span,
        op: impl Fn([f32; 3]) -> [f32; 3],
    ) -> Eval<Value> {
        let elems = self.expect_list(list, func, span)?;
        let mut out = Vec::with_capacity(elems.len());
        for e in elems.iter() {
            let c = self.read_vec3(func, e, span)?;
            out.push(build_vec3(e, op(c)));
        }
        Ok(Value::list(out))
    }

    /// Read a Vec3 argument — a struct value with exactly three `f32` fields — into `[f32; 3]`
    /// (slot order), or a type error.
    fn read_vec3(&mut self, func: &str, value: &Value, span: Span) -> Eval<[f32; 3]> {
        if let Value::Object(obj) = value {
            let slots = obj.slots.borrow();
            if slots.len() == 3
                && let (Value::F32(x), Value::F32(y), Value::F32(z)) =
                    (&slots[0], &slots[1], &slots[2])
            {
                return Ok([*x, *y, *z]);
            }
        }
        Err(self.runtime_error(
            DiagnosticCode::TypeMismatch,
            span,
            format!(
                "`vec.{func}` expects a Vec3 (a struct of three f32 fields), found {}",
                value.type_name()
            ),
        ))
    }

    /// Read a numeric scalar (`f32`/`float`/`int`) as an `f32` — the `vec.scale` factor.
    fn read_scalar_f32(&mut self, func: &str, value: &Value, span: Span) -> Eval<f32> {
        match value {
            Value::F32(f) => Ok(*f),
            Value::Float(f) => Ok(*f as f32),
            Value::Int(i) => Ok(*i as f32),
            _ => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`vec.{func}` expects a number factor, found {}",
                    value.type_name()
                ),
            )),
        }
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
        let nargs: Vec<noeta_stdlib::NativeValue> = args.iter().map(marshal_native_arg).collect();
        // `cell` is an independent `Rc`, so borrowing it and `self.host` at once is fine (the
        // FileHandle discipline).
        let result = noeta_stdlib::registry::dispatch_method(
            &mut **cell.borrow_mut(),
            name,
            &mut *self.host,
            &nargs,
        );
        match result {
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
                        noeta_stdlib::MapKey::Extern(e) => {
                            Value::Extern(Rc::new(RefCell::new(e.clone())))
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
        }
    }

    /// Read a map-key argument (string or key-capable extern), raising the shared map-key error
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
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("{} is not callable", other.type_name()),
            )),
        }
    }

    /// Read a reactive node, driving a `computed`'s recompute through [`call`](Self::call) if it is
    /// dirty (reactivity S3) — the tree-walker twin of the VM's `Vm::read_reactive`. A `signal` read
    /// never enters the callback (nothing to run); a dirty `computed` runs its body (and, transitively,
    /// any dirty computeds it reads), memoizing as it goes. The graph is cloned out as an `Rc` so its
    /// `&self` `read` can borrow it while the callback borrows `self` for `call`. A body that aborts is
    /// captured deterministically — the first abort stops further recomputes and propagates.
    fn read_reactive(&mut self, node: noeta_reactive::NodeId, span: Span) -> Eval<Value> {
        let graph = std::rc::Rc::clone(&self.reactive);
        let mut abort: Option<Unwind> = None;
        let value = graph.read(node, &mut |body: Value| -> Value {
            if abort.is_some() {
                return Value::Unit;
            }
            match self.call(body, Vec::new(), span) {
                Ok(value) => value,
                Err(unwind) => {
                    abort = Some(unwind);
                    Value::Unit
                }
            }
        });
        match abort {
            Some(unwind) => Err(unwind),
            None => Ok(value),
        }
    }

    /// Run the reactive graph's pending effects to a fixpoint (reactivity S2) — the tree-walker twin of
    /// the VM's `Vm::drive_flush`. Invokes each effect body through [`call`](Self::call). The graph is
    /// cloned out as an `Rc` so its `&self` `flush` can borrow it while the run callback borrows `self`
    /// for `call`. The first effect body to abort (by the deterministic flush order) stops further
    /// bodies and propagates, identically to the VM.
    fn drive_flush(&mut self, span: Span) -> Eval<()> {
        let graph = std::rc::Rc::clone(&self.reactive);
        let mut abort: Option<Unwind> = None;
        let overflowed = graph
            .flush(&mut |body: Value| -> Value {
                if abort.is_some() {
                    return Value::Unit;
                }
                match self.call(body, Vec::new(), span) {
                    Ok(value) => value,
                    Err(unwind) => {
                        abort = Some(unwind);
                        Value::Unit
                    }
                }
            })
            .is_err();
        // A body-driven abort (panic / `?`) takes priority — surface it as itself. Otherwise, a
        // non-converging flush (a self-reinforcing effect) becomes the reactive-cycle runtime error.
        if let Some(unwind) = abort {
            return Err(unwind);
        }
        if overflowed {
            return Err(self.runtime_error(
                DiagnosticCode::ReactiveCycle,
                span,
                format!(
                    "reactive update did not converge after {} steps — an effect keeps changing a \
                     signal it depends on",
                    noeta_reactive::MAX_FLUSH_STEPS
                ),
            ));
        }
        Ok(())
    }

    fn call_closure(&mut self, closure: &Rc<Closure>, args: Vec<Value>, span: Span) -> Eval<Value> {
        let required = required_count(&closure.defaults);
        if args.len() < required || args.len() > closure.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("function", required, closure.params.len(), args.len()),
            ));
        }
        let supplied = args.len();
        let call_scope = Scope::child(&closure.captured);
        for (param, arg) in closure.params.iter().zip(args) {
            call_scope.declare(param.clone(), arg, false);
        }
        // Fill any omitted trailing parameters from their defaults, each evaluated in the closure's
        // captured (definition/global) scope — never seeing the call's other arguments.
        for i in supplied..closure.params.len() {
            let value = self.eval_default(closure, i)?;
            call_scope.declare(closure.params[i].clone(), value, false);
        }
        // Shadow the call for the abort traceback: (callee name, call-site span). Popped on every
        // exit — an abort's trace is snapshotted deeper, at the diagnostic (see `record_abort_trace`).
        self.call_sites.push((closure.name.clone(), span));
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = self.exec_ir_fn_body(&closure.body);
        self.scope = saved;
        self.call_sites.pop();
        catch_return(result)
    }

    /// Call an instance method: the receiver's fields are bound directly into the method
    /// scope (so `total()` can reference `items` without a `self.` prefix), and `self` is
    /// also bound to the whole object. Parameters bind last, so they win over a field of
    /// the same name.
    fn call_method_on(
        &mut self,
        object: &Rc<ObjectValue>,
        method: &Rc<Closure>,
        args: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        let required = required_count(&method.defaults);
        if args.len() < required || args.len() > method.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("method", required, method.params.len(), args.len()),
            ));
        }
        let supplied = args.len();
        let call_scope = Scope::child(&method.captured);
        // Bind only `self` — fields are **not** snapshotted into the scope. A bare field read
        // resolves live off `self` (see `eval_ir_atom`), mirroring the VM (which loads fields off the
        // receiver register, never a copy), so a field mutated mid-method — including through an alias
        // — is observed by a later bare read. A bare *write* `n = v` therefore declares a local (the
        // name is not in scope); mutating a field is the explicit `self.f = v`.
        call_scope.declare("self".to_string(), Value::Object(Rc::clone(object)), false);
        for (param, arg) in method.params.iter().zip(args) {
            call_scope.declare(param.clone(), arg, false);
        }
        // Omitted trailing parameters take their defaults, evaluated in the method's captured
        // (definition/global) scope — not against the receiver's fields, `self`, or other arguments.
        for i in supplied..method.params.len() {
            let value = self.eval_default(method, i)?;
            call_scope.declare(method.params[i].clone(), value, false);
        }
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
        let required = required_count(&method.defaults);
        if args.len() < required || args.len() > method.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("method", required, method.params.len(), args.len()),
            ));
        }
        let supplied = args.len();
        let call_scope = Scope::child(&method.captured);
        call_scope.declare("self".to_string(), receiver, false);
        for (param, arg) in method.params.iter().zip(args) {
            call_scope.declare(param.clone(), arg, false);
        }
        for i in supplied..method.params.len() {
            let value = self.eval_default(method, i)?;
            call_scope.declare(method.params[i].clone(), value, false);
        }
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
            }
            // `sleep(ms)` — a leaf timer future (Track A.2). Its deadline is fixed at creation from the
            // current logical clock; awaiting it advances the clock to that deadline. A negative or
            // non-int `ms` is a `TypeMismatch` (checked identically in the VM).
            Builtin::Sleep => {
                self.expect_arity(builtin, &args, 1, span)?;
                let Value::Int(ms) = args[0] else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sleep` expects an int (ms), found {}", args[0].display()),
                    ));
                };
                if ms < 0 {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sleep` expects a non-negative duration, found {ms}"),
                    ));
                }
                Ok(Value::Timer(self.executor.now() + ms as u64))
            }
            // `all(list)` — await every future concurrently, results as a `List<T>` in order (A.9).
            Builtin::All => {
                self.expect_arity(builtin, &args, 1, span)?;
                let Value::List(repr) = &args[0] else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`all` expects a list of futures, found {}",
                            args[0].type_name()
                        ),
                    ));
                };
                let handles: Vec<Value> = (0..repr.len())
                    .map(|i| repr.get(i).expect("in bounds"))
                    .collect();
                let n = handles.len();
                let mut results: Vec<Option<Value>> = vec![None; n];
                loop {
                    for i in 0..n {
                        if results[i].is_none()
                            && let Some(v) = self.poll_once(&handles[i], span)?
                        {
                            results[i] = Some(v);
                        }
                    }
                    if results.iter().all(Option::is_some) {
                        let items: Vec<Value> =
                            results.into_iter().map(|r| r.expect("all ready")).collect();
                        return Ok(Value::list(items));
                    }
                    let progressed = self.poll_all_scopes_round(span)?;
                    if !progressed && self.executor.advance().is_none() {
                        return Err(self.runtime_error(
                            DiagnosticCode::Panic,
                            span,
                            "async deadlock: `all` awaited futures with no pending timers"
                                .to_string(),
                        ));
                    }
                }
            }
            // `race(list)` — await concurrently, first result wins, losers cancelled (A.9 + A.8).
            Builtin::Race => {
                self.expect_arity(builtin, &args, 1, span)?;
                let Value::List(repr) = &args[0] else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`race` expects a list of futures, found {}",
                            args[0].type_name()
                        ),
                    ));
                };
                let handles: Vec<Value> = (0..repr.len())
                    .map(|i| repr.get(i).expect("in bounds"))
                    .collect();
                let n = handles.len();
                if n == 0 {
                    return Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        "`race` requires at least one future".to_string(),
                    ));
                }
                loop {
                    for i in 0..n {
                        if let Some(v) = self.poll_once(&handles[i], span)? {
                            for (j, hj) in handles.iter().enumerate() {
                                if j != i {
                                    self.cancel_task(hj);
                                }
                            }
                            return Ok(v);
                        }
                    }
                    let progressed = self.poll_all_scopes_round(span)?;
                    if !progressed && self.executor.advance().is_none() {
                        return Err(self.runtime_error(
                            DiagnosticCode::Panic,
                            span,
                            "async deadlock: `race` awaited futures with no pending timers"
                                .to_string(),
                        ));
                    }
                }
            }
            // `map_bounded(items, n, f)` — apply async `f` to each item, at most `n` in flight, results
            // in item order (Track A.9). The tree-walker mirror of the VM's sliding window.
            Builtin::MapBounded => {
                self.expect_arity(builtin, &args, 3, span)?;
                let Value::List(repr) = &args[0] else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`map_bounded` expects a list, found {}",
                            args[0].type_name()
                        ),
                    ));
                };
                let Value::Int(limit) = args[1] else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`map_bounded` expects an int concurrency limit, found {}",
                            args[1].type_name()
                        ),
                    ));
                };
                let items: Vec<Value> = (0..repr.len())
                    .map(|i| repr.get(i).expect("in bounds"))
                    .collect();
                let f = args[2].clone();
                let window = limit.max(1) as usize;
                let count = items.len();
                let mut results: Vec<Option<Value>> = vec![None; count];
                let mut in_flight: Vec<(usize, Value)> = Vec::new();
                let mut next = 0usize;
                let mut done = 0usize;
                loop {
                    while in_flight.len() < window && next < count {
                        let fut = self.call(f.clone(), vec![items[next].clone()], span)?;
                        in_flight.push((next, fut));
                        next += 1;
                    }
                    if done == count {
                        let out: Vec<Value> =
                            results.into_iter().map(|r| r.expect("all done")).collect();
                        return Ok(Value::list(out));
                    }
                    let mut progressed = false;
                    let mut k = 0;
                    while k < in_flight.len() {
                        let (idx, fut) = (in_flight[k].0, in_flight[k].1.clone());
                        if let Some(v) = self.poll_once(&fut, span)? {
                            results[idx] = Some(v);
                            in_flight.remove(k);
                            done += 1;
                            progressed = true;
                        } else {
                            k += 1;
                        }
                    }
                    if !self.scopes.is_empty() {
                        progressed |= self.poll_all_scopes_round(span)?;
                    }
                    if !progressed && self.executor.advance().is_none() {
                        return Err(self.runtime_error(
                            DiagnosticCode::Panic,
                            span,
                            "async deadlock: `map_bounded` stalled with no pending timers"
                                .to_string(),
                        ));
                    }
                }
            }
            Builtin::Signal => {
                self.expect_arity(builtin, &args, 1, span)?;
                // `signal(v)` — allocate a reactive cell holding `v`. `args` is owned here, so the
                // value moves straight into the graph (an `Rc` clone keeps it live).
                let value = args.into_iter().next().unwrap();
                let id = self.reactive.signal(value);
                Ok(Value::Reactive(noeta_reactive::NodeKind::Signal, id))
            }
            Builtin::Computed => {
                self.expect_arity(builtin, &args, 1, span)?;
                // `computed(fn)` — register a lazy derivation, storing the body closure. It is created
                // dirty and computes on first `.get()`; no flush now (nothing eager runs).
                let body = args.into_iter().next().unwrap();
                let id = self.reactive.computed(body);
                Ok(Value::Reactive(noeta_reactive::NodeKind::Computed, id))
            }
            Builtin::Effect => {
                self.expect_arity(builtin, &args, 1, span)?;
                // `effect(fn)` — register the effect (created queued), storing the body closure, then
                // flush to run it once now (subscribing it to the signals it reads). If we are already
                // inside a flush (an effect created within another effect's body), the ongoing flush
                // drains it — do not nest. (reactivity S4)
                let body = args.into_iter().next().unwrap();
                let id = self.reactive.effect(body);
                if !self.reactive.is_flushing() {
                    self.drive_flush(span)?;
                }
                Ok(Value::Reactive(noeta_reactive::NodeKind::Effect, id))
            }
        }
    }

    /// Poll a future once (Track A.3 — the tree-walker twin of the VM's `Vm::poll_once`). A leaf timer
    /// is ready once the executor clock reaches its deadline, else it registers the deadline and reports
    /// `None` (pending). A step future's poll runs the state machine to its next suspend: the step
    /// returns the raw completion value (ready) or the pending sentinel (`None`). A non-future passes
    /// through as ready (totality for the uncheck­ed property test).
    fn poll_once(&mut self, future: &Value, span: Span) -> Eval<Option<Value>> {
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
            // `tx.send(v)` (isolates I.1): enqueue when the buffer has room (ready → unit), else
            // suspend (pending) until a `recv` frees a slot. Sending on a closed channel is a bug
            // (E0010) — the receiver would never see it.
            Value::ChannelSend(id, value) => {
                let id = *id;
                let chan = &self.channels[id.index()];
                if chan.closed {
                    return Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        "cannot send on a closed channel".to_string(),
                    ));
                }
                if chan.buffer.len() < chan.capacity {
                    let value = (**value).clone();
                    self.channels[id.index()].buffer.push_back(value);
                    self.channel_progress += 1;
                    Ok(Some(Value::Unit))
                } else {
                    Ok(None)
                }
            }
            // `rx.recv()` (isolates I.1): dequeue the next message (ready → `some(v)`), yield `none`
            // once the channel is closed and drained, else suspend (pending) on an empty open buffer.
            Value::ChannelRecv(id) => {
                let id = *id;
                if let Some(value) = self.channels[id.index()].buffer.pop_front() {
                    self.channel_progress += 1;
                    Ok(Some(builtin_enum("Option", "some", vec![value])))
                } else if self.channels[id.index()].closed {
                    Ok(Some(builtin_enum("Option", "none", vec![])))
                } else {
                    Ok(None)
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
                if task.result.is_none() && !task.cancelled {
                    let future = task.future.clone();
                    if let Some(value) = self.poll_once(&future, span)? {
                        self.scopes[si][ti].result = Some(value);
                        completed = true;
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

    /// Join the innermost scope (Track A.3b): drive tasks round-robin until the innermost scope's tasks
    /// all complete. Each round polls **all** open scopes (A.7) so an outer scope's siblings interleave
    /// with the inner join; the loop exits on the innermost scope alone. On a round where nothing
    /// completed, advance the logical clock; a pending scope with no timer to advance is a deterministic
    /// deadlock.
    fn join_scope(&mut self, span: Span) -> Eval<()> {
        let si = self.scopes.len() - 1;
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
        loop {
            let before = self.channel_progress;
            if let Some(value) = self.poll_once(&future, span)? {
                return Ok(value);
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
        }
        match ops::apply_binary(op, &left, &right) {
            Ok(value) => Ok(value),
            Err(error) => Err(self.runtime_error(error.code, span, error.text)),
        }
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

/// A boxed `List<f32>` from scalar results (the output of `dot_all`/`length_all`).
fn f32_list(scalars: &[f32]) -> Value {
    Value::list(scalars.iter().map(|&f| Value::F32(f)).collect())
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

/// Build a Vec3 result with the same type (shape/`def`) as `like`, from three `f32` components
/// (P-PACK Phase 4). The caller (`call_vec`) has verified `like` is a 3-`f32` object.
fn build_vec3(like: &Value, c: [f32; 3]) -> Value {
    let Value::Object(obj) = like else {
        unreachable!("read_vec3 verified an object")
    };
    Value::Object(Rc::new(ObjectValue::new(
        Rc::clone(&obj.def),
        vec![Value::F32(c[0]), Value::F32(c[1]), Value::F32(c[2])],
    )))
}

/// Construct a built-in `Result`/`Option`/`Ordering` value (`Ok`/`Err`/`some`/`none`, or
/// `Ordering.Less`/`Equal`/`Greater`). These reuse the ordinary [`EnumValue`] representation, so
/// they participate in `match` and equality like any enum; only `Result`/`Option`'s display and
/// the `?`/`??` operators treat them specially.
fn builtin_enum(enum_name: &str, variant: &str, data: Vec<Value>) -> Value {
    Value::Enum(Rc::new(EnumValue {
        enum_name: enum_name.to_string(),
        variant: variant.to_string(),
        data,
        reflect: None,
    }))
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
        Value::List(repr) => repr
            .reflect()
            .map(|r| (*r).clone())
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
        // An extern-type value reflects as its registered nominal type (`Uuid`), mirroring the
        // checker's `Type::Named` for it.
        Value::Extern(e) => TypeRepr::Named(e.borrow().type_name().to_string(), Vec::new()),
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
        | Value::Reactive(..)
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
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Bytes
        | TypeRepr::Unit
        | TypeRepr::Dyn => Vec::new(),
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
        A::TypeRef(name) => build_type_value(&reflection.type_ref_repr(name)),
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
            let head_ok = match name.as_str() {
                "int" => matches!(value, Value::Int(_)),
                "float" => matches!(value, Value::Float(_)),
                "bool" => matches!(value, Value::Bool(_)),
                "string" => matches!(value, Value::Str(_)),
                "bytes" => matches!(value, Value::Bytes(_)),
                "void" | "unit" => matches!(value, Value::Unit),
                // Narrowing to the open top is a no-op: every value is a `dyn`.
                "dyn" | "Any" => true,
                "List" | "list" => matches!(value, Value::List(_)),
                "Map" | "map" => matches!(value, Value::Map(..)),
                "Set" | "set" => matches!(value, Value::Set(..)),
                // Abstract kind-types match any value of that declaration kind (structs and classes are
                // both `Object`s, told apart by `TypeDef::is_struct`).
                "Enum" => matches!(value, Value::Enum(_)),
                "Struct" => matches!(value, Value::Object(o) if o.def.is_struct),
                "Class" => matches!(value, Value::Object(o) if !o.def.is_struct),
                // `Option`/`Result` are enums whose shape name is the type name, like a user
                // enum; an extern-type value matches its registered type name (`x is Uuid`).
                other => match value {
                    Value::Object(object) => object.def.name() == other,
                    Value::Enum(enum_value) => enum_value.enum_name == other,
                    Value::Extern(e) => e.borrow().type_name() == other,
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
                    .map(noeta_ast::reflect::typeref_to_repr)
                    .collect();
                noeta_ast::reflect::narrow_args_match(&target, &eval_type_repr(value))
            } else {
                head_ok
            }
        }
    }
}

/// Extract the owned [`noeta_stdlib::MapKey`] a map operation keys by: a string, or a
/// key-capable extern value (a boxed snapshot — extern-types X4). `None` for anything else;
/// the caller raises the shared map-key error. Mirrors the VM's key extraction.
pub(crate) fn value_map_key(value: &Value) -> Option<noeta_stdlib::MapKey> {
    match value {
        Value::Str(s) => Some(noeta_stdlib::MapKey::from(s.as_str())),
        Value::Extern(e) if noeta_stdlib::map_key::extern_key_capable(&**e.borrow()) => {
            Some(noeta_stdlib::MapKey::Extern(e.borrow().clone()))
        }
        _ => None,
    }
}

/// Project a tree-walker `Value` onto the native-extension registry's argument view. One of the
/// two functions (with [`materialize_native`]) that form the backend's half of the value seam;
/// every migrated module call goes through these rather than a per-function `read_*`. The
/// scalar/host modules use only the scalar and string shapes; richer shapes are added as the
/// modules that need them migrate. Mirrors the VM-side projection.
fn marshal_native_arg(value: &Value) -> noeta_stdlib::NativeValue {
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
        other => NativeValue::Opaque(other.type_name()),
    }
}

/// Project a primitive tree-walker value onto a [`noeta_stdlib::Scalar`], or `None` if not primitive.
fn value_to_scalar(value: &Value) -> Option<noeta_stdlib::Scalar> {
    use noeta_stdlib::Scalar;
    Some(match value {
        Value::Int(n) => Scalar::Int(*n),
        Value::Float(f) => Scalar::Float(*f),
        Value::F32(f) => Scalar::F32(*f),
        Value::Bool(b) => Scalar::Bool(*b),
        _ => return None,
    })
}

fn scalar_to_value(scalar: noeta_stdlib::Scalar) -> Value {
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
fn materialize_native(out: noeta_stdlib::NativeOut) -> Value {
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
/// VM's `compare_field`), anything else goes through [`compare_primitive`]. `None` is an
/// incomparable pairing, which the caller turns into a runtime type error.
fn compare_field(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match a {
        Value::Object(la) => object_structural_compare(la, b),
        _ => compare_primitive(a, b),
    }
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
/// Mirrors the VM's [`noeta_value::Value::to_native_deep`] so both backends agree.
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
        Value::Reactive(kind, _) => {
            NativeValue::Str(format!("<{}>", kind.type_name().to_ascii_lowercase()))
        }
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
        // Extern-type values order through their contract (extern-types X1) — a total order per
        // key-capable kind; `None` for unordered kinds. Mirrors the VM's `compare_primitive`.
        (Value::Extern(a), Value::Extern(b)) => a.borrow().cmp_value(&**b.borrow()),
        _ => {
            let num = |v: &Value| match v {
                Value::Int(i) => Some(*i as f64),
                Value::Float(f) => Some(*f),
                _ => None,
            };
            num(left)?.partial_cmp(&num(right)?)
        }
    }
}

/// Build a set's canonical form from `items`: every element must be mutually orderable (a single
/// orderable primitive — int, float, or string); the result is sorted and de-duplicated. Returns
/// `None` if any element is non-orderable or of a different kind, so the caller raises the shared
/// unorderable error. Mirrors the VM's `canonical_set` so both backends build identical sets.
fn canonical_set(items: &[Value]) -> Option<Vec<Value>> {
    if items.is_empty() {
        return Some(Vec::new());
    }
    if items
        .iter()
        .any(|item| compare_primitive(&items[0], item).is_none())
    {
        return None;
    }
    let mut canonical = items.to_vec();
    canonical.sort_by(|a, b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
    canonical.dedup_by(|a, b| compare_primitive(a, b) == Some(std::cmp::Ordering::Equal));
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
        TreeWalkBackend::new().run(&program_of(text))
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
