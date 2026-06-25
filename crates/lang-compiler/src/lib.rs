//! The bytecode compiler: **Core IR → [`Module`]**.
//!
//! Since the memory-management migration's Phase 2 the compiler lowers the shared
//! [A-normal-form Core IR][lang_ir] both backends run, not the surface AST: [`compile`] first
//! lowers the parsed program to `lang_ir` (the same lowering the IR interpreter consumes), then
//! emits bytecode from it. Because the IR has already named every intermediate value and fixed
//! evaluation order, the compiler is a near-1:1 structural lowering — an IR `let v = a + b`
//! becomes an `Op::Binary`, a constructor an `Op::MakeRecord`/`MakeEnum`, an IR `if` a branch —
//! rather than re-deriving order by recursively flattening nested expressions. Type
//! registration (shapes, the method/destructor proto table) still reads the surface declarations
//! the IR carries verbatim. Register allocation stays monotonic this phase; the reuse-aware
//! allocator and precise drops are the next phase's IR passes (which also re-establish the
//! record-update/COW in-place reuse this phase drops — see the migration README).
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

use lang_ast::{BinaryOp, Program, TypeRef};
use lang_builtins::PRELUDE_NAMES;
use lang_bytecode::{
    BoolSide, Builtin, CaptureFrom, Chunk, Const, MethodEntry, Module, NarrowTarget, Op, Reg,
};
use lang_ir::{
    Atom, Block, Const as IrConst, Decl, ForPattern, Func, InterpPart, Pattern, Rvalue, Stmt, Temp,
    Thunk,
};

mod freevars;
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
    compile_with_sites(program, lang_check::resolve_type_of_sites(program))
}

/// Which record-update reuse the compiler may apply to a self-update `acc = Type { ...acc, f: v }`.
///
/// **Inert since Phase 2 of the memory-management migration.** The AST-keyed record-update / COW
/// in-place reuse this selected was dropped when the compiler moved onto the A-normal-form Core
/// IR (ANF decomposes the `acc = Type { ...acc, … }` shape the recognizer depended on into a
/// `let` + reassignment, scattering the temporaries it keyed on). Reuse is re-established —
/// principally, on the IR — by Phase 3's RC/last-use passes, which the README says "subsume
/// P-REUSE". The enum and [`compile_with_options`] are retained so the perf bench's API is
/// unchanged; every mode now produces the same copying lowering (so the bench measures the
/// transient regression Phase 3 recovers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReuseMode {
    /// No reuse — a self-update lowers to an ordinary copying `MakeRecord { spread }`. The baseline.
    Off,
    /// Was: runtime `refcount() == 1 && same-shape` check at the construct. Now identical to [`Off`].
    Runtime,
    /// Was: the runtime check elided where linearity was proved. Now identical to [`Off`].
    #[default]
    Static,
}

/// Compile under an explicit [reuse mode][ReuseMode]. Retained for the perf bench's API; the mode
/// is inert since Phase 2 (see [`ReuseMode`]), so this is behavior-identical to [`compile`].
pub fn compile_with_options(program: &Program, reuse: ReuseMode) -> Result<Module, Unsupported> {
    compile_inner(program, lang_check::resolve_type_of_sites(program), reuse)
}

/// Compile a program using a **precomputed** `type_of` site map instead of re-deriving it.
///
/// [`compile`] re-runs the checker (via `resolve_type_of_sites`) to obtain the map, which on a
/// path that already type-checked the program — the CLI, the `lang-db` `bytecode` query, the
/// differential harness — means a redundant checker run. An orchestrator that already holds a
/// [`lang_check::Checked`] threads its `type_of_sites` here so the checker runs only once. The
/// map is a pure function of the program, so this is behavior-identical to [`compile`].
pub fn compile_with_sites(
    program: &Program,
    type_of_sites: HashMap<Span, lang_ast::reflect::TypeRepr>,
) -> Result<Module, Unsupported> {
    compile_inner(program, type_of_sites, ReuseMode::default())
}

