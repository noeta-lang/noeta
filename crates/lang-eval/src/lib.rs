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

/// The observable outcome of running a program: everything it wrote to stdout, its
/// process exit code, and any runtime diagnostics it produced. This is the unit the
/// conformance harness compares and the unit two backends are checked to agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub stdout: String,
    pub exit_code: i32,
    pub diagnostics: Vec<Diagnostic>,
}

impl RunResult {
    /// Whether the run produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// An execution backend. M0 ships exactly one (`TreeWalkBackend`); M1 adds the
/// bytecode VM as a second, and they are cross-checked against this contract.
pub trait Backend {
    fn run(&self, program: &Program) -> RunResult;
}

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
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::NextId => "next_id",
            Builtin::Len => "len",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Sum => "sum",
        }
    }

    /// The prelude functions registered in every program's global scope.
    const PRELUDE: &'static [Builtin] = &[
        Builtin::NextId,
        Builtin::Len,
        Builtin::Map,
        Builtin::Filter,
        Builtin::Sum,
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
        if self.data.is_empty() {
            format!("{}.{}", self.enum_name, self.variant)
        } else {
            let parts: Vec<String> = self.data.iter().map(Value::display).collect();
            format!("{}.{}({})", self.enum_name, self.variant, parts.join(", "))
        }
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
    /// Whether this came from a `type X = {...}` record (vs. a `class`). Cosmetic in M0.
    is_record: bool,
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
    /// `Type { field: value, ... }`, fields in declared order.
    pub fn display(&self) -> String {
        let parts: Vec<String> = self
            .def
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
            .collect();
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
    parent: Option<Rc<Scope>>,
}

/// The outcome of trying to reassign an existing binding through the scope chain.
enum AssignOutcome {
    Assigned,
    Immutable,
    NotFound,
}

impl Scope {
    fn global() -> Rc<Scope> {
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent: None,
        })
    }

    fn child(parent: &Rc<Scope>) -> Rc<Scope> {
        Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
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
        self.vars
            .borrow_mut()
            .insert(name, Binding { value, mutable });
    }

    /// Reassign an existing binding, searching outward through the chain.
    fn assign(&self, name: &str, value: Value) -> AssignOutcome {
        if let Some(binding) = self.vars.borrow_mut().get_mut(name) {
            return if binding.mutable {
                binding.value = value;
                AssignOutcome::Assigned
            } else {
                AssignOutcome::Immutable
            };
        }
        match &self.parent {
            Some(parent) => parent.assign(name, value),
            None => AssignOutcome::NotFound,
        }
    }
}

/// Sentinel returned by evaluation when an error has already been recorded and
/// execution of the current program should stop (a panic-like abort).
struct Aborted;

type Eval<T> = Result<T, Aborted>;

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
                // A top-level `return` or a runtime error stops the program.
                Ok(Flow::Return(_)) | Err(Aborted) => break,
            }
        }
        let exit_code = if self.diagnostics.is_empty() { 0 } else { 1 };
        RunResult {
            stdout: self.stdout,
            exit_code,
            diagnostics: self.diagnostics,
        }
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
            is_record: true,
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
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
            is_record: false,
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

        // `..base` fills the unnamed fields first (named initializers override below).
        if let Some(spread) = &lit.spread {
            match self.eval_expr(spread)? {
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
            if !def.has_field(&init.name) {
                return Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    init.name_span,
                    format!("type `{}` has no field `{}`", def.name(), init.name),
                ));
            }
            let value = self.eval_expr(&init.value)?;
            fields.insert(init.name.clone(), value);
        }

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
            AssignOutcome::Assigned => Ok(()),
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
                Err(Aborted)
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
                    Err(Aborted)
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
        result
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
        result
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
    fn runtime_error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Aborted {
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Aborted
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
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse errors: {:?}",
            parsed.diagnostics
        );
        TreeWalkBackend::new().run(&parsed.program)
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
    fn unknown_field_in_literal_is_an_error() {
        let result = run("type R = { a: int }; r = R { a: 1, b: 2 };");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::UnknownName);
    }

    #[test]
    fn object_displays_as_a_literal() {
        let src = "type Pt = { x: int, y: int }; echo Pt { x: 1, y: 2 };";
        assert_eq!(run(src).stdout, "Pt {x: 1, y: 2}\n");
    }
}
