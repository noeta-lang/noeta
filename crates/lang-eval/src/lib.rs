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
use std::rc::Rc;

use lang_ast::{
    BinaryOp, ClassDecl, EnumDecl, Expr, FnDecl, ForPattern, MatchArm, ObjectLit, Pattern, Program,
    RecordDecl, Stmt, StrPart, TypeRef, UnaryOp,
};
use lang_builtins::IdGen;
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::Span;

mod ops;
mod value;
pub use value::Value;

// The `Backend`/`RunResult` seam moved into its own crate in M1 so the tree-walker and the
// bytecode VM are siblings (neither depends on the other). Re-exported here so existing
// `lang_eval::{Backend, RunResult}` users keep working.
pub use lang_backend::{Backend, RunResult};

/// The default seed for the deterministic id source, so output is reproducible.
const DEFAULT_SEED: u64 = 1;

/// The M0 tree-walking interpreter, exposed as a [`Backend`].
#[derive(Debug, Clone)]
pub struct TreeWalkBackend {
    seed: u64,
}

impl TreeWalkBackend {
    pub fn new() -> TreeWalkBackend {
        TreeWalkBackend { seed: DEFAULT_SEED }
    }

    /// Use a specific seed for the id source (tests pin this for reproducibility).
    pub fn with_seed(seed: u64) -> TreeWalkBackend {
        TreeWalkBackend { seed }
    }

    /// Run against a caller-provided [`lang_stdlib::Host`] (M2.3) instead of the deterministic
    /// sandbox. The CLI uses this to give `lang run` the real host (real `env`/`args`, real disk);
    /// the [`Backend::run`] path keeps the sandbox so the conformance differential stays
    /// deterministic.
    pub fn run_with_host(&self, program: &Program, host: Box<dyn lang_stdlib::Host>) -> RunResult {
        Interpreter::with_host(self.seed, host).run(program)
    }

    /// Run against the deterministic sandbox using a **precomputed** `type_of` site map instead
    /// of re-deriving it. An orchestrator that already type-checked the program (holding a
    /// [`lang_check::Checked`]) threads its `type_of_sites` here so the checker is not re-run for
    /// the eval backend. Behavior-identical to [`Backend::run`] — the map is a pure function of
    /// the program.
    pub fn run_with_sites(
        &self,
        program: &Program,
        type_of_sites: std::collections::HashMap<lang_span::Span, lang_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::new(self.seed).run_with_sites(program, type_of_sites)
    }

    /// As [`TreeWalkBackend::run_with_host`], but with a **precomputed** `type_of` site map (see
    /// [`TreeWalkBackend::run_with_sites`]). The CLI uses this to give `lang run` the real host
    /// while threading the map from its single type-check gate.
    pub fn run_with_host_sites(
        &self,
        program: &Program,
        host: Box<dyn lang_stdlib::Host>,
        type_of_sites: std::collections::HashMap<lang_span::Span, lang_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::with_host(self.seed, host).run_with_sites(program, type_of_sites)
    }
}

impl Default for TreeWalkBackend {
    fn default() -> TreeWalkBackend {
        TreeWalkBackend::new()
    }
}

impl Backend for TreeWalkBackend {
    fn run(&self, program: &Program) -> RunResult {
        Interpreter::new(self.seed).run(program)
    }
}

/// A persistent evaluation session — the REPL backend. Unlike [`TreeWalkBackend::run`],
/// which builds a fresh interpreter per program (the clean slate the differential oracle
/// needs), a session keeps its scope and id counter alive across [`Session::eval`] calls,
/// so bindings, `fn`/`type`/`enum`/`class` declarations, and `next_id()` continuity persist
/// between REPL entries.
pub struct Session {
    interp: Interpreter,
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
        Session {
            interp: Interpreter::new(DEFAULT_SEED),
        }
    }

    /// Evaluate a program against the persistent scope. Returns just this batch's stdout
    /// and diagnostics; if the final statement is a bare expression, its non-unit value's
    /// display form is returned in `value` so the REPL can echo it (`1 + 2` → `3`).
    pub fn eval(&mut self, program: &Program) -> SessionOutput {
        self.interp.stdout.clear();
        self.interp.diagnostics.clear();
        let mut value = None;
        let last = program.stmts.len().saturating_sub(1);
        for (i, stmt) in program.stmts.iter().enumerate() {
            // A trailing bare expression is evaluated for its value rather than discarded.
            if i == last
                && let Stmt::Expr { expr, .. } = stmt
            {
                if let Ok(v) = self.interp.eval_expr(expr)
                    && !matches!(v, Value::Unit)
                {
                    value = Some(v.display());
                }
                break;
            }
            match self.interp.exec_stmt(stmt) {
                Ok(Flow::Normal) => {}
                // `break`/`continue` cannot occur at the top level (the checker rejects them
                // outside a loop); treat as a no-op for exhaustiveness.
                Ok(Flow::Break) | Ok(Flow::Continue) => {}
                Ok(Flow::Return(_)) | Err(_) => break,
            }
        }
        SessionOutput {
            stdout: std::mem::take(&mut self.interp.stdout),
            diagnostics: std::mem::take(&mut self.interp.diagnostics),
            value,
        }
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
}

// --- Functions and scopes ---

/// A built-in (native) function from the prelude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `next_id()` — a deterministic, seeded counter.
    NextId,
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
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::NextId => "next_id",
            Builtin::Len => "len",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Sum => "sum",
            Builtin::MakeOk => "Ok",
            Builtin::MakeErr => "Err",
            Builtin::MakeSome => "some",
            Builtin::Panic => "panic",
        }
    }

    /// The prelude functions registered in every program's global scope. `none` is a
    /// prelude *value* (not a function), so it is bound separately in [`Interpreter::new`].
    const PRELUDE: &'static [Builtin] = &[
        Builtin::NextId,
        Builtin::Len,
        Builtin::Map,
        Builtin::Filter,
        Builtin::Sum,
        Builtin::MakeOk,
        Builtin::MakeErr,
        Builtin::MakeSome,
        Builtin::Panic,
    ];
}

/// The body of a user function: either a single arrow expression or a `{ ... }` block.
enum FnBody {
    Arrow(Expr),
    Block(Vec<Stmt>),
}

/// The definition of an enum type, registered as an `EnumType` value.
#[derive(Debug)]
pub struct EnumDef {
    name: String,
    variants: Vec<VariantInfo>,
}

impl EnumDef {
    pub fn name(&self) -> &str {
        &self.name
    }

    fn variant(&self, name: &str) -> Option<&VariantInfo> {
        self.variants.iter().find(|v| v.name == name)
    }
}

#[derive(Debug)]
struct VariantInfo {
    name: String,
    field_names: Vec<String>,
}

/// A constructed enum value.
#[derive(Debug, PartialEq)]
pub struct EnumValue {
    enum_name: String,
    variant: String,
    data: Vec<Value>,
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

/// The definition of a record or class type, registered as a [`Value::Type`].
///
/// Records and classes share one representation: a record is just a class with no
/// methods. `new`/`draft`/etc. are ordinary entries in `methods` (associated functions
/// returning the type); the distinction between an associated function and an instance
/// method is made at the call site (a type receiver vs. an instance receiver), not here.
pub struct TypeDef {
    name: String,
    fields: Vec<FieldSpec>,
    methods: HashMap<String, Rc<Closure>>,
    /// The class's `destruct` block, if any — run by the runtime when the last reference to an
    /// instance drops (not directly callable). Shared so an `ObjectValue` can reach it.
    destructor: Option<Rc<Vec<Stmt>>>,
    /// Whether this came from a `type X = {...}` record (vs. a `class`). Cosmetic in M0.
    is_record: bool,
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
}

impl TypeDef {
    pub fn name(&self) -> &str {
        &self.name
    }

    fn has_field(&self, name: &str) -> bool {
        self.fields.iter().any(|f| f.name == name)
    }
}

impl std::fmt::Debug for TypeDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Methods hold `Rc<Closure>` whose captured scope can be cyclic; never recurse.
        f.debug_struct("TypeDef")
            .field("name", &self.name)
            .field("is_record", &self.is_record)
            .field("opaque", &self.opaque)
            .finish_non_exhaustive()
    }
}

/// One declared field of a [`TypeDef`]: its name and whether it was declared `mut`.
/// `mutable` is recorded for M1 tooling (field-assignment checking); M0 objects are
/// immutable after construction, so it is not yet read.
struct FieldSpec {
    name: String,
    #[allow(dead_code)]
    mutable: bool,
}

/// A record or class instance: its type and the values of its fields. Immutable in M0
/// (`..` structural update produces a new object rather than mutating one).
pub struct ObjectValue {
    def: Rc<TypeDef>,
    fields: BTreeMap<String, Value>,
}

impl ObjectValue {
    /// `Type { field: value, ... }`. Fields are shown in declared order for records and
    /// classes; for an opaque imported stub (no declared fields) the actual bag is shown
    /// in key order.
    pub fn display(&self) -> String {
        let parts: Vec<String> = if self.def.opaque {
            self.fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", value.repr()))
                .collect()
        } else {
            self.def
                .fields
                .iter()
                .map(|f| {
                    let value = self
                        .fields
                        .get(&f.name)
                        .map(Value::repr)
                        .unwrap_or_default();
                    format!("{}: {value}", f.name)
                })
                .collect()
        };
        format!("{} {{{}}}", self.def.name, parts.join(", "))
    }
}

impl std::fmt::Debug for ObjectValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectValue({})", self.display())
    }
}

impl PartialEq for ObjectValue {
    fn eq(&self, other: &ObjectValue) -> bool {
        // Structural equality: same type name and equal fields (M0 records and classes).
        self.def.name == other.def.name && self.fields == other.fields
    }
}