fn compile_inner(
    program: &Program,
    type_of_sites: HashMap<Span, lang_ast::reflect::TypeRepr>,
    _reuse: ReuseMode,
) -> Result<Module, Unsupported> {
    // Lower the surface program to the shared Core IR, then compile *that* to bytecode. The same
    // lowering the IR interpreter consumes, so both backends execute one program (Phase 2).
    let ir = lang_ir::lower(program).map_err(|u| Unsupported {
        reason: format!("not yet lowered to the Core IR: {}", u.feature),
    })?;
    let mut module = ModuleCompiler {
        protos: vec![Chunk::placeholder()],
        shapes: Vec::new(),
        methods: Vec::new(),
        destructors: Vec::new(),
        comparable_derives: Vec::new(),
        tojson_derives: Vec::new(),
        types: HashMap::new(),
        module_globals: HashMap::new(),
        type_of_sites,
        cache_slots: 0,
    };
    // Type registration reads the surface declarations (shapes, derives, the method/destructor
    // proto table) the IR carries verbatim; bodies are lowered from the IR.
    module.register_globals(program);
    module.register_types(program);
    module.compile_methods(&ir)?;
    let main = {
        let mut fc = FnCompiler::new(&mut module, true, None, Vec::new(), Vec::new());
        fc.init_temps(ir.temp_count);
        for stmt in &ir.top.stmts {
            fc.stmt(stmt)?;
        }
        fc.code.push(Op::Halt);
        fc.into_chunk(0, Vec::new())
    };
    module.protos[0] = main;
    Ok(Module {
        protos: module.protos,
        shapes: module.shapes,
        methods: module.methods,
        destructors: module.destructors,
        comparable_derives: module.comparable_derives,
        tojson_derives: module.tojson_derives,
        cache_slots: module.cache_slots,
        // The attribute manifest + type registry, built from the AST by the *same* pure builder the
        // tree-walker uses — so reflection is identical across backends by construction.
        reflection: lang_ast::reflect::build(program),
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
    types: HashMap<String, TypeInfo>,
    /// Every top-level value global's name and whether it is mutable. Computed before any body
    /// is compiled so a nested function can resolve a global (and check its mutability on
    /// assignment) and so the free-variable analysis can tell a global from a captured local.
    module_globals: HashMap<String, bool>,
    /// The concrete static type the checker resolved for each `type_of(value)` site (keyed by the
    /// `Expr::TypeOf` span), harvested from the *same* program the tree-walker harvests, so both
    /// backends bake identical full-fidelity `Type` constants (`type_of` fidelity A, P2.3). A site
    /// absent here lowers to the runtime head-constructor op instead.
    type_of_sites: HashMap<Span, lang_ast::reflect::TypeRepr>,
    /// Running count of inline-cache slots assigned so far. Each `LoadField`/`CallMethod` emission
    /// takes the next id (module-global across all chunks); the total becomes [`Module::cache_slots`],
    /// sizing the VM's per-run cache array. See [`ModuleCompiler::next_cache_slot`].
    cache_slots: u32,
}

impl ModuleCompiler {
    /// Reserve and return the next inline-cache slot id (module-global across all chunks). Called
    /// once per `LoadField`/`CallMethod` emission; the final count sizes the VM's per-run cache array.
    fn next_cache_slot(&mut self) -> u32 {
        let slot = self.cache_slots;
        self.cache_slots += 1;
        slot
    }

    /// Pre-pass: collect every top-level value global (a binding or `fn`/native-module name) and
    /// its mutability, so functions can resolve and assign globals and the capture analysis can
    /// distinguish a global from an enclosing local.
    fn register_globals(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                lang_ast::Stmt::Binding { mut_decl, name, .. } => {
                    self.module_globals.insert(name.clone(), *mut_decl);
                }
                lang_ast::Stmt::Fn(decl) => {
                    self.module_globals.insert(decl.name.clone(), false);
                }
                lang_ast::Stmt::Use { path, names, .. } => {
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
                lang_ast::Stmt::Record(decl) => {
                    let fields = decl.fields.iter().map(|f| f.name.clone()).collect();
                    if lang_ast::derives_trait(&decl.derives, "Comparable") {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    // `@derive(Serialize<Json>)` synthesizes the structural JSON serializer (`Json`
                    // is the only format today, so it maps to the existing `to_json` codegen).
                    if lang_ast::derives_trait(&decl.derives, "Serialize") {
                        self.tojson_derives.push(decl.name.clone());
                    }
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Record { fields });
                }
                lang_ast::Stmt::Class(decl) => {
                    let fields: Vec<String> = decl.fields.iter().map(|f| f.name.clone()).collect();
                    // A hand-written `compare` (via `impl Comparable`) takes precedence over the
                    // derived structural ordering.
                    if lang_ast::derives_trait(&decl.derives, "Comparable")
                        && !decl.methods.iter().any(|m| m.name == "compare")
                    {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    // A hand-written `to_json` takes precedence over the derived serializer.
                    if lang_ast::derives_trait(&decl.derives, "Serialize")
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
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Class { fields, fns });
                }
                lang_ast::Stmt::Enum(decl) => {
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
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Enum { variants });
                }
                lang_ast::Stmt::Use { path, names, .. } => {
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

    /// Pass 2: compile each class method (and the `destruct` block) into its reserved prototype,
    /// from the lowered IR. Methods see all registered types (forward references work) and run
    /// with the receiver in register 0, the declared parameters in registers `1..`, and field
    /// names resolving to the receiver.
    fn compile_methods(&mut self, ir: &lang_ir::Program) -> Result<(), Unsupported> {
        for stmt in &ir.top.stmts {
            let Stmt::Decl(Decl::Class(class)) = stmt else {
                continue;
            };
            let name = class.decl.name.clone();
            let field_set: HashSet<String> =
                class.decl.fields.iter().map(|f| f.name.clone()).collect();
            for (method, func) in &class.methods {
                let TypeInfo::Class { fns, .. } = &self.types[&name] else {
                    unreachable!("a class registered as non-class");
                };
                let proto = fns[method];
                let chunk = self.compile_func(
                    func,
                    Some(MethodCtx {
                        fields: field_set.clone(),
                    }),
                    Vec::new(),
                    Vec::new(),
                )?;
                self.protos[proto as usize] = chunk;
            }
            // The `destruct` block compiles like a parameterless method (fields in scope).
            if let Some(func) = &class.destructor {
                let proto = self
                    .destructors
                    .iter()
                    .find(|(n, _)| n == &name)
                    .map(|(_, proto)| *proto)
                    .expect("a destructor proto was reserved in pass 1");
                let chunk = self.compile_func(
                    func,
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

    /// Compile one IR [`Func`] (function/closure/method/`destruct` body) into a [`Chunk`].
    fn compile_func(
        &mut self,
        func: &Func,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<Chunk, Unsupported> {
        self.compile_chunk(
            &func.params,
            &func.defaults,
            &func.body,
            func.temp_count,
            method,
            upvalues,
            enclosing_locals,
        )
    }

    /// Compile a function-like body into a [`Chunk`]: its parameters, defaulted-parameter thunks,
    /// and a [`Block`] body sized for `temp_count` frame temporaries. A `method` context reserves
    /// register 0 for the receiver (`self`) and resolves the class's field names against it.
    /// `upvalues` are the names (and mutability) this function captures from enclosing functions,
    /// in the order its parent will supply the cells; `enclosing_locals` are the enclosing
    /// functions' capturable local names (outermost first), so the function can lower its own
    /// nested closures. A body [`Block`] with a `tail` atom (a closure/arrow or a default thunk)
    /// returns that atom; a block body without one falls off the end as an implicit unit return.
    #[allow(clippy::too_many_arguments)]
    fn compile_chunk(
        &mut self,
        params: &[String],
        defaults: &[Option<Thunk>],
        body: &Block,
        temp_count: u32,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<Chunk, Unsupported> {
        let is_method = method.is_some();
        let globals = self.global_names();
        let analysis = freevars::analyze(params, defaults, body, &enclosing_locals, &globals);

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

        // Compile each defaulted parameter's default value into a zero-argument thunk prototype,
        // recording the `(parameter register, thunk proto)` pair. Parameter registers are fixed by
        // declaration order — a method reserves register 0 for the receiver, so its parameters start
        // at 1 — which lets the VM fill an omitted argument's register from its thunk. The thunk is
        // compiled with **this function's own upvalue layout**, so a default that references a
        // captured variable resolves to the same upvalue index the body would use; at call time the
        // VM hands the thunk the closure's upvalue cells. For a top-level function or method
        // `upvalues` is empty, so the thunk resolves globals only. Compiled before the body's
        // `FnCompiler` borrows `self`.
        let base: u16 = if is_method { 1 } else { 0 };
        let mut default_pairs: Vec<(u16, u32)> = Vec::new();
        for (j, default) in defaults.iter().enumerate() {
            if let Some(thunk) = default {
                let proto = self.add_thunk(thunk, upvalues.clone(), enclosing_locals.clone())?;
                default_pairs.push((base + j as u16, proto));
            }
        }

        let mut fc = FnCompiler::new(self, false, method, upvalues, enclosing_locals);
        fc.celled = analysis.celled;
        fc.local_layer = local_layer;
        fc.forbidden = forbidden;
        fc.init_temps(temp_count);

        // A method reserves register 0 for the receiver; ordinary functions do not.
        if is_method {
            fc.alloc_reg();
        }
        fc.scopes.push(HashMap::new());
        for param in params {
            let reg = fc.alloc_reg();
            let celled = fc.celled.contains(param);
            // A captured parameter is boxed into a cell so the closure shares the live binding.
            if celled {
                fc.code.push(Op::MakeCell { dst: reg, src: reg });
            }
            fc.scopes.last_mut().unwrap().insert(
                param.clone(),
                Var {
                    reg,
                    mutable: false,
                    celled,
                },
            );
        }
        for stmt in &body.stmts {
            fc.stmt(stmt)?;
        }
        // A value-position body (closure/arrow, default thunk) returns its tail atom; a block
        // body that falls off the end implicitly returns unit (M0's `exec_fn_body`).
        if let Some(tail) = &body.tail {
            let src = fc.atom_reg(tail)?;
            fc.code.push(Op::Return { src });
        }
        fc.code.push(Op::Halt);
        let num_params = params.len() as u16 + if is_method { 1 } else { 0 };
        Ok(fc.into_chunk(num_params, default_pairs))
    }

    /// Compile an IR [`Func`] into a fresh prototype and return its index.
    fn add_function(
        &mut self,
        func: &Func,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<u32, Unsupported> {
        let chunk = self.compile_func(func, None, upvalues, enclosing_locals)?;
        let idx = self.protos.len() as u32;
        self.protos.push(chunk);
        Ok(idx)
    }

    /// Compile a defaulted-parameter [`Thunk`] (a zero-parameter value-position body) into a fresh
    /// prototype and return its index.
    fn add_thunk(
        &mut self,
        thunk: &Thunk,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<u32, Unsupported> {
        let chunk = self.compile_chunk(
            &[],
            &[],
            &thunk.body,
            thunk.temp_count,
            None,
            upvalues,
            enclosing_locals,
        )?;
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

/// Per-prototype compilation state (one register file, one constant/diagnostic pool).
struct FnCompiler<'m> {
    module: &'m mut ModuleCompiler,
    code: Vec<Op>,
    consts: Vec<Const>,
    diags: Vec<Diagnostic>,
    /// The register holding each frame temporary's value, indexed by [`Temp`] index. ANF
    /// temporaries are write-once and defined before use, so each slot is filled when its
    /// defining `Stmt::Let` (or a `match`/`&&`/`??` destination) is lowered and read thereafter.
    /// Sized to the frame's `temp_count` by [`Self::init_temps`].
    temp_regs: Vec<Option<Reg>>,
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
    /// Enclosing-loop jump-patch sites, innermost last. Each `break`/`continue` records a pending
    /// `Jump` here; the loop patches them to its exit / continue target once those are known.
    loops: Vec<LoopCtx>,
}

/// Pending forward jumps from `break`/`continue` inside one loop, patched at the loop's end.
#[derive(Default)]
struct LoopCtx {
    /// Code positions of `break` jumps — patched to the instruction after the loop.
    breaks: Vec<usize>,
    /// Code positions of `continue` jumps — patched to the loop's continue target (the `while`
    /// condition re-test, or the `for` index increment).
    continues: Vec<usize>,
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
            temp_regs: Vec::new(),
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
            loops: Vec::new(),
        }
    }

    /// Size the frame-temporary register map to `temp_count` (all slots initially unfilled).
    /// Called once per body before lowering.
    fn init_temps(&mut self, temp_count: u32) {
        self.temp_regs = vec![None; temp_count as usize];
    }

    /// Allocate the register backing a temporary at its defining site (`let t = …`, or a
    /// `match`/`&&`/`??` destination) and record it. A temp is write-once, so each slot is filled
    /// exactly once.
    fn define_temp(&mut self, t: Temp) -> Reg {
        let reg = self.alloc_reg();
        self.temp_regs[t.index()] = Some(reg);
        reg
    }

    /// The register holding temporary `t`'s value. ANF guarantees the temp was defined before this
    /// read, so the slot is filled.
    fn temp_reg(&self, t: Temp) -> Reg {
        self.temp_regs[t.index()].expect("ANF temporary read before it was defined")
    }

    /// The enclosing-locals chain to pass to one of *this* function's nested closures: the chain
    /// we were given, extended with our own capturable layer.
    fn child_enclosing(&self) -> Vec<HashSet<String>> {
        let mut chain = self.enclosing_locals.clone();
        chain.push(self.local_layer.clone());
        chain
    }

    fn into_chunk(self, num_params: u16, defaults: Vec<(u16, u32)>) -> Chunk {
        Chunk {
            code: self.code,
            consts: self.consts,
            diagnostics: self.diags,
            num_params,
            num_registers: self.next_reg,
            defaults,
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

    /// Compute, for a nested closure/`fn` `func`, the ordered upvalue list (name + mutability) it
    /// captures and the matching `CaptureFrom` source for each (in this — the building —
    /// function's terms). Returns `Unsupported` if a capture reaches a method's `self`/field or
    /// cannot be sourced (e.g. a forward capture of a not-yet-declared local), so such a program
    /// is skipped rather than miscompiled.
    fn resolve_captures(&self, func: &Func) -> Result<CaptureLayout, Unsupported> {
        let globals = self.module.global_names();
        let enclosing = self.child_enclosing();
        let free = freevars::free_vars(
            &func.params,
            &func.defaults,
            &func.body,
            &enclosing,
            &globals,
        );
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
            // `let t = rvalue` — bind the operation's result to its frame temporary's register.
            Stmt::Let { dst, rvalue, .. } => {
                let reg = self.define_temp(*dst);
                self.rvalue(rvalue, reg)
            }
            // A bare expression statement: evaluate for effect into a scratch register, discard.
            Stmt::Eval { rvalue, .. } => {
                let reg = self.alloc_reg();
                self.rvalue(rvalue, reg)
            }
            // Releasing a discarded temporary promptly is a Phase-3 (precise-RC) concern; this
            // phase keeps today's VM reclamation — frame teardown releases it — so `Drop` is a
            // no-op here. (The IR *interpreter* honors it for its own destructor-timing fidelity.)
            // A source-variable `DropVar` (Phase-3 drop-insertion) is likewise a no-op until the
            // backend-lowering slice emits an `Op::Drop` on the binding's register.
            Stmt::Drop(_) | Stmt::DropVar { .. } => Ok(()),
            Stmt::Bind {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => self.binding(*mut_decl, name, *name_span, value),
            Stmt::Echo { value, span } => {
                let t = self.atom_reg(value)?;
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
            Stmt::Return { value, span } => self.return_stmt(value.as_ref(), *span),
            Stmt::If {
                cond,
                then_block,
                else_block,
                span,
            } => self.if_stmt(cond, then_block, else_block.as_ref(), *span),
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => self.for_stmt(pattern, iterable, body, *span),
            Stmt::While { cond, body, span } => self.while_stmt(cond, body, *span),
            // `break`/`continue` emit a placeholder `Jump` recorded on the innermost loop, which
            // patches it once its exit / continue target is known. The checker guarantees a loop is
            // present, so `loops` is non-empty here.
            Stmt::Break { .. } => {
                let site = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                self.loops
                    .last_mut()
                    .expect("`break` inside a loop (checker-enforced)")
                    .breaks
                    .push(site);
                Ok(())
            }
            Stmt::Continue { .. } => {
                let site = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                self.loops
                    .last_mut()
                    .expect("`continue` inside a loop (checker-enforced)")
                    .continues
                    .push(site);
                Ok(())
            }
            Stmt::Match {
                scrutinee,
                arms,
                dst,
                span,
            } => self.match_stmt(scrutinee, arms, *dst, *span),
            Stmt::Logical {
                dst,
                op,
                left,
                right,
                span,
            } => self.logical(*dst, *op, left, right, *span),
            Stmt::Coalesce {
                dst,
                value,
                fallback,
                span,
            } => self.coalesce(*dst, value, fallback, *span),
            Stmt::Decl(decl) => self.decl(decl),
        }
    }

    /// Lower a declaration statement. A `fn` binds a closure; type declarations were registered in
    /// the pre-pass and emit no code; `use std.{json, ...}` binds each native module as a global.
    fn decl(&mut self, decl: &Decl) -> Result<(), Unsupported> {
        match decl {
            Decl::Fn { name, func, .. } => self.declare_fn(name, func),
            Decl::Class(_) | Decl::Enum(_) | Decl::Record(_) => Ok(()),
            Decl::Use { path, names, .. } => {
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
    fn declare_fn(&mut self, name: &str, func: &Func) -> Result<(), Unsupported> {
        if self.at_global_depth() {
            let proto = self.module.add_function(func, Vec::new(), Vec::new())?;
            let t = self.alloc_reg();
            self.code.push(Op::MakeClosure {
                dst: t,
                proto,
                captures: Box::new([]),
            });
            self.globals
                .insert(name.to_string(), GlobalInfo { mutable: false });
            self.code.push(Op::StoreGlobal {
                name: name.to_string(),
                src: t,
            });
            return Ok(());
        }

        let celled = self.celled.contains(name);
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
                name.to_string(),
                Var {
                    reg,
                    mutable: false,
                    celled: true,
                },
            );
        }
        let (upvalues, captures) = self.resolve_captures(func)?;
        let enclosing = self.child_enclosing();
        let proto = self.module.add_function(func, upvalues, enclosing)?;
        let t = self.alloc_reg();
        self.code.push(Op::MakeClosure {
            dst: t,
            proto,
            captures: captures.into_boxed_slice(),
        });
        if celled {
            self.code.push(Op::CellSet { cell: reg, src: t });
        } else {
            self.declare_local(name, t, false);
        }
        Ok(())
    }

    fn return_stmt(&mut self, value: Option<&Atom>, _span: Span) -> Result<(), Unsupported> {
        let src = match value {
            Some(atom) => self.atom_reg(atom)?,
            None => {
                let t = self.alloc_reg();
                let k = self.add_const(Const::Unit);
                self.code.push(Op::LoadConst { dst: t, k });
                t
            }
        };
        self.code.push(Op::Return { src });
        Ok(())
    }

    /// `if cond { then } else { else }`, lowered to a bool-check and forward jumps. Mirrors
    /// the tree-walker: a non-bool condition is E0007 at the `if`'s span, and each branch
    /// body runs in its own (block) scope. The condition is a pre-computed bool atom.
    fn if_stmt(
        &mut self,
        cond: &Atom,
        then_block: &Block,
        else_block: Option<&Block>,
        span: Span,
    ) -> Result<(), Unsupported> {
        let rc = self.atom_reg(cond)?;
        self.code.push(Op::RequireCondBool { reg: rc, span });
        let jf = self.code.len();
        self.code.push(Op::JumpIfFalse { reg: rc, target: 0 });

        self.block(then_block)?;

        match else_block {
            Some(else_block) => {
                let j_end = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                let else_start = self.code.len() as u32;
                self.patch_jump(jf, else_start);
                self.block(else_block)?;
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
        iterable: &Atom,
        body: &Block,
        span: Span,
    ) -> Result<(), Unsupported> {
        // The loop's bookkeeping registers live in the enclosing frame (a list snapshot, its
        // length, the running index, and a constant 1 to advance it). The iterable is a
        // pre-computed atom, snapshotted once into a fresh register.
        let src = self.atom_reg(iterable)?;
        let items = self.alloc_reg();
        self.code.push(Op::IterSnapshot {
            dst: items,
            src,
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
        self.loops.push(LoopCtx::default());
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
            for stmt in &body.stmts {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        let ctx = self.loops.pop().expect("loop context");
        self.scopes.pop();
        result?;

        // `continue` skips to the index increment (so the loop still advances), which begins here.
        let increment = self.code.len() as u32;
        for site in ctx.continues {
            self.patch_jump(site, increment);
        }
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
        // `break` lands past the back-edge.
        for site in ctx.breaks {
            self.patch_jump(site, end);
        }
        Ok(())
    }

    /// `while <cond> { body }`, lowered to a top-tested loop: evaluate the condition, require it
    /// be a bool, exit if false, run the body in a fresh scope, then jump back. Mirrors the
    /// tree-walker — a bare reassignment in the body updates its enclosing binding's register, so
    /// the condition makes progress. The condition is a re-evaluated **block** (its `let`s plus a
    /// tail bool atom); its straight-line code runs in the enclosing scope each iteration.
    fn while_stmt(&mut self, cond: &Block, body: &Block, span: Span) -> Result<(), Unsupported> {
        let loop_top = self.code.len() as u32;
        let rc = self.value_block(cond)?;
        self.code.push(Op::RequireCondBool { reg: rc, span });
        let exit_jump = self.code.len();
        self.code.push(Op::JumpIfFalse { reg: rc, target: 0 });

        self.loops.push(LoopCtx::default());
        let result = self.block(body);
        let ctx = self.loops.pop().expect("loop context");
        result?;

        // `continue` re-tests the condition, so it targets the loop top.
        for site in ctx.continues {
            self.patch_jump(site, loop_top);
        }
        self.code.push(Op::Jump { target: loop_top });
        let end = self.code.len() as u32;
        self.patch_jump(exit_jump, end);
        // `break` lands just past the back-edge.
        for site in ctx.breaks {
            self.patch_jump(site, end);
        }
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

    /// Compile a statement-position block (an `if`/`while` body) in its own (block) scope.
    fn block(&mut self, block: &Block) -> Result<(), Unsupported> {
        self.scopes.push(HashMap::new());
        let result = (|| {
            for stmt in &block.stmts {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    /// Compile a value-position block (a `while` condition, a `&&`/`||` right operand, a `??`
    /// fallback) **inline** in the current scope — its straight-line `let`s were sub-expressions,
    /// not a new lexical scope — and return the register holding its tail atom.
    fn value_block(&mut self, block: &Block) -> Result<Reg, Unsupported> {
        for stmt in &block.stmts {
            self.stmt(stmt)?;
        }
        let tail = block
            .tail
            .as_ref()
            .expect("a value-position block always has a tail atom");
        self.atom_reg(tail)
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
        value: &Atom,
    ) -> Result<(), Unsupported> {
        // The value is a pre-computed atom (its side effects already ran in the preceding `let`s);
        // `atom_reg` only materializes the register holding it. The binding rule then applies — so
        // a reassignment to an immutable still runs the value (no observable change), matching the
        // tree-walker's `bind`.
        let src = self.atom_reg(value)?;

        if mut_decl {
            if self.at_global_depth() {
                self.globals
                    .insert(name.to_string(), GlobalInfo { mutable: true });
                self.code.push(Op::StoreGlobal {
                    name: name.to_string(),
                    src,
                });
            } else {
                self.declare_local(name, src, true);
            }
            return Ok(());
        }

        // A bare `x = v`: reassign the nearest existing binding (searching local scopes, then
        // captured upvalues, then globals — mirroring the tree-walker's outward `Scope::assign`),
        // else declare a fresh local.
        if let Some(var) = self.lookup_local(name) {
            if !var.mutable {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            } else if var.celled {
                self.code.push(Op::CellSet { cell: var.reg, src });
            } else {
                self.code.push(Op::Move { dst: var.reg, src });
            }
            return Ok(());
        }

        // A captured upvalue: reassign through its cell, enforcing the source binding's mutability.
        if let Some(&index) = self.upvalue_index.get(name) {
            if self.upvalue_mut[index as usize] {
                self.code.push(Op::UpvalueSet { index, src });
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
            if mutable {
                self.code.push(Op::StoreGlobal {
                    name: name.to_string(),
                    src,
                });
            } else {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            return Ok(());
        }

        // Not found anywhere: a new immutable binding in the current scope.
        if self.scopes.is_empty() {
            self.globals
                .insert(name.to_string(), GlobalInfo { mutable: false });
            self.code.push(Op::StoreGlobal {
                name: name.to_string(),
                src,
            });
        } else {
            self.declare_local(name, src, false);
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

    /// Produce a register holding `atom`'s value. A temporary or a directly-held local / `self`
    /// resolves to its existing register (operands are read-only); a constant, or a celled /
    /// captured / global / field name, is materialized into a fresh register.
    fn atom_reg(&mut self, atom: &Atom) -> Result<Reg, Unsupported> {
        match atom {
            Atom::Const(c) => {
                let dst = self.alloc_reg();
                let k = self.add_const(const_value(c));
                self.code.push(Op::LoadConst { dst, k });
                Ok(dst)
            }
            Atom::Temp(t) => Ok(self.temp_reg(*t)),
            Atom::Var { name, span } => self.var_reg(name, *span),
        }
    }

    /// Materialize each atom in `atoms` into a register, in order.
    fn atom_regs(&mut self, atoms: &[Atom]) -> Result<Box<[Reg]>, Unsupported> {
        let mut regs = Vec::with_capacity(atoms.len());
        for a in atoms {
            regs.push(self.atom_reg(a)?);
        }
        Ok(regs.into_boxed_slice())
    }

    /// Resolve a source-variable reference to a register holding its value, mirroring the
    /// tree-walker's name resolution (`Expr::Ident` evaluation).
    fn var_reg(&mut self, name: &str, span: Span) -> Result<Reg, Unsupported> {
        match self.resolve(name) {
            // A directly-held local or the method receiver is read in place (operands are read-only;
            // any op that stores the value retains it, so a later reassignment cannot disturb it).
            Resolved::Local(reg) => Ok(reg),
            Resolved::SelfRecv => Ok(0),
            Resolved::CelledLocal(cell) => {
                let dst = self.alloc_reg();
                self.code.push(Op::CellGet { dst, cell });
                Ok(dst)
            }
            Resolved::Upvalue(index) => {
                let dst = self.alloc_reg();
                self.code.push(Op::UpvalueGet { dst, index });
                Ok(dst)
            }
            Resolved::Field => {
                let dst = self.alloc_reg();
                let cache = self.module.next_cache_slot();
                self.code.push(Op::LoadField {
                    dst,
                    obj: 0,
                    field: name.to_string(),
                    span,
                    cache,
                });
                Ok(dst)
            }
            Resolved::Global => {
                let dst = self.alloc_reg();
                self.code.push(Op::LoadGlobal {
                    dst,
                    name: name.to_string(),
                    span,
                });
                Ok(dst)
            }
            // `none` is the one prelude *value* (not a function): the `Option.none` variant.
            Resolved::Prelude if name == "none" => {
                let dst = self.alloc_reg();
                let shape = self.module.builtin_enum_shape("Option", "none");
                self.code.push(Op::MakeEnum {
                    dst,
                    shape,
                    args: Box::new([]),
                });
                Ok(dst)
            }
            // A bare reference to a collection builtin becomes a first-class native-function value (a
            // direct call still uses `CallBuiltin`). Other prelude names used as values (the
            // `Ok`/`Err`/`some` constructors, `panic`, `next_id`) are not yet first-class — skip.
            Resolved::Prelude => match Builtin::from_name(name) {
                Some(func) => {
                    let dst = self.alloc_reg();
                    self.code.push(Op::LoadNativeFn { dst, func });
                    Ok(dst)
                }
                None => unsupported("reference to a prelude value/builtin"),
            },
        }
    }

    /// The register a result is written to: a temporary's allocated register, or — in a discard
    /// position (`dst == None`) — a fresh scratch register.
    fn dst_reg(&mut self, dst: Option<Temp>) -> Reg {
        match dst {
            Some(t) => self.define_temp(t),
            None => self.alloc_reg(),
        }
    }

    /// Lower a primitive operation ([`Rvalue`]) into register `dst`. Operands are already atoms
    /// (ANF), so this is a near-1:1 mapping to bytecode — no recursive flattening.
    fn rvalue(&mut self, rvalue: &Rvalue, dst: Reg) -> Result<(), Unsupported> {
        match rvalue {
            Rvalue::Use(atom) => {
                // Copy the atom into `dst` (a retaining `Move`), so the temp is an independent
                // snapshot even if its source is later reassigned.
                let src = self.atom_reg(atom)?;
                self.code.push(Op::Move { dst, src });
                Ok(())
            }
            Rvalue::Unary { op, operand, span } => {
                let src = self.atom_reg(operand)?;
                self.code.push(Op::Unary {
                    op: *op,
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            // `&&`/`||` never reach here (they lower to `Stmt::Logical`); every other infix does.
            Rvalue::Binary { op, lhs, rhs, span } => {
                let a = self.atom_reg(lhs)?;
                let b = self.atom_reg(rhs)?;
                self.code.push(Op::Binary {
                    op: *op,
                    dst,
                    a,
                    b,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::Call { callee, args, span } => self.lower_call(callee, args, dst, *span),
            Rvalue::Method {
                receiver,
                name,
                args,
                span,
                ..
            } => self.lower_method(receiver, name, args, dst, *span),
            Rvalue::Field {
                receiver,
                name,
                span,
                ..
            } => self.lower_field(receiver, name, dst, *span),
            Rvalue::Index {
                receiver,
                index,
                span,
            } => {
                let recv = self.atom_reg(receiver)?;
                let idx = self.atom_reg(index)?;
                self.code.push(Op::Index {
                    dst,
                    recv,
                    index: idx,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::List { items, .. } => {
                let items = self.atom_regs(items)?;
                self.code.push(Op::MakeList { dst, items });
                Ok(())
            }
            Rvalue::Map { entries, span } => {
                // Evaluate each key, check it is a string (matching M0's per-entry error timing),
                // then the value — then assemble the map.
                let mut pairs = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let key_reg = self.atom_reg(key)?;
                    self.code.push(Op::RequireMapKey {
                        reg: key_reg,
                        span: *span,
                    });
                    let value_reg = self.atom_reg(value)?;
                    pairs.push((key_reg, value_reg));
                }
                self.code.push(Op::MakeMap {
                    dst,
                    entries: pairs.into_boxed_slice(),
                });
                Ok(())
            }
            Rvalue::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let start_reg = self.atom_reg(start)?;
                let end_reg = self.atom_reg(end)?;
                self.code.push(Op::MakeRange {
                    dst,
                    start: start_reg,
                    end: end_reg,
                    inclusive: *inclusive,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::Object {
                type_name,
                type_name_span,
                fields,
                spread,
                span,
            } => self.lower_object(type_name, *type_name_span, fields, spread, dst, *span),
            Rvalue::Interp { parts, span } => self.lower_interp(parts, dst, *span),
            Rvalue::Closure { func, .. } => {
                // Resolve the closure's captures in this (the building) frame's terms, compile its
                // body with the matching upvalue layout, and emit `MakeClosure` so the VM threads
                // the captured cells into the new closure.
                let (upvalues, captures) = self.resolve_captures(func)?;
                let enclosing = self.child_enclosing();
                let proto = self.module.add_function(func, upvalues, enclosing)?;
                self.code.push(Op::MakeClosure {
                    dst,
                    proto,
                    captures: captures.into_boxed_slice(),
                });
                Ok(())
            }
            Rvalue::Try { operand, span } => {
                let src = self.atom_reg(operand)?;
                self.code.push(Op::TryUnwrap {
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::As { operand, ty, .. } => {
                let src = self.atom_reg(operand)?;
                let target = narrow_target(ty);
                let some_shape = self.module.builtin_enum_shape("Option", "some");
                let none_shape = self.module.builtin_enum_shape("Option", "none");
                self.code.push(Op::Narrow {
                    dst,
                    src,
                    target,
                    some_shape,
                    none_shape,
                });
                Ok(())
            }
            Rvalue::TypeTest { operand, ty, .. } => {
                let src = self.atom_reg(operand)?;
                let target = narrow_target(ty);
                self.code.push(Op::IsType { dst, src, target });
                Ok(())
            }
            Rvalue::TypeOf { operand, span } => {
                // Evaluate the operand for its side effects in both fidelities. When the checker
                // resolved a concrete static type for this site, bake the precise `Type` constant
                // (fidelity A); otherwise classify the runtime value's head constructor (fidelity B).
                let src = self.atom_reg(operand)?;
                match self.module.type_of_sites.get(span) {
                    Some(repr) => self.code.push(Op::TypeOfStatic {
                        dst,
                        repr: repr.clone(),
                    }),
                    None => self.code.push(Op::TypeOf { dst, src }),
                }
                Ok(())
            }
            Rvalue::AttributesOf { ty, .. } => {
                // The attribute type is resolved at compile time (closed-world); the VM reads the
                // matching manifest entries from `Module::reflection` and materializes them.
                let type_name = match ty {
                    TypeRef::Named { name, .. } => name.clone(),
                    _ => String::new(),
                };
                self.code.push(Op::AttributesOf { dst, type_name });
                Ok(())
            }
            Rvalue::RolesOf { .. } => {
                self.code.push(Op::RolesOf { dst });
                Ok(())
            }
            Rvalue::Invoke {
                recv,
                name,
                args,
                span,
            } => self.lower_invoke(recv, name, args, dst, *span),
        }
    }

    /// Lower a string interpolation: start from an empty string and fold each part's display form
    /// in with `~` concatenation, mirroring the tree-walker (literal text verbatim, a hole via its
    /// `display`). The interpolation's own span is used for each `Stringify`, matching M0.
    fn lower_interp(
        &mut self,
        parts: &[InterpPart],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let empty = self.add_const(Const::Str(String::new()));
        self.code.push(Op::LoadConst { dst, k: empty });
        for part in parts {
            let r = self.alloc_reg();
            match part {
                InterpPart::Literal(text) => {
                    let k = self.add_const(Const::Str(text.clone()));
                    self.code.push(Op::LoadConst { dst: r, k });
                }
                InterpPart::Hole { atom, .. } => {
                    let src = self.atom_reg(atom)?;
                    // Route a `Display` object through its `to_string` before the concatenation
                    // stringifies it; identity for every other value.
                    self.code.push(Op::Stringify { dst: r, src, span });
                }
            }
            self.code.push(Op::Binary {
                op: BinaryOp::Concat,
                dst,
                a: dst,
                b: r,
                span,
            });
        }
        Ok(())
    }

    /// Lower an ordinary call `callee(args)` (a method call is [`Rvalue::Method`], lowered
    /// separately). A prelude function called directly by name routes to its dedicated op.
    fn lower_call(
        &mut self,
        callee: &Atom,
        args: &[Atom],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        // A prelude function called directly by name. A user binding of the same name shadows the
        // prelude (then `resolve` is not `Prelude`, so this falls to the ordinary call below).
        if let Atom::Var { name, .. } = callee
            && matches!(self.resolve(name), Resolved::Prelude)
        {
            if let Some(builtin) = Builtin::from_name(name) {
                // `len`/`map`/`filter`/`sum` — the collection builtins.
                let args = self.atom_regs(args)?;
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
                "Ok" => self.make_result_option("Result", "Ok", args, dst),
                "Err" => self.make_result_option("Result", "Err", args, dst),
                "some" => self.make_result_option("Option", "some", args, dst),
                "panic" => self.make_panic(args, span),
                "next_id" if args.is_empty() => {
                    self.code.push(Op::NextId { dst });
                    Ok(())
                }
                _ => unsupported("prelude function not in the VM subset"),
            };
        }
        let callee_reg = self.atom_reg(callee)?;
        let args = self.atom_regs(args)?;
        self.code.push(Op::Call {
            dst,
            callee: callee_reg,
            args,
            span,
        });
        Ok(())
    }

    /// Lower a method/associated call `receiver.name(args)`. A bare type-name receiver resolves at
    /// compile time to an enum-variant construction or an associated-function call; any other
    /// receiver is a runtime-dispatched instance method.
    fn lower_method(
        &mut self,
        receiver: &Atom,
        name: &str,
        args: &[Atom],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        // `Type.something(args)` where `Type` is a known type name. Keyed purely on the type
        // registry (a same-named local does not shadow a type member), matching the tree-walker.
        if let Atom::Var {
            name: type_name, ..
        } = receiver
        {
            if let Some(TypeInfo::Enum { .. }) = self.module.types.get(type_name) {
                return self.make_enum(type_name, name, args, dst);
            }
            if let Some(TypeInfo::Class { fns, .. }) = self.module.types.get(type_name)
                && let Some(&proto) = fns.get(name)
            {
                return self.call_associated(proto, args, dst, span);
            }
        }
        // Otherwise the receiver is a value: a runtime-dispatched method call (a user instance
        // method, or a `count`/`enumerate` built-in — the VM decides).
        let recv = self.atom_reg(receiver)?;
        let args = self.atom_regs(args)?;
        let cache = self.module.next_cache_slot();
        self.code.push(Op::CallMethod {
            dst,
            recv,
            method: name.to_string(),
            args,
            span,
            cache,
        });
        Ok(())
    }

    /// Lower bare member access `receiver.name`: a no-data enum variant (`Status.Pending`) or a
    /// field load.
    fn lower_field(
        &mut self,
        receiver: &Atom,
        name: &str,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        if let Atom::Var {
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
        let obj = self.atom_reg(receiver)?;
        let cache = self.module.next_cache_slot();
        self.code.push(Op::LoadField {
            dst,
            obj,
            field: name.to_string(),
            span,
            cache,
        });
        Ok(())
    }

    /// `Type { field: value, ...spread }` — construct a record/class/opaque instance, or raise the
    /// tree-walker's runtime error for an unknown type.
    fn lower_object(
        &mut self,
        type_name: &str,
        type_name_span: Span,
        fields: &[lang_ir::ObjectFieldInit],
        spread: &Option<(Atom, Span)>,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        match self.module.types.get(type_name) {
            Some(TypeInfo::Record { fields: decl }) => {
                let decl = decl.clone();
                self.make_record(
                    type_name,
                    ShapeKind::Record,
                    &decl,
                    fields,
                    spread,
                    dst,
                    span,
                )
            }
            Some(TypeInfo::Class { fields: decl, .. }) => {
                let decl = decl.clone();
                self.make_record(
                    type_name,
                    ShapeKind::Class,
                    &decl,
                    fields,
                    spread,
                    dst,
                    span,
                )
            }
            Some(TypeInfo::Opaque) => self.make_opaque(type_name, fields, spread, dst),
            Some(TypeInfo::Enum { .. }) => unsupported("enum type used as a record literal"),
            None => {
                // The tree-walker looks the type up first and errors before touching fields.
                let idx = self.add_diag(unknown_type_diag(type_name, type_name_span));
                self.code.push(Op::Raise { idx });
                Ok(())
            }
        }
    }

    /// Construct a declared record/class instance. The full-initialization guarantee (E0009) is
    /// enforced at runtime by `MakeRecord`; an unknown field is a compile-time-detected runtime
    /// raise (its value atom was already computed by the preceding `let`s).
    #[allow(clippy::too_many_arguments)]
    fn make_record(
        &mut self,
        type_name: &str,
        kind: ShapeKind,
        decl_fields: &[String],
        inits: &[lang_ir::ObjectFieldInit],
        spread: &Option<(Atom, Span)>,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let shape = self.module.intern_shape(Shape::object(
            kind,
            type_name.to_string(),
            decl_fields.to_vec(),
        ));
        let spread_reg = match spread {
            Some((a, _)) => Some(self.atom_reg(a)?),
            None => None,
        };
        let mut named: Vec<(u16, Reg)> = Vec::with_capacity(inits.len());
        for init in inits {
            let Some(slot) = decl_fields.iter().position(|f| f == &init.name) else {
                let idx = self.add_diag(unknown_field_diag(type_name, &init.name, init.name_span));
                self.code.push(Op::Raise { idx });
                return Ok(());
            };
            let r = self.atom_reg(&init.value)?;
            named.push((slot as u16, r));
        }
        self.code.push(Op::MakeRecord {
            dst,
            shape,
            named: named.into_boxed_slice(),
            spread: spread_reg,
            span,
        });
        Ok(())
    }

    /// Construct an opaque (`use`-imported) instance: any fields are accepted, the runtime builds a
    /// sorted-key shape, and there are no field checks.
    fn make_opaque(
        &mut self,
        type_name: &str,
        inits: &[lang_ir::ObjectFieldInit],
        spread: &Option<(Atom, Span)>,
        dst: Reg,
    ) -> Result<(), Unsupported> {
        let spread_reg = match spread {
            Some((a, _)) => Some(self.atom_reg(a)?),
            None => None,
        };
        let mut keys: Vec<(String, Reg)> = Vec::with_capacity(inits.len());
        for init in inits {
            let r = self.atom_reg(&init.value)?;
            keys.push((init.name.clone(), r));
        }
        self.code.push(Op::MakeOpaque {
            dst,
            type_name: type_name.to_string(),
            keys: keys.into_boxed_slice(),
            spread: spread_reg,
        });
        Ok(())
    }

    /// Construct an enum variant carrying data (`OrderError.NegativePrice(i)`).
    fn make_enum(
        &mut self,
        type_name: &str,
        variant: &str,
        args: &[Atom],
        dst: Reg,
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
        let args = self.atom_regs(args)?;
        self.code.push(Op::MakeEnum { dst, shape, args });
        Ok(())
    }

    /// Call an associated function `Type.f(args)`. The method prototype reserves register 0 for
    /// `self`; an associated call has no receiver, so unit is passed there.
    fn call_associated(
        &mut self,
        proto: u32,
        args: &[Atom],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let self_reg = self.alloc_reg();
        let k = self.add_const(Const::Unit);
        self.code.push(Op::LoadConst { dst: self_reg, k });
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
        arg_regs.push(self_reg);
        for a in args {
            arg_regs.push(self.atom_reg(a)?);
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

    /// Construct a built-in `Result`/`Option` value (`Ok`/`Err`/`some`). `Ok` accepts 0 or 1
    /// arguments (the void success `Ok()` and the wrapping `Ok(x)`); `Err`/`some` take 1.
    fn make_result_option(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Atom],
        dst: Reg,
    ) -> Result<(), Unsupported> {
        let arg_regs = self.atom_regs(args)?;
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

    /// `panic(msg)` — evaluate the message and emit the abort op (E0010). The `dst` is unused (the
    /// op aborts the program), matching the tree-walker.
    fn make_panic(&mut self, args: &[Atom], span: Span) -> Result<(), Unsupported> {
        let arg_regs = self.atom_regs(args)?;
        if arg_regs.len() != 1 {
            return unsupported("`panic` with an unexpected argument count");
        }
        self.code.push(Op::Panic {
            msg: arg_regs[0],
            span,
        });
        Ok(())
    }

    /// `invoke(recv, name, args)` — fallible by-name dispatch. A bare type-name receiver becomes a
    /// first-class type handle; any other receiver compiles normally. Both flow through the
    /// runtime-dispatched `Op::Invoke`.
    fn lower_invoke(
        &mut self,
        recv: &Atom,
        name: &Atom,
        args: &Atom,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let recv_reg = if let Atom::Var {
            name: type_name, ..
        } = recv
            && self.module.types.contains_key(type_name)
        {
            let r = self.alloc_reg();
            self.code.push(Op::TypeValue {
                dst: r,
                name: type_name.clone(),
            });
            r
        } else {
            self.atom_reg(recv)?
        };
        let name_reg = self.atom_reg(name)?;
        let args_reg = self.atom_reg(args)?;
        let ok_shape = self.module.builtin_enum_shape("Result", "Ok");
        let err_shape = self.module.builtin_enum_shape("Result", "Err");
        self.code.push(Op::Invoke {
            dst,
            recv: recv_reg,
            name: name_reg,
            args: args_reg,
            ok_shape,
            err_shape,
            span,
        });
        Ok(())
    }

    /// `value ?? fallback` — unwrap the success payload, or evaluate `fallback` on the empty case
    /// (mirroring the tree-walker's `eval_coalesce`). `value` is a pre-computed atom; `fallback`
    /// is a lazily-evaluated value block.
    fn coalesce(
        &mut self,
        dst: Option<Temp>,
        value: &Atom,
        fallback: &Block,
        span: Span,
    ) -> Result<(), Unsupported> {
        let out = self.dst_reg(dst);
        let src = self.atom_reg(value)?;
        let coalesce_pos = self.code.len();
        self.code.push(Op::Coalesce {
            dst: out,
            src,
            fallback: 0,
            span,
        });
        // Success path: `out` is set, then jump past the fallback.
        let jump_end = self.code.len();
        self.code.push(Op::Jump { target: 0 });
        let fallback_start = self.code.len() as u32;
        self.patch_jump(coalesce_pos, fallback_start);
        let fb = self.value_block(fallback)?;
        self.code.push(Op::Move { dst: out, src: fb });
        let end = self.code.len() as u32;
        self.patch_jump(jump_end, end);
        Ok(())
    }

    /// `match scrutinee { pattern => body, ... }` — a linear decision chain: each arm tests its
    /// pattern (jumping to the next arm on mismatch), binds, evaluates its body, and jumps to the
    /// end. A value matching no arm hits `MatchFail` (E0007). In expression position (`dst`
    /// `Some`) each arm body is a value block whose tail is moved into the destination.
    fn match_stmt(
        &mut self,
        scrutinee: &Atom,
        arms: &[lang_ir::Arm],
        dst: Option<Temp>,
        span: Span,
    ) -> Result<(), Unsupported> {
        let s = self.atom_reg(scrutinee)?;
        let out = dst.map(|t| self.define_temp(t));
        let mut end_jumps: Vec<usize> = Vec::new();
        for arm in arms {
            let mut fail_jumps: Vec<usize> = Vec::new();
            self.scopes.push(HashMap::new());
            self.emit_pattern(&arm.pattern, s, &mut fail_jumps);
            let body = (|| {
                for stmt in &arm.body.stmts {
                    self.stmt(stmt)?;
                }
                if let (Some(out), Some(tail)) = (out, &arm.body.tail) {
                    let r = self.atom_reg(tail)?;
                    self.code.push(Op::Move { dst: out, src: r });
                }
                Ok(())
            })();
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

    /// Emit the test for one pattern against value register `reg`, recording into `fail_jumps` the
    /// positions of every conditional that must jump to the arm's failure target. Bindings alias
    /// the matched register into the current (arm) scope. Mirrors `match_pattern`.
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
            // `is T` — test the head constructor with the shared matcher into a temp bool, then
            // fall through on `true` / jump to the next arm on `false`. Binds nothing.
            Pattern::IsType { ty, .. } => {
                let test = self.alloc_reg();
                self.code.push(Op::IsType {
                    dst: test,
                    src: reg,
                    target: narrow_target(ty),
                });
                fail_jumps.push(self.code.len());
                self.code.push(Op::JumpIfFalse {
                    reg: test,
                    target: 0,
                });
            }
        }
    }

    /// Lower `a && b` / `a || b` to branches, matching the tree-walker's `eval_logical`: the left
    /// operand (a pre-computed atom) must be a bool; on short-circuit its value is the result;
    /// otherwise the right operand (a lazily-evaluated value block) must be a bool and is the
    /// result.
    fn logical(
        &mut self,
        dst: Option<Temp>,
        op: BinaryOp,
        left: &Atom,
        right: &Block,
        span: Span,
    ) -> Result<(), Unsupported> {
        let out = self.dst_reg(dst);
        let left_reg = self.atom_reg(left)?;
        self.code.push(Op::Move {
            dst: out,
            src: left_reg,
        });
        self.code.push(Op::RequireBool {
            reg: out,
            side: BoolSide::Left,
            op,
            span,
        });
        let jump_pos = self.code.len();
        // Short-circuit: `&&` stops when the left is false, `||` when it is true.
        self.code.push(match op {
            BinaryOp::And => Op::JumpIfFalse {
                reg: out,
                target: 0,
            },
            BinaryOp::Or => Op::JumpIfTrue {
                reg: out,
                target: 0,
            },
            _ => unreachable!("logical only handles && and ||"),
        });
        let right_reg = self.value_block(right)?;
        self.code.push(Op::Move {
            dst: out,
            src: right_reg,
        });
        self.code.push(Op::RequireBool {
            reg: out,
            side: BoolSide::Right,
            op,
            span,
        });
        let end = self.code.len() as u32;
        self.patch_jump(jump_pos, end);
        Ok(())
    }
}

/// Map a Core-IR constant to its bytecode-pool [`Const`].
fn const_value(c: &IrConst) -> Const {
    match c {
        IrConst::Unit => Const::Unit,
        IrConst::Bool(b) => Const::Bool(*b),
        IrConst::Int(i) => Const::Int(*i),
        IrConst::Float(f) => Const::Float(*f),
        IrConst::Str(s) => Const::Str(s.clone()),
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

/// Reduce a narrowing target type (`x.as<T>()`) to its runtime head constructor. Mirrors the
/// tree-walker's `runtime_matches` mapping exactly so both backends decide a narrowing the same
/// way. `Option`/`Result` and user records/classes/enums all become `Named` (matched by shape
/// name); generic arguments are dropped (erasure).
fn narrow_target(ty: &TypeRef) -> NarrowTarget {
    match ty {
        TypeRef::Union { members, .. } => {
            NarrowTarget::AnyOf(members.iter().map(narrow_target).collect())
        }
        TypeRef::Optional { .. } => NarrowTarget::Named("Option".to_string()),
        TypeRef::Named { name, .. } => match name.as_str() {
            "int" => NarrowTarget::Int,
            "float" => NarrowTarget::Float,
            "bool" => NarrowTarget::Bool,
            "string" => NarrowTarget::String,
            "void" | "unit" => NarrowTarget::Unit,
            "dyn" | "Any" => NarrowTarget::Dyn,
            "List" | "list" => NarrowTarget::List,
            "Map" | "map" => NarrowTarget::Map,
            "Set" | "set" => NarrowTarget::Set,
            "Enum" => NarrowTarget::AnyEnum,
            "Record" => NarrowTarget::AnyRecord,
            "Class" => NarrowTarget::AnyClass,
            other => NarrowTarget::Named(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::compile;
    use lang_ast::AttrValue;
    use lang_ast::reflect::AttributeRecord;
    use lang_lexer::lex;
    use lang_parser::parse;
    use lang_span::{Source, SourceId};

    /// Compile `src` and return its attribute manifest (the VM-side view of the shared artifact).
    fn manifest(src: &str) -> Vec<AttributeRecord> {
        let source = Source::new(SourceId::FIRST, "test.lang", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "test program must parse cleanly: {:?}",
            parsed.diagnostics
        );
        compile(&parsed.program)
            .expect("compiles")
            .reflection
            .manifest
    }

    #[test]
    fn bare_attribute_has_no_args() {
        let m = manifest("#[Entity]\ntype User = { id: int };\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].target, "User");
        assert_eq!(m[0].name, "Entity");
        assert!(m[0].args.is_empty());
    }

    #[test]
    fn positional_literal_args_reach_the_manifest() {
        // The richer-argument case: a string literal survives end-to-end into the manifest as a
        // typed value (not the identifier-only form of the earlier prototype).
        let m = manifest("#[Route(\"/users\")]\ntype Users = { id: int };\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].args.len(), 1);
        assert_eq!(m[0].args[0].name, None);
        assert_eq!(m[0].args[0].value, AttrValue::Str("/users".to_string()));
    }

    #[test]
    fn named_and_mixed_literal_args_reach_the_manifest() {
        let m = manifest("#[Cache(ttl: 60, eager: true)]\ntype Page = { id: int };\n");
        assert_eq!(m[0].args.len(), 2);
        assert_eq!(m[0].args[0].name, Some("ttl".to_string()));
        assert_eq!(m[0].args[0].value, AttrValue::Int(60));
        assert_eq!(m[0].args[1].name, Some("eager".to_string()));
        assert_eq!(m[0].args[1].value, AttrValue::Bool(true));
    }

    #[test]
    fn vm_reflection_is_the_shared_builder_output() {
        // P2.0's parity guarantee: the VM-side artifact (in the compiled `Module`) is *exactly*
        // what `lang_ast::reflect::build` produces from the AST — the same pure builder the
        // tree-walker calls. So both backends agree on reflection by construction, no drift.
        let src = "#[Entity]\ntype User = { id: int };\nenum Color { Red; Rgb(r: int); }\n";
        let source = Source::new(SourceId::FIRST, "t.lang", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let from_module = compile(&parsed.program).expect("compiles").reflection;
        let from_builder = lang_ast::reflect::build(&parsed.program);
        assert_eq!(from_module, from_builder);
        // Deterministic: the same AST always yields the same artifact.
        assert_eq!(from_builder, lang_ast::reflect::build(&parsed.program));
    }

    #[test]
    fn type_registry_records_declared_shapes() {
        // The shared artifact also carries every declared type's reflectable shape (name, kind,
        // member names) — the half `attributes_of` materialization and `type_of` will read.
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "type Point = { x: int, y: int };\nenum Color { Red; Rgb(r: int); }\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let reflection = compile(&parsed.program).expect("compiles").reflection;
        let point = reflection.type_named("Point").expect("Point registered");
        assert_eq!(point.kind, lang_ast::reflect::TypeKind::Record);
        assert_eq!(point.fields, vec!["x".to_string(), "y".to_string()]);
        let color = reflection.type_named("Color").expect("Color registered");
        assert_eq!(color.kind, lang_ast::reflect::TypeKind::Enum);
        assert_eq!(color.variants.len(), 2);
        assert_eq!(color.variants[1].name, "Rgb");
        assert_eq!(color.variants[1].fields, vec!["r".to_string()]);
    }
}
