//! The bytecode compiler: AST → [`Module`].
//!
//! M1.2 lowers the literal/binding/arithmetic core (M1.0) plus **functions**: `fn`
//! declarations, calls, arrow closures, the `|>` pipeline, `return`, and the `if`/`else`
//! statement (needed for recursion). Anything outside the supported subset returns
//! [`Unsupported`]; the differential harness skips those, so coverage climbs slice by slice
//! while every compiled program is asserted identical to the M0 tree-walker.
//!
//! ## The two-level scope model
//!
//! The tree-walker resolves names through a chain of reference-counted lexical scopes. The
//! VM splits that into two tiers:
//!
//! - **Globals.** Every top-level binding and `fn` name lives in a runtime global table,
//!   read/written by name (`LoadGlobal`/`StoreGlobal`). A function's free variables resolve
//!   here at call time — which is faithful, because the tree-walker's captured scope for a
//!   top-level function *is* the (shared, mutable) global scope, so reads see live values.
//! - **Frame-locals.** Parameters and locals live in registers, one register file per call
//!   frame. Block scopes (`if`/`else` bodies) nest within the same register file.
//!
//! This is why M1.2 needs no upvalue machinery: the only functions it compiles are defined
//! at the top level, so they capture nothing but globals. A function or closure defined
//! *inside* another function could capture a non-global local, so those are [`Unsupported`]
//! for now (the upvalue path arrives with a later slice). The compiler stays faithful to the
//! tree-walker's evaluation order and exact diagnostic text/spans, because the differential
//! oracle compares full `RunResult`s. Registers are allocated monotonically (one per value,
//! no reuse) — simple and obviously correct.

use std::collections::HashMap;

use lang_ast::{BinaryOp, Expr, FnDecl, ForPattern, Param, Program, Stmt};
use lang_builtins::PRELUDE_NAMES;
use lang_bytecode::{BoolSide, Builtin, Chunk, Const, Method, Module, Op, Reg};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
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

/// Compile a whole program to a [`Module`], or report the first unsupported construct.
pub fn compile(program: &Program) -> Result<Module, Unsupported> {
    let mut module = ModuleCompiler {
        protos: vec![Chunk::placeholder()],
    };
    let main = {
        let mut fc = FnCompiler::new(&mut module, true);
        for stmt in &program.stmts {
            fc.stmt(stmt)?;
        }
        fc.code.push(Op::Halt);
        fc.into_chunk(0)
    };
    module.protos[0] = main;
    Ok(Module {
        protos: module.protos,
    })
}

/// Accumulates the prototype table across the recursive compilation of nested functions.
struct ModuleCompiler {
    protos: Vec<Chunk>,
}