/// A user function value: its parameter names, its body, and the lexical scope it was
/// defined in (captured for closures and recursion).
pub struct Closure {
    params: Vec<String>,
    /// Each parameter's default-value expression, parallel to `params` (`None` for a required
    /// parameter). A call that omits a trailing argument evaluates the matching default in the
    /// closure's `captured` (definition/global) scope — never seeing other parameters or fields,
    /// matching the VM's globals-only default thunks.
    defaults: Vec<Option<Expr>>,
    body: FnBody,
    captured: Rc<Scope>,
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
/// captures (a global function holding the global scope) leak until process exit in
/// M0; the planned cycle collector reclaims these in M1.
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

impl Scope {
    fn global() -> Rc<Scope> {
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            parent: None,
        })
    }

    fn child(parent: &Rc<Scope>) -> Rc<Scope> {
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            parent: Some(Rc::clone(parent)),
        })
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        if let Some(binding) = self.vars.borrow().get(name) {
            return Some(binding.value.clone());
        }
        self.parent.as_ref().and_then(|parent| parent.lookup(name))
    }

    fn declare(&self, name: String, value: Value, mutable: bool) {
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

/// One program's worth of evaluation state.
struct Interpreter {
    stdout: String,
    diagnostics: Vec<Diagnostic>,
    ids: IdGen,
    scope: Rc<Scope>,
    /// All host-coupled effects (filesystem, seeded PRNG, logical clock) behind the M2.1
    /// [`lang_stdlib::Host`] seam. Conformance constructs a deterministic
    /// [`lang_stdlib::SandboxHost`] so file IO, PRNG, and clock stay isolated and identical to
    /// the VM by construction; a real host (later M2 slices) swaps in without touching this struct.
    host: Box<dyn lang_stdlib::Host>,
    /// The shared reflection artifact (attribute manifest + type registry), built from the program
    /// by the *same* `lang_ast::reflect::build` the VM uses — so `attributes_of` materializes
    /// identical values in both backends. Populated at the start of `run`.
    reflection: lang_ast::reflect::ReflectionInfo,
    /// The concrete static type the checker resolved for each `type_of(value)` site (keyed by the
    /// `Expr::TypeOf` span), harvested via `lang_check::resolve_type_of_sites` from the *same*
    /// program the VM harvests — so both backends bake identical full-fidelity `Type` constants
    /// (`type_of` fidelity A, P2.3). A site absent here uses the runtime head-constructor path.
    type_of_sites: std::collections::HashMap<lang_span::Span, lang_ast::reflect::TypeRepr>,
}

impl Interpreter {
    fn new(seed: u64) -> Interpreter {
        Interpreter::with_host(seed, Box::new(lang_stdlib::SandboxHost::new()))
    }

    /// Build an interpreter against a caller-provided [`lang_stdlib::Host`] (M2.3). `new` uses the
    /// deterministic sandbox (what the differential needs); the CLI/REPL pass a real host here.
    fn with_host(seed: u64, host: Box<dyn lang_stdlib::Host>) -> Interpreter {
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
            })),
            false,
        );
        Interpreter {
            stdout: String::new(),
            diagnostics: Vec::new(),
            ids: IdGen::new(seed),
            scope: global,
            host,
            reflection: lang_ast::reflect::ReflectionInfo::default(),
            type_of_sites: std::collections::HashMap::new(),
        }
    }

    fn run(self, program: &Program) -> RunResult {
        let type_of_sites = lang_check::resolve_type_of_sites(program);
        self.run_with_sites(program, type_of_sites)
    }

    fn run_with_sites(
        mut self,
        program: &Program,
        type_of_sites: std::collections::HashMap<lang_span::Span, lang_ast::reflect::TypeRepr>,
    ) -> RunResult {
        self.reflection = lang_ast::reflect::build(program);
        self.type_of_sites = type_of_sites;
        for stmt in &program.stmts {
            match self.exec_stmt(stmt) {
                Ok(Flow::Normal) => {}
                // `break`/`continue` cannot occur at the top level (the checker rejects them
                // outside a loop); treat as a no-op for exhaustiveness.
                Ok(Flow::Break) | Ok(Flow::Continue) => {}
                // A top-level `return`, a `?` short-circuit, or a runtime error all stop
                // the program. A `?`-induced return records no diagnostic, so exit stays 0.
                Ok(Flow::Return(_)) | Err(Unwind::Return(_)) | Err(Unwind::Abort) => break,
            }
        }
        // Destroy the top-level bindings at program end, in reverse declaration order, running
        // each destructor on its last reference — the deterministic destruction the spec wants.
        self.destroy_globals();
        let exit_code = if self.diagnostics.is_empty() { 0 } else { 1 };
        RunResult {
            stdout: self.stdout,
            exit_code,
            diagnostics: self.diagnostics,
        }
    }

    /// Destroy the global scope's bindings in reverse declaration order.
    fn destroy_globals(&mut self) {
        for value in self.scope.drain_reverse() {
            self.destroy_value(value);
        }
    }

    /// Run an object's destructor if `value` is the last reference to a destructor-carrying
    /// instance, then let it drop. Mirrors the VM's `release_value`.
    fn destroy_value(&mut self, value: Value) {
        let Value::Object(obj) = &value else { return };
        if Rc::strong_count(obj) != 1 {
            return;
        }
        let Some(body) = obj.def.destructor.clone() else {
            return;
        };
        // Run the `destruct` block with the instance's fields and `self` in scope, like a
        // parameterless method. It runs for its effects; its control flow/errors are not part
        // of any expression's value, so they are swallowed at this boundary.
        let scope = Scope::child(&self.scope);
        for (name, field) in &obj.fields {
            scope.declare(name.clone(), field.clone(), false);
        }
        scope.declare("self".to_string(), value.clone(), false);
        let saved = std::mem::replace(&mut self.scope, scope);
        let _ = self.exec_stmts(&body);
        self.scope = saved;
        // `value` drops here; being the last reference, the object is freed.
    }

    fn exec_stmt(&mut self, stmt: &Stmt) -> Eval<Flow> {
        match stmt {
            Stmt::Echo { value, span } => {
                let v = self.eval_expr(value)?;
                let text = self.display_value(&v, *span)?;
                self.stdout.push_str(&text);
                self.stdout.push('\n');
                Ok(Flow::Normal)
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => {
                // Copy-on-write fast path for the self-append accumulator `acc ~= [x]` (which the
                // parser desugars to the reassignment `acc = acc ~ [x]`). The naive path copies the
                // whole left list every step — O(n²). Here we take the old value out of its scope
                // slot *before* evaluating the right-hand side, so (absent other aliases) we hold
                // the only reference and can append in place. Guarded so it only fires when it is
                // provably equivalent: a reassignment (`!mut_decl`) of a name that the RHS does not
                // itself mention (else `acc = acc ~ acc` would read the vacated slot), and a live
                // mutable binding. Aliasing stays correct by construction — a second reference
                // (`b = acc`) keeps the refcount > 1, so `cow_concat` copies, preserving immutable
                // semantics. Any case the guard rejects falls through to the ordinary path.
                if !*mut_decl
                    && let Expr::Binary {
                        op: BinaryOp::Concat,
                        lhs,
                        rhs,
                        ..
                    } = value
                    && let Expr::Ident { name: lhs_name, .. } = lhs.as_ref()
                    && lhs_name == name
                    && !rhs.mentions(name)
                    && let Some(old) = self.scope.take_mut(name)
                {
                    let right = match self.eval_expr(rhs) {
                        Ok(right) => right,
                        // Restore the binding before unwinding so the vacated slot is never observed.
                        Err(unwind) => {
                            self.scope.assign(name, old);
                            return Err(unwind);
                        }
                    };
                    self.scope.assign(name, cow_concat(old, right));
                    return Ok(Flow::Normal);
                }
                let value = self.eval_expr(value)?;
                self.bind(*mut_decl, name, *name_span, value)?;
                Ok(Flow::Normal)
            }
            Stmt::Fn(decl) => {
                self.declare_fn(decl);
                Ok(Flow::Normal)
            }
            Stmt::Enum(decl) => {
                self.declare_enum(decl);
                Ok(Flow::Normal)
            }
            Stmt::Record(decl) => {
                self.declare_record(decl);
                Ok(Flow::Normal)
            }
            Stmt::Class(decl) => {
                self.declare_class(decl);
                Ok(Flow::Normal)
            }
            // A standalone `impl Trait for T {}` is a compile-time capability declaration
            // (validated by the checker); a marker/capability impl has no runtime effect.
            Stmt::Impl(_) => Ok(Flow::Normal),
            // `namespace` is a no-op in M0 (no module scoping yet); `use` registers each
            // imported name as an opaque stub so references resolve.
            Stmt::Namespace { .. } => Ok(Flow::Normal),
            Stmt::Use { path, names, .. } => {
                self.declare_use(path, names);
                Ok(Flow::Normal)
            }
            Stmt::Return { value, .. } => {
                let value = match value {
                    Some(expr) => self.eval_expr(expr)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(value))
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                let taken = match self.eval_expr(cond)? {
                    Value::Bool(b) => b,
                    other => {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`if` condition must be a bool, found {}", other.type_name()),
                        ));
                    }
                };
                if taken {
                    self.exec_block(then_body)
                } else if let Some(else_body) = else_body {
                    self.exec_block(else_body)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => self.exec_for(pattern, iterable, body, *span),
            Stmt::While { cond, body, span } => self.exec_while(cond, body, *span),
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
            Stmt::Expr { expr, .. } => {
                self.eval_expr(expr)?;
                Ok(Flow::Normal)
            }
        }
    }

    /// Run a block of statements in a fresh child scope, propagating any `return`.
    fn exec_block(&mut self, stmts: &[Stmt]) -> Eval<Flow> {
        let child = Scope::child(&self.scope);
        let saved = std::mem::replace(&mut self.scope, child);
        let result = self.exec_stmts(stmts);
        self.scope = saved;
        result
    }

    /// Execute statements in the current scope, stopping at the first non-local flow (`return`,
    /// `break`, `continue`) and propagating it to the enclosing function or loop.
    fn exec_stmts(&mut self, stmts: &[Stmt]) -> Eval<Flow> {
        for stmt in stmts {
            let flow = self.exec_stmt(stmt)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_for(
        &mut self,
        pattern: &ForPattern,
        iterable: &Expr,
        body: &[Stmt],
        span: Span,
    ) -> Eval<Flow> {
        let iterable_value = self.eval_expr(iterable)?;
        let elements = match &iterable_value {
            Value::List(items) => (**items).clone(),
            // A set iterates in its canonical (sorted) order — deterministic, like the VM.
            Value::Set(items) => (**items).clone(),
            // Iterating a map yields its values, in deterministic key order.
            Value::Map(entries) => entries.values().cloned().collect(),
            // A user object lights up the `Iterable` trait: `for x in o` iterates the list its
            // `iter` method returns.
            Value::Object(object) if object.def.methods.contains_key("iter") => {
                match self.call_method(iterable_value.clone(), "iter", Vec::new(), span)? {
                    Value::List(items) => (*items).clone(),
                    other => {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!("`iter` must return a list, found {}", other.type_name()),
                        ));
                    }
                }
            }
            other => {
                return Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("cannot iterate over {}", other.type_name()),
                ));
            }
        };

        for element in elements {
            let child = Scope::child(&self.scope);
            self.bind_for_pattern(&child, pattern, element, span)?;
            let saved = std::mem::replace(&mut self.scope, child);
            let flow = self.exec_stmts(body);
            self.scope = saved;
            match flow? {
                Flow::Return(value) => return Ok(Flow::Return(value)),
                Flow::Break => break,
                // `continue` ends this iteration; `Normal` falls through to the same place.
                Flow::Continue | Flow::Normal => {}
            }
        }
        Ok(Flow::Normal)
    }

    /// `while <cond> { body }` — re-evaluate the condition (which must be a bool) before each
    /// iteration, running the body in a fresh child scope. A bare reassignment in the body (e.g.
    /// a loop counter `i += 1`) updates the enclosing binding through the scope chain, so the
    /// condition can make progress.
    fn exec_while(&mut self, cond: &Expr, body: &[Stmt], span: Span) -> Eval<Flow> {
        loop {
            let taken = match self.eval_expr(cond)? {
                Value::Bool(b) => b,
                other => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`while` condition must be a bool, found {}",
                            other.type_name()
                        ),
                    ));
                }
            };
            if !taken {
                return Ok(Flow::Normal);
            }
            match self.exec_block(body)? {
                Flow::Return(value) => return Ok(Flow::Return(value)),
                Flow::Break => return Ok(Flow::Normal),
                Flow::Continue | Flow::Normal => {}
            }
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
            ForPattern::Pair { first, second, .. } => match element {
                Value::List(items) if items.len() == 2 => {
                    scope.declare(first.clone(), items[0].clone(), false);
                    scope.declare(second.clone(), items[1].clone(), false);
                    Ok(())
                }
                other => Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "destructuring `(a, b)` expects a 2-element list, found {}",
                        other.type_name()
                    ),
                )),
            },
        }
    }

    fn declare_fn(&mut self, decl: &FnDecl) {
        let closure = Closure {
            params: decl.params.iter().map(|p| p.name.clone()).collect(),
            defaults: decl.params.iter().map(|p| p.default.clone()).collect(),
            body: FnBody::Block(decl.body.clone()),
            captured: Rc::clone(&self.scope),
        };
        self.scope
            .declare(decl.name.clone(), Value::Function(Rc::new(closure)), false);
    }

    /// Register an enum type as an `EnumType` value. Backed values (`= "pending"`) are
    /// parsed but not stored in M0 — backed enums match by variant like plain ones.
    fn declare_enum(&mut self, decl: &EnumDecl) {
        let variants = decl
            .variants
            .iter()
            .map(|v| VariantInfo {
                name: v.name.clone(),
                field_names: v.fields.iter().map(|f| f.name.clone()).collect(),
            })
            .collect();
        let def = EnumDef {
            name: decl.name.clone(),
            variants,
        };
        self.scope
            .declare(decl.name.clone(), Value::EnumType(Rc::new(def)), false);
    }

    /// Register a structural record type. Records have fields but no methods; they are
    /// constructed via the all-fields literal and compared structurally.
    fn declare_record(&mut self, decl: &RecordDecl) {
        let fields = decl
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.name.clone(),
                mutable: f.mut_field,
            })
            .collect();
        let def = TypeDef {
            name: decl.name.clone(),
            fields,
            methods: HashMap::new(),
            destructor: None,
            is_record: true,
            derives_comparable: lang_ast::derives_trait(&decl.derives, "Comparable"),
            derives_tojson: lang_ast::derives_trait(&decl.derives, "Serialize"),
            opaque: false,
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Register the names imported by a `use` declaration. Real module loading is M1; in
    /// M0 each imported name resolves to an *opaque stub type* so references and all-fields
    /// literals (`User { name: ... }`) work even though the type's real shape is unknown.
    fn declare_use(&mut self, path: &[String], names: &[lang_ast::UseName]) {
        // `use std.{json, ...}` binds each recognized name to its Ring 2 native module; other
        // imports (and unrecognized `std` names) fall back to the opaque-stub binding.
        let is_std = path == ["std"];
        for imported in names {
            let value = if is_std
                && let Some(module) = lang_stdlib::NativeModule::from_name(&imported.name)
            {
                Value::NativeModule(module)
            } else {
                Value::Type(Rc::new(TypeDef {
                    name: imported.name.clone(),
                    fields: Vec::new(),
                    methods: HashMap::new(),
                    destructor: None,
                    is_record: false,
                    derives_comparable: false,
                    derives_tojson: false,
                    opaque: true,
                }))
            };
            self.scope.declare(imported.name.clone(), value, false);
        }
    }

    /// Register a class type. Methods are compiled to closures capturing the current
    /// (global) scope, exactly like top-level `fn`s, so they can see the class itself
    /// (for `Type { ... }` literals) and other globals.
    fn declare_class(&mut self, decl: &ClassDecl) {
        let fields = decl
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.name.clone(),
                mutable: f.mut_field,
            })
            .collect();
        let methods = decl
            .methods
            .iter()
            .map(|m| {
                let closure = Closure {
                    params: m.params.iter().map(|p| p.name.clone()).collect(),
                    defaults: m.params.iter().map(|p| p.default.clone()).collect(),
                    body: FnBody::Block(m.body.clone()),
                    captured: Rc::clone(&self.scope),
                };
                (m.name.clone(), Rc::new(closure))
            })
            .collect();
        let def = TypeDef {
            name: decl.name.clone(),
            fields,
            methods,
            destructor: decl.destructor.clone().map(Rc::new),
            is_record: false,
            // A hand-written `compare` (via `impl Comparable`) takes precedence over derivation.
            derives_comparable: lang_ast::derives_trait(&decl.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            // A hand-written `to_json` takes precedence over the derived serializer.
            derives_tojson: lang_ast::derives_trait(&decl.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            opaque: false,
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Construct a record/class instance from an all-fields object literal. This is the
    /// full-initialization choke point: every declared field must end up set (by a named
    /// initializer or the `..` spread), or it is a [`DiagnosticCode::MissingField`] error.
    /// Materialize the `#[type_name(...)]` attributes from the manifest into a `List<Attributed<T>>`
    /// — each a real `T` record (built from its stored args) paired with its target. Builds fresh
    /// `TypeDef`s from the shared reflection info; the VM builds the matching shapes the same way, so
    /// the materialized values agree across backends by construction.
    fn materialize_attributes(&self, type_name: &str) -> Value {
        let info = self.reflection.type_named(type_name);
        let fields: Vec<String> = info.map(|t| t.fields.clone()).unwrap_or_default();
        let is_record = !matches!(
            info.map(|t| t.kind),
            Some(lang_ast::reflect::TypeKind::Class)
        );
        let attr_def = Rc::new(fresh_type_def(type_name, &fields, is_record));
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
                let values = lang_ast::reflect::materialize_args(a, &fields);
                let t_fields: BTreeMap<String, Value> = fields
                    .iter()
                    .cloned()
                    .zip(
                        values
                            .iter()
                            .map(|v| attr_value_to_eval(v, &self.reflection)),
                    )
                    .collect();
                let t_value = Value::Object(Rc::new(ObjectValue {
                    def: attr_def.clone(),
                    fields: t_fields,
                }));
                let mut a_fields = BTreeMap::new();
                a_fields.insert("target".to_string(), Value::Str(a.target.clone()));
                a_fields.insert("value".to_string(), t_value);
                Value::Object(Rc::new(ObjectValue {
                    def: attributed_def.clone(),
                    fields: a_fields,
                }))
            })
            .collect();
        Value::List(Rc::new(items))
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
                let mut fields = BTreeMap::new();
                fields.insert("target".to_string(), Value::Str(r.target.clone()));
                fields.insert(
                    "role".to_string(),
                    builtin_enum(&r.enum_name, &r.variant, Vec::new()),
                );
                Value::Object(Rc::new(ObjectValue {
                    def: binding_def.clone(),
                    fields,
                }))
            })
            .collect();
        Value::List(Rc::new(items))
    }

    fn eval_object(&mut self, lit: &ObjectLit) -> Eval<Value> {
        let def = match self.scope.lookup(&lit.type_name) {
            Some(Value::Type(def)) => def,
            Some(other) => {
                return Err(self.runtime_error(
                    DiagnosticCode::TypeMismatch,
                    lit.type_name_span,
                    format!(
                        "`{}` is a {}, not a record or class type",
                        lit.type_name,
                        other.type_name()
                    ),
                ));
            }
            None => {
                return Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    lit.type_name_span,
                    format!("cannot find type `{}` in this scope", lit.type_name),
                ));
            }
        };

        let mut fields: BTreeMap<String, Value> = BTreeMap::new();

        // `...base` fills the unnamed fields first (named initializers override below). For
        // an opaque imported stub the field set is unknown, so the whole base is copied.
        if let Some(spread) = &lit.spread {
            match self.eval_expr(spread)? {
                Value::Object(base) if def.opaque => {
                    for (name, value) in &base.fields {
                        fields.insert(name.clone(), value.clone());
                    }
                }
                Value::Object(base) => {
                    for spec in &def.fields {
                        if let Some(value) = base.fields.get(&spec.name) {
                            fields.insert(spec.name.clone(), value.clone());
                        }
                    }
                }
                other => {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        spread.span(),
                        format!("spread `..` expects an object, found {}", other.type_name()),
                    ));
                }
            }
        }

        for init in &lit.fields {
            // An opaque stub accepts any field (its real shape is unknown until M1).
            if !def.opaque && !def.has_field(&init.name) {
                return Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    init.name_span,
                    format!("type `{}` has no field `{}`", def.name(), init.name),
                ));
            }
            let value = self.eval_expr(&init.value)?;
            fields.insert(init.name.clone(), value);
        }

        // The full-initialization guarantee applies only to types with a known field set;
        // an opaque import has none to be missing.
        let missing: Vec<&str> = def
            .fields
            .iter()
            .filter(|spec| !fields.contains_key(&spec.name))
            .map(|spec| spec.name.as_str())
            .collect();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(self.runtime_error(
                DiagnosticCode::MissingField,
                lit.span,
                format!(
                    "missing field(s) {list} in `{}` literal — every field must be set",
                    def.name()
                ),
            ));
        }

        Ok(Value::Object(Rc::new(ObjectValue {
            def: Rc::clone(&def),
            fields,
        })))
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
        })))
    }

    /// Apply the binding rules: `mut` declares/overwrites a mutable binding in the
    /// current scope; a bare `name = expr` reassigns an existing (mutable) binding if
    /// one is in scope, errors on an immutable one, and otherwise introduces a new
    /// immutable binding locally.
    fn bind(&mut self, mut_decl: bool, name: &str, name_span: Span, value: Value) -> Eval<()> {
        if mut_decl {
            self.scope.declare(name.to_string(), value, true);
            return Ok(());
        }
        match self.scope.assign(name, value.clone()) {
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

    fn eval_expr(&mut self, expr: &Expr) -> Eval<Value> {
        match expr {
            Expr::Str { value, .. } => Ok(Value::Str(value.clone())),
            Expr::Int { value, .. } => Ok(Value::Int(*value)),
            Expr::Float { value, .. } => Ok(Value::Float(*value)),
            Expr::Bool { value, .. } => Ok(Value::Bool(*value)),
            Expr::Ident { name, span } => match self.scope.lookup(name) {
                Some(value) => Ok(value),
                None => {
                    self.diagnostics.push(Diagnostic::error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!("cannot find `{name}` in this scope"),
                    ));
                    Err(Unwind::Abort)
                }
            },
            Expr::Unary { op, operand, span } => {
                let value = self.eval_expr(operand)?;
                self.eval_unary(*op, value, *span)
            }
            Expr::Binary { op, lhs, rhs, span } => self.eval_binary(*op, lhs, rhs, *span),
            Expr::Closure { params, body, .. } => Ok(Value::Function(Rc::new(Closure {
                params: params.iter().map(|p| p.name.clone()).collect(),
                // Closure parameters cannot declare defaults (the parser forbids it), so these are
                // all `None`; kept parallel to `params` for a uniform call path.
                defaults: params.iter().map(|p| p.default.clone()).collect(),
                body: FnBody::Arrow((**body).clone()),
                captured: Rc::clone(&self.scope),
            }))),
            Expr::Call { callee, args, span } => self.eval_call(callee, args, None, *span),
            Expr::Pipeline { left, right, span } => {
                let left = self.eval_expr(left)?;
                self.eval_pipeline(left, right, *span)
            }
            Expr::List { items, .. } => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_expr(item)?);
                }
                Ok(Value::List(Rc::new(values)))
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let lo = self.eval_expr(start)?;
                let hi = self.eval_expr(end)?;
                match (lo, hi) {
                    (Value::Int(a), Value::Int(b)) => {
                        // `..=` is exclusive with `upper = b + 1`; `saturating_add` keeps the
                        // (unmaterializable) `i64::MAX` edge from panicking. An empty range yields
                        // an empty list.
                        let upper = if *inclusive { b.saturating_add(1) } else { b };
                        let items: Vec<Value> = (a..upper).map(Value::Int).collect();
                        Ok(Value::List(Rc::new(items)))
                    }
                    (a, b) => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "range bounds must be ints, found {} and {}",
                            a.type_name(),
                            b.type_name()
                        ),
                    )),
                }
            }
            Expr::Map { entries, span } => {
                let mut map = BTreeMap::new();
                for (key_expr, value_expr) in entries {
                    let key = match self.eval_expr(key_expr)? {
                        Value::Str(s) => s,
                        other => {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("map keys must be strings, found {}", other.type_name()),
                            ));
                        }
                    };
                    let value = self.eval_expr(value_expr)?;
                    map.insert(key, value);
                }
                Ok(Value::Map(Rc::new(map)))
            }
            Expr::Member {
                receiver,
                name,
                span,
                ..
            } => {
                let receiver = self.eval_expr(receiver)?;
                match receiver {
                    // `Status.Pending` — construct a no-data variant.
                    Value::EnumType(def) => self.make_variant(&def, name, vec![], *span),
                    // `order.id` — field access on an instance.
                    Value::Object(object) => match object.fields.get(name) {
                        Some(value) => Ok(value.clone()),
                        None => Err(self.runtime_error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("type `{}` has no field `{name}`", object.def.name()),
                        )),
                    },
                    // `Order.new` (without a call) — the associated function as a value.
                    Value::Type(def) => match def.methods.get(name) {
                        Some(method) => Ok(Value::Function(Rc::clone(method))),
                        None => Err(self.runtime_error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("type `{}` has no associated function `{name}`", def.name()),
                        )),
                    },
                    other => Err(self.runtime_error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!("no field `{name}` on {}", other.type_name()),
                    )),
                }
            }
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                let receiver = self.eval_expr(receiver)?;
                let index = self.eval_expr(index)?;
                self.eval_index(receiver, index, *span)
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                let value = self.eval_expr(scrutinee)?;
                self.eval_match(value, arms, *span)
            }
            Expr::Object(lit) => self.eval_object(lit),
            Expr::Try { expr, span } => {
                let value = self.eval_expr(expr)?;
                self.eval_try(value, *span)
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => {
                let value = self.eval_expr(value)?;
                self.eval_coalesce(value, fallback, *span)
            }
            // `expr.as<T>()` — checked narrowing: `some(value)` if the runtime value is a `T`,
            // `none` otherwise. Generics are erased, so the match tests the head constructor only
            // (`List<int>` checks "is a list"); the element type is trusted from the annotation.
            Expr::As { expr, ty, .. } => {
                let value = self.eval_expr(expr)?;
                if runtime_matches(&value, ty) {
                    Ok(builtin_enum("Option", "some", vec![value]))
                } else {
                    Ok(builtin_enum("Option", "none", vec![]))
                }
            }
            Expr::TypeTest { expr, ty, .. } => {
                let value = self.eval_expr(expr)?;
                Ok(Value::Bool(runtime_matches(&value, ty)))
            }
            Expr::AttributesOf { ty, .. } => {
                let type_name = match ty {
                    TypeRef::Named { name, .. } => name.as_str(),
                    _ => "",
                };
                Ok(self.materialize_attributes(type_name))
            }
            Expr::RolesOf { .. } => Ok(self.materialize_roles()),
            Expr::TypeOf { value, span } => {
                // Evaluate the operand for its side effects in both fidelities. A concrete static
                // type resolved by the checker builds the precise `Type` constant (fidelity A);
                // otherwise classify the runtime value's head constructor (fidelity B).
                let v = self.eval_expr(value)?;
                match self.type_of_sites.get(span) {
                    Some(repr) => Ok(build_type_value(repr)),
                    None => Ok(build_type_value(&eval_type_repr(&v))),
                }
            }
            Expr::Invoke {
                recv,
                name,
                args,
                span,
            } => {
                let receiver = self.eval_expr(recv)?;
                let name_val = self.eval_expr(name)?;
                let args_val = self.eval_expr(args)?;
                self.invoke_dynamic(receiver, name_val, args_val, *span)
            }
            Expr::Interp { parts, .. } => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        StrPart::Literal(text) => out.push_str(text),
                        StrPart::Hole(expr) => {
                            let v = self.eval_expr(expr)?;
                            out.push_str(&self.display_value(&v, expr.span())?);
                        }
                    }
                }
                Ok(Value::Str(out))
            }
        }
    }

    /// Evaluate a call expression. If `callee` is a member access it is a method call;
    /// otherwise it is an ordinary call. `prepend` supplies a leading argument (used by
    /// the pipeline operator to thread the piped value as the first argument).
    fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        prepend: Option<Value>,
        span: Span,
    ) -> Eval<Value> {
        if let Expr::Member { receiver, name, .. } = callee {
            let receiver = self.eval_expr(receiver)?;
            let mut values = Vec::with_capacity(args.len() + 1);
            values.extend(prepend);
            for arg in args {
                values.push(self.eval_expr(arg)?);
            }
            return self.call_method(receiver, name, values, span);
        }
        let callee = self.eval_expr(callee)?;
        let mut values = Vec::with_capacity(args.len() + 1);
        values.extend(prepend);
        for arg in args {
            values.push(self.eval_expr(arg)?);
        }
        self.call(callee, values, span)
    }

    /// `x |> f(a)` evaluates `f`, prepends `x` to its arguments, and calls it.
    /// `x |> f` (no call) is `f(x)`; `x |> obj.m()` is `obj.m(x)`.
    fn eval_pipeline(&mut self, left: Value, right: &Expr, span: Span) -> Eval<Value> {
        match right {
            Expr::Call { callee, args, .. } => self.eval_call(callee, args, Some(left), span),
            Expr::Member { receiver, name, .. } => {
                let receiver = self.eval_expr(receiver)?;
                self.call_method(receiver, name, vec![left], span)
            }
            _ => {
                let callee = self.eval_expr(right)?;
                self.call(callee, vec![left], span)
            }
        }
    }

    /// Evaluate a `match`: try each arm's pattern in order, bind on the first match, and
    /// evaluate that arm's body in a child scope. M0 match is non-exhaustive (a checker
    /// concern, M1), so a value matching no arm is a runtime error.
    fn eval_match(&mut self, value: Value, arms: &[MatchArm], span: Span) -> Eval<Value> {
        for arm in arms {
            if let Some(bindings) = match_pattern(&arm.pattern, &value) {
                let child = Scope::child(&self.scope);
                for (name, bound) in bindings {
                    child.declare(name, bound, false);
                }
                let saved = std::mem::replace(&mut self.scope, child);
                let result = self.eval_expr(&arm.body);
                self.scope = saved;
                return result;
            }
        }
        Err(self.runtime_error(
            DiagnosticCode::TypeMismatch,
            span,
            format!("no match arm matched the value {}", value.display()),
        ))
    }

    /// The `?` operator. On `Ok(x)`/`some(x)` it yields `x`; on `Err(e)`/`none` it
    /// short-circuits via [`Unwind::Return`], propagating that value out of the enclosing
    /// function (caught at the call boundary in [`Interpreter::call_closure`]).
    fn eval_try(&mut self, value: Value, span: Span) -> Eval<Value> {
        match try_branch(&value) {
            Some(TryBranch::Success(inner)) => Ok(inner),
            Some(TryBranch::Empty) => Err(Unwind::Return(value)),
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

    /// The `??` operator. On `Ok(x)`/`some(x)` it yields `x`; on `Err(_)`/`none` it
    /// evaluates and yields the `fallback` expression.
    fn eval_coalesce(&mut self, value: Value, fallback: &Expr, span: Span) -> Eval<Value> {
        match try_branch(&value) {
            Some(TryBranch::Success(inner)) => Ok(inner),
            Some(TryBranch::Empty) => self.eval_expr(fallback),
            None => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`??` expects a `Result` or `Option` on the left, found {}",
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
            return self.make_variant(&Rc::clone(def), name, args, span);
        }
        // `json.parse(...)` — a Ring 2 native module function call.
        if let Value::NativeModule(module) = &receiver {
            return self.call_native_module(*module, name, &args, span);
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
                    lang_ast::ordering_variant(ordering),
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
        // argument typing live once in `lang-stdlib`, shared with the VM, so the two backends
        // cannot drift. `Unknown` falls through to the collection methods below.
        if let Value::Str(s) = &receiver {
            let projected: Vec<_> = args.iter().map(project_arg).collect();
            match lang_stdlib::string_method(s, name, &projected) {
                lang_stdlib::Dispatch::Done(output) => return Ok(output_to_value(output)),
                lang_stdlib::Dispatch::Err(error) => {
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                lang_stdlib::Dispatch::Unknown => {}
            }
        }
        // Ring 1 list methods (reverse/contains/join) — Value-specific, so implemented per
        // backend, but the method set is the shared `ListMethod` enum: a non-exhaustive `match`
        // here would not compile, so the VM cannot omit a method this backend offers.
        if let Value::List(items) = &receiver
            && let Some(method) = lang_stdlib::ListMethod::from_name(name)
        {
            return self.call_list_method(method, items, name, &args, span);
        }
        // Ring 1 set methods (contains/union/intersection).
        if let Value::Set(items) = &receiver
            && let Some(method) = lang_stdlib::SetMethod::from_name(name)
        {
            return self.call_set_method(method, items, name, &args, span);
        }
        // File-handle methods (read_line/read/write/close) — the shared `FileHandleMethod` enum
        // keeps the two backends in lockstep, like the collection methods above.
        if let Value::FileHandle(handle) = &receiver
            && let Some(method) = lang_stdlib::FileHandleMethod::from_name(name)
        {
            let handle = Rc::clone(handle);
            return self.call_file_handle_method(method, &handle, name, &args, span);
        }
        // Ring 1 map methods (keys/values/has).
        if let Value::Map(entries) = &receiver
            && let Some(method) = lang_stdlib::MapMethod::from_name(name)
        {
            return self.call_map_method(method, entries, name, &args, span);
        }
        let arity_ok = args.is_empty();
        let result = match (name, &receiver) {
            ("count", Value::List(items)) if arity_ok => Some(Value::Int(items.len() as i64)),
            ("count", Value::Set(items)) if arity_ok => Some(Value::Int(items.len() as i64)),
            ("count", Value::Map(entries)) if arity_ok => Some(Value::Int(entries.len() as i64)),
            ("count", Value::Str(s)) if arity_ok => Some(Value::Int(s.chars().count() as i64)),
            ("enumerate", Value::List(items)) if arity_ok => {
                let pairs = items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| Value::List(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                    .collect();
                Some(Value::List(Rc::new(pairs)))
            }
            _ => None,
        };
        match result {
            Some(value) => Ok(value),
            None if !arity_ok => Err(self.runtime_error(
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
        let args: Vec<Value> = items.as_ref().clone();
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
    /// arity/type misuse is reported through the shared `lang-stdlib` error builders so both
    /// backends produce identical diagnostics.
    fn call_list_method(
        &mut self,
        method: lang_stdlib::ListMethod,
        items: &[Value],
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match method {
            lang_stdlib::ListMethod::Reverse => {
                self.expect_std_arity(name, args, 0, span)?;
                let mut reversed = items.to_vec();
                reversed.reverse();
                Ok(Value::List(Rc::new(reversed)))
            }
            lang_stdlib::ListMethod::Contains => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(Value::Bool(items.iter().any(|item| *item == args[0])))
            }
            lang_stdlib::ListMethod::Join => {
                self.expect_std_arity(name, args, 1, span)?;
                let separator = self.expect_std_string(name, &args[0], span)?.to_string();
                let joined = items
                    .iter()
                    .map(Value::display)
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::Str(joined))
            }
            lang_stdlib::ListMethod::Sorted => {
                self.expect_std_arity(name, args, 0, span)?;
                // Every element must be mutually orderable with the first (homogeneous numbers
                // or strings); otherwise there is no total order to sort by. A stable sort then
                // keeps equal elements in input order, matching the VM exactly.
                if items
                    .iter()
                    .any(|item| compare_primitive(&items[0], item).is_none())
                {
                    let error = lang_stdlib::unorderable_error(name);
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                let mut sorted = items.to_vec();
                sorted.sort_by(|a, b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
                Ok(Value::List(Rc::new(sorted)))
            }
            lang_stdlib::ListMethod::Slice => {
                self.expect_std_arity(name, args, 2, span)?;
                let start = self.expect_std_int(name, &args[0], span)?;
                let end = self.expect_std_int(name, &args[1], span)?;
                let len = items.len();
                if start < 0 || end < start || end as usize > len {
                    let error = lang_stdlib::slice_bounds_error(start, end, len);
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                }
                let slice = items[start as usize..end as usize].to_vec();
                Ok(Value::List(Rc::new(slice)))
            }
            lang_stdlib::ListMethod::First => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(match items.first() {
                    Some(value) => builtin_enum("Option", "some", vec![value.clone()]),
                    None => builtin_enum("Option", "none", Vec::new()),
                })
            }
            lang_stdlib::ListMethod::Last => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(match items.last() {
                    Some(value) => builtin_enum("Option", "some", vec![value.clone()]),
                    None => builtin_enum("Option", "none", Vec::new()),
                })
            }
            lang_stdlib::ListMethod::ToSet => {
                self.expect_std_arity(name, args, 0, span)?;
                match canonical_set(items) {
                    Some(canonical) => Ok(Value::Set(Rc::new(canonical))),
                    None => {
                        let error = lang_stdlib::unorderable_error(name);
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
        }
    }

    /// A Ring 1 set method (`contains`/`union`/`intersection`). Mirrors the VM's
    /// `call_set_method`. The receiver `items` are already canonical (sorted, de-duplicated).
    fn call_set_method(
        &mut self,
        method: lang_stdlib::SetMethod,
        items: &[Value],
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match method {
            lang_stdlib::SetMethod::Contains => {
                self.expect_std_arity(name, args, 1, span)?;
                Ok(Value::Bool(items.iter().any(|item| *item == args[0])))
            }
            lang_stdlib::SetMethod::Union => {
                self.expect_std_arity(name, args, 1, span)?;
                let other = self.expect_std_set(name, &args[0], span)?;
                let mut combined = items.to_vec();
                combined.extend(other.iter().cloned());
                // Both operands are valid sets, so every element is orderable.
                let canonical = canonical_set(&combined).expect("set elements are orderable");
                Ok(Value::Set(Rc::new(canonical)))
            }
            lang_stdlib::SetMethod::Intersection => {
                self.expect_std_arity(name, args, 1, span)?;
                let other = self.expect_std_set(name, &args[0], span)?;
                // `items` is already canonical, so filtering it preserves sorted, de-duplicated order.
                let kept: Vec<Value> = items
                    .iter()
                    .filter(|item| other.iter().any(|o| *item == o))
                    .cloned()
                    .collect();
                Ok(Value::Set(Rc::new(kept)))
            }
        }
    }

    /// Read a set argument for a set method, raising the shared `lang-stdlib` type error.
    fn expect_std_set<'a>(
        &mut self,
        name: &str,
        value: &'a Value,
        span: Span,
    ) -> Eval<&'a Rc<Vec<Value>>> {
        match value {
            Value::Set(items) => Ok(items),
            _ => {
                let error = lang_stdlib::type_error(name, "set");
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Dispatch a Ring 2 native module function call (`json.parse(...)`). Mirrors the VM's
    /// `call_native_module`.
    fn call_native_module(
        &mut self,
        module: lang_stdlib::NativeModule,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match module {
            lang_stdlib::NativeModule::Json => self.call_json(func, args, span),
            lang_stdlib::NativeModule::Math => self.call_math(func, args, span),
            lang_stdlib::NativeModule::Random => self.call_random(func, args, span),
            lang_stdlib::NativeModule::Fs => self.call_fs(func, args, span),
            lang_stdlib::NativeModule::Time => self.call_time(func, args, span),
            lang_stdlib::NativeModule::Env => self.call_env(func, args, span),
            lang_stdlib::NativeModule::Args => self.call_args(func, args, span),
        }
    }

    /// The `fs` module: file IO over the sandboxed in-memory [`lang_stdlib::fs::Vfs`]. The VFS
    /// semantics are shared, so the two backends are identical by construction. Mirrors the VM's
    /// `call_fs`.
    fn call_fs(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        match func {
            "write" => {
                self.expect_std_arity(func, args, 2, span)?;
                let path = self.expect_std_string(func, &args[0], span)?.to_string();
                let content = self.expect_std_string(func, &args[1], span)?.to_string();
                match self.host.fs_write(&path, &content) {
                    Ok(()) => Ok(Value::Unit),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "append" => {
                self.expect_std_arity(func, args, 2, span)?;
                let path = self.expect_std_string(func, &args[0], span)?.to_string();
                let content = self.expect_std_string(func, &args[1], span)?.to_string();
                match self.host.fs_append(&path, &content) {
                    Ok(()) => Ok(Value::Unit),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "read_lines" => {
                self.expect_std_arity(func, args, 1, span)?;
                let path = self.expect_std_string(func, &args[0], span)?;
                match self.host.fs_read(path) {
                    Ok(content) => {
                        let lines = content.lines().map(|l| Value::Str(l.to_string())).collect();
                        Ok(Value::List(Rc::new(lines)))
                    }
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "read" => {
                self.expect_std_arity(func, args, 1, span)?;
                let path = self.expect_std_string(func, &args[0], span)?;
                match self.host.fs_read(path) {
                    Ok(content) => Ok(Value::Str(content)),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "exists" => {
                self.expect_std_arity(func, args, 1, span)?;
                let path = self.expect_std_string(func, &args[0], span)?;
                Ok(Value::Bool(self.host.fs_exists(path)))
            }
            "remove" => {
                self.expect_std_arity(func, args, 1, span)?;
                let path = self.expect_std_string(func, &args[0], span)?.to_string();
                match self.host.fs_remove(&path) {
                    Ok(existed) => Ok(Value::Bool(existed)),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            // `list()` lists every file; `list(dir)` lists a directory's immediate children.
            "list" => {
                let result = match args.len() {
                    0 => self.host.fs_list(),
                    1 => {
                        let dir = self.expect_std_string(func, &args[0], span)?;
                        self.host.fs_list_dir(dir)
                    }
                    n => {
                        let error = lang_stdlib::arity_error(func, 1, n);
                        return Err(self.runtime_error(
                            std_error_code(error.kind),
                            span,
                            error.message,
                        ));
                    }
                };
                match result {
                    Ok(paths) => {
                        let paths = paths.into_iter().map(Value::Str).collect();
                        Ok(Value::List(Rc::new(paths)))
                    }
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "mkdir" => {
                self.expect_std_arity(func, args, 1, span)?;
                let path = self.expect_std_string(func, &args[0], span)?.to_string();
                match self.host.fs_mkdir(&path) {
                    Ok(()) => Ok(Value::Unit),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "is_dir" => {
                self.expect_std_arity(func, args, 1, span)?;
                let path = self.expect_std_string(func, &args[0], span)?;
                Ok(Value::Bool(self.host.fs_is_dir(path)))
            }
            // `open(path, mode)` → a cursor file handle. Read mode snapshots the file (a missing
            // file is the same E0021 as `fs.read`); write/append buffer until `close`. Mirrors the
            // VM's `call_fs` "open" arm.
            "open" => {
                self.expect_std_arity(func, args, 2, span)?;
                let path = self.expect_std_string(func, &args[0], span)?.to_string();
                let mode_spec = self.expect_std_string(func, &args[1], span)?.to_string();
                let Some(mode) = lang_stdlib::FileMode::parse(&mode_spec) else {
                    let error = lang_stdlib::handle::unknown_mode_error(&mode_spec);
                    return Err(self.runtime_error(
                        std_error_code(error.kind),
                        span,
                        error.message,
                    ));
                };
                let handle = match mode {
                    lang_stdlib::FileMode::Read => match self.host.fs_read(&path) {
                        Ok(content) => lang_stdlib::FileHandle::open_read(&path, content),
                        Err(error) => {
                            return Err(self.runtime_error(
                                std_error_code(error.kind),
                                span,
                                error.message,
                            ));
                        }
                    },
                    lang_stdlib::FileMode::Write => lang_stdlib::FileHandle::open_write(&path),
                    lang_stdlib::FileMode::Append => lang_stdlib::FileHandle::open_append(&path),
                };
                Ok(Value::FileHandle(Rc::new(RefCell::new(handle))))
            }
            _ => {
                let error = lang_stdlib::no_function_error("fs", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Dispatch a file-handle method (`read_line`/`read`/`write`/`close`). Mirrors the VM's
    /// `call_file_handle_method`: the cursor logic lives in the shared `FileHandle`, so the two
    /// backends differ only in value glue (building `some`/`none`, routing the close flush through
    /// `self.host`).
    fn call_file_handle_method(
        &mut self,
        method: lang_stdlib::FileHandleMethod,
        handle: &Rc<RefCell<lang_stdlib::FileHandle>>,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        use lang_stdlib::FileHandleMethod as M;
        match method {
            M::ReadLine => {
                self.expect_std_arity(name, args, 0, span)?;
                match handle.borrow_mut().read_line() {
                    Ok(Some(line)) => Ok(builtin_enum("Option", "some", vec![Value::Str(line)])),
                    Ok(None) => Ok(builtin_enum("Option", "none", Vec::new())),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Read => {
                self.expect_std_arity(name, args, 1, span)?;
                let count = self.expect_std_int(name, &args[0], span)?;
                match handle.borrow_mut().read(count) {
                    Ok(Some(chunk)) => Ok(builtin_enum("Option", "some", vec![Value::Str(chunk)])),
                    Ok(None) => Ok(builtin_enum("Option", "none", Vec::new())),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Write => {
                self.expect_std_arity(name, args, 1, span)?;
                let chunk = self.expect_std_string(name, &args[0], span)?.to_string();
                match handle.borrow_mut().write(&chunk) {
                    Ok(()) => Ok(Value::Unit),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Close => {
                self.expect_std_arity(name, args, 0, span)?;
                // Take the flush instruction first (the borrow ends), then hit the host.
                let flush = handle.borrow_mut().close();
                let result = match flush {
                    None => Ok(()),
                    Some(lang_stdlib::Flush::Write { path, content }) => {
                        self.host.fs_write(&path, &content)
                    }
                    Some(lang_stdlib::Flush::Append { path, content }) => {
                        self.host.fs_append(&path, &content)
                    }
                };
                match result {
                    Ok(()) => Ok(Value::Unit),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
        }
    }

    /// The `time` module: a deterministic logical clock (no wall-clock, so output stays
    /// reproducible and identical to the VM). `monotonic()` reads-then-increments; `sleep(ms)`
    /// advances the clock without actually blocking. Mirrors the VM's `call_time`.
    fn call_time(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        match func {
            "monotonic" => {
                self.expect_std_arity(func, args, 0, span)?;
                let now = self.host.clock_monotonic();
                Ok(Value::Int(now as i64))
            }
            "sleep" => {
                self.expect_std_arity(func, args, 1, span)?;
                let ms = self.expect_std_int(func, &args[0], span)?;
                self.host.clock_sleep(ms);
                Ok(Value::Unit)
            }
            _ => {
                let error = lang_stdlib::no_function_error("time", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `env` module: host environment introspection over the host's fixed sandbox fixture.
    /// `get(key)` returns the value or an `E0021` if absent (mirroring `fs.read`); `keys()` is
    /// sorted. Mirrors the VM's `call_env`.
    fn call_env(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        match func {
            "get" => {
                self.expect_std_arity(func, args, 1, span)?;
                let key = self.expect_std_string(func, &args[0], span)?.to_string();
                match self.host.env_get(&key) {
                    Some(value) => Ok(Value::Str(value)),
                    None => {
                        let error = lang_stdlib::env::not_found_error(&key);
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "keys" => {
                self.expect_std_arity(func, args, 0, span)?;
                let keys = self.host.env_keys().into_iter().map(Value::Str).collect();
                Ok(Value::List(Rc::new(keys)))
            }
            _ => {
                let error = lang_stdlib::no_function_error("env", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `args` module: the program's argument vector. `all()` returns it as a list. Mirrors the
    /// VM's `call_args`.
    fn call_args(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        match func {
            "all" => {
                self.expect_std_arity(func, args, 0, span)?;
                let all = self.host.args().into_iter().map(Value::Str).collect();
                Ok(Value::List(Rc::new(all)))
            }
            _ => {
                let error = lang_stdlib::no_function_error("args", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `math` module: pure scalar functions whose semantics live entirely in
    /// `lang-stdlib::math`, so both backends compute identically. Args project onto `Arg`, the
    /// shared dispatcher runs, and the `Output` lifts back. Mirrors the VM's `call_math`.
    fn call_math(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        let projected: Vec<lang_stdlib::Arg> = args.iter().map(project_arg).collect();
        match lang_stdlib::math::call(func, &projected) {
            lang_stdlib::Dispatch::Done(output) => Ok(output_to_value(output)),
            lang_stdlib::Dispatch::Err(error) => {
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
            lang_stdlib::Dispatch::Unknown => {
                let error = lang_stdlib::no_function_error("math", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `random` module: a seeded PRNG. Stepping is shared (`lang-stdlib::random`); the state
    /// lives in the host (`self.host`), threaded through each draw so a given seed yields the same
    /// stream the VM produces. `seed(n)` re-seeds, `int(lo, hi)` and `float()` advance. Mirrors the
    /// VM's `call_random`.
    fn call_random(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        match func {
            "seed" => {
                self.expect_std_arity(func, args, 1, span)?;
                let n = self.expect_std_int(func, &args[0], span)?;
                self.host.rng_seed(n);
                Ok(Value::Unit)
            }
            "int" => {
                self.expect_std_arity(func, args, 2, span)?;
                let lo = self.expect_std_int(func, &args[0], span)?;
                let hi = self.expect_std_int(func, &args[1], span)?;
                match self.host.rng_int(lo, hi) {
                    Ok(value) => Ok(Value::Int(value)),
                    Err(error) => {
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "float" => {
                self.expect_std_arity(func, args, 0, span)?;
                Ok(Value::Float(self.host.rng_float()))
            }
            _ => {
                let error = lang_stdlib::no_function_error("random", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `json` module: `parse(text) -> value` and `stringify(value) -> string`. Parsing goes
    /// through the shared `lang-stdlib` parser (so both backends build identical values);
    /// stringifying reuses the structural `value_to_json`.
    fn call_json(&mut self, func: &str, args: &[Value], span: Span) -> Eval<Value> {
        match func {
            "parse" => {
                self.expect_std_arity(func, args, 1, span)?;
                let text = self.expect_std_string(func, &args[0], span)?;
                match lang_stdlib::json::parse(text) {
                    Ok(json) => Ok(json_to_value(json)),
                    Err(detail) => {
                        let error = lang_stdlib::invalid_json_error(&detail);
                        Err(self.runtime_error(std_error_code(error.kind), span, error.message))
                    }
                }
            }
            "stringify" => {
                self.expect_std_arity(func, args, 1, span)?;
                Ok(Value::Str(value_to_json(&args[0])))
            }
            _ => {
                let error = lang_stdlib::no_function_error("json", func);
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// A Ring 1 map method (`keys`/`values`/`has`). Mirrors the VM's `call_map_method`.
    fn call_map_method(
        &mut self,
        method: lang_stdlib::MapMethod,
        entries: &BTreeMap<String, Value>,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        match method {
            lang_stdlib::MapMethod::Keys => {
                self.expect_std_arity(name, args, 0, span)?;
                let keys = entries.keys().map(|k| Value::Str(k.clone())).collect();
                Ok(Value::List(Rc::new(keys)))
            }
            lang_stdlib::MapMethod::Values => {
                self.expect_std_arity(name, args, 0, span)?;
                Ok(Value::List(Rc::new(entries.values().cloned().collect())))
            }
            lang_stdlib::MapMethod::Has => {
                self.expect_std_arity(name, args, 1, span)?;
                let key = self.expect_std_string(name, &args[0], span)?;
                Ok(Value::Bool(entries.contains_key(key)))
            }
        }
    }

    /// Enforce a collection method's arity, raising the shared `lang-stdlib` arity error.
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
            let error = lang_stdlib::arity_error(name, expected, args.len());
            Err(self.runtime_error(std_error_code(error.kind), span, error.message))
        }
    }

    /// Read a string argument for a collection method, raising the shared `lang-stdlib` type error.
    fn expect_std_string<'a>(&mut self, name: &str, value: &'a Value, span: Span) -> Eval<&'a str> {
        match value {
            Value::Str(s) => Ok(s),
            _ => {
                let error = lang_stdlib::type_error(name, "string");
                Err(self.runtime_error(std_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Read an int argument for a collection method, raising the shared `lang-stdlib` type error.
    fn expect_std_int(&mut self, name: &str, value: &Value, span: Span) -> Eval<i64> {
        match value {
            Value::Int(i) => Ok(*i),
            _ => {
                let error = lang_stdlib::type_error(name, "int");
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
        Ok(value.display())
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
                Ok(items[i as usize].clone())
            }
            // `m[k]` on a map looks the value up by its string key; a missing key is `E0018`.
            Value::Map(entries) => {
                let Value::Str(key) = &index else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("map index must be a string, found {}", index.type_name()),
                    ));
                };
                match entries.get(key) {
                    Some(value) => Ok(value.clone()),
                    None => Err(self.runtime_error(
                        DiagnosticCode::KeyNotFound,
                        span,
                        format!("map has no key {key:?}"),
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
            Value::Function(closure) => self.call_closure(&closure, args, span),
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("{} is not callable", other.type_name()),
            )),
        }
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
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = match &closure.body {
            FnBody::Arrow(expr) => self.eval_expr(expr),
            FnBody::Block(stmts) => self.exec_fn_body(stmts),
        };
        self.scope = saved;
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
        for (name, value) in &object.fields {
            call_scope.declare(name.clone(), value.clone(), false);
        }
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
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = match &method.body {
            FnBody::Arrow(expr) => self.eval_expr(expr),
            FnBody::Block(stmts) => self.exec_fn_body(stmts),
        };
        self.scope = saved;
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
        let result = self.eval_expr(default);
        self.scope = saved;
        result
    }

    fn exec_fn_body(&mut self, stmts: &[Stmt]) -> Eval<Value> {
        match self.exec_stmts(stmts)? {
            Flow::Return(value) => Ok(value),
            // A bare `break`/`continue` cannot escape to a function boundary (the checker rejects
            // one outside a loop, and a loop intercepts its own); fall through like `Normal`.
            Flow::Normal | Flow::Break | Flow::Continue => Ok(Value::Unit),
        }
    }

    fn call_builtin(&mut self, builtin: Builtin, args: Vec<Value>, span: Span) -> Eval<Value> {
        match builtin {
            Builtin::NextId => {
                self.expect_arity(builtin, &args, 0, span)?;
                Ok(Value::Int(self.ids.next_id() as i64))
            }
            Builtin::Len => {
                self.expect_arity(builtin, &args, 1, span)?;
                match &args[0] {
                    Value::List(items) => Ok(Value::Int(items.len() as i64)),
                    Value::Set(items) => Ok(Value::Int(items.len() as i64)),
                    Value::Map(entries) => Ok(Value::Int(entries.len() as i64)),
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
                Ok(Value::List(Rc::new(result)))
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
                Ok(Value::List(Rc::new(result)))
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
            Value::List(items) => Ok(Rc::clone(items)),
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

    fn eval_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Eval<Value> {
        // Logical operators short-circuit, so the right side is evaluated lazily.
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let left = self.eval_expr(lhs)?;
            return self.eval_logical(op, left, rhs, span);
        }
        let left = self.eval_expr(lhs)?;
        let right = self.eval_expr(rhs)?;
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
                        op.ordering_satisfies(lang_ast::ordering_variant(ordering)),
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
        match ops::apply_binary(op, &left, &right) {
            Ok(value) => Ok(value),
            Err(error) => Err(self.runtime_error(error.code, span, error.text)),
        }
    }

    fn eval_logical(&mut self, op: BinaryOp, left: Value, rhs: &Expr, span: Span) -> Eval<Value> {
        let Value::Bool(left) = left else {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` expects a bool on the left, found {}",
                    op.symbol(),
                    left.type_name()
                ),
            ));
        };
        let short_circuit = match op {
            BinaryOp::And => !left,
            BinaryOp::Or => left,
            _ => unreachable!("eval_logical only handles && and ||"),
        };
        if short_circuit {
            return Ok(Value::Bool(left));
        }
        let right = self.eval_expr(rhs)?;
        match right {
            Value::Bool(b) => Ok(Value::Bool(b)),
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` expects a bool on the right, found {}",
                    op.symbol(),
                    other.type_name()
                ),
            )),
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
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Unwind::Abort
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
        (Value::List(mut a), Value::List(b)) => {
            if let Some(items) = Rc::get_mut(&mut a) {
                items.extend(b.iter().cloned());
                Value::List(a)
            } else {
                let mut items = (*a).clone();
                items.extend(b.iter().cloned());
                Value::List(Rc::new(items))
            }
        }
        (left, right) => Value::Str(format!("{}{}", left.display(), right.display())),
    }
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
    }))
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
fn eval_type_repr(value: &Value) -> lang_ast::reflect::TypeRepr {
    use lang_ast::reflect::TypeRepr;
    let dyn_ = || Box::new(TypeRepr::Dyn);
    match value {
        Value::Bool(_) => TypeRepr::Bool,
        Value::Int(_) => TypeRepr::Int,
        Value::Float(_) => TypeRepr::Float,
        Value::Str(_) => TypeRepr::Str,
        Value::Unit => TypeRepr::Unit,
        Value::List(_) => TypeRepr::List(dyn_()),
        Value::Set(_) => TypeRepr::Set(dyn_()),
        Value::Map(_) => TypeRepr::Map(dyn_(), dyn_()),
        Value::Function(_) | Value::Builtin(_) => TypeRepr::Fn(Vec::new(), dyn_()),
        Value::Enum(e) => match e.enum_name.as_str() {
            "Option" => TypeRepr::Option(dyn_()),
            "Result" => TypeRepr::Result(dyn_(), dyn_()),
            other => TypeRepr::Enum(other.to_string(), Vec::new()),
        },
        Value::Object(o) if o.def.is_record => {
            TypeRepr::Record(o.def.name().to_string(), Vec::new())
        }
        Value::Object(o) => TypeRepr::Class(o.def.name().to_string(), Vec::new()),
        // A type value, module, file handle, or enum-type has no nameable lattice type → the top.
        Value::EnumType(_) | Value::Type(_) | Value::NativeModule(_) | Value::FileHandle(_) => {
            TypeRepr::Dyn
        }
    }
}

/// Build the prelude `Type` enum value from a [`TypeRepr`], recursively. Reuses the ordinary
/// [`EnumValue`] representation (enum name `Type`), so the value participates in `match` like any
/// enum and is structurally identical to the VM's `build_type_value`.
fn build_type_value(repr: &lang_ast::reflect::TypeRepr) -> Value {
    use lang_ast::reflect::{TYPE_ENUM, TypeRepr};
    let list = |items: Vec<Value>| Value::List(Rc::new(items));
    let data: Vec<Value> = match repr {
        TypeRepr::Int
        | TypeRepr::Float
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Unit
        | TypeRepr::Dyn => Vec::new(),
        TypeRepr::List(t) | TypeRepr::Set(t) | TypeRepr::Option(t) => {
            vec![build_type_value(t)]
        }
        TypeRepr::Map(k, v) | TypeRepr::Result(k, v) => {
            vec![build_type_value(k), build_type_value(v)]
        }
        TypeRepr::Enum(name, args)
        | TypeRepr::Record(name, args)
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

/// A minimal `TypeDef` for a reflection-materialized record (no methods, derives, or destructor) —
/// the tree-walker counterpart to the VM's fresh `Shape`. Both carry only name + field names.
fn fresh_type_def(name: &str, fields: &[String], is_record: bool) -> TypeDef {
    TypeDef {
        name: name.to_string(),
        fields: fields
            .iter()
            .map(|f| FieldSpec {
                name: f.clone(),
                mutable: false,
            })
            .collect(),
        methods: HashMap::new(),
        destructor: None,
        is_record,
        derives_comparable: false,
        derives_tojson: false,
        opaque: false,
    }
}

/// If `value` is a reflection `Type` value naming a nominal type (`Type.Named`/`Record`/`Class`/
/// `Enum`, whose first payload is the type's name), return that name — so a stored type reference
/// can be used as an `invoke` receiver. Mirrors the VM's `reflection_type_name`.
fn reflection_type_name(value: &Value) -> Option<String> {
    let Value::Enum(ev) = value else {
        return None;
    };
    if ev.enum_name == lang_ast::reflect::TYPE_ENUM
        && matches!(ev.variant.as_str(), "Named" | "Record" | "Class" | "Enum")
        && let Some(Value::Str(name)) = ev.data.first()
    {
        return Some(name.clone());
    }
    None
}

/// Convert a manifest attribute-argument literal tree to a tree-walker value, recursing through the
/// collection and nominal literals. A type reference materializes as the reflection `Type` ADT
/// classified by the named type's *kind* (`Type.Record`/`Enum`/`Class`, or `Type.Named` for an
/// unknown-kind name) via the shared [`reflect::ReflectionInfo::type_ref_repr`]; a set is
/// canonicalized exactly like the runtime `to_set` (sorted/deduped when orderable, else insertion
/// order). The VM's `attr_value_to_vm` builds the matching values the same way, so the materialized
/// attribute agrees across the differential by construction.
fn attr_value_to_eval(
    value: &lang_ast::AttrValue,
    reflection: &lang_ast::reflect::ReflectionInfo,
) -> Value {
    use lang_ast::AttrValue as A;
    let recur = |v: &A| attr_value_to_eval(v, reflection);
    match value {
        A::Str(s) => Value::Str(s.clone()),
        A::Int(n) => Value::Int(*n),
        A::Float(f) => Value::Float(*f),
        A::Bool(b) => Value::Bool(*b),
        A::List(items) => Value::List(Rc::new(items.iter().map(recur).collect())),
        A::Set(items) => {
            let vals: Vec<Value> = items.iter().map(recur).collect();
            Value::Set(Rc::new(canonical_set(&vals).unwrap_or(vals)))
        }
        A::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                map.insert(k.clone(), recur(v));
            }
            Value::Map(Rc::new(map))
        }
        A::Enum {
            enum_name,
            variant,
            args,
        } => builtin_enum(enum_name, variant, args.iter().map(recur).collect()),
        A::Record { type_name, fields } => {
            let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
            let def = Rc::new(fresh_type_def(type_name, &names, true));
            let f: BTreeMap<String, Value> =
                fields.iter().map(|(n, v)| (n.clone(), recur(v))).collect();
            Value::Object(Rc::new(ObjectValue { def, fields: f }))
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
        TypeRef::Named { name, .. } => match name.as_str() {
            "int" => matches!(value, Value::Int(_)),
            "float" => matches!(value, Value::Float(_)),
            "bool" => matches!(value, Value::Bool(_)),
            "string" => matches!(value, Value::Str(_)),
            "void" | "unit" => matches!(value, Value::Unit),
            // Narrowing to the open top is a no-op: every value is a `dyn`.
            "dyn" | "Any" => true,
            "List" | "list" => matches!(value, Value::List(_)),
            "Map" | "map" => matches!(value, Value::Map(_)),
            "Set" | "set" => matches!(value, Value::Set(_)),
            // Abstract kind-types match any value of that declaration kind (records and classes are
            // both `Object`s, told apart by `TypeDef::is_record`).
            "Enum" => matches!(value, Value::Enum(_)),
            "Record" => matches!(value, Value::Object(o) if o.def.is_record),
            "Class" => matches!(value, Value::Object(o) if !o.def.is_record),
            // `Option`/`Result` are enums whose shape name is the type name, like a user enum.
            other => match value {
                Value::Object(object) => object.def.name() == other,
                Value::Enum(enum_value) => enum_value.enum_name == other,
                _ => false,
            },
        },
    }
}

/// Project a tree-walker `Value` onto the backend-agnostic [`lang_stdlib::Arg`] the shared
/// stdlib dispatch reads. Only the primitive shapes the stdlib introspects are distinguished;
/// everything else collapses to `Other`. Mirrors the VM-side projection so both backends feed
/// the shared surface identically.
fn project_arg(value: &Value) -> lang_stdlib::Arg<'_> {
    match value {
        Value::Str(s) => lang_stdlib::Arg::Str(s),
        Value::Int(i) => lang_stdlib::Arg::Int(*i),
        Value::Float(f) => lang_stdlib::Arg::Float(*f),
        Value::Bool(b) => lang_stdlib::Arg::Bool(*b),
        _ => lang_stdlib::Arg::Other,
    }
}

/// Lift a shared stdlib [`lang_stdlib::Output`] back into a tree-walker `Value`.
fn output_to_value(output: lang_stdlib::Output) -> Value {
    match output {
        lang_stdlib::Output::Str(s) => Value::Str(s),
        lang_stdlib::Output::Bool(b) => Value::Bool(b),
        lang_stdlib::Output::Int(i) => Value::Int(i),
        lang_stdlib::Output::Float(f) => Value::Float(f),
        lang_stdlib::Output::StrList(items) => {
            Value::List(Rc::new(items.into_iter().map(Value::Str).collect()))
        }
    }
}

/// Map a stdlib misuse kind onto a diagnostic code: arity/argument-type mistakes are a
/// `TypeMismatch`; an out-of-range index/range is an `IndexOutOfBounds`.
fn std_error_code(kind: lang_stdlib::ErrorKind) -> DiagnosticCode {
    match kind {
        lang_stdlib::ErrorKind::Arity | lang_stdlib::ErrorKind::ArgType => {
            DiagnosticCode::TypeMismatch
        }
        lang_stdlib::ErrorKind::Bounds => DiagnosticCode::IndexOutOfBounds,
        lang_stdlib::ErrorKind::UnknownName => DiagnosticCode::UnknownName,
        lang_stdlib::ErrorKind::Io => DiagnosticCode::IoError,
    }
}

/// Field-wise (declared order) ordering of two same-type objects, the behavior synthesized by
/// `@derive(Comparable)`. Compares fields lexicographically via [`compare_primitive`]. Returns
/// `None` if `right` is not an object of the same type, or any field is non-primitive — the caller
/// turns that into a runtime type error. Mirrors `lang_value::structural_compare` (VM side).
fn object_structural_compare(left: &ObjectValue, right: &Value) -> Option<std::cmp::Ordering> {
    let Value::Object(rb) = right else {
        return None;
    };
    if left.def.name != rb.def.name {
        return None;
    }
    for f in &left.def.fields {
        let a = left.fields.get(&f.name)?;
        let b = rb.fields.get(&f.name)?;
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
/// `Value::to_json`: scalars reuse `display` (so both backends format numbers identically),
/// strings are quoted/escaped via [`json_string`], lists become JSON arrays, maps and objects
/// JSON objects (objects in declared field order), unit is `null`, and any other value falls
/// back to its quoted display form.
fn value_to_json(value: &Value) -> String {
    match value {
        Value::Bool(_) | Value::Int(_) | Value::Float(_) => value.display(),
        Value::Str(s) => json_string(s),
        Value::List(items) | Value::Set(items) => {
            let parts: Vec<String> = items.iter().map(value_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Map(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}:{}", json_string(k), value_to_json(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Object(object) => {
            // Declared field order for records/classes; an opaque imported stub uses key order.
            let parts: Vec<String> = if object.def.opaque {
                object
                    .fields
                    .iter()
                    .map(|(name, v)| format!("{}:{}", json_string(name), value_to_json(v)))
                    .collect()
            } else {
                object
                    .def
                    .fields
                    .iter()
                    .map(|f| {
                        let v = object.fields.get(&f.name).cloned().unwrap_or(Value::Unit);
                        format!("{}:{}", json_string(&f.name), value_to_json(&v))
                    })
                    .collect()
            };
            format!("{{{}}}", parts.join(","))
        }
        Value::Enum(e) => json_string(&e.variant),
        Value::Unit => "null".to_string(),
        other => json_string(&other.display()),
    }
}

/// Encode a string as a JSON string literal (quotes + the mandatory escapes). Byte-identical to
/// the VM's copy so `@derive(Serialize<Json>)` renders the same under both backends.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The total order of two primitives for `x.compare(y)`: integers compare exactly, strings
/// lexically, and any other numeric pairing as `f64`. Returns `None` when the operands are not
/// comparable (different non-numeric kinds, or a `NaN` float).
fn compare_primitive(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
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

/// Convert a parsed JSON tree into a tree-walker value: arrays become lists, objects become
/// sorted-key maps, `null` becomes unit. Mirrors the VM's `json_to_value` so `json.parse` builds
/// identical values in both backends.
fn json_to_value(json: lang_stdlib::json::Json) -> Value {
    use lang_stdlib::json::Json;
    match json {
        Json::Null => Value::Unit,
        Json::Bool(b) => Value::Bool(b),
        Json::Int(i) => Value::Int(i),
        Json::Float(f) => Value::Float(f),
        Json::Str(s) => Value::Str(s),
        Json::Array(items) => Value::List(Rc::new(items.into_iter().map(json_to_value).collect())),
        Json::Object(entries) => Value::Map(Rc::new(
            entries
                .into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        )),
    }
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
fn required_count(defaults: &[Option<Expr>]) -> usize {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_lexer::lex;
    use lang_parser::parse;
    use lang_span::{Source, SourceId};

    fn run(text: &str) -> RunResult {
        TreeWalkBackend::new().run(&program_of(text))
    }

    fn program_of(text: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "test.lang", text);
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
        // reproduces this identically (see lang-vm), guarded by the differential oracle.
        let out = run(
            "class Money {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): Money { return Money { amount: a, currency: c }; }\n  impl Add {\n    fn add(other: Money): Money { return Money { amount: amount + other.amount, currency: currency }; }\n  }\n}\na = Money.new(5, \"USD\");\nb = Money.new(3, \"USD\");\nt = a + b;\necho t.amount;\necho t.currency;\n",
        );
        assert_eq!(out.stdout, "8\nUSD\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn equatable_overrides_equality_and_negates_for_ne() {
        // `impl Equatable` routes `==`/`!=` to `eq` (here ignoring `tag`); `!=` negates. The VM
        // reproduces this identically (see lang-vm), guarded by the differential oracle.
        let out = run(
            "class M {\n  amount: int\n  tag: int\n  fn new(a: int, t: int): M { return M { amount: a, tag: t }; }\n  impl Equatable {\n    fn eq(other: M): bool { return amount == other.amount; }\n  }\n}\na = M.new(5, 1);\nb = M.new(5, 2);\necho a == b;\necho a != b;\necho a == M.new(9, 1);\n",
        );
        assert_eq!(out.stdout, "true\nfalse\nfalse\n");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn comparable_overloads_ordering_operators() {
        // `impl Comparable` routes `< <= > >=` to `compare` (delegating to the built-in primitive
        // `.compare()`); the returned `Ordering` is mapped to each operator's bool.
        let out = run(
            "class M {\n  amount: int\n  fn new(a: int): M { return M { amount: a }; }\n  impl Comparable {\n    fn compare(other: M): Ordering { return amount.compare(other.amount); }\n  }\n}\na = M.new(5);\nb = M.new(8);\necho a < b;\necho a > b;\necho a <= b;\necho a >= b;\n",
        );
        assert_eq!(out.stdout, "true\nfalse\ntrue\nfalse\n");
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
            session.eval(&program_of("next_id();")).value.as_deref(),
            Some("1")
        );
        assert_eq!(
            session.eval(&program_of("next_id();")).value.as_deref(),
            Some("2")
        );
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
    fn list_and_map_literals_and_len() {
        assert_eq!(run("echo [1, 2, 3];").stdout, "[1, 2, 3]\n");
        assert_eq!(run("echo len([1, 2, 3]);").stdout, "3\n");
        assert_eq!(run("echo {\"a\": 1, \"b\": 2}.count();").stdout, "2\n");
    }

    #[test]
    fn map_filter_sum_pipeline() {
        let src =
            "echo [1, 2, 3, 4] |> filter(fn(n) => n % 2 == 0) |> map(fn(n) => n * 10) |> sum();";
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
        assert_eq!(run("echo next_id(); echo next_id();").stdout, "1\n2\n");
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
        let src = "type Item = { price: float, qty: int }; a = Item { price: 2.5, qty: 4 }; echo a.price; echo a.price * a.qty;";
        assert_eq!(run(src).stdout, "2.5\n10.0\n");
    }

    #[test]
    fn records_compare_structurally() {
        let src = "type P = { x: int, y: int }; a = P { x: 1, y: 2 }; b = P { x: 1, y: 2 }; c = P { x: 1, y: 9 }; echo a == b; echo a == c;";
        assert_eq!(run(src).stdout, "true\nfalse\n");
    }

    #[test]
    fn class_constructor_and_instance_method() {
        let src = "class Box { v: int fn new(v: int): Box { return Box { v: v }; } fn doubled(): int { return v * 2; } } b = Box.new(21); echo b.doubled(); echo b.v;";
        assert_eq!(run(src).stdout, "42\n21\n");
    }

    #[test]
    fn method_takes_arguments_alongside_fields() {
        let src = "class Counter { base: int fn new(base: int): Counter { return Counter { base: base }; } fn plus(n: int): int { return base + n; } } c = Counter.new(10); echo c.plus(5);";
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
        let src = "class R { name: string fn new(name: string): R { return R { name: name }; } destruct { echo \"close ${name}\"; } } a = R.new(\"a\"); b = R.new(\"b\"); echo \"body\";";
        assert_eq!(run(src).stdout, "body\nclose b\nclose a\n");
    }

    #[test]
    fn reassignment_destroys_the_displaced_instance() {
        let src = "class R { name: string fn new(name: string): R { return R { name: name }; } destruct { echo \"close ${name}\"; } } mut x = R.new(\"first\"); x = R.new(\"second\"); echo \"mid\";";
        assert_eq!(run(src).stdout, "close first\nmid\nclose second\n");
    }

    #[test]
    fn unknown_field_in_literal_is_an_error() {
        let result = run("type R = { a: int }; r = R { a: 1, b: 2 };");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::UnknownName);
    }

    #[test]
    fn object_displays_as_a_literal() {
        let src = "type Pt = { x: int, y: int }; echo Pt { x: 1, y: 2 };";
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
