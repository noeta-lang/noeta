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
    RecordDecl, Stmt, StrPart, UnaryOp,
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
}

/// One program's worth of evaluation state.
struct Interpreter {
    stdout: String,
    diagnostics: Vec<Diagnostic>,
    ids: IdGen,
    scope: Rc<Scope>,
}

impl Interpreter {
    fn new(seed: u64) -> Interpreter {
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
        Interpreter {
            stdout: String::new(),
            diagnostics: Vec::new(),
            ids: IdGen::new(seed),
            scope: global,
        }
    }

    fn run(mut self, program: &Program) -> RunResult {
        for stmt in &program.stmts {
            match self.exec_stmt(stmt) {
                Ok(Flow::Normal) => {}
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
            Stmt::Echo { value, .. } => {
                let value = self.eval_expr(value)?;
                self.stdout.push_str(&value.display());
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
            // `namespace` is a no-op in M0 (no module scoping yet); `use` registers each
            // imported name as an opaque stub so references resolve.
            Stmt::Namespace { .. } => Ok(Flow::Normal),
            Stmt::Use { names, .. } => {
                self.declare_use(names);
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

    /// Execute statements in the current scope, stopping at the first `return`.
    fn exec_stmts(&mut self, stmts: &[Stmt]) -> Eval<Flow> {
        for stmt in stmts {
            if let Flow::Return(value) = self.exec_stmt(stmt)? {
                return Ok(Flow::Return(value));
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
        let elements = match self.eval_expr(iterable)? {
            Value::List(items) => (*items).clone(),
            // Iterating a map yields its values, in deterministic key order.
            Value::Map(entries) => entries.values().cloned().collect(),
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
            if let Flow::Return(value) = flow? {
                return Ok(Flow::Return(value));
            }
        }
        Ok(Flow::Normal)
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
            opaque: false,
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Register the names imported by a `use` declaration. Real module loading is M1; in
    /// M0 each imported name resolves to an *opaque stub type* so references and all-fields
    /// literals (`User { name: ... }`) work even though the type's real shape is unknown.
    fn declare_use(&mut self, names: &[lang_ast::UseName]) {
        for imported in names {
            let def = TypeDef {
                name: imported.name.clone(),
                fields: Vec::new(),
                methods: HashMap::new(),
                destructor: None,
                is_record: false,
                opaque: true,
            };
            self.scope
                .declare(imported.name.clone(), Value::Type(Rc::new(def)), false);
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
            opaque: false,
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Construct a record/class instance from an all-fields object literal. This is the
    /// full-initialization choke point: every declared field must end up set (by a named
    /// initializer or the `..` spread), or it is a [`DiagnosticCode::MissingField`] error.
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

        // `..base` fills the unnamed fields first (named initializers override below). For
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
            Expr::Interp { parts, .. } => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        StrPart::Literal(text) => out.push_str(text),
                        StrPart::Hole(expr) => out.push_str(&self.eval_expr(expr)?.display()),
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
        let arity_ok = args.is_empty();
        let result = match (name, &receiver) {
            ("count", Value::List(items)) if arity_ok => Some(Value::Int(items.len() as i64)),
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
        if args.len() != closure.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "this function takes {} argument(s) but {} were supplied",
                    closure.params.len(),
                    args.len()
                ),
            ));
        }
        let call_scope = Scope::child(&closure.captured);
        for (param, arg) in closure.params.iter().zip(args) {
            call_scope.declare(param.clone(), arg, false);
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
        if args.len() != method.params.len() {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "this method takes {} argument(s) but {} were supplied",
                    method.params.len(),
                    args.len()
                ),
            ));
        }
        let call_scope = Scope::child(&method.captured);
        for (name, value) in &object.fields {
            call_scope.declare(name.clone(), value.clone(), false);
        }
        call_scope.declare("self".to_string(), Value::Object(Rc::clone(object)), false);
        for (param, arg) in method.params.iter().zip(args) {
            call_scope.declare(param.clone(), arg, false);
        }
        let saved = std::mem::replace(&mut self.scope, call_scope);
        let result = match &method.body {
            FnBody::Arrow(expr) => self.eval_expr(expr),
            FnBody::Block(stmts) => self.exec_fn_body(stmts),
        };
        self.scope = saved;
        catch_return(result)
    }

    fn exec_fn_body(&mut self, stmts: &[Stmt]) -> Eval<Value> {
        match self.exec_stmts(stmts)? {
            Flow::Return(value) => Ok(value),
            Flow::Normal => Ok(Value::Unit),
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
                    Value::Map(entries) => Ok(Value::Int(entries.len() as i64)),
                    Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
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
        // `Equatable` `eq` method (`!=` negating the bool). Comparisons (`Comparable`) stay
        // built-in for now — they return `Ordering`, which is not a language type yet (M1.8b).
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

/// Construct a built-in `Result`/`Option` value (`Ok`/`Err`/`some`/`none`). These reuse
/// the ordinary [`EnumValue`] representation, so they participate in `match` and equality
/// like any enum; only their display and the `?`/`??` operators treat them specially.
fn builtin_enum(enum_name: &str, variant: &str, data: Vec<Value>) -> Value {
    Value::Enum(Rc::new(EnumValue {
        enum_name: enum_name.to_string(),
        variant: variant.to_string(),
        data,
    }))
}

/// At a function-call boundary, turn a `?`-induced early return into the call's value;
/// pass every other outcome (normal value or fatal abort) through unchanged.
fn catch_return(result: Eval<Value>) -> Eval<Value> {
    match result {
        Err(Unwind::Return(value)) => Ok(value),
        other => other,
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
            run("name = \"Niro\"; echo \"Hello {name}\";").stdout,
            "Hello Niro\n"
        );
        assert_eq!(run("echo \"sum is {1 + 2 * 3}\";").stdout, "sum is 7\n");
        assert_eq!(
            run("id = 1; echo \"Order #{id} ready\";").stdout,
            "Order #1 ready\n"
        );
    }

    #[test]
    fn interpolation_escapes_and_literal_braces() {
        assert_eq!(run("echo \"a\\tb\";").stdout, "a\tb\n");
        assert_eq!(run("echo \"{{literal}}\";").stdout, "{literal}\n");
    }

    #[test]
    fn plain_enum_and_match() {
        let src = "enum Color { Red; Green; Blue; } c = Color.Green; echo match c { Color.Red => \"r\", Color.Green => \"g\", Color.Blue => \"b\" };";
        assert_eq!(run(src).stdout, "g\n");
    }

    #[test]
    fn algebraic_enum_binds_data() {
        let src = "enum E { Empty; Code(n: int); } x = E.Code(42); echo match x { E.Empty => \"empty\", E.Code(n) => \"code {n}\" };";
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
        let src = "class M { amount: int currency: string fn new(a: int, c: string): M { return M { amount: a, currency: c }; } } a = M.new(500, \"USD\"); b = M { amount: 300, ..a }; echo b.amount; echo b.currency; echo a.amount;";
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
        let src = "class R { name: string fn new(name: string): R { return R { name: name }; } destruct { echo \"close {name}\"; } } a = R.new(\"a\"); b = R.new(\"b\"); echo \"body\";";
        assert_eq!(run(src).stdout, "body\nclose b\nclose a\n");
    }

    #[test]
    fn reassignment_destroys_the_displaced_instance() {
        let src = "class R { name: string fn new(name: string): R { return R { name: name }; } destruct { echo \"close {name}\"; } } mut x = R.new(\"first\"); x = R.new(\"second\"); echo \"mid\";";
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
                   echo match x { E.Bad(i) => \"bad {i}\" };";
        assert_eq!(run(src).stdout, "bad 5\n");
    }

    #[test]
    fn result_and_option_participate_in_match() {
        let src = "fn run_it(b): int { if b { return Ok(1); } return Err(\"no\"); } \
                   echo match run_it(true) { Ok(n) => \"ok {n}\", Err(e) => \"err {e}\" }; \
                   echo match run_it(false) { Ok(n) => \"ok {n}\", Err(e) => \"err {e}\" };";
        assert_eq!(run(src).stdout, "ok 1\nerr no\n");
    }
}