impl ModuleCompiler {
    /// Compile one `fn`/closure body into a fresh prototype and return its index.
    fn add_function(&mut self, params: &[Param], body: Body<'_>) -> Result<u32, Unsupported> {
        let mut fc = FnCompiler::new(self, false);
        // Parameters occupy registers `0..num_params` in the (single) param scope.
        fc.scopes.push(HashMap::new());
        for param in params {
            let reg = fc.alloc_reg();
            fc.scopes.last_mut().unwrap().insert(
                param.name.clone(),
                Var {
                    reg,
                    mutable: false,
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
        let num_params = params.len() as u16;
        let chunk = fc.into_chunk(num_params);
        let idx = self.protos.len() as u32;
        self.protos.push(chunk);
        Ok(idx)
    }
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
}

#[derive(Clone, Copy)]
struct Var {
    reg: Reg,
    mutable: bool,
}

#[derive(Clone, Copy)]
struct GlobalInfo {
    mutable: bool,
}

/// How a name resolves at a use site.
enum Resolved {
    /// A frame-local register.
    Local(Reg),
    /// A global, read via `LoadGlobal` (the name may or may not exist at runtime).
    Global,
    /// A prelude value/builtin — not yet modeled by the VM, so the program is skipped.
    Prelude,
}

impl<'m> FnCompiler<'m> {
    fn new(module: &'m mut ModuleCompiler, is_main: bool) -> FnCompiler<'m> {
        FnCompiler {
            module,
            code: Vec::new(),
            consts: Vec::new(),
            diags: Vec::new(),
            scopes: Vec::new(),
            globals: HashMap::new(),
            next_reg: 0,
            is_main,
        }
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
            return Resolved::Local(var.reg);
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

    /// The collection builtin a callee name refers to, but only when the name still resolves
    /// to the prelude (a user binding of the same name shadows it) and the builtin is one
    /// this slice implements (`len`/`map`/`filter`/`sum`). Other prelude names stay
    /// unsupported, so a program using them is skipped rather than miscompiled.
    fn resolve_builtin(&self, name: &str) -> Option<Builtin> {
        match self.resolve(name) {
            Resolved::Prelude => Builtin::from_name(name),
            _ => None,
        }
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Unsupported> {
        match stmt {
            Stmt::Echo { value, .. } => {
                let t = self.alloc_reg();
                self.expr(value, t)?;
                self.code.push(Op::Echo { reg: t });
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
            Stmt::Expr { expr, .. } => {
                // Evaluated for its side effects (and any error); the value is discarded.
                let t = self.alloc_reg();
                self.expr(expr, t)
            }
            _ => unsupported("statement outside the VM subset"),
        }
    }

    /// `fn name(params) { body }` — compile the body to a prototype, then bind `name` to a
    /// closure over it. Only top-level functions are supported (they capture only globals);
    /// a `fn` nested inside another function could capture a local, so it is unsupported.
    fn declare_fn(&mut self, decl: &FnDecl) -> Result<(), Unsupported> {
        if !self.at_global_depth() {
            return unsupported("nested function declaration (may capture a non-global local)");
        }
        let proto = self
            .module
            .add_function(&decl.params, Body::Block(&decl.body))?;
        let t = self.alloc_reg();
        self.code.push(Op::MakeClosure { dst: t, proto });
        self.globals
            .insert(decl.name.clone(), GlobalInfo { mutable: false });
        self.code.push(Op::StoreGlobal {
            name: decl.name.clone(),
            src: t,
        });
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

    /// Register an already-populated register as an immutable loop-body binding. Unlike
    /// [`FnCompiler::declare_local`] this emits no `Move`: the element/destructure op has
    /// already written the value into `reg`.
    fn bind_loop_var(&mut self, name: &str, reg: Reg) {
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Var {
                reg,
                mutable: false,
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
            | Op::JumpIfTrue { target: t, .. } => *t = target,
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

        // A bare `x = v`: reassign the nearest existing binding, else declare anew.
        if let Some(var) = self.lookup_local(name) {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            if var.mutable {
                self.code.push(Op::Move {
                    dst: var.reg,
                    src: t,
                });
            } else {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            return Ok(());
        }

        if !self.is_main {
            // Inside a function, a bare assign to a non-local could (in the tree-walker)
            // reach outward and reassign a global — runtime-dependent. No corpus function
            // does this; skip rather than risk diverging.
            return unsupported("assignment to a non-local binding inside a function");
        }

        if let Some(info) = self.globals.get(name).copied() {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            if info.mutable {
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
    /// name already exists in the innermost scope (a re-`mut` shadow).
    fn declare_local(&mut self, name: &str, src: Reg, mutable: bool) {
        let reg = match self.scopes.last().unwrap().get(name) {
            Some(v) => v.reg,
            None => self.alloc_reg(),
        };
        self.code.push(Op::Move { dst: reg, src });
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), Var { reg, mutable });
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
                Resolved::Global => self.code.push(Op::LoadGlobal {
                    dst,
                    name: name.clone(),
                    span: *span,
                }),
                Resolved::Prelude => return unsupported("reference to a prelude value/builtin"),
            },
            Expr::Closure { params, body, .. } => {
                if !self.at_global_depth() {
                    return unsupported("closure outside the top level (may capture a local)");
                }
                let proto = self.module.add_function(params, Body::Arrow(body))?;
                self.code.push(Op::MakeClosure { dst, proto });
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
            _ => return unsupported("expression outside the VM subset"),
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
            // A zero-argument builtin method (`xs.count()`, `xs.enumerate()`). Anything else
            // (user methods, arguments, pipeline-into-method) awaits the object model.
            if prepend.is_none()
                && args.is_empty()
                && let Some(method) = Method::from_name(name)
            {
                let recv = self.alloc_reg();
                self.expr(receiver, recv)?;
                self.code.push(Op::CallMethod {
                    dst,
                    recv,
                    method,
                    span,
                });
                return Ok(());
            }
            return unsupported("method call");
        }
        // A prelude collection builtin called directly by name (`len(x)`, `xs |> map(f)`).
        // A user binding of the same name shadows the builtin (resolved as an ordinary call).
        if let Expr::Ident { name, .. } = callee
            && let Some(builtin) = self.resolve_builtin(name)
        {
            let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
            arg_regs.extend(prepend);
            for arg in args {
                let r = self.alloc_reg();
                self.expr(arg, r)?;
                arg_regs.push(r);
            }
            self.code.push(Op::CallBuiltin {
                dst,
                builtin,
                args: arg_regs.into_boxed_slice(),
                span,
            });
            return Ok(());
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
            Expr::Member { .. } => unsupported("pipeline into a method call"),
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
