//! The bytecode compiler: AST → [`Module`].
//!
//! As of M1.5 the compiler lowers the **whole** M0 language: the literal/binding/arithmetic
//! core (M1.0); **functions** (`fn`, calls, arrow closures, the `|>` pipeline, `return`,
//! `if`/`else` — M1.2); **collections** (`[...]`/`{...}` literals, `for`-iteration, the
//! `len`/`map`/`filter`/`sum` builtins, `.count()`/`.enumerate()`, string interpolation —
//! M1.3); the **object model** (records/classes/enums on shapes, member access, methods,
//! `...spread` — M1.4); and **`match`/`?`/`??`** with the `Result`/`Option` constructors and
//! `panic`/`next_id` (M1.5). A nested closure or `fn` that captures an enclosing function's
//! local is lowered via **upvalues** (slice F1 — see [`freevars`]); the few constructs still
//! outside the subset (a closure inside a *method* capturing `self`/a field; a prelude
//! value/builtin used as a value) return [`Unsupported`] and the differential harness skips
//! them — but every M0 corpus program compiles and is asserted identical to the tree-walker.
//!
//! ## The scope model
//!
//! The tree-walker resolves names through a chain of reference-counted lexical scopes. The
//! VM splits that into three tiers:
//!
//! - **Globals.** Every top-level binding and `fn` name lives in a runtime global table,
//!   read/written by name (`LoadGlobal`/`StoreGlobal`). A top-level function's free variables
//!   resolve here at call time — faithful, because the tree-walker's captured scope for a
//!   top-level function *is* the (shared, mutable) global scope, so reads see live values.
//! - **Frame-locals.** Parameters and locals live in registers, one register file per call
//!   frame. Block scopes (`if`/`else` bodies) nest within the same register file.
//! - **Upvalues.** A local captured by an inner closure is boxed into a heap *cell* shared
//!   between the defining frame and every capturing closure, so a closure reads (and mutates)
//!   the live binding — matching the tree-walker's `Rc`-captured scope chain. The free-variable
//!   analysis in [`freevars`] decides which locals are celled and lays out each closure's
//!   ordered upvalues; the closure carries the cells (`MakeClosure` captures), and the body
//!   reaches them with `UpvalueGet`/`UpvalueSet`.
//!
//! The compiler stays faithful to the tree-walker's evaluation order and exact diagnostic
//! text/spans, because the differential oracle compares full `RunResult`s. Registers are
//! allocated monotonically (one per value, no reuse) — simple and obviously correct.

use std::collections::{HashMap, HashSet};

use lang_ast::{
    BinaryOp, Expr, FnDecl, ForPattern, ObjectLit, Param, Pattern, Program, Stmt, StrPart,
};
use lang_builtins::PRELUDE_NAMES;
use lang_bytecode::{
    AttributeRecord, BoolSide, Builtin, CaptureFrom, Chunk, Const, MethodEntry, Module, Op, Reg,
};

mod freevars;
use freevars::FnBody;
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_object::{Shape, ShapeKind};
use lang_span::Span;

/// Why a program could not be lowered to bytecode yet — a node outside the current subset.
/// The differential harness treats this as "skip", not "fail".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub reason: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported by the VM: {}", self.reason)
    }
}

fn unsupported<T>(reason: impl Into<String>) -> Result<T, Unsupported> {
    Err(Unsupported {
        reason: reason.into(),
    })
}

/// Whether `use <path>.{name}` imports a Ring 2 native module (`use std.{json}`) rather than a
/// sibling-module declaration. Such names are bound as global values, not opaque types.
fn is_native_module(path: &[String], name: &str) -> bool {
    path == ["std"] && lang_stdlib::NativeModule::from_name(name).is_some()
}

/// Compile a whole program to a [`Module`], or report the first unsupported construct.
///
/// Three passes: (1) register every top-level type so forward references resolve and shapes
/// exist before any body is compiled; (2) compile each class method/associated function into
/// its reserved prototype; (3) compile the top-level program. Splitting (1) from (3) mirrors
/// the tree-walker, whose type declarations are all evaluated before the driver code runs.
pub fn compile(program: &Program) -> Result<Module, Unsupported> {
    let mut module = ModuleCompiler {
        protos: vec![Chunk::placeholder()],
        shapes: Vec::new(),
        methods: Vec::new(),
        destructors: Vec::new(),
        comparable_derives: Vec::new(),
        tojson_derives: Vec::new(),
        manifest: Vec::new(),
        types: HashMap::new(),
        module_globals: HashMap::new(),
    };
    module.register_globals(program);
    module.register_types(program);
    module.compile_methods(program)?;
    let main = {
        let mut fc = FnCompiler::new(&mut module, true, None, Vec::new(), Vec::new());
        for stmt in &program.stmts {
            fc.stmt(stmt)?;
        }
        fc.code.push(Op::Halt);
        fc.into_chunk(0)
    };
    module.protos[0] = main;
    Ok(Module {
        protos: module.protos,
        shapes: module.shapes,
        methods: module.methods,
        destructors: module.destructors,
        comparable_derives: module.comparable_derives,
        tojson_derives: module.tojson_derives,
        manifest: module.manifest,
    })
}

/// What a top-level type name denotes, with the layout/dispatch data the compiler needs to
/// lower object literals, member access, method calls, and enum construction.
enum TypeInfo {
    /// A structural record (`type X = {...}`): declared field order.
    Record { fields: Vec<String> },
    /// A class: declared field order plus each `fn`'s reserved prototype index (a class `fn`
    /// is callable both as an associated function `X.f(...)` and as an instance method
    /// `obj.f(...)`, so one prototype serves both — see [`ModuleCompiler::compile_methods`]).
    Class {
        fields: Vec<String>,
        fns: HashMap<String, u32>,
    },
    /// An enum: each variant's positional data-field names.
    Enum {
        variants: HashMap<String, Vec<String>>,
    },
    /// A `use`-imported stub whose real field set is unknown until a literal supplies it.
    Opaque,
}

/// Accumulates the prototype table, the shape/method side tables, and the top-level type
/// environment across compilation.
struct ModuleCompiler {
    protos: Vec<Chunk>,
    shapes: Vec<Shape>,
    methods: Vec<MethodEntry>,
    destructors: Vec<(String, u32)>,
    comparable_derives: Vec<String>,
    tojson_derives: Vec<String>,
    manifest: Vec<AttributeRecord>,
    types: HashMap<String, TypeInfo>,
    /// Every top-level value global's name and whether it is mutable. Computed before any body
    /// is compiled so a nested function can resolve a global (and check its mutability on
    /// assignment) and so the free-variable analysis can tell a global from a captured local.
    module_globals: HashMap<String, bool>,
}

impl ModuleCompiler {
    /// Pre-pass: collect every top-level value global (a binding or `fn`/native-module name) and
    /// its mutability, so functions can resolve and assign globals and the capture analysis can
    /// distinguish a global from an enclosing local.
    fn register_globals(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                Stmt::Binding { mut_decl, name, .. } => {
                    self.module_globals.insert(name.clone(), *mut_decl);
                }
                Stmt::Fn(decl) => {
                    self.module_globals.insert(decl.name.clone(), false);
                }
                Stmt::Use { path, names, .. } => {
                    for imported in names {
                        if is_native_module(path, &imported.name) {
                            self.module_globals.insert(imported.name.clone(), false);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The set of top-level global names (for the free-variable analysis' globals argument).
    fn global_names(&self) -> HashSet<String> {
        self.module_globals.keys().cloned().collect()
    }

    /// Record a declaration's `#[...]` data attributes into the build manifest, in source order.
    fn record_attributes(&mut self, type_name: &str, attrs: &[lang_ast::Attribute]) {
        for attr in attrs {
            self.manifest.push(AttributeRecord {
                type_name: type_name.to_string(),
                name: attr.name.clone(),
                args: attr.args.iter().map(|(arg, _)| arg.clone()).collect(),
            });
        }
    }

    /// Pass 1: register every top-level `type`/`class`/`enum`/`use` so bodies compiled later
    /// can resolve them, and reserve a placeholder prototype for each class `fn`.
    fn register_types(&mut self, program: &Program) {
        // The built-in `Ordering` enum is namable like any other (so `Ordering.Less` can be
        // constructed, not only received from `.compare()`); registered first so a user `enum
        // Ordering` would shadow it. Its variants carry no data, matching `make_ordering`.
        self.types.insert(
            "Ordering".to_string(),
            TypeInfo::Enum {
                variants: ["Less", "Equal", "Greater"]
                    .into_iter()
                    .map(|v| (v.to_string(), Vec::new()))
                    .collect(),
            },
        );
        for stmt in &program.stmts {
            match stmt {
                Stmt::Record(decl) => {
                    let fields = decl.fields.iter().map(|f| f.name.clone()).collect();
                    if lang_ast::derives_trait(&decl.derives, "Comparable") {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    if lang_ast::derives_trait(&decl.derives, "ToJson") {
                        self.tojson_derives.push(decl.name.clone());
                    }
                    self.record_attributes(&decl.name, &decl.attrs);
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Record { fields });
                }
                Stmt::Class(decl) => {
                    let fields: Vec<String> = decl.fields.iter().map(|f| f.name.clone()).collect();
                    // A hand-written `compare` (via `impl Comparable`) takes precedence over the
                    // derived structural ordering.
                    if lang_ast::derives_trait(&decl.derives, "Comparable")
                        && !decl.methods.iter().any(|m| m.name == "compare")
                    {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    // A hand-written `to_json` takes precedence over the derived serializer.
                    if lang_ast::derives_trait(&decl.derives, "ToJson")
                        && !decl.methods.iter().any(|m| m.name == "to_json")
                    {
                        self.tojson_derives.push(decl.name.clone());
                    }
                    let mut fns = HashMap::new();
                    for method in &decl.methods {
                        let proto = self.protos.len() as u32;
                        self.protos.push(Chunk::placeholder());
                        fns.insert(method.name.clone(), proto);
                        self.methods.push(MethodEntry {
                            type_name: decl.name.clone(),
                            method: method.name.clone(),
                            proto,
                        });
                    }
                    // Reserve a prototype for the `destruct` block (compiled like a method).
                    if decl.destructor.is_some() {
                        let proto = self.protos.len() as u32;
                        self.protos.push(Chunk::placeholder());
                        self.destructors.push((decl.name.clone(), proto));
                    }
                    self.record_attributes(&decl.name, &decl.attrs);
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Class { fields, fns });
                }
                Stmt::Enum(decl) => {
                    let variants = decl
                        .variants
                        .iter()
                        .map(|v| {
                            (
                                v.name.clone(),
                                v.fields.iter().map(|f| f.name.clone()).collect(),
                            )
                        })
                        .collect();
                    self.record_attributes(&decl.name, &decl.attrs);
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Enum { variants });
                }
                Stmt::Use { path, names, .. } => {
                    for imported in names {
                        // A `use std.{json}` native module resolves as a global value (bound at
                        // the `use` site), not an opaque type, so it is not registered here.
                        if is_native_module(path, &imported.name) {
                            continue;
                        }
                        self.types.insert(imported.name.clone(), TypeInfo::Opaque);
                    }
                }
                _ => {}
            }
        }
    }

    /// Pass 2: compile each class `fn` body into its reserved prototype. Methods see all
    /// registered types (forward references work) and run with the receiver in register 0,
    /// the declared parameters in registers `1..`, and field names resolving to the receiver.
    fn compile_methods(&mut self, program: &Program) -> Result<(), Unsupported> {
        for stmt in &program.stmts {
            let Stmt::Class(decl) = stmt else { continue };
            let field_set: HashSet<String> = decl.fields.iter().map(|f| f.name.clone()).collect();
            for method in &decl.methods {
                let TypeInfo::Class { fns, .. } = &self.types[&decl.name] else {
                    unreachable!("a class registered as non-class");
                };
                let proto = fns[&method.name];
                let chunk = self.compile_fn_body(
                    &method.params,
                    Body::Block(&method.body),
                    Some(MethodCtx {
                        fields: field_set.clone(),
                    }),
                    Vec::new(),
                    Vec::new(),
                )?;
                self.protos[proto as usize] = chunk;
            }
            // The `destruct` block compiles like a parameterless method (fields in scope).
            if let Some(body) = &decl.destructor {
                let proto = self
                    .destructors
                    .iter()
                    .find(|(name, _)| name == &decl.name)
                    .map(|(_, proto)| *proto)
                    .expect("a destructor proto was reserved in pass 1");
                let chunk = self.compile_fn_body(
                    &[],
                    Body::Block(body),
                    Some(MethodCtx {
                        fields: field_set.clone(),
                    }),
                    Vec::new(),
                    Vec::new(),
                )?;
                self.protos[proto as usize] = chunk;
            }
        }
        Ok(())
    }

    /// Compile one function/closure/method body into a [`Chunk`]. A `method` context reserves
    /// register 0 for the receiver (`self`) and resolves the class's field names against it.
    /// `upvalues` are the names (and mutability) this function captures from enclosing functions,
    /// in the order its parent will supply the cells; `enclosing_locals` are the enclosing
    /// functions' capturable local names (outermost first), so the function can lower its own
    /// nested closures.
    fn compile_fn_body(
        &mut self,
        params: &[Param],
        body: Body<'_>,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<Chunk, Unsupported> {
        let is_method = method.is_some();
        let globals = self.global_names();
        let fn_body = match body {
            Body::Block(stmts) => FnBody::Block(stmts),
            Body::Arrow(expr) => FnBody::Arrow(expr),
        };
        let analysis = freevars::analyze(params, fn_body, &enclosing_locals, &globals);

        // The capturable layer this function exposes to its own nested closures. A method also
        // exposes `self`/its fields, but capturing them is left unsupported this slice — they go
        // into `forbidden` so a nested closure that reaches for one is skipped, not miscompiled.
        let mut local_layer = analysis.local.clone();
        let mut forbidden = HashSet::new();
        if let Some(ctx) = &method {
            forbidden.insert("self".to_string());
            forbidden.extend(ctx.fields.iter().cloned());
            local_layer.extend(forbidden.iter().cloned());
        }

        let mut fc = FnCompiler::new(self, false, method, upvalues, enclosing_locals);
        fc.celled = analysis.celled;
        fc.local_layer = local_layer;
        fc.forbidden = forbidden;

        // A method reserves register 0 for the receiver; ordinary functions do not.
        if is_method {
            fc.alloc_reg();
        }
        fc.scopes.push(HashMap::new());
        for param in params {
            let reg = fc.alloc_reg();
            let celled = fc.celled.contains(&param.name);
            // A captured parameter is boxed into a cell so the closure shares the live binding.
            if celled {
                fc.code.push(Op::MakeCell { dst: reg, src: reg });
            }
            fc.scopes.last_mut().unwrap().insert(
                param.name.clone(),
                Var {
                    reg,
                    mutable: false,
                    celled,
                },
            );
        }
        match body {
            Body::Block(stmts) => {
                for stmt in stmts {
                    fc.stmt(stmt)?;
                }
            }
            Body::Arrow(expr) => {
                let t = fc.alloc_reg();
                fc.expr(expr, t)?;
                fc.code.push(Op::Return { src: t });
            }
        }
        // A block body that falls off the end implicitly returns unit (M0's `exec_fn_body`).
        fc.code.push(Op::Halt);
        let num_params = params.len() as u16 + if is_method { 1 } else { 0 };
        Ok(fc.into_chunk(num_params))
    }

    /// Compile a `fn` body into a fresh prototype and return its index.
    fn add_function(
        &mut self,
        params: &[Param],
        body: Body<'_>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<u32, Unsupported> {
        let chunk = self.compile_fn_body(params, body, None, upvalues, enclosing_locals)?;
        let idx = self.protos.len() as u32;
        self.protos.push(chunk);
        Ok(idx)
    }

    /// Compile an arrow closure body into a fresh prototype and return its index.
    fn add_closure(
        &mut self,
        params: &[Param],
        body: &Expr,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<u32, Unsupported> {
        let chunk =
            self.compile_fn_body(params, Body::Arrow(body), None, upvalues, enclosing_locals)?;
        let idx = self.protos.len() as u32;
        self.protos.push(chunk);
        Ok(idx)
    }

    /// Intern a shape, returning its index in the shape table (equal shapes share an index, so
    /// equal-built aggregates share one runtime shape — the declared-order determinism the
    /// object model guarantees).
    fn intern_shape(&mut self, shape: Shape) -> u32 {
        if let Some(i) = self.shapes.iter().position(|s| *s == shape) {
            return i as u32;
        }
        let idx = self.shapes.len() as u32;
        self.shapes.push(shape);
        idx
    }

    /// Intern a built-in `Result`/`Option` variant shape (these display with their bare
    /// constructor, `Ok(x)`/`none`, rather than `Type.Variant`). The data carried varies at
    /// the use site, so the shape needs no declared field names.
    fn builtin_enum_shape(&mut self, enum_name: &str, variant: &str) -> u32 {
        self.intern_shape(Shape::enum_variant(
            enum_name.to_string(),
            variant.to_string(),
            Vec::new(),
            true,
        ))
    }
}

/// The method-compilation context: the field names that resolve to the receiver in register 0.
struct MethodCtx {
    fields: HashSet<String>,
}

/// A function body: a statement block (`fn`) or a single arrow expression (closure).
enum Body<'a> {
    Block(&'a [Stmt]),
    Arrow(&'a Expr),
}

/// Per-prototype compilation state (one register file, one constant/diagnostic pool).
struct FnCompiler<'m> {
    module: &'m mut ModuleCompiler,
    code: Vec<Op>,
    consts: Vec<Const>,
    diags: Vec<Diagnostic>,
    /// Register scopes, innermost last. Empty only in `main` at the global depth.
    scopes: Vec<HashMap<String, Var>>,
    /// Declared top-level globals and their mutability (tracked only in `main`).
    globals: HashMap<String, GlobalInfo>,
    next_reg: u16,
    is_main: bool,
    /// When compiling a class method, the receiver's field names (resolved against register 0).
    method: Option<MethodCtx>,
    /// This function's captured upvalues: name → index, indexed into the frame's upvalue cells.
    upvalue_index: HashMap<String, u16>,
    /// Whether each captured upvalue (by index) is mutable in its defining scope — governs
    /// whether an `x = v` reassignment through the upvalue is allowed or raises E0006.
    upvalue_mut: Vec<bool>,
    /// The names of this function's own locals that an inner closure captures, so they are
    /// stored as cells (computed by the free-variable analysis before lowering).
    celled: HashSet<String>,
    /// Names a nested closure must not capture from here (a method's `self`/fields): capturing
    /// one is left unsupported this slice, so it is skipped rather than miscompiled.
    forbidden: HashSet<String>,
    /// The enclosing functions' capturable locals (outermost first) — for lowering this
    /// function's own nested closures.
    enclosing_locals: Vec<HashSet<String>>,
    /// This function's own capturable locals (the layer it exposes to its nested closures).
    local_layer: HashSet<String>,
}

#[derive(Clone, Copy)]
struct Var {
    reg: Reg,
    mutable: bool,
    /// Whether this local lives in a cell (it is captured by an inner closure). A celled local
    /// is read with `CellGet`, written with `CellSet`, and shared with the capturing closure.
    celled: bool,
}

#[derive(Clone, Copy)]
struct GlobalInfo {
    mutable: bool,
}

/// A nested closure's resolved captures: its ordered upvalue list (each name and whether it is
/// mutable, for the child's reassignment check) paired with the `CaptureFrom` source the building
/// frame uses to supply each cell. The two vectors are index-aligned.
type CaptureLayout = (Vec<(String, bool)>, Vec<CaptureFrom>);

/// How a name resolves at a use site.
enum Resolved {
    /// A frame-local register holding the value directly.
    Local(Reg),
    /// A frame-local register holding a cell (a captured local); read/written through it.
    CelledLocal(Reg),
    /// The method receiver (`self`) — register 0 in a method body.
    SelfRecv,
    /// A field of the method receiver, loaded via `LoadField` from register 0.
    Field,
    /// An upvalue captured from an enclosing function (read via its index). Reassignment goes
    /// through `binding`, which checks the upvalue's mutability separately.
    Upvalue(u16),
    /// A global, read via `LoadGlobal` (the name may or may not exist at runtime).
    Global,
    /// A prelude value/builtin — not yet modeled by the VM, so the program is skipped.
    Prelude,
}

impl<'m> FnCompiler<'m> {
    fn new(
        module: &'m mut ModuleCompiler,
        is_main: bool,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> FnCompiler<'m> {
        let mut upvalue_index = HashMap::new();
        let mut upvalue_mut = Vec::with_capacity(upvalues.len());
        for (i, (name, mutable)) in upvalues.into_iter().enumerate() {
            upvalue_index.insert(name, i as u16);
            upvalue_mut.push(mutable);
        }
        FnCompiler {
            module,
            code: Vec::new(),
            consts: Vec::new(),
            diags: Vec::new(),
            scopes: Vec::new(),
            globals: HashMap::new(),
            next_reg: 0,
            is_main,
            method,
            upvalue_index,
            upvalue_mut,
            celled: HashSet::new(),
            forbidden: HashSet::new(),
            enclosing_locals,
            local_layer: HashSet::new(),
        }
    }

    /// The enclosing-locals chain to pass to one of *this* function's nested closures: the chain
    /// we were given, extended with our own capturable layer.
    fn child_enclosing(&self) -> Vec<HashSet<String>> {
        let mut chain = self.enclosing_locals.clone();
        chain.push(self.local_layer.clone());
        chain
    }

    fn into_chunk(self, num_params: u16) -> Chunk {
        Chunk {
            code: self.code,
            consts: self.consts,
            diagnostics: self.diags,
            num_params,
            num_registers: self.next_reg,
        }
    }

    fn alloc_reg(&mut self) -> Reg {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn add_const(&mut self, value: Const) -> u16 {
        let idx = self.consts.len() as u16;
        self.consts.push(value);
        idx
    }

    fn add_diag(&mut self, diag: Diagnostic) -> u16 {
        let idx = self.diags.len() as u16;
        self.diags.push(diag);
        idx
    }

    /// Whether we are at `main`'s global depth (no register scope pushed). A binding here is
    /// a global; once a block (or function) is entered, bindings become frame-locals.
    fn at_global_depth(&self) -> bool {
        self.is_main && self.scopes.is_empty()
    }

    fn lookup_local(&self, name: &str) -> Option<Var> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn resolve(&self, name: &str) -> Resolved {
        if let Some(var) = self.lookup_local(name) {
            return if var.celled {
                Resolved::CelledLocal(var.reg)
            } else {
                Resolved::Local(var.reg)
            };
        }
        // Inside a method, `self` and the class's fields resolve against the receiver. Locals
        // (parameters) are checked first, so a parameter shadows a same-named field — matching
        // the tree-walker, which binds fields, then `self`, then parameters (last wins).
        if let Some(ctx) = &self.method {
            if name == "self" {
                return Resolved::SelfRecv;
            }
            if ctx.fields.contains(name) {
                return Resolved::Field;
            }
        }
        // A name captured from an enclosing function resolves to an upvalue cell. Checked before
        // globals/prelude: the tree-walker captures the nearest lexical binding, so a same-named
        // global must not silently take its place.
        if let Some(&index) = self.upvalue_index.get(name) {
            return Resolved::Upvalue(index);
        }
        if self.is_main && self.globals.contains_key(name) {
            return Resolved::Global;
        }
        if PRELUDE_NAMES.contains(&name) {
            return Resolved::Prelude;
        }
        // Unknown in `main` (→ runtime E0005), or a forward/global reference inside a function.
        Resolved::Global
    }

    /// Compute, for a nested closure/`fn` with the given params and body, the ordered upvalue
    /// list (name + mutability) it captures and the matching `CaptureFrom` source for each (in
    /// this — the building — function's terms). Returns `Unsupported` if a capture reaches a
    /// method's `self`/field or cannot be sourced (e.g. a forward capture of a not-yet-declared
    /// local), so such a program is skipped rather than miscompiled.
    fn resolve_captures(
        &self,
        params: &[Param],
        body: FnBody<'_>,
    ) -> Result<CaptureLayout, Unsupported> {
        let globals = self.module.global_names();
        let enclosing = self.child_enclosing();
        let free = freevars::free_vars(params, body, &enclosing, &globals);
        let mut upvalues = Vec::with_capacity(free.len());
        let mut captures = Vec::with_capacity(free.len());
        for name in free {
            if self.forbidden.contains(&name) {
                return unsupported("a closure inside a method capturing `self` or a field");
            }
            if let Some(var) = self.lookup_local(&name) {
                if !var.celled {
                    return unsupported("a forward capture of a not-yet-celled local");
                }
                upvalues.push((name, var.mutable));
                captures.push(CaptureFrom::Local(var.reg));
            } else if let Some(&index) = self.upvalue_index.get(&name) {
                upvalues.push((name, self.upvalue_mut[index as usize]));
                captures.push(CaptureFrom::Upvalue(index));
            } else {
                // A free name the analysis flagged but that is neither a live celled local nor an
                // upvalue here (e.g. captured before its binding was lowered) — skip the program.
                return unsupported("a capture that could not be sourced from the enclosing frame");
            }
        }
        Ok((upvalues, captures))
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Unsupported> {
        match stmt {
            Stmt::Echo { value, span } => {
                let t = self.alloc_reg();
                self.expr(value, t)?;
                // Route a `Display` object through its `to_string`; identity otherwise.
                let s = self.alloc_reg();
                self.code.push(Op::Stringify {
                    dst: s,
                    src: t,
                    span: *span,
                });
                self.code.push(Op::Echo { reg: s });
                Ok(())
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => self.binding(*mut_decl, name, *name_span, value),
            Stmt::Fn(decl) => self.declare_fn(decl),
            Stmt::Return { value, span } => self.return_stmt(value.as_ref(), *span),
            Stmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => self.if_stmt(cond, then_body, else_body.as_deref(), *span),
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => self.for_stmt(pattern, iterable, body, *span),
            Stmt::While { cond, body, span } => self.while_stmt(cond, body, *span),
            Stmt::Expr { expr, .. } => {
                // Evaluated for its side effects (and any error); the value is discarded.
                let t = self.alloc_reg();
                self.expr(expr, t)
            }
            // Type declarations are registered in the pre-pass and class methods compiled
            // separately; as statements they emit no code (the tree-walker likewise just
            // records them in scope). `namespace` is a no-op; a non-`std` `use` registers
            // opaque stubs (also at compile time).
            Stmt::Record(_) | Stmt::Class(_) | Stmt::Enum(_) | Stmt::Namespace { .. } => Ok(()),
            // `use std.{json, ...}` binds each native module as a global value (mirroring the
            // tree-walker's `declare_use`); other imports emit nothing.
            Stmt::Use { path, names, .. } => {
                for imported in names {
                    if is_native_module(path, &imported.name) {
                        let value = self.alloc_reg();
                        let k = self.add_const(Const::NativeModule(imported.name.clone()));
                        self.code.push(Op::LoadConst { dst: value, k });
                        self.code.push(Op::StoreGlobal {
                            name: imported.name.clone(),
                            src: value,
                        });
                    }
                }
                Ok(())
            }
        }
    }

    /// `fn name(params) { body }` — compile the body to a prototype, then bind `name` to a
    /// closure over it. A top-level `fn` is a global capturing only globals. A nested `fn` is a
    /// local that may capture enclosing locals as upvalues; if it (or a sibling) captures the
    /// `fn` itself, the binding is celled — and the cell is created *before* the closure is built
    /// so a self-recursive `fn` can capture its own (still-unset) cell.
    fn declare_fn(&mut self, decl: &FnDecl) -> Result<(), Unsupported> {
        if self.at_global_depth() {
            let proto = self.module.add_function(
                &decl.params,
                Body::Block(&decl.body),
                Vec::new(),
                Vec::new(),
            )?;
            let t = self.alloc_reg();
            self.code.push(Op::MakeClosure {
                dst: t,
                proto,
                captures: Box::new([]),
            });
            self.globals
                .insert(decl.name.clone(), GlobalInfo { mutable: false });
            self.code.push(Op::StoreGlobal {
                name: decl.name.clone(),
                src: t,
            });
            return Ok(());
        }

        let celled = self.celled.contains(&decl.name);
        let reg = self.alloc_reg();
        if celled {
            // Pre-create the cell (holding unit) and bind the name, so the body's references to
            // itself source this cell; the closure value is stored into it once built.
            let unit = self.alloc_reg();
            let k = self.add_const(Const::Unit);
            self.code.push(Op::LoadConst { dst: unit, k });
            self.code.push(Op::MakeCell {
                dst: reg,
                src: unit,
            });
            self.scopes.last_mut().unwrap().insert(
                decl.name.clone(),
                Var {
                    reg,
                    mutable: false,
                    celled: true,
                },
            );
        }
        let (upvalues, captures) =
            self.resolve_captures(&decl.params, FnBody::Block(&decl.body))?;
        let enclosing = self.child_enclosing();
        let proto =
            self.module
                .add_function(&decl.params, Body::Block(&decl.body), upvalues, enclosing)?;
        let t = self.alloc_reg();
        self.code.push(Op::MakeClosure {
            dst: t,
            proto,
            captures: captures.into_boxed_slice(),
        });
        if celled {
            self.code.push(Op::CellSet { cell: reg, src: t });
        } else {
            self.declare_local(&decl.name, t, false);
        }
        Ok(())
    }

    fn return_stmt(&mut self, value: Option<&Expr>, _span: Span) -> Result<(), Unsupported> {
        let t = self.alloc_reg();
        match value {
            Some(expr) => self.expr(expr, t)?,
            None => {
                let k = self.add_const(Const::Unit);
                self.code.push(Op::LoadConst { dst: t, k });
            }
        }
        self.code.push(Op::Return { src: t });
        Ok(())
    }

    /// `if cond { then } else { else }`, lowered to a bool-check and forward jumps. Mirrors
    /// the tree-walker: a non-bool condition is E0007 at the `if`'s span, and each branch
    /// body runs in its own (block) scope.
    fn if_stmt(
        &mut self,
        cond: &Expr,
        then_body: &[Stmt],
        else_body: Option<&[Stmt]>,
        span: Span,
    ) -> Result<(), Unsupported> {
        let rc = self.alloc_reg();
        self.expr(cond, rc)?;
        self.code.push(Op::RequireCondBool { reg: rc, span });
        let jf = self.code.len();
        self.code.push(Op::JumpIfFalse { reg: rc, target: 0 });

        self.block(then_body)?;

        match else_body {
            Some(else_body) => {
                let j_end = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                let else_start = self.code.len() as u32;
                self.patch_jump(jf, else_start);
                self.block(else_body)?;
                let end = self.code.len() as u32;
                self.patch_jump(j_end, end);
            }
            None => {
                let end = self.code.len() as u32;
                self.patch_jump(jf, end);
            }
        }
        Ok(())
    }

    /// `for <pattern> in <iterable> { body }`, lowered to an index loop over a snapshot of
    /// the iterable's elements. Mirrors the tree-walker: the iterable is snapshotted once
    /// (`IterSnapshot`), a non-iterable is E0007 at the `for`'s span, each iteration binds the
    /// pattern in a fresh body scope, and a map iterates its values in sorted-key order.
    fn for_stmt(
        &mut self,
        pattern: &ForPattern,
        iterable: &Expr,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), Unsupported> {
        // The loop's bookkeeping registers live in the enclosing frame (a list snapshot, its
        // length, the running index, and a constant 1 to advance it).
        let items = self.alloc_reg();
        self.expr(iterable, items)?;
        self.code.push(Op::IterSnapshot {
            dst: items,
            src: items,
            span,
        });
        let len = self.alloc_reg();
        self.code.push(Op::ListLen {
            dst: len,
            src: items,
            span,
        });
        let index = self.alloc_reg();
        let zero = self.add_const(Const::Int(0));
        self.code.push(Op::LoadConst {
            dst: index,
            k: zero,
        });
        let one_reg = self.alloc_reg();
        let one = self.add_const(Const::Int(1));
        self.code.push(Op::LoadConst {
            dst: one_reg,
            k: one,
        });

        let loop_top = self.code.len() as u32;
        let cond = self.alloc_reg();
        self.code.push(Op::Binary {
            op: BinaryOp::Lt,
            dst: cond,
            a: index,
            b: len,
            span,
        });
        let exit_jump = self.code.len();
        self.code.push(Op::JumpIfFalse {
            reg: cond,
            target: 0,
        });

        // Fetch the current element and bind the loop pattern in a fresh body scope.
        let element = self.alloc_reg();
        self.code.push(Op::ListGet {
            dst: element,
            list: items,
            index,
        });
        self.scopes.push(HashMap::new());
        let result = (|| {
            match pattern {
                ForPattern::Single { name, .. } => {
                    self.bind_loop_var(name, element);
                }
                ForPattern::Pair { first, second, .. } => {
                    let first_reg = self.alloc_reg();
                    let second_reg = self.alloc_reg();
                    self.code.push(Op::DestructurePair {
                        first: first_reg,
                        second: second_reg,
                        src: element,
                        span,
                    });
                    self.bind_loop_var(first, first_reg);
                    self.bind_loop_var(second, second_reg);
                }
            }
            for stmt in body {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        self.scopes.pop();
        result?;

        // Advance the index and loop back; patch the exit once the end is known.
        self.code.push(Op::Binary {
            op: BinaryOp::Add,
            dst: index,
            a: index,
            b: one_reg,
            span,
        });
        self.code.push(Op::Jump { target: loop_top });
        let end = self.code.len() as u32;
        self.patch_jump(exit_jump, end);
        Ok(())
    }

    /// `while <cond> { body }`, lowered to a top-tested loop: evaluate the condition, require it
    /// be a bool, exit if false, run the body in a fresh scope, then jump back. Mirrors the
    /// tree-walker — a bare reassignment in the body updates its enclosing binding's register, so
    /// the condition makes progress.
    fn while_stmt(&mut self, cond: &Expr, body: &[Stmt], span: Span) -> Result<(), Unsupported> {
        let loop_top = self.code.len() as u32;
        let rc = self.alloc_reg();
        self.expr(cond, rc)?;
        self.code.push(Op::RequireCondBool { reg: rc, span });
        let exit_jump = self.code.len();
        self.code.push(Op::JumpIfFalse { reg: rc, target: 0 });

        self.block(body)?;

        self.code.push(Op::Jump { target: loop_top });
        let end = self.code.len() as u32;
        self.patch_jump(exit_jump, end);
        Ok(())
    }

    /// Register an already-populated register as an immutable loop-body binding. Unlike
    /// [`FnCompiler::declare_local`] this emits no `Move`: the element/destructure op has
    /// already written the value into `reg`.
    fn bind_loop_var(&mut self, name: &str, reg: Reg) {
        let celled = self.celled.contains(name);
        // A captured loop variable is boxed in place; the `MakeCell` sits inside the loop body, so
        // each iteration captures a distinct cell (matching the tree-walker's per-iteration scope).
        if celled {
            self.code.push(Op::MakeCell { dst: reg, src: reg });
        }
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Var {
                reg,
                mutable: false,
                celled,
            },
        );
    }

    /// Compile a brace-delimited block in its own (block) scope.
    fn block(&mut self, stmts: &[Stmt]) -> Result<(), Unsupported> {
        self.scopes.push(HashMap::new());
        let result = (|| {
            for stmt in stmts {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    fn patch_jump(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            Op::Jump { target: t }
            | Op::JumpIfFalse { target: t, .. }
            | Op::JumpIfTrue { target: t, .. }
            | Op::Coalesce { fallback: t, .. }
            | Op::MatchInt { fail: t, .. }
            | Op::MatchStr { fail: t, .. }
            | Op::MatchBool { fail: t, .. }
            | Op::MatchVariant { fail: t, .. } => *t = target,
            _ => unreachable!("patching a jump we just emitted"),
        }
    }

    /// `mut x = v`, an immutable `x = v` declaration, or a reassignment — mirroring the
    /// tree-walker's `bind`: the value is always evaluated first, then the binding rule
    /// applies (so a reassignment to an immutable still runs the value's side effects).
    fn binding(
        &mut self,
        mut_decl: bool,
        name: &str,
        name_span: Span,
        value: &Expr,
    ) -> Result<(), Unsupported> {
        if mut_decl {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            if self.at_global_depth() {
                self.globals
                    .insert(name.to_string(), GlobalInfo { mutable: true });
                self.code.push(Op::StoreGlobal {
                    name: name.to_string(),
                    src: t,
                });
            } else {
                self.declare_local(name, t, true);
            }
            return Ok(());
        }

        // A bare `x = v`: reassign the nearest existing binding (searching local scopes, then
        // captured upvalues, then globals — mirroring the tree-walker's outward `Scope::assign`),
        // else declare a fresh local.
        if let Some(var) = self.lookup_local(name) {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            if !var.mutable {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            } else if var.celled {
                self.code.push(Op::CellSet {
                    cell: var.reg,
                    src: t,
                });
            } else {
                self.code.push(Op::Move {
                    dst: var.reg,
                    src: t,
                });
            }
            return Ok(());
        }

        // A captured upvalue: reassign through its cell, enforcing the source binding's mutability.
        if let Some(&index) = self.upvalue_index.get(name) {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            if self.upvalue_mut[index as usize] {
                self.code.push(Op::UpvalueSet { index, src: t });
            } else {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            return Ok(());
        }

        // A global: in `main` only the globals declared so far are visible (matching the
        // tree-walker's not-yet-declared lookups); inside a function every module global is.
        let global_mut = if self.is_main {
            self.globals.get(name).map(|info| info.mutable)
        } else {
            self.module.module_globals.get(name).copied()
        };
        if let Some(mutable) = global_mut {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            if mutable {
                self.code.push(Op::StoreGlobal {
                    name: name.to_string(),
                    src: t,
                });
            } else {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            return Ok(());
        }

        // Not found anywhere: a new immutable binding in the current scope.
        let t = self.alloc_reg();
        self.expr(value, t)?;
        if self.scopes.is_empty() {
            self.globals
                .insert(name.to_string(), GlobalInfo { mutable: false });
            self.code.push(Op::StoreGlobal {
                name: name.to_string(),
                src: t,
            });
        } else {
            self.declare_local(name, t, false);
        }
        Ok(())
    }

    /// Bind `name` to a fresh local register initialized from `src`, reusing the slot if the
    /// name already exists in the innermost scope (a re-`mut` shadow). A captured (celled) local
    /// boxes `src` into a fresh cell instead — re-run each time the binding executes (e.g. per
    /// loop iteration), so each entry gets a distinct cell, matching the tree-walker's fresh
    /// per-iteration scope.
    fn declare_local(&mut self, name: &str, src: Reg, mutable: bool) {
        let celled = self.celled.contains(name);
        let reg = match self.scopes.last().unwrap().get(name) {
            Some(v) => v.reg,
            None => self.alloc_reg(),
        };
        if celled {
            self.code.push(Op::MakeCell { dst: reg, src });
        } else {
            self.code.push(Op::Move { dst: reg, src });
        }
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Var {
                reg,
                mutable,
                celled,
            },
        );
    }

    /// Lower `expr` so its value ends up in register `dst`.
    fn expr(&mut self, expr: &Expr, dst: Reg) -> Result<(), Unsupported> {
        match expr {
            Expr::Str { value, .. } => {
                let k = self.add_const(Const::Str(value.clone()));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Int { value, .. } => {
                let k = self.add_const(Const::Int(*value));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Float { value, .. } => {
                let k = self.add_const(Const::Float(*value));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Bool { value, .. } => {
                let k = self.add_const(Const::Bool(*value));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Ident { name, span } => match self.resolve(name) {
                Resolved::Local(reg) => self.code.push(Op::Move { dst, src: reg }),
                Resolved::CelledLocal(reg) => self.code.push(Op::CellGet { dst, cell: reg }),
                Resolved::Upvalue(index) => self.code.push(Op::UpvalueGet { dst, index }),
                Resolved::SelfRecv => self.code.push(Op::Move { dst, src: 0 }),
                Resolved::Field => self.code.push(Op::LoadField {
                    dst,
                    obj: 0,
                    field: name.clone(),
                    span: *span,
                }),
                Resolved::Global => self.code.push(Op::LoadGlobal {
                    dst,
                    name: name.clone(),
                    span: *span,
                }),
                // `none` is the one prelude *value* (not a function): the `Option.none` variant.
                Resolved::Prelude if name == "none" => {
                    let shape = self.module.builtin_enum_shape("Option", "none");
                    self.code.push(Op::MakeEnum {
                        dst,
                        shape,
                        args: Box::new([]),
                    });
                }
                // A bare reference to a collection builtin becomes a first-class native-function
                // value (a direct call still uses `CallBuiltin`). Other prelude names used as
                // values (the `Ok`/`Err`/`some` constructors, `panic`, `next_id`) are not yet
                // first-class, so they remain unsupported.
                Resolved::Prelude => match Builtin::from_name(name) {
                    Some(func) => self.code.push(Op::LoadNativeFn { dst, func }),
                    None => return unsupported("reference to a prelude value/builtin"),
                },
            },
            Expr::Closure { params, body, .. } => {
                // Resolve the closure's captures in this (the building) frame's terms, then
                // compile its body with the matching upvalue layout and emit `MakeClosure` so the
                // VM threads the captured cells into the new closure.
                let (upvalues, captures) = self.resolve_captures(params, FnBody::Arrow(body))?;
                let enclosing = self.child_enclosing();
                let proto = self.module.add_closure(params, body, upvalues, enclosing)?;
                self.code.push(Op::MakeClosure {
                    dst,
                    proto,
                    captures: captures.into_boxed_slice(),
                });
            }
            Expr::List { items, .. } => {
                let mut regs = Vec::with_capacity(items.len());
                for item in items {
                    let r = self.alloc_reg();
                    self.expr(item, r)?;
                    regs.push(r);
                }
                self.code.push(Op::MakeList {
                    dst,
                    items: regs.into_boxed_slice(),
                });
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let start_reg = self.alloc_reg();
                self.expr(start, start_reg)?;
                let end_reg = self.alloc_reg();
                self.expr(end, end_reg)?;
                self.code.push(Op::MakeRange {
                    dst,
                    start: start_reg,
                    end: end_reg,
                    inclusive: *inclusive,
                    span: *span,
                });
            }
            Expr::Map { entries, span } => {
                // Evaluate each key, check it is a string (matching M0's per-entry error
                // timing), then evaluate the value — then assemble the map.
                let mut pairs = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let key_reg = self.alloc_reg();
                    self.expr(key, key_reg)?;
                    self.code.push(Op::RequireMapKey {
                        reg: key_reg,
                        span: *span,
                    });
                    let value_reg = self.alloc_reg();
                    self.expr(value, value_reg)?;
                    pairs.push((key_reg, value_reg));
                }
                self.code.push(Op::MakeMap {
                    dst,
                    entries: pairs.into_boxed_slice(),
                });
            }
            Expr::Interp { parts, span } => {
                // Build the string by concatenating each part's display form, mirroring the
                // tree-walker: literal text verbatim, a `{expr}` hole via its `display`. `~`
                // concatenation already produces exactly that, so fold the parts with it.
                let empty = self.add_const(Const::Str(String::new()));
                self.code.push(Op::LoadConst { dst, k: empty });
                for part in parts {
                    let r = self.alloc_reg();
                    match part {
                        StrPart::Literal(text) => {
                            let k = self.add_const(Const::Str(text.clone()));
                            self.code.push(Op::LoadConst { dst: r, k });
                        }
                        StrPart::Hole(expr) => {
                            self.expr(expr, r)?;
                            // Route a `Display` object through its `to_string` before the
                            // concatenation stringifies it; identity for every other value.
                            self.code.push(Op::Stringify {
                                dst: r,
                                src: r,
                                span: *span,
                            });
                        }
                    }
                    self.code.push(Op::Binary {
                        op: BinaryOp::Concat,
                        dst,
                        a: dst,
                        b: r,
                        span: *span,
                    });
                }
            }
            Expr::Object(lit) => self.object_literal(lit, dst)?,
            Expr::Member {
                receiver,
                name,
                span,
                ..
            } => self.member(receiver, name, dst, *span)?,
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                let recv = self.alloc_reg();
                self.expr(receiver, recv)?;
                let idx = self.alloc_reg();
                self.expr(index, idx)?;
                self.code.push(Op::Index {
                    dst,
                    recv,
                    index: idx,
                    span: *span,
                });
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => self.match_expr(scrutinee, arms, dst, *span)?,
            Expr::Try { expr, span } => {
                let src = self.alloc_reg();
                self.expr(expr, src)?;
                self.code.push(Op::TryUnwrap {
                    dst,
                    src,
                    span: *span,
                });
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => self.coalesce(value, fallback, dst, *span)?,
            Expr::Call { callee, args, span } => self.call(callee, args, None, dst, *span)?,
            Expr::Pipeline { left, right, span } => self.pipeline(left, right, dst, *span)?,
            Expr::Unary { op, operand, span } => {
                self.expr(operand, dst)?;
                self.code.push(Op::Unary {
                    op: *op,
                    dst,
                    src: dst,
                    span: *span,
                });
            }
            Expr::Binary { op, lhs, rhs, span } => {
                if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    self.logical(*op, lhs, rhs, dst, *span)?;
                } else {
                    self.expr(lhs, dst)?;
                    let r = self.alloc_reg();
                    self.expr(rhs, r)?;
                    self.code.push(Op::Binary {
                        op: *op,
                        dst,
                        a: dst,
                        b: r,
                        span: *span,
                    });
                }
            }
        }
        Ok(())
    }

    /// Lower a call `callee(args)`, optionally with a `prepend`ed first argument (the value
    /// threaded by the pipeline operator). Evaluation order mirrors the tree-walker exactly:
    /// the prepended value first (it is computed before this point), then the callee, then
    /// the remaining arguments left to right.
    fn call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        prepend: Option<Reg>,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        if let Expr::Member { receiver, name, .. } = callee {
            // `Type.something(args)` where `Type` is a known type name: an enum-variant
            // construction or an associated-function call, both resolved here at compile time.
            if let Expr::Ident {
                name: type_name, ..
            } = &**receiver
            {
                if let Some(TypeInfo::Enum { .. }) = self.module.types.get(type_name) {
                    return self.make_enum(type_name, name, args, prepend, dst, span);
                }
                if let Some(TypeInfo::Class { fns, .. }) = self.module.types.get(type_name)
                    && let Some(&proto) = fns.get(name)
                {
                    return self.call_associated(proto, args, prepend, dst, span);
                }
            }
            // Otherwise the receiver is a value: a runtime-dispatched method call (a user
            // instance method, or a `count`/`enumerate` built-in — the VM decides).
            let recv = self.alloc_reg();
            self.expr(receiver, recv)?;
            let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
            arg_regs.extend(prepend);
            for arg in args {
                let r = self.alloc_reg();
                self.expr(arg, r)?;
                arg_regs.push(r);
            }
            self.code.push(Op::CallMethod {
                dst,
                recv,
                method: name.clone(),
                args: arg_regs.into_boxed_slice(),
                span,
            });
            return Ok(());
        }
        // A prelude function called directly by name. A user binding of the same name shadows
        // the prelude (resolved as an ordinary call below).
        if let Expr::Ident { name, .. } = callee
            && matches!(self.resolve(name), Resolved::Prelude)
        {
            if let Some(builtin) = Builtin::from_name(name) {
                // `len`/`map`/`filter`/`sum` — the collection builtins.
                let args = self.eval_args(args, prepend)?;
                self.code.push(Op::CallBuiltin {
                    dst,
                    builtin,
                    args,
                    span,
                });
                return Ok(());
            }
            return match name.as_str() {
                // `Ok(x)`/`Ok()`, `Err(e)`, `some(x)` — the Result/Option constructors.
                "Ok" => self.make_result_option("Result", "Ok", args, prepend, dst, span),
                "Err" => self.make_result_option("Result", "Err", args, prepend, dst, span),
                "some" => self.make_result_option("Option", "some", args, prepend, dst, span),
                "panic" => self.make_panic(args, prepend, dst, span),
                "next_id" if prepend.is_none() && args.is_empty() => {
                    self.code.push(Op::NextId { dst });
                    Ok(())
                }
                _ => unsupported("prelude function not in the VM subset"),
            };
        }
        let callee_reg = self.alloc_reg();
        self.expr(callee, callee_reg)?;
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
        arg_regs.extend(prepend);
        for arg in args {
            let r = self.alloc_reg();
            self.expr(arg, r)?;
            arg_regs.push(r);
        }
        self.code.push(Op::Call {
            dst,
            callee: callee_reg,
            args: arg_regs.into_boxed_slice(),
            span,
        });
        Ok(())
    }

    /// `left |> right`: thread `left` as `right`'s first argument, result into `dst`. `x |>
    /// f(a)` is `f(x, a)`, `x |> f` is `f(x)`. The left operand is evaluated first (as in the
    /// tree-walker), then the callee, then any further arguments.
    fn pipeline(
        &mut self,
        left: &Expr,
        right: &Expr,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let left_reg = self.alloc_reg();
        self.expr(left, left_reg)?;
        match right {
            Expr::Call { callee, args, .. } => self.call(callee, args, Some(left_reg), dst, span),
            // `x |> obj.m` is `obj.m(x)` — thread the piped value as the sole argument.
            Expr::Member { .. } => self.call(right, &[], Some(left_reg), dst, span),
            _ => {
                // `x |> f` — call the value of `right` with the single threaded argument.
                let callee_reg = self.alloc_reg();
                self.expr(right, callee_reg)?;
                self.code.push(Op::Call {
                    dst,
                    callee: callee_reg,
                    args: Box::new([left_reg]),
                    span,
                });
                Ok(())
            }
        }
    }

    /// `Type { field: value, ...spread }` — construct a record/class/opaque instance, or raise
    /// the tree-walker's runtime error for an unknown type.
    fn object_literal(&mut self, lit: &ObjectLit, dst: Reg) -> Result<(), Unsupported> {
        match self.module.types.get(&lit.type_name) {
            Some(TypeInfo::Record { fields }) => {
                let fields = fields.clone();
                self.make_record(lit, ShapeKind::Record, fields, dst)
            }
            Some(TypeInfo::Class { fields, .. }) => {
                let fields = fields.clone();
                self.make_record(lit, ShapeKind::Class, fields, dst)
            }
            Some(TypeInfo::Opaque) => self.make_opaque(lit, dst),
            Some(TypeInfo::Enum { .. }) => unsupported("enum type used as a record literal"),
            None => {
                // The tree-walker looks the type up first and errors before touching fields.
                let idx = self.add_diag(unknown_type_diag(&lit.type_name, lit.type_name_span));
                self.code.push(Op::Raise { idx });
                Ok(())
            }
        }
    }

    /// Construct a declared record/class instance. Field initializers are checked and
    /// evaluated in source order (after the spread), reproducing the tree-walker's timing; the
    /// full-initialization guarantee (E0009) is enforced at runtime by `MakeRecord`.
    fn make_record(
        &mut self,
        lit: &ObjectLit,
        kind: ShapeKind,
        fields: Vec<String>,
        dst: Reg,
    ) -> Result<(), Unsupported> {
        let shape =
            self.module
                .intern_shape(Shape::object(kind, lit.type_name.clone(), fields.clone()));
        let spread = self.spread_reg(lit)?;
        let mut named: Vec<(u16, Reg)> = Vec::with_capacity(lit.fields.len());
        for init in &lit.fields {
            let Some(slot) = fields.iter().position(|f| f == &init.name) else {
                // An unknown field errors before its value is evaluated (tree-walker timing).
                let idx = self.add_diag(unknown_field_diag(
                    &lit.type_name,
                    &init.name,
                    init.name_span,
                ));
                self.code.push(Op::Raise { idx });
                return Ok(());
            };
            let r = self.alloc_reg();
            self.expr(&init.value, r)?;
            named.push((slot as u16, r));
        }
        self.code.push(Op::MakeRecord {
            dst,
            shape,
            named: named.into_boxed_slice(),
            spread,
            span: lit.span,
        });
        Ok(())
    }

    /// Construct an opaque (`use`-imported) instance: any fields are accepted, the runtime
    /// builds a sorted-key shape, and there are no field checks.
    fn make_opaque(&mut self, lit: &ObjectLit, dst: Reg) -> Result<(), Unsupported> {
        let spread = self.spread_reg(lit)?;
        let mut keys: Vec<(String, Reg)> = Vec::with_capacity(lit.fields.len());
        for init in &lit.fields {
            let r = self.alloc_reg();
            self.expr(&init.value, r)?;
            keys.push((init.name.clone(), r));
        }
        self.code.push(Op::MakeOpaque {
            dst,
            type_name: lit.type_name.clone(),
            keys: keys.into_boxed_slice(),
            spread,
        });
        Ok(())
    }

    /// Evaluate an object literal's `...spread` base into a register, if present.
    fn spread_reg(&mut self, lit: &ObjectLit) -> Result<Option<Reg>, Unsupported> {
        match &lit.spread {
            Some(spread) => {
                let r = self.alloc_reg();
                self.expr(spread, r)?;
                Ok(Some(r))
            }
            None => Ok(None),
        }
    }

    /// Bare member access: a no-data enum variant (`Status.Pending`) or a field load.
    fn member(
        &mut self,
        receiver: &Expr,
        name: &str,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        if let Expr::Ident {
            name: type_name, ..
        } = receiver
        {
            enum Member {
                EmptyVariant,
                Unsupported(&'static str),
                FieldAccess,
            }
            let kind = match self.module.types.get(type_name) {
                Some(TypeInfo::Enum { variants }) => match variants.get(name) {
                    Some(fields) if fields.is_empty() => Member::EmptyVariant,
                    Some(_) => Member::Unsupported("data-carrying variant used without arguments"),
                    None => Member::Unsupported("unknown enum variant"),
                },
                Some(_) => Member::Unsupported("type member used as a value"),
                None => Member::FieldAccess,
            };
            match kind {
                Member::EmptyVariant => {
                    let shape = self.module.intern_shape(Shape::enum_variant(
                        type_name.clone(),
                        name.to_string(),
                        Vec::new(),
                        false,
                    ));
                    self.code.push(Op::MakeEnum {
                        dst,
                        shape,
                        args: Box::new([]),
                    });
                    return Ok(());
                }
                Member::Unsupported(reason) => return unsupported(reason),
                Member::FieldAccess => {}
            }
        }
        let obj = self.alloc_reg();
        self.expr(receiver, obj)?;
        self.code.push(Op::LoadField {
            dst,
            obj,
            field: name.to_string(),
            span,
        });
        Ok(())
    }

    /// Construct an enum variant carrying data (`OrderError.NegativePrice(i)`), optionally with
    /// a pipeline-threaded leading value.
    fn make_enum(
        &mut self,
        type_name: &str,
        variant: &str,
        args: &[Expr],
        prepend: Option<Reg>,
        dst: Reg,
        _span: Span,
    ) -> Result<(), Unsupported> {
        let fields = match self.module.types.get(type_name) {
            Some(TypeInfo::Enum { variants }) => match variants.get(variant) {
                Some(fields) => fields.clone(),
                None => return unsupported("unknown enum variant"),
            },
            _ => unreachable!("make_enum is only reached for enum types"),
        };
        let shape = self.module.intern_shape(Shape::enum_variant(
            type_name.to_string(),
            variant.to_string(),
            fields,
            false,
        ));
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
        arg_regs.extend(prepend);
        for arg in args {
            let r = self.alloc_reg();
            self.expr(arg, r)?;
            arg_regs.push(r);
        }
        self.code.push(Op::MakeEnum {
            dst,
            shape,
            args: arg_regs.into_boxed_slice(),
        });
        Ok(())
    }

    /// Call an associated function `Type.f(args)`. The method prototype reserves register 0
    /// for `self`; an associated call has no receiver, so unit is passed there (the tree-walker
    /// binds no `self` for a type-receiver call).
    fn call_associated(
        &mut self,
        proto: u32,
        args: &[Expr],
        prepend: Option<Reg>,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let self_reg = self.alloc_reg();
        let k = self.add_const(Const::Unit);
        self.code.push(Op::LoadConst { dst: self_reg, k });
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 2);
        arg_regs.push(self_reg);
        arg_regs.extend(prepend);
        for arg in args {
            let r = self.alloc_reg();
            self.expr(arg, r)?;
            arg_regs.push(r);
        }
        let callee = self.alloc_reg();
        self.code.push(Op::MakeClosure {
            dst: callee,
            proto,
            captures: Box::new([]),
        });
        self.code.push(Op::Call {
            dst,
            callee,
            args: arg_regs.into_boxed_slice(),
            span,
        });
        Ok(())
    }

    /// Evaluate a `prepend` (the pipeline-threaded value, if any) followed by `args`, each into
    /// a fresh register, returning the register list.
    fn eval_args(
        &mut self,
        args: &[Expr],
        prepend: Option<Reg>,
    ) -> Result<Box<[Reg]>, Unsupported> {
        let mut regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
        regs.extend(prepend);
        for arg in args {
            let r = self.alloc_reg();
            self.expr(arg, r)?;
            regs.push(r);
        }
        Ok(regs.into_boxed_slice())
    }

    /// Construct a built-in `Result`/`Option` value (`Ok`/`Err`/`some`). `Ok` accepts 0 or 1
    /// arguments (the void success `Ok()` and the wrapping `Ok(x)`); `Err`/`some` take 1.
    fn make_result_option(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Expr],
        prepend: Option<Reg>,
        dst: Reg,
        _span: Span,
    ) -> Result<(), Unsupported> {
        let arg_regs = self.eval_args(args, prepend)?;
        let allowed = if variant == "Ok" { 0..=1 } else { 1..=1 };
        if !allowed.contains(&arg_regs.len()) {
            return unsupported("Result/Option constructor with an unexpected argument count");
        }
        let shape = self.module.builtin_enum_shape(enum_name, variant);
        self.code.push(Op::MakeEnum {
            dst,
            shape,
            args: arg_regs,
        });
        Ok(())
    }

    /// `panic(msg)` — evaluate the message and emit the abort op (E0010).
    fn make_panic(
        &mut self,
        args: &[Expr],
        prepend: Option<Reg>,
        _dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let arg_regs = self.eval_args(args, prepend)?;
        if arg_regs.len() != 1 {
            return unsupported("`panic` with an unexpected argument count");
        }
        self.code.push(Op::Panic {
            msg: arg_regs[0],
            span,
        });
        Ok(())
    }

    /// `value ?? fallback` — unwrap the success payload, or evaluate `fallback` on the empty
    /// case (mirroring the tree-walker's `eval_coalesce`).
    fn coalesce(
        &mut self,
        value: &Expr,
        fallback: &Expr,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let src = self.alloc_reg();
        self.expr(value, src)?;
        let coalesce_pos = self.code.len();
        self.code.push(Op::Coalesce {
            dst,
            src,
            fallback: 0,
            span,
        });
        // Success path: `dst` is set, then jump past the fallback.
        let jump_end = self.code.len();
        self.code.push(Op::Jump { target: 0 });
        let fallback_start = self.code.len() as u32;
        self.patch_jump(coalesce_pos, fallback_start);
        self.expr(fallback, dst)?;
        let end = self.code.len() as u32;
        self.patch_jump(jump_end, end);
        Ok(())
    }

    /// `match scrutinee { pattern => body, ... }` — lowered to a linear decision chain: each
    /// arm tests its pattern (jumping to the next arm on mismatch), binds, evaluates its body
    /// into `dst`, and jumps to the end. A value matching no arm hits `MatchFail` (E0007),
    /// reproducing the tree-walker's runtime non-exhaustive-match error.
    fn match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[lang_ast::MatchArm],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let s = self.alloc_reg();
        self.expr(scrutinee, s)?;
        let mut end_jumps: Vec<usize> = Vec::new();
        for arm in arms {
            let mut fail_jumps: Vec<usize> = Vec::new();
            self.scopes.push(HashMap::new());
            self.emit_pattern(&arm.pattern, s, &mut fail_jumps);
            let body = self.expr(&arm.body, dst);
            self.scopes.pop();
            body?;
            end_jumps.push(self.code.len());
            self.code.push(Op::Jump { target: 0 });
            // A mismatch in this arm jumps to the next arm (or the `MatchFail` below).
            let next = self.code.len() as u32;
            for pos in fail_jumps {
                self.patch_jump(pos, next);
            }
        }
        self.code.push(Op::MatchFail { src: s, span });
        let end = self.code.len() as u32;
        for pos in end_jumps {
            self.patch_jump(pos, end);
        }
        Ok(())
    }

    /// Emit the test for one pattern against value register `reg`, recording into `fail_jumps`
    /// the positions of every conditional that must jump to the arm's failure target. Bindings
    /// alias the matched register into the current (arm) scope. Mirrors `match_pattern`.
    fn emit_pattern(&mut self, pattern: &Pattern, reg: Reg, fail_jumps: &mut Vec<usize>) {
        match pattern {
            Pattern::Wildcard { .. } => {}
            Pattern::Binding { name, .. } => self.bind_loop_var(name, reg),
            Pattern::Int { value, .. } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchInt {
                    src: reg,
                    value: *value,
                    fail: 0,
                });
            }
            Pattern::Str { value, .. } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchStr {
                    src: reg,
                    value: value.clone(),
                    fail: 0,
                });
            }
            Pattern::Bool { value, .. } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchBool {
                    src: reg,
                    value: *value,
                    fail: 0,
                });
            }
            Pattern::Variant {
                type_name,
                variant,
                bindings,
                ..
            } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchVariant {
                    src: reg,
                    type_name: type_name.clone(),
                    variant: variant.clone(),
                    arity: bindings.len() as u16,
                    fail: 0,
                });
                for (i, sub) in bindings.iter().enumerate() {
                    let dr = self.alloc_reg();
                    self.code.push(Op::ExtractField {
                        dst: dr,
                        src: reg,
                        index: i as u16,
                    });
                    self.emit_pattern(sub, dr, fail_jumps);
                }
            }
        }
    }

    /// Lower `a && b` / `a || b` to branches, matching the tree-walker's `eval_logical`:
    /// the left operand must be a bool; on short-circuit its value is the result; otherwise
    /// the right operand must be a bool and is the result.
    fn logical(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        self.expr(lhs, dst)?;
        self.code.push(Op::RequireBool {
            reg: dst,
            side: BoolSide::Left,
            op,
            span,
        });
        let jump_pos = self.code.len();
        // Short-circuit: `&&` stops when the left is false, `||` when it is true.
        self.code.push(match op {
            BinaryOp::And => Op::JumpIfFalse {
                reg: dst,
                target: 0,
            },
            BinaryOp::Or => Op::JumpIfTrue {
                reg: dst,
                target: 0,
            },
            _ => unreachable!("logical only handles && and ||"),
        });
        self.expr(rhs, dst)?;
        self.code.push(Op::RequireBool {
            reg: dst,
            side: BoolSide::Right,
            op,
            span,
        });
        let end = self.code.len() as u32;
        self.patch_jump(jump_pos, end);
        Ok(())
    }
}

fn immutable_diag(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ImmutableAssignment,
        span,
        format!("cannot assign to `{name}`, which is immutable"),
    )
    .with_help(format!(
        "declare it with `mut {name} = ...` to allow reassignment"
    ))
}

/// `cannot find type `T` in this scope` — an object literal naming an undeclared type
/// (mirrors the tree-walker's `eval_object` type lookup).
fn unknown_type_diag(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownName,
        span,
        format!("cannot find type `{name}` in this scope"),
    )
}

/// `type `T` has no field `f`` — an object literal initializing a field the type does not
/// declare (mirrors the tree-walker's `has_field` check).
fn unknown_field_diag(type_name: &str, field: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownName,
        span,
        format!("type `{type_name}` has no field `{field}`"),
    )
}
