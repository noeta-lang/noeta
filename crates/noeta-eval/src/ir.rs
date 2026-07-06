//! The **Core-IR tree-interpreter** — the reference evaluation backend. It walks the lowered
//! [`noeta_ir`] (not the surface AST: the original AST walker was retired once it was neither a
//! production path nor the differential oracle — the oracle is this interpreter vs. the bytecode
//! VM), and is the path `lang run` and the conformance reference both execute.
//!
//! # The shared machinery
//!
//! Everything below the orchestration layer lives on the [`Interpreter`](super::Interpreter) struct
//! and is shared with the bytecode VM's semantics by construction — operator semantics
//! (`apply_binary_op`, `eval_unary`), indexing (`eval_index`), display (`display_value`), method
//! dispatch, object/enum construction, the leak-counted [`Value`] model, the lexical [`Scope`], and
//! end-of-program destruction (`destroy_globals`, which fires lowered IR `destruct` blocks). This
//! interpreter contributes only the *orchestration*: it reads pre-computed **atoms** and walks the
//! `let`-sequenced [`noeta_ir::Stmt`]s.
//!
//! # Two storage classes
//!
//! Source variables live in [`Scope`] exactly as before (so captures, reassignment, and
//! `destroy_globals` are unchanged). ANF temporaries live in a [`Frame`] — a flat per-
//! activation store that drops at activation end. Because destructors fire **only** at
//! global teardown (never on a local or temporary drop), a temporary's lifetime is invisible
//! to observable behavior, so the `Frame` model needs no last-use analysis to stay faithful
//! in this phase.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use noeta_ast::Program;
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

use crate::{
    ChannelId, Closure, EnumDef, Eval, FieldSpec, Flow, Interpreter, IterState, ListRepr,
    PackedList, RunResult, ScopeId, TaskId, TreeWalkBackend, TypeDef, Unwind, Value, VariantInfo,
    compare_primitive,
};

/// The flat temporary store for one function activation (or the top level). Indexed by
/// [`noeta_ir::Temp`]; a slot is `None` until its defining `let` runs.
/// Whether an atom is an ANF temporary (vs a named source variable or a constant). A temp receiver
/// is *owned* — single-use by the ANF invariant, holding no live binding — so after an access
/// consumes it its destructor must fire at last use (Phase 4.4). A `Var` receiver is borrowed (its
/// binding fires at its own drop), so it is left alone.
fn is_temp(atom: &noeta_ir::Atom) -> bool {
    matches!(atom, noeta_ir::Atom::Temp(_))
}

/// Stamp a generic enum-variant construction's reflected type onto the freshly-built value (runtime
/// type-argument reflection, R2b.2), so `type_of` recovers the enum's type arguments after a `dyn`
/// launder. `reflect` is `Some` only for a generic enum construction; a non-enum result or an
/// ordinary call is returned unchanged. The value was just built and is uniquely owned, so its parts
/// move into a re-tagged `EnumValue` with no clone; an unexpectedly-shared value is left untagged.
fn tag_enum_reflect(value: Value, reflect: &Option<noeta_ast::reflect::TypeRepr>) -> Value {
    match (reflect, value) {
        (Some(repr), Value::Enum(rc)) => match Rc::try_unwrap(rc) {
            Ok(ev) => Value::Enum(Rc::new(crate::EnumValue {
                reflect: Some(Rc::new(repr.clone())),
                ..ev
            })),
            Err(rc) => Value::Enum(rc),
        },
        (_, value) => value,
    }
}

struct Frame {
    temps: Vec<Option<Value>>,
}

impl Frame {
    fn new(count: u32) -> Frame {
        Frame {
            temps: vec![None; count as usize],
        }
    }

    /// Read a temporary, **moving** its value out of the slot. ANF temporaries are single-use,
    /// so a temp is consumed exactly once; taking the value (rather than cloning) means its
    /// reference lives no longer than the matching intermediate does in the tree-walker — which
    /// keeps reference-count-gated destruction firing at the same points. A second read, or a
    /// read before the defining `let`, is a lowering bug.
    fn take(&mut self, t: noeta_ir::Temp) -> Value {
        self.temps[t.index()]
            .take()
            .expect("Core-IR temporary read before write or read twice (lowering bug)")
    }

    fn set(&mut self, t: noeta_ir::Temp, value: Value) {
        self.temps[t.index()] = Some(value);
    }
}

impl TreeWalkBackend {
    /// Run a program through the Core-IR interpreter. `ast` supplies the reflection manifest
    /// (built identically to the AST walker, so attributes/roles/type facts match);
    /// `ir` is the lowered program to execute; `type_of_sites` is the checker's `type_of` map.
    /// (`List<packed>` layout is carried inline on the IR by `lower_with_packed`, so it needs no
    /// separate channel here.)
    pub fn run_ir(
        &self,
        ast: &Program,
        ir: &noeta_ir::Program,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::new(self.seed).run_ir(ast, ir, type_of_sites)
    }

    /// As [`TreeWalkBackend::run_ir`], but against a caller-provided [`noeta_stdlib::Host`]
    /// (the real host) instead of the deterministic sandbox. `lang run` uses this so its
    /// user-facing execution goes through the same Core-IR reference (with last-use destruction)
    /// the conformance oracle pins, rather than the superseded AST-walk path (Phase 7 retired the
    /// AST-walk host entry points; this is the sole host-mode runner).
    pub fn run_ir_with_host(
        &self,
        ast: &Program,
        ir: &noeta_ir::Program,
        host: Box<dyn noeta_stdlib::Host>,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::with_host(self.seed, host).run_ir(ast, ir, type_of_sites)
    }

    /// As [`TreeWalkBackend::run_ir_with_host`], but also swapping the async executor (Track A.4).
    /// The CLI pairs a real host with a real wall-clock executor so `sleep`/`concurrent` run against
    /// real time; conformance never calls this (it keeps the default [`noeta_stdlib::SandboxExecutor`]),
    /// so this path is out-of-oracle.
    pub fn run_ir_with_host_and_executor(
        &self,
        ast: &Program,
        ir: &noeta_ir::Program,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::with_host_and_executor(self.seed, host, executor).run_ir(
            ast,
            ir,
            type_of_sites,
        )
    }
}

impl Interpreter {
    /// Execute a lowered program — the whole-program entry point: build the reflection manifest,
    /// run the top-level statements in the global scope, then destroy the global bindings in reverse
    /// declaration order.
    fn run_ir(
        mut self,
        ast: &Program,
        ir: &noeta_ir::Program,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
    ) -> RunResult {
        self.reflection = noeta_ast::reflect::build(ast);
        self.type_of_sites = type_of_sites;
        let mut frame = Frame::new(ir.temp_count);
        // The top-level statements run directly in the global scope (no child).
        match self.exec_ir_stmts(&ir.top.stmts, &mut frame) {
            Ok(Flow::Normal) | Ok(Flow::Break) | Ok(Flow::Continue) => {}
            // A top-level `return`, a `?` short-circuit, or a runtime error stops the program.
            Ok(Flow::Return(_)) | Err(Unwind::Return(_)) | Err(Unwind::Abort) => {}
        }
        // Release every value held by the reactive graph (reactivity S1) before global teardown, so a
        // value kept alive only by an undisposed signal drops here — the tree-walker twin of the VM's
        // `vm.reactive.clear()`, keeping the leak oracle's residency at 0.
        self.reactive.clear();
        self.destroy_globals();
        // Reap cycles left after teardown so residency reaches 0: closure-capture cycles (Phase 6.3)
        // and reference-`class` field cycles (object-model slice 2c).
        self.reap_captured_scope_cycles();
        self.reap_object_cycles();
        let exit_code = if self.diagnostics.is_empty() { 0 } else { 1 };
        RunResult {
            stdout: self.stdout,
            exit_code,
            diagnostics: self.diagnostics,
        }
    }

    /// Execute one REPL batch of lowered top-level statements **in the persistent global scope**,
    /// with a fresh temporary frame sized to this batch. Unlike [`Interpreter::run_ir`] it does
    /// *not* rebuild reflection (the [`Session`](crate::Session) sets it) and does *not* destroy the
    /// global bindings afterward — the scope, its bindings, and its declarations persist across
    /// batches, exactly as the REPL requires. ANF temporaries are per-batch and do not persist, so a
    /// fresh `Frame` each call is correct. Returns the batch's terminating [`Flow`] (a top-level
    /// `return`/error stops it, mirroring the AST-walker session loop).
    pub(crate) fn run_ir_batch(&mut self, ir: &noeta_ir::Program) -> Eval<Flow> {
        let mut frame = Frame::new(ir.temp_count);
        self.exec_ir_stmts(&ir.top.stmts, &mut frame)
    }

    /// Execute a statement sequence in the current scope, stopping at the first non-local
    /// flow (`return`/`break`/`continue`) — the IR analogue of `exec_stmts`.
    fn exec_ir_stmts(&mut self, stmts: &[noeta_ir::Stmt], frame: &mut Frame) -> Eval<Flow> {
        for stmt in stmts {
            let flow = self.exec_ir_stmt(stmt, frame)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_ir_stmt(&mut self, stmt: &noeta_ir::Stmt, frame: &mut Frame) -> Eval<Flow> {
        match stmt {
            noeta_ir::Stmt::Let { dst, rvalue, .. } => {
                let value = self.eval_ir_rvalue(rvalue, frame)?;
                frame.set(*dst, value);
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Eval { rvalue, .. } => {
                self.eval_ir_rvalue(rvalue, frame)?;
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Bind {
                mut_decl,
                name,
                name_span,
                value,
                field_assign,
                ..
            } => {
                let value = self.eval_ir_atom(value, frame)?;
                if *field_assign {
                    self.bind_field_assign(name, *name_span, value)?;
                } else {
                    self.bind(*mut_decl, name, *name_span, value)?;
                }
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Echo { value, span } => {
                let v = self.eval_ir_atom(value, frame)?;
                let text = self.display_value(&v, *span)?;
                self.stdout.push_str(&text);
                self.stdout.push('\n');
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Logical {
                dst,
                op,
                left,
                right,
                span,
            } => {
                let value = self.eval_ir_logical(*op, left, right, *span, frame)?;
                if let Some(dst) = dst {
                    frame.set(*dst, value);
                }
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Return { value, .. } => {
                let value = match value {
                    Some(atom) => self.eval_ir_atom(atom, frame)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(value))
            }
            noeta_ir::Stmt::Break { .. } => Ok(Flow::Break),
            noeta_ir::Stmt::Continue { .. } => Ok(Flow::Continue),
            // Open a structured-concurrency scope (Track A.3b): a fresh, empty task list.
            noeta_ir::Stmt::ScopeBegin { .. } => {
                self.scopes.push(Vec::new());
                Ok(Flow::Normal)
            }
            // Close the scope: drive every remaining task to completion (the join), then pop it —
            // releasing the tasks' futures and results (automatic under `Rc`).
            noeta_ir::Stmt::ScopeEnd { span } => {
                self.join_scope(*span)?;
                if let Some(scope) = self.scopes.pop() {
                    // Destructor-aware release of each task's future/result (the VM's `ScopeEnd` mirror):
                    // a **cancelled** task (a `race` loser) abandoned its future mid-body with a live
                    // captured value, whose destructor must run at its last reference here.
                    for task in scope {
                        self.destroy_value(task.future);
                        if let Some(result) = task.result {
                            self.destroy_value(result);
                        }
                    }
                }
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::If {
                cond,
                then_block,
                else_block,
                span,
            } => {
                let taken = match self.eval_ir_atom(cond, frame)? {
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
                    self.exec_ir_block_scoped(then_block, frame)
                } else if let Some(else_block) = else_block {
                    self.exec_ir_block_scoped(else_block, frame)
                } else {
                    Ok(Flow::Normal)
                }
            }
            noeta_ir::Stmt::While {
                cond, body, span, ..
            } => self.exec_ir_while(cond, body, *span, frame),
            noeta_ir::Stmt::For {
                pattern,
                iterable,
                body,
                span,
                stream,
            } => self.exec_ir_for(pattern, iterable, body, *span, *stream, frame),
            noeta_ir::Stmt::Match {
                scrutinee,
                arms,
                dst,
                span,
            } => {
                let value = self.eval_ir_atom(scrutinee, frame)?;
                self.exec_ir_match(value, arms, *dst, *span, frame)
            }
            noeta_ir::Stmt::Decl(decl) => {
                self.exec_ir_decl(decl);
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Drop(t) => {
                // The discarded result of a bare expression statement. Route it through
                // `destroy_value` (not a silent slot clear) so a destructor-bearing value used only
                // as a statement (`Resource.new();`) fires its `destruct` at its last reference —
                // Phase 4.4 (a temp is an owner too, spec §2). Non-aggregates/aliased values no-op.
                let value = frame.take(*t);
                self.destroy_value(value);
                Ok(Flow::Normal)
            }
            // A source-variable drop (Phase 3 drop-insertion): release the binding's value at its
            // last use, aligning this backend's reclamation timing with the VM's. When the drop is
            // destructor-relevant (Phase 4), take the value out and run `destroy_value`, firing its
            // `destruct` block if this is the final reference (the VM's `release_value` mirror);
            // otherwise the value reaches no destructor, so the plain `release_binding` is used.
            noeta_ir::Stmt::DropVar { name, relevant, .. } => {
                if *relevant {
                    if let Some(value) = self.scope.take_for_drop(name) {
                        self.destroy_value(value);
                    }
                } else {
                    self.scope.release_binding(name);
                }
                Ok(Flow::Normal)
            }
            noeta_ir::Stmt::Coalesce {
                dst,
                value,
                fallback,
                span,
            } => {
                let v = self.eval_ir_atom(value, frame)?;
                let result = match crate::try_branch(&v) {
                    Some(crate::TryBranch::Success(inner)) => inner,
                    Some(crate::TryBranch::Empty) => {
                        self.eval_ir_block_value(fallback, frame, *span)?
                    }
                    None => {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`??` expects a `Result` or `Option` on the left, found {}",
                                v.type_name()
                            ),
                        ));
                    }
                };
                if let Some(dst) = dst {
                    frame.set(*dst, result);
                }
                Ok(Flow::Normal)
            }
        }
    }

    /// Evaluate an IR `match`: try each arm's pattern in order, bind on the first match, run
    /// that arm's body block in a child scope, and write its value to `dst`. Mirrors
    /// `eval_match`, including the no-arm-matched runtime error.
    fn exec_ir_match(
        &mut self,
        value: Value,
        arms: &[noeta_ir::Arm],
        dst: Option<noeta_ir::Temp>,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Flow> {
        for arm in arms {
            if let Some(bindings) = crate::match_pattern(&arm.pattern, &value) {
                let child = crate::Scope::child(&self.scope);
                for (name, bound) in bindings {
                    child.declare(name, bound, false);
                }
                let saved = std::mem::replace(&mut self.scope, child);
                let result = self.eval_ir_block_value(&arm.body, frame, arm.span);
                if matches!(result, Err(Unwind::Abort)) {
                    self.fire_aborted_scope();
                }
                self.scope = saved;
                let v = result?;
                if let Some(dst) = dst {
                    frame.set(dst, v);
                }
                return Ok(Flow::Normal);
            }
        }
        Err(self.runtime_error(
            DiagnosticCode::TypeMismatch,
            span,
            format!("no match arm matched the value {}", value.display()),
        ))
    }

    /// Register a declaration. `fn`/`class` build IR-bodied closures; `enum`/`struct`/`use`
    /// carry no executable body, so they reuse the tree-walker's registration unchanged.
    fn exec_ir_decl(&mut self, decl: &noeta_ir::Decl) {
        match decl {
            noeta_ir::Decl::Fn { name, func, .. } => {
                let closure = self.make_ir_closure(func);
                self.scope
                    .declare(name.clone(), Value::Function(Rc::new(closure)), false);
            }
            noeta_ir::Decl::Class(class) => self.declare_ir_class(class),
            noeta_ir::Decl::Enum(en) => self.declare_ir_enum(en),
            noeta_ir::Decl::Struct(s) => self.declare_ir_struct(s),
            noeta_ir::Decl::Use { path, names, .. } => self.declare_use(path, names),
        }
    }

    /// Build a closure value from a lowered IR function template, capturing the current
    /// lexical scope — the IR analogue of `declare_fn`'s/`Expr::Closure`'s construction.
    fn make_ir_closure(&self, func: &Rc<noeta_ir::Func>) -> Closure {
        Closure::new(
            func.params.clone(),
            func.defaults.clone(),
            Rc::clone(func),
            Rc::clone(&self.scope),
        )
    }

    /// Register a struct (the value kind) whose methods are IR-bodied closures. Mirrors
    /// [`Self::declare_ir_class`] but `is_struct: true` and never a destructor (structs are pure
    /// data). Fields/derives come from the carried surface declaration; methods are lowered IR funcs.
    fn declare_ir_struct(&mut self, strukt: &noeta_ir::StructDef) {
        let decl = &strukt.decl;
        let fields = decl
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.name.clone(),
            })
            .collect();
        let methods = strukt
            .methods
            .iter()
            .map(|(name, func)| (name.clone(), Rc::new(self.make_ir_closure(func))))
            .collect();
        let def = TypeDef {
            name: decl.name.clone(),
            fields,
            methods,
            destructor: None,
            is_struct: true,
            // A value kind: `==` is always structural.
            structural_eq: true,
            // A hand-written `compare`/`to_json` takes precedence over derivation.
            derives_comparable: noeta_ast::derives_trait(&decl.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            derives_tojson: noeta_ast::derives_trait(&decl.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            opaque: false,
            field_defaults: strukt.field_defaults.clone(),
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Register an enum whose methods are IR-bodied closures (object-model slice 3). Mirrors
    /// [`Self::declare_ir_struct`]: variants/derives come from the carried surface declaration; the
    /// methods are the lowered IR funcs. The AST-walker counterpart is `declare_enum`.
    fn declare_ir_enum(&mut self, en: &noeta_ir::EnumDef) {
        let decl = &en.decl;
        let variants = decl
            .variants
            .iter()
            .map(|v| VariantInfo {
                name: v.name.clone(),
                field_names: v.fields.iter().map(|f| f.name.clone()).collect(),
            })
            .collect();
        let methods = en
            .methods
            .iter()
            .map(|(name, func)| (name.clone(), Rc::new(self.make_ir_closure(func))))
            .collect();
        let def = EnumDef {
            name: decl.name.clone(),
            variants,
            methods,
        };
        self.scope
            .declare(decl.name.clone(), Value::EnumType(Rc::new(def)), false);
    }

    /// Register a class whose methods are IR-bodied closures. Mirrors `declare_class`: fields,
    /// derives, and the (still-surface) destructor come from the carried declaration; the
    /// methods are the lowered IR funcs.
    fn declare_ir_class(&mut self, class: &noeta_ir::ClassDef) {
        let decl = &class.decl;
        let fields = decl
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.name.clone(),
            })
            .collect();
        let methods = class
            .methods
            .iter()
            .map(|(name, func)| (name.clone(), Rc::new(self.make_ir_closure(func))))
            .collect();
        let def = TypeDef {
            name: decl.name.clone(),
            fields,
            methods,
            // The lowered `destruct` block (a parameterless IR `Func`), run via `exec_ir_fn_body`
            // with fields + `self` in scope — the same IR the VM compiles, so destructor execution
            // no longer routes through the retired AST walker.
            destructor: class.destructor.clone(),
            is_struct: false,
            // A reference `class`: `==` is identity unless the class is `Equatable` (derives it or
            // hand-`impl`s `eq`) — the same rule `declare_class` applies.
            structural_eq: noeta_ast::derives_trait(&decl.derives, "Equatable")
                || decl.methods.iter().any(|m| m.name == "eq"),
            // A hand-written `compare`/`to_json` takes precedence over derivation — the same
            // rule `declare_class` applies.
            derives_comparable: noeta_ast::derives_trait(&decl.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            derives_tojson: noeta_ast::derives_trait(&decl.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            opaque: false,
            field_defaults: class.field_defaults.clone(),
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Run a lowered function body as a call: allocate its temporary frame, run its
    /// statements, and yield the explicit `return` value, else the arrow tail, else unit.
    /// Mirrors `exec_fn_body` (block) and arrow-body evaluation. Called from the shared call
    /// machinery (`call_closure`/`call_method_on`) when a closure has an IR body.
    pub(crate) fn exec_ir_fn_body(&mut self, func: &noeta_ir::Func) -> Eval<Value> {
        let mut frame = Frame::new(func.temp_count);
        let result = self.exec_ir_stmts(&func.body.stmts, &mut frame);
        if matches!(result, Err(Unwind::Abort)) {
            // A panic abandons this frame: destroy its live locals (reverse-construction) before the
            // abort unwinds to the caller (Phase 4.2c-ii).
            self.fire_aborted_scope();
        }
        match result? {
            Flow::Return(value) => Ok(value),
            // No explicit return: a bare `break`/`continue` cannot escape a function boundary
            // (the checker rejects one outside a loop), so they fall through like `Normal`.
            Flow::Normal | Flow::Break | Flow::Continue => match &func.body.tail {
                Some(atom) => self.eval_ir_atom(atom, &mut frame),
                None => Ok(Value::Unit),
            },
        }
    }

    /// Evaluate a defaulted-parameter thunk in its own temporary frame (the current scope is
    /// the closure's captured scope, set up by `eval_default`). A default always yields a
    /// value, so its block has a tail and produces no non-local flow. Called from `eval_default`.
    pub(crate) fn exec_ir_thunk(&mut self, thunk: &noeta_ir::Thunk) -> Eval<Value> {
        let mut frame = Frame::new(thunk.temp_count);
        match self.exec_ir_stmts(&thunk.body.stmts, &mut frame)? {
            Flow::Normal => {}
            Flow::Return(_) | Flow::Break | Flow::Continue => {
                unreachable!("a default-value thunk cannot produce non-local flow (lowering bug)")
            }
        }
        match &thunk.body.tail {
            Some(atom) => self.eval_ir_atom(atom, &mut frame),
            None => {
                unreachable!("a default-value thunk must have a tail expression (lowering bug)")
            }
        }
    }

    /// Run a statement-position block in a fresh child scope (the IR analogue of
    /// `exec_block`), propagating any non-local flow. The temporary frame is shared with the
    /// enclosing activation.
    fn exec_ir_block_scoped(&mut self, block: &noeta_ir::Block, frame: &mut Frame) -> Eval<Flow> {
        let child = crate::Scope::child(&self.scope);
        let saved = std::mem::replace(&mut self.scope, child);
        let result = self.exec_ir_stmts(&block.stmts, frame);
        if matches!(result, Err(Unwind::Abort)) {
            self.fire_aborted_scope();
        }
        self.scope = saved;
        result
    }

    /// `while cond { body }` — mirrors `exec_while`: re-evaluate the condition block each
    /// iteration (it must yield a bool), running the body in a fresh child scope.
    fn exec_ir_while(
        &mut self,
        cond: &noeta_ir::Block,
        body: &noeta_ir::Block,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Flow> {
        loop {
            let taken = match self.eval_ir_block_value(cond, frame, span)? {
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
            match self.exec_ir_block_scoped(body, frame)? {
                Flow::Return(value) => return Ok(Flow::Return(value)),
                Flow::Break => return Ok(Flow::Normal),
                Flow::Continue | Flow::Normal => {}
            }
        }
    }

    /// `for pattern in iterable { body }` — mirrors `exec_for`: materialize the elements, then
    /// bind the pattern and run the body in a fresh child scope per element, honoring
    /// `break`/`continue`/`return`.
    fn exec_ir_for(
        &mut self,
        pattern: &noeta_ir::ForPattern,
        iterable: &noeta_ir::Atom,
        body: &noeta_ir::Block,
        span: Span,
        stream: bool,
        frame: &mut Frame,
    ) -> Eval<Flow> {
        let iterable_value = self.eval_ir_atom(iterable, frame)?;
        // A statically-typed `Iterator<T>` source streams via `next()` (Track I.2) — one element at a
        // time, so a lazy pipeline never materializes and an early `break` stops it; a `map`/`filter`
        // closure runs inside the advance. A collection keeps the snapshot fast path.
        if stream {
            loop {
                let element = match self.iter_value_next(&iterable_value, span)? {
                    Some(e) => e,
                    None => break,
                };
                let child = crate::Scope::child(&self.scope);
                self.bind_for_pattern(&child, pattern, element, span)?;
                let saved = std::mem::replace(&mut self.scope, child);
                let flow = self.exec_ir_stmts(&body.stmts, frame);
                if matches!(flow, Err(Unwind::Abort)) {
                    self.fire_aborted_scope();
                }
                self.scope = saved;
                match flow {
                    // An early `return` or a `?` propagation (`Unwind::Return`) unwinds past the loop;
                    // destroy the streamed iterator here so a generator's captured local runs its
                    // destructor (the VM's return_stmt / TryUnwrap-`on_error` iterator-drop mirror). A
                    // named iterable defers via its `strong_count`; a temp is destroyed now.
                    Ok(Flow::Return(value)) => {
                        self.destroy_value(iterable_value);
                        return Ok(Flow::Return(value));
                    }
                    Err(Unwind::Return(value)) => {
                        self.destroy_value(iterable_value);
                        return Err(Unwind::Return(value));
                    }
                    // An abort was already routed through `fire_aborted_scope`; propagate it unchanged.
                    Err(e) => return Err(e),
                    Ok(Flow::Break) => break,
                    Ok(Flow::Continue) | Ok(Flow::Normal) => {}
                }
            }
            // At exhaustion or `break`, destroy the streamed iterator destructor-aware so a generator's
            // captured destructor-bearing local runs at its last reference (the VM's temp-iterable
            // post-loop drop mirror). A *named* iterable was read by clone, so its `strong_count > 1`
            // defers here to its binding's scope-end drop; a temp was moved out and is destroyed now.
            self.destroy_value(iterable_value);
            return Ok(Flow::Normal);
        }
        let elements = self.iter_elements(iterable_value, span)?;
        for element in elements {
            let child = crate::Scope::child(&self.scope);
            self.bind_for_pattern(&child, pattern, element, span)?;
            let saved = std::mem::replace(&mut self.scope, child);
            let flow = self.exec_ir_stmts(&body.stmts, frame);
            if matches!(flow, Err(Unwind::Abort)) {
                self.fire_aborted_scope();
            }
            self.scope = saved;
            match flow? {
                Flow::Return(value) => return Ok(Flow::Return(value)),
                Flow::Break => break,
                Flow::Continue | Flow::Normal => {}
            }
        }
        Ok(Flow::Normal)
    }

    /// Resolve an atom to a value: a constant, a frame temporary (moved out — see
    /// [`Frame::take`]), or a lexical lookup. The `Var` not-found path reproduces the AST
    /// walker's `Ident` diagnostic exactly.
    fn eval_ir_atom(&mut self, atom: &noeta_ir::Atom, frame: &mut Frame) -> Eval<Value> {
        match atom {
            noeta_ir::Atom::Const(c) => Ok(const_value(c)),
            noeta_ir::Atom::Temp(t) => Ok(frame.take(*t)),
            noeta_ir::Atom::Var { name, span } => match self.scope.lookup(name) {
                Some(value) => Ok(value),
                None => {
                    // Not a local. A bare name inside a method NEVER resolves to a field
                    // (prelude-redesign EX.1 — member access is explicit: `self.field`), so a miss
                    // here is a plain unknown name, exactly as the VM reports it.
                    self.diagnostics.push(Diagnostic::error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!("cannot find `{name}` in this scope"),
                    ));
                    Err(Unwind::Abort)
                }
            },
        }
    }

    /// Read a member off a *borrowed* receiver value: an enum-variant constructor, an object field
    /// load, or an associated-function reference (mirrors `eval_expr`'s `Member` arm). Shared by the
    /// `Field` rvalue and the fused `IndexField` fallback so both produce identical results and
    /// diagnostics. The receiver is borrowed (the field is cloned out); the caller owns lifetime.
    fn read_member(&mut self, recv: &Value, name: &str, span: Span) -> Eval<Value> {
        match recv {
            Value::EnumType(def) => self.make_variant(def, name, vec![], span),
            Value::Object(object) => match object.field(name) {
                Some(value) => Ok(value),
                None => Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("type `{}` has no field `{name}`", object.def.name()),
                )),
            },
            Value::Type(def) => match def.methods.get(name) {
                Some(method) => Ok(Value::Function(Rc::clone(method))),
                None => Err(self.runtime_error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("type `{}` has no associated function `{name}`", def.name()),
                )),
            },
            other => Err(self.runtime_error(
                DiagnosticCode::UnknownName,
                span,
                format!("no field `{name}` on {}", other.type_name()),
            )),
        }
    }

    /// Compute a primitive operation over already-resolved atoms. Each arm mirrors the
    /// matching `eval_expr` arm, delegating to the shared leaf helpers so behavior is
    /// identical by construction.
    fn eval_ir_rvalue(&mut self, rvalue: &noeta_ir::Rvalue, frame: &mut Frame) -> Eval<Value> {
        match rvalue {
            noeta_ir::Rvalue::Use(atom) => self.eval_ir_atom(atom, frame),
            noeta_ir::Rvalue::Unary { op, operand, span } => {
                let value = self.eval_ir_atom(operand, frame)?;
                self.eval_unary(*op, value, *span)
            }
            noeta_ir::Rvalue::MaskWidth {
                operand,
                signed,
                bits,
                ..
            } => {
                // Reduce an erased fixed-width integer (an `Int`) into its declared width (Tier W) via
                // the same shared helper the VM calls, so wraparound agrees by construction. A non-int
                // (only if the checker's IntN guarantee broke) passes through unchanged.
                let value = self.eval_ir_atom(operand, frame)?;
                Ok(match value {
                    Value::Int(n) => Value::Int(noeta_stdlib::mask_to_width(n, *signed, *bits)),
                    other => other,
                })
            }
            noeta_ir::Rvalue::Binary {
                op,
                lhs,
                rhs,
                reuse,
                span,
            } => {
                // In-place self-append (Phase 5.1b): a marked `acc = acc ~ rhs` moves the accumulator
                // out of its (reassigned) binding and extends its list buffer in place when uniquely
                // owned — the IR-interpreter analogue of the VM's `ConcatInPlace` (and the same
                // `cow_concat` the AST walker uses). The `rhs` is evaluated *before* the accumulator is
                // taken; the reuse pass guarantees `rhs` does not mention the base, so it never observes
                // the vacated slot. The token is only ever set for `Concat` with a `Var` left, but the
                // guards are explicit so any other shape falls through to the ordinary copying path.
                if *reuse
                    && *op == noeta_ir::BinaryOp::Concat
                    && let noeta_ir::Atom::Var { name, .. } = lhs
                {
                    let right = self.eval_ir_atom(rhs, frame)?;
                    if let Some(old) = self.scope.take_mut(name) {
                        return Ok(crate::cow_concat(old, right));
                    }
                    // Defensive: not a mutable binding (cannot happen for a checked self-append).
                    let left = self.eval_ir_atom(lhs, frame)?;
                    return self.apply_binary_op(*op, left, right, *span);
                }
                let left = self.eval_ir_atom(lhs, frame)?;
                let right = self.eval_ir_atom(rhs, frame)?;
                self.apply_binary_op(*op, left, right, *span)
            }
            noeta_ir::Rvalue::WideInt {
                op,
                lhs,
                rhs,
                signed,
                bits,
                span,
            } => {
                let left = self.eval_ir_atom(lhs, frame)?;
                let right = self.eval_ir_atom(rhs, frame)?;
                self.apply_binary_wide_op(*op, left, right, *signed, *bits, *span)
            }
            noeta_ir::Rvalue::WidthIntMethod {
                receiver,
                method,
                args,
                bits,
                ..
            } => {
                // Width-exact bit intrinsic (Tier W5): the twin of the VM's `Op::WidthIntMethod`,
                // computed within `bits` via the shared `int_method_width`.
                let recv = self.eval_ir_atom(receiver, frame)?;
                let recv_int = match recv {
                    Value::Int(n) => n,
                    _ => 0,
                };
                let amount = match args.first() {
                    Some(a) => match self.eval_ir_atom(a, frame)? {
                        Value::Int(n) => n,
                        _ => 0,
                    },
                    None => 0,
                };
                Ok(Value::Int(noeta_stdlib::int_method_width(
                    recv_int, *method, amount, *bits,
                )))
            }
            noeta_ir::Rvalue::List { items, reflect, .. } => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_ir_atom(item, frame)?);
                }
                // Stamp the checker-resolved element type onto the list (R1) so `type_of` recovers it
                // after a `dyn` launder — the tree-walker twin of the VM's node tag, agreeing by
                // construction. `None` → an untagged list, reflecting head-only.
                let repr =
                    ListRepr::boxed(Rc::new(values)).with_reflect(reflect.clone().map(Rc::new));
                Ok(Value::List(repr))
            }
            noeta_ir::Rvalue::PackedListNew { layout, .. } => {
                // Start a streaming flat build (P-PACK 2.5): an empty `List<packed>` buffer, or an
                // empty boxed list if the element type can't resolve to a packed schema (a defensive
                // fall-back — the layout comes from the checker, so this is never hit for a valid
                // `@packed` type). Subsequent pushes extend whichever representation this produced.
                match self.resolve_packed_schema(layout) {
                    Some(schema) => Ok(Value::List(ListRepr::Packed(PackedList::empty(schema)))),
                    None => Ok(Value::list(Vec::new())),
                }
            }
            noeta_ir::Rvalue::PackedListPush { list, value, span } => {
                // Append the freshly-built `value` to the accumulator `list` and yield it. The
                // accumulator is an ANF temp (uniquely owned), so a packed buffer extends in place.
                // On a pack failure the packed list demotes to boxed and the value is pushed there,
                // keeping the flat form an exact optimization; the element object is consumed either
                // way (packed: copied to words and dropped; boxed: moved into the list).
                let list = self.eval_ir_atom(list, frame)?;
                let element = self.eval_ir_atom(value, frame)?;
                match list {
                    Value::List(ListRepr::Packed(mut packed)) => {
                        if packed.push(&element) {
                            self.destroy_value(element);
                            Ok(Value::List(ListRepr::Packed(packed)))
                        } else {
                            let mut boxed = packed.to_boxed();
                            boxed.push(element);
                            Ok(Value::list(boxed))
                        }
                    }
                    Value::List(ListRepr::Boxed { items: rc, .. }) => {
                        // The accumulator is a moved temp, so it is uniquely owned — take the vector
                        // without copying (the clone is only a defensive fall-back).
                        let mut boxed = Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone());
                        boxed.push(element);
                        Ok(Value::list(boxed))
                    }
                    other => Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "packed-list push onto a non-list value `{}`",
                            other.display()
                        ),
                    )),
                }
            }
            noeta_ir::Rvalue::Tuple { items, .. } => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_ir_atom(item, frame)?);
                }
                Ok(Value::Tuple(Rc::new(values)))
            }
            noeta_ir::Rvalue::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let recv = self.eval_ir_atom(receiver, frame)?;
                let result = self.tuple_index(recv.clone(), *index, *span);
                if is_temp(receiver) {
                    self.destroy_value(recv);
                }
                result
            }
            noeta_ir::Rvalue::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let lo = self.eval_ir_atom(start, frame)?;
                let hi = self.eval_ir_atom(end, frame)?;
                match (lo, hi) {
                    (Value::Int(a), Value::Int(b)) => {
                        let upper = if *inclusive { b.saturating_add(1) } else { b };
                        let items: Vec<Value> = (a..upper).map(Value::Int).collect();
                        Ok(Value::list(items))
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
            noeta_ir::Rvalue::Map {
                entries,
                reflect,
                span,
            } => {
                let mut map = BTreeMap::new();
                for (key_atom, value_atom) in entries {
                    let key = match self.eval_ir_atom(key_atom, frame)? {
                        Value::Str(s) => s,
                        other => {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("map keys must be strings, found {}", other.type_name()),
                            ));
                        }
                    };
                    let value = self.eval_ir_atom(value_atom, frame)?;
                    map.insert(key, value);
                }
                // Stamp the checker-resolved `Map(K, V)` type (R1) so `type_of` recovers it after a
                // `dyn` launder — the tree-walker twin of the VM's node tag. `None` → untagged.
                Ok(Value::map_value_tagged(
                    Rc::new(map),
                    reflect.clone().map(Rc::new),
                ))
            }
            noeta_ir::Rvalue::Index {
                receiver,
                index,
                span,
            } => {
                let recv = self.eval_ir_atom(receiver, frame)?;
                let idx = self.eval_ir_atom(index, frame)?;
                if is_temp(receiver) {
                    // The receiver is an owned temporary whose only use is this access; fire its
                    // destructor after (Phase 4.4). `eval_index` consumes the receiver, so clone for
                    // the call and destroy the held copy — firing iff it is the last reference.
                    let result = self.eval_index(recv.clone(), idx, *span);
                    self.destroy_value(recv);
                    result
                } else {
                    self.eval_index(recv, idx, *span)
                }
            }
            noeta_ir::Rvalue::Interp { parts, .. } => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        noeta_ir::InterpPart::Literal(text) => out.push_str(text),
                        noeta_ir::InterpPart::Hole { atom, span } => {
                            let v = self.eval_ir_atom(atom, frame)?;
                            out.push_str(&self.display_value(&v, *span)?);
                        }
                    }
                }
                Ok(Value::Str(out))
            }
            noeta_ir::Rvalue::Call { callee, args, span } => {
                let callee = self.eval_ir_atom(callee, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                self.call(callee, values, *span)
            }
            noeta_ir::Rvalue::Method {
                receiver,
                name,
                args,
                reuse,
                reflect,
                span,
                ..
            } => {
                // In-place collection self-update (Phase 5.1c): a marked `m = m.set(k,v)` moves the
                // receiver out of its (reassigned) binding so a uniquely-owned map can be mutated in
                // place; mirrors the VM's reuse-aware dispatch. A non-map receiver (e.g. a user method
                // named `set`) falls back to an ordinary consuming call with the moved-out value, so
                // the following `Bind` still rebinds it. `take_mut` is name-keyed, so this reuses a
                // local *or* a global accumulator (the VM does locals this slice; reuse is invisible,
                // so the two backends still agree).
                if *reuse
                    && let noeta_ir::Atom::Var {
                        name: recv_name, ..
                    } = receiver
                {
                    let values = self.eval_ir_atoms(args, frame)?;
                    let recv = match self.scope.take_mut(recv_name) {
                        Some(v) => v,
                        None => self.eval_ir_atom(receiver, frame)?,
                    };
                    if matches!(&recv, Value::Map(..)) {
                        if name == "set" && values.len() == 2 {
                            return self.map_set_in_place(recv, values, *span);
                        }
                        if name == "remove" && values.len() == 1 {
                            return self.map_remove_in_place(recv, values, *span);
                        }
                    }
                    if matches!(&recv, Value::List(_)) && name == "set" && values.len() == 2 {
                        return self.list_set_in_place(recv, values, *span);
                    }
                    if matches!(&recv, Value::Set(..)) && values.len() == 1 {
                        if name == "add" {
                            return self.set_add_in_place(recv, values, *span);
                        }
                        if name == "remove" {
                            return self.set_remove_in_place(recv, values, *span);
                        }
                    }
                    return self.call_method(recv, name, values, *span);
                }
                let recv = self.eval_ir_atom(receiver, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                let result = if is_temp(receiver) {
                    // An owned temp receiver (`Resource.new().use()`): fire its destructor after the
                    // call (Phase 4.4). `call_method` consumes the receiver, so clone for the call
                    // and destroy the held copy — last-reference-gated, so a method that returns
                    // `self` (the result aliases it) correctly defers destruction.
                    let result = self.call_method(recv.clone(), name, values, *span);
                    self.destroy_value(recv);
                    result
                } else {
                    self.call_method(recv, name, values, *span)
                };
                // When this "method call" was a generic enum-variant construction, stamp the reflected
                // type onto the freshly-built value (R2b.2) — the tree-walker twin of the VM's node tag.
                result.map(|v| tag_enum_reflect(v, reflect))
            }
            noeta_ir::Rvalue::Field {
                receiver,
                name,
                span,
                ..
            } => {
                // Mirrors `eval_expr`'s `Member` arm: enum-variant constructor, object field
                // load, or associated-function reference. The receiver is *borrowed* to read the
                // field (cloned out), so an owned temp receiver — e.g. the `b.inner` projected out
                // of a container in `b.inner.tag` — is destroyed afterward (Phase 4.4), firing its
                // destructor iff it held the last reference (the field clone is a separate object,
                // so it survives).
                let recv = self.eval_ir_atom(receiver, frame)?;
                let result = self.read_member(&recv, name, *span);
                if is_temp(receiver) {
                    self.destroy_value(recv);
                }
                result
            }
            // An unbound method handle (`Type.method` as a value) — a static callable value; the
            // receiver type name is a string, so nothing is evaluated.
            noeta_ir::Rvalue::MethodHandle {
                ty,
                method,
                associated,
                ..
            } => Ok(Value::MethodHandle(ty.clone(), method.clone(), *associated)),
            // A bound method handle (`value.method`, EX.2b): evaluate and capture the receiver.
            noeta_ir::Rvalue::BoundHandle { recv, method, .. } => {
                let recv = self.eval_ir_atom(recv, frame)?;
                Ok(Value::BoundMethod(Box::new(recv), method.clone()))
            }
            noeta_ir::Rvalue::IndexField {
                receiver,
                index,
                field,
                span,
                ..
            } => {
                // Fused `list[i].field` (P-PACK 2.5+). A packed list decodes the one field directly,
                // without materializing the element. Any miss (non-int/out-of-range index, or unknown
                // field) falls through to the ordinary index-then-load, which reproduces the exact
                // diagnostics of the unfused `Index` + `Field` it replaces.
                let recv = self.eval_ir_atom(receiver, frame)?;
                let idx = self.eval_ir_atom(index, frame)?;
                if let Value::List(ListRepr::Packed(packed)) = &recv
                    && let Value::Int(i) = &idx
                    && *i >= 0
                    && let Some(value) = packed.field(*i as usize, field)
                {
                    if is_temp(receiver) {
                        self.destroy_value(recv);
                    }
                    return Ok(value);
                }
                // Fallback: index (a boxed/demoted list, or to surface the bounds/type error) then
                // read the field. `eval_index` consumes the receiver, so clone for the call and
                // destroy the held copy iff it was an owned temp — exactly as the `Index` arm does.
                let element = if is_temp(receiver) {
                    let r = self.eval_index(recv.clone(), idx, *span);
                    self.destroy_value(recv);
                    r
                } else {
                    self.eval_index(recv, idx, *span)
                }?;
                let result = self.read_member(&element, field, *span);
                // The element is a fresh owned temp (a cloned boxed element or a materialized packed
                // one); its field was cloned out, so destroy it now (firing its destructor iff last).
                self.destroy_value(element);
                result
            }
            noeta_ir::Rvalue::SetField {
                receiver,
                name,
                value,
                reuse,
                span,
                ..
            } => {
                let new_value = self.eval_ir_atom(value, frame)?;
                // A reuse-marked self-update (`x.f = v` ⟶ `x = SetField(x, f, v)`) moves the object
                // out of its (being-reassigned) binding so a uniquely-owned instance can be mutated
                // in place; an aliased instance copies. A non-`Var` receiver (or unmarked) reads the
                // object value and copies (functional update). Either way value semantics hold.
                if *reuse
                    && let noeta_ir::Atom::Var {
                        name: recv_name, ..
                    } = receiver
                    && let Some(recv) = self.scope.take_mut(recv_name)
                {
                    return self.set_field_in_place(recv, name, new_value, *span);
                }
                let recv = self.eval_ir_atom(receiver, frame)?;
                self.set_field_in_place(recv, name, new_value, *span)
            }
            noeta_ir::Rvalue::Try {
                operand,
                on_error,
                span,
            } => {
                let value = self.eval_ir_atom(operand, frame)?;
                self.eval_try_ir(value, on_error, *span)
            }
            noeta_ir::Rvalue::As { operand, ty, .. } => {
                let value = self.eval_ir_atom(operand, frame)?;
                if crate::runtime_matches(&value, ty) {
                    Ok(crate::builtin_enum("Option", "some", vec![value]))
                } else {
                    Ok(crate::builtin_enum("Option", "none", vec![]))
                }
            }
            noeta_ir::Rvalue::TypeTest { operand, ty, .. } => {
                let value = self.eval_ir_atom(operand, frame)?;
                Ok(Value::Bool(crate::runtime_matches(&value, ty)))
            }
            noeta_ir::Rvalue::TypeOf { operand, span } => {
                let v = self.eval_ir_atom(operand, frame)?;
                match self.type_of_sites.get(span) {
                    Some(repr) => Ok(crate::build_type_value(repr)),
                    None => Ok(crate::build_type_value(&crate::eval_type_repr(&v))),
                }
            }
            noeta_ir::Rvalue::FromBytes { blob, layout, span } => {
                // Deserialize a `bytes` buffer into a flat `List<T>` (P-PACK 4.4): resolve T's schema,
                // then wrap the raw bytes as a packed list — the inverse of `to_bytes`, an O(n) copy.
                let blob_val = self.eval_ir_atom(blob, frame)?;
                let Value::Bytes(bytes) = blob_val else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "`from_bytes` expects a `bytes` value, found {}",
                            blob_val.type_name()
                        ),
                    ));
                };
                let schema = layout
                    .as_ref()
                    .and_then(|l| self.resolve_packed_schema(l))
                    .ok_or_else(|| {
                        self.runtime_error(
                            DiagnosticCode::InvalidPackedType,
                            *span,
                            "`from_bytes` requires a packable `@packed` struct element type"
                                .to_string(),
                        )
                    })?;
                // The buffer must be a whole number of elements; a partial blob is corrupt input.
                if schema.byte_size == 0 || bytes.len() % schema.byte_size != 0 {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "`from_bytes` buffer of {} bytes is not a whole number of {}-byte elements",
                            bytes.len(),
                            schema.byte_size
                        ),
                    ));
                }
                Ok(Value::packed_list_from(schema, (*bytes).clone()))
            }
            noeta_ir::Rvalue::AttributesOf { ty, .. } => {
                let type_name = match ty {
                    noeta_ir::TypeRef::Named { name, .. } => name.as_str(),
                    _ => "",
                };
                Ok(self.materialize_attributes(type_name))
            }
            noeta_ir::Rvalue::Object {
                type_name,
                type_name_span,
                fields,
                spread,
                reuse,
                reflect,
                span,
            } => {
                // The checker-resolved reflected type (R2) for a generic instantiation; `None` for a
                // non-generic type → the value reflects head-only. Wrapped in an `Rc` to match the tag
                // stored on the object (a cheap refcount bump on construction).
                let reflect = reflect.clone().map(Rc::new);
                // In-place reuse (Phase 5): a marked self-update `acc = Type { ...acc, … }` moves the
                // accumulator out of its (reassigned) binding and mutates it in place when uniquely
                // owned, mirroring the VM's `MakeStructInPlace`. The token guarantees the spread is the
                // reassigned base; both backends gate on the runtime refcount so an alias copies.
                if *reuse && let Some((noeta_ir::Atom::Var { name, .. }, _)) = spread {
                    return self.construct_object_reuse(
                        type_name,
                        *type_name_span,
                        fields,
                        name,
                        reflect,
                        *span,
                        frame,
                    );
                }
                let spread = match spread {
                    Some((atom, sp)) => Some((self.eval_ir_atom(atom, frame)?, *sp)),
                    None => None,
                };
                let mut field_values = Vec::with_capacity(fields.len());
                for f in fields {
                    let value = self.eval_ir_atom(&f.value, frame)?;
                    field_values.push((f.name.clone(), f.name_span, value));
                }
                self.construct_object(
                    type_name,
                    *type_name_span,
                    field_values,
                    spread,
                    reflect,
                    *span,
                )
            }
            noeta_ir::Rvalue::Closure { func, .. } => {
                Ok(Value::Function(Rc::new(self.make_ir_closure(func))))
            }
            // Wrap the step closure into a generator iterator (Track G.1b) — the tree-walker mirror of
            // the VM's `Op::MakeGen`.
            noeta_ir::Rvalue::MakeGen { step, .. } => {
                let step = self.eval_ir_atom(step, frame)?;
                Ok(Value::Iter(Rc::new(RefCell::new(IterState::Gen { step }))))
            }
            // Wrap the lazy thunk into a future (Track A.1) — the tree-walker mirror of the VM's
            // `Op::MakeFuture`. The thunk is not run until the future is awaited (`RunFuture`).
            noeta_ir::Rvalue::MakeFuture { thunk, .. } => {
                let thunk = self.eval_ir_atom(thunk, frame)?;
                Ok(Value::Future(Rc::new(thunk)))
            }
            // Run an awaited future to completion (Track A.2): a step/thunk future runs to its value;
            // a leaf timer suspends until the executor clock reaches its deadline. See
            // [`Interpreter::drive_future`]. A non-future (only reachable via the uncheck­ed property
            // test) passes straight through so evaluation stays total.
            noeta_ir::Rvalue::RunFuture { future, span } => {
                // `.await` consumes the future (a spent future cannot be awaited again — double-await
                // already deadlocks). Drive it, then destroy it so a destructor-bearing local captured
                // in the async fn's state runs at the future's last reference (matching the VM's
                // take-and-release). For a temp source `eval_ir_atom` already moved it out, so this
                // holds the sole reference; a still-live binding source keeps a reference and defers.
                let future = self.eval_ir_atom(future, frame)?;
                let value = self.drive_future(future.clone(), *span)?;
                self.destroy_value(future);
                Ok(value)
            }
            // Poll a future once (Track A.3 state machine): `some(v)` if ready, `none` if pending —
            // the tree-walker mirror of the VM's `Op::PollFuture`.
            noeta_ir::Rvalue::PollFuture { future, span } => {
                let future = self.eval_ir_atom(future, frame)?;
                Ok(match self.poll_once(&future, *span)? {
                    Some(value) => crate::builtin_enum("Option", "some", vec![value]),
                    None => crate::builtin_enum("Option", "none", vec![]),
                })
            }
            // The async pending sentinel (Track A.3) — what a step returns when it suspends.
            noeta_ir::Rvalue::Pending { .. } => Ok(Value::Pending),
            // `spawn e` (Track A.3b): register the future as a task in the current scope, yielding a
            // handle referencing it by `(scope, task)`. Lazy — the task is not polled until the scope
            // is driven (a `.await` inside the block or the join at `}`). A `spawn` outside any scope is
            // E0041 at check, so `self.scopes` is non-empty here for a clean program.
            noeta_ir::Rvalue::Spawn { future, .. } => {
                let future = self.eval_ir_atom(future, frame)?;
                if self.scopes.is_empty() {
                    // Unreachable for a checked program (E0041); keep evaluation total.
                    return Ok(future);
                }
                let scope_idx = self.scopes.len() - 1;
                let task_idx = self.scopes[scope_idx].len();
                self.scopes[scope_idx].push(crate::Task {
                    future,
                    result: None,
                    cancelled: false,
                });
                Ok(Value::Handle(
                    ScopeId::from_index(scope_idx),
                    TaskId::from_index(task_idx),
                ))
            }
            // `isolate f(args)` (isolates I.4b). The tree-walker only ever runs the deterministic
            // sandbox, where an isolate is observationally a cooperative task: build the future by
            // calling `callee(args)` and register it exactly as `spawn` does (identical to the old
            // lowering, which pre-built `f(args)` and `Spawn`ed it) — so the differential is unchanged.
            // Real OS-thread execution is a VM-only, out-of-oracle path.
            noeta_ir::Rvalue::SpawnIsolate { callee, args, span } => {
                let callee = self.eval_ir_atom(callee, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                let future = self.call(callee, values, *span)?;
                if self.scopes.is_empty() {
                    return Ok(future);
                }
                let scope_idx = self.scopes.len() - 1;
                let task_idx = self.scopes[scope_idx].len();
                self.scopes[scope_idx].push(crate::Task {
                    future,
                    result: None,
                    cancelled: false,
                });
                Ok(Value::Handle(
                    ScopeId::from_index(scope_idx),
                    TaskId::from_index(task_idx),
                ))
            }
            // `channel::<T>(cap)` (isolates I.1): register a new bounded channel and yield its
            // `(Sender, Receiver)` endpoint pair. The message type is checker-only; only the capacity
            // reaches here. A negative capacity is a runtime error (E0010), like other bad arguments.
            noeta_ir::Rvalue::MakeChannel { capacity, span } => {
                let cap = self.eval_ir_atom(capacity, frame)?;
                let Value::Int(cap) = cap else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "`channel` expects an int capacity, found {}",
                            cap.type_name()
                        ),
                    ));
                };
                if cap < 0 {
                    return Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        *span,
                        format!("`channel` capacity must be non-negative, found {cap}"),
                    ));
                }
                let id = self.channels.len();
                self.channels.push(crate::Channel {
                    buffer: std::collections::VecDeque::new(),
                    capacity: cap as usize,
                    closed: false,
                });
                Ok(Value::Tuple(Rc::new(vec![
                    Value::Sender(ChannelId::from_index(id)),
                    Value::Receiver(ChannelId::from_index(id)),
                ])))
            }
            noeta_ir::Rvalue::RolesOf { .. } => Ok(self.materialize_roles()),
            noeta_ir::Rvalue::Invoke {
                recv,
                name,
                args,
                span,
            } => {
                let receiver = self.eval_ir_atom(recv, frame)?;
                let name_val = self.eval_ir_atom(name, frame)?;
                let args_val = self.eval_ir_atom(args, frame)?;
                self.invoke_dynamic(receiver, name_val, args_val, *span)
            }
            noeta_ir::Rvalue::ExtCall {
                module,
                func,
                args,
                recipe,
                span,
            } => {
                let arg_vals: Vec<Value> = args
                    .iter()
                    .map(|a| self.eval_ir_atom(a, frame))
                    .collect::<Result<_, _>>()?;
                // The recipe is required; its absence was already reported by the checker.
                let Some(recipe) = recipe else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{module}.{func}::<T>(...)` has no resolved result type"),
                    ));
                };
                // The only call-site-typed native function today is `json.parse::<T>(text)`.
                if module == "json" && func == "parse" {
                    let Some(Value::Str(text)) = arg_vals.first() else {
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "`json.parse` expects a `string` argument".to_string(),
                        ));
                    };
                    match noeta_stdlib::json::parse_typed(text, recipe) {
                        Ok(out) => self.materialize_recipe(out, *span),
                        Err(error) => Err(self.runtime_error(
                            crate::std_error_code(error.kind),
                            *span,
                            error.message,
                        )),
                    }
                } else {
                    Err(self.runtime_error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!(
                            "`{module}.{func}::<T>(...)` is not a call-site-typed native function"
                        ),
                    ))
                }
            }
        }
    }

    /// Materialize a `json.parse::<T>` result tree ([`noeta_stdlib::NativeOut`]) into a value of `T`.
    /// A struct is built through [`Self::construct_object`] — its real registered definition, so the
    /// instance has its methods/defaults exactly like a literal; the VM builds a matching same-name
    /// shape, so both backends agree.
    fn materialize_recipe(&mut self, out: noeta_stdlib::NativeOut, span: Span) -> Eval<Value> {
        use noeta_stdlib::{NativeOut, Scalar};
        match out {
            NativeOut::Scalar(Scalar::Int(n)) => Ok(Value::Int(n)),
            NativeOut::Scalar(Scalar::Float(f)) => Ok(Value::Float(f)),
            NativeOut::Scalar(Scalar::F32(f)) => Ok(Value::F32(f)),
            NativeOut::Scalar(Scalar::Bool(b)) => Ok(Value::Bool(b)),
            NativeOut::Str(s) => Ok(Value::Str(s)),
            NativeOut::Bytes(b) => Ok(Value::Bytes(Rc::new(b))),
            NativeOut::Unit => Ok(Value::Unit),
            NativeOut::None => Ok(crate::builtin_enum("Option", "none", vec![])),
            NativeOut::Some(inner) => {
                let value = self.materialize_recipe(*inner, span)?;
                Ok(crate::builtin_enum("Option", "some", vec![value]))
            }
            NativeOut::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.materialize_recipe(item, span)?);
                }
                Ok(Value::list(values))
            }
            NativeOut::Map(entries) => {
                let mut map = std::collections::BTreeMap::new();
                for (key, value) in entries {
                    let value = self.materialize_recipe(value, span)?;
                    map.insert(key, value);
                }
                Ok(Value::map_value(Rc::new(map)))
            }
            NativeOut::Struct { name, fields } => {
                let mut field_values = Vec::with_capacity(fields.len());
                for (fname, fout) in fields {
                    let value = self.materialize_recipe(fout, span)?;
                    field_values.push((fname, span, value));
                }
                // A `json.parse::<T>` result carries no reflected tag (R2) — its concrete type is
                // recovered head-only from the shape; untagged.
                self.construct_object(&name, span, field_values, None, None, span)
            }
            // `Object` (shape-from-argument) and `FileHandle` are never produced by a recipe decode.
            NativeOut::Object(_) | NativeOut::FileHandle(_) => {
                unreachable!("json.parse recipe decode never yields an Object/FileHandle result")
            }
        }
    }

    /// In-place list `set` for a marked self-update `xs = xs.set(i, v)` (the `xs[i] = v` desugaring).
    /// The receiver was moved out of its binding by the caller; when uniquely owned its slot `i` is
    /// overwritten in place (the displaced element destroyed now, matching copy-and-reassign), else it
    /// copies. An out-of-range index is E0016. `values` is `[index, value]`.
    fn list_set_in_place(
        &mut self,
        recv: Value,
        mut values: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        let Value::List(repr) = recv else {
            unreachable!("caller checked the receiver is a list")
        };
        // The in-place reuse path operates on a boxed `Rc<Vec<Value>>`. A boxed list passes its `Rc`
        // straight through (so a uniquely-owned list still mutates in place); a packed list (P-PACK
        // 2.3) has no specialized `set` yet, so it materializes to a fresh boxed vector — correct,
        // just not flat. The result is a boxed list either way.
        let mut rc = match repr {
            ListRepr::Boxed { items: rc, .. } => rc,
            ListRepr::Packed(_) => repr.to_rc_vec(),
        };
        let new_value = values.pop().expect("set takes two args");
        let index_value = values.pop().expect("set takes two args");
        let Value::Int(i) = index_value else {
            return self.call_method(
                Value::list_rc(rc),
                "set",
                vec![index_value, new_value],
                span,
            );
        };
        if i < 0 || i as usize >= rc.len() {
            return Err(self.runtime_error(
                DiagnosticCode::IndexOutOfBounds,
                span,
                format!("index {i} out of bounds for list of length {}", rc.len()),
            ));
        }
        let i = i as usize;
        match Rc::get_mut(&mut rc) {
            Some(items) => {
                let old = std::mem::replace(&mut items[i], new_value);
                self.destroy_value(old);
                Ok(Value::list_rc(rc))
            }
            None => {
                let mut new = (*rc).clone();
                new[i] = new_value;
                Ok(Value::list(new))
            }
        }
    }

    /// In-place map `set` for a marked self-update `m = m.set(k, v)` (Phase 5.1c). The receiver has
    /// already been moved out of its binding by the caller. When uniquely owned its backing map is
    /// mutated in place (the displaced value, if any, fires its destructor now — matching the
    /// copy-and-reassign baseline, which releases it when the old map dies); an aliased map copies,
    /// preserving the other owner's view. `values` is `[key, value]`.
    fn map_set_in_place(&mut self, recv: Value, mut values: Vec<Value>, span: Span) -> Eval<Value> {
        let Value::Map(mut rc, _) = recv else {
            unreachable!("caller checked the receiver is a map")
        };
        let new_value = values.pop().expect("set takes two args");
        let key_value = values.pop().expect("set takes two args");
        let Value::Str(key) = key_value else {
            // Defensive: a non-string key cannot occur for a checked map `set`; rebuild via the
            // ordinary path so the error (if any) matches.
            return self.call_method(
                Value::map_value(rc),
                "set",
                vec![key_value, new_value],
                span,
            );
        };
        match Rc::get_mut(&mut rc) {
            Some(map) => {
                if let Some(old) = map.insert(key, new_value) {
                    self.destroy_value(old);
                }
                Ok(Value::map_value(rc))
            }
            None => {
                let mut new = (*rc).clone();
                new.insert(key, new_value);
                Ok(Value::map_value(Rc::new(new)))
            }
        }
    }

    /// In-place map `remove` for a marked self-update `m = m.remove(k)` (Phase 5.1c), the companion to
    /// [`Interpreter::map_set_in_place`]. `values` is `[key]`.
    fn map_remove_in_place(
        &mut self,
        recv: Value,
        mut values: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        let Value::Map(mut rc, _) = recv else {
            unreachable!("caller checked the receiver is a map")
        };
        let key_value = values.pop().expect("remove takes one arg");
        let Value::Str(key) = key_value else {
            return self.call_method(Value::map_value(rc), "remove", vec![key_value], span);
        };
        match Rc::get_mut(&mut rc) {
            Some(map) => {
                if let Some(old) = map.remove(&key) {
                    self.destroy_value(old);
                }
                Ok(Value::map_value(rc))
            }
            None => {
                let mut new = (*rc).clone();
                new.remove(&key);
                Ok(Value::map_value(Rc::new(new)))
            }
        }
    }

    /// In-place set `add` for a marked self-update `s = s.add(x)`, the set analogue of
    /// [`Interpreter::map_set_in_place`]. A uniquely-owned, canonically-ordered set binary-search-
    /// inserts `x` at its sorted position (a no-op if an equal element is already present — the
    /// candidate is discarded, as the copy path's de-duplication discards the duplicate). An
    /// unorderable element (a runtime error) or an aliased set falls back to the ordinary copy path so
    /// the result, and any error message, matches exactly. `values` is `[element]`.
    fn set_add_in_place(&mut self, recv: Value, mut values: Vec<Value>, span: Span) -> Eval<Value> {
        let Value::Set(mut rc, _) = recv else {
            unreachable!("caller checked the receiver is a set")
        };
        let value = values.pop().expect("add takes one arg");
        // A set is homogeneous in its orderability class, so comparing against its first element
        // settles whether `value` is orderable at all.
        let orderable = rc
            .first()
            .is_none_or(|first| compare_primitive(first, &value).is_some());
        if !orderable {
            return self.call_method(Value::set_value(rc), "add", vec![value], span);
        }
        match Rc::get_mut(&mut rc) {
            Some(items) => {
                if let Err(pos) = items.binary_search_by(|item| {
                    compare_primitive(item, &value).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    items.insert(pos, value);
                }
                Ok(Value::set_value(rc))
            }
            None => self.call_method(Value::set_value(rc), "add", vec![value], span),
        }
    }

    /// In-place set `remove` for a marked self-update `s = s.remove(x)`, the companion to
    /// [`Interpreter::set_add_in_place`]. A uniquely-owned set binary-search-removes an element equal
    /// to `x` (its destructor fires now — matching the copy baseline, which releases it when the old
    /// set dies); an unorderable target finds nothing (a no-op, the set is unchanged); an aliased set
    /// copies. `values` is `[element]`.
    fn set_remove_in_place(
        &mut self,
        recv: Value,
        mut values: Vec<Value>,
        span: Span,
    ) -> Eval<Value> {
        let Value::Set(mut rc, _) = recv else {
            unreachable!("caller checked the receiver is a set")
        };
        let target = values.pop().expect("remove takes one arg");
        let orderable = rc
            .first()
            .is_none_or(|first| compare_primitive(first, &target).is_some());
        if !orderable {
            return Ok(Value::set_value(rc));
        }
        match Rc::get_mut(&mut rc) {
            Some(items) => {
                if let Ok(pos) = items.binary_search_by(|item| {
                    compare_primitive(item, &target).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    let old = items.remove(pos);
                    self.destroy_value(old);
                }
                Ok(Value::set_value(rc))
            }
            None => self.call_method(Value::set_value(rc), "remove", vec![target], span),
        }
    }

    /// In-place struct reuse for a marked self-update `acc = Type { ...acc, f: v }` (Phase 5, the
    /// IR-interpreter analogue of the VM's `Op::MakeStructInPlace`). The reuse pass guarantees the
    /// spread base `base_name` is the very binding the result is reassigned to (so moving it out is
    /// sound) and that `Type` has no own `destruct` (so reusing the allocation never skips a
    /// container destructor). The accumulator is moved out of its binding; if it is uniquely owned
    /// (no alias) its field map is mutated in place — only the overridden keys change, each
    /// displacing its old value through `destroy_value` so a replaced field's `destruct` fires at the
    /// right time (spec §4/§5); an aliased base copies, preserving the other owner's view. Both paths
    /// gate on the runtime refcount exactly as the VM does, so the two backends agree.
    #[allow(clippy::too_many_arguments)]
    fn construct_object_reuse(
        &mut self,
        type_name: &str,
        type_name_span: Span,
        fields: &[noeta_ir::ObjectFieldInit],
        base_name: &str,
        reflect: Option<Rc<noeta_ast::reflect::TypeRepr>>,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Value> {
        // The override values are already-computed temps (ANF); read them in source order, matching
        // the normal Object path's field evaluation.
        let mut overrides: Vec<(String, Value)> = Vec::with_capacity(fields.len());
        for f in fields {
            let value = self.eval_ir_atom(&f.value, frame)?;
            overrides.push((f.name.clone(), value));
        }
        // Move the accumulator out of its (mutable, being-reassigned) binding.
        match self.scope.take_mut(base_name) {
            Some(Value::Object(rc)) if rc.def.name() == type_name && !rc.def.opaque => {
                let def = Rc::clone(&rc.def);
                // A functional update `x = T { ...x, f: v }` is a **rebind** for both kinds (a new
                // value bound to `x`); reuse is a pure optimization gated on unique ownership — when
                // `x` has no other alias, mutating its allocation in place is unobservable. (Distinct
                // from the in-place `x.f = v` class mutation in `set_field_in_place`.)
                if Rc::strong_count(&rc) == 1 {
                    // Reuse keeps the accumulator's existing reflected type (R2) — a self-update
                    // rebuilds a value of the same (generic) type, matching the VM's reuse path.
                    for (name, value) in overrides {
                        if let Some(old) = rc.set_field_value(&name, value) {
                            self.destroy_value(old);
                        }
                    }
                    return Ok(Value::Object(rc));
                }
                // Aliased: copy, preserving the alias's view. The displaced fields live in `rc`,
                // whose `Rc` drops at the end of this statement (releasing the scope's old
                // reference); the alias keeps the original object. The fresh copy carries the
                // literal's reflected type (R2), matching the VM's copy branch.
                let mut new_slots = rc.fields_snapshot();
                for (name, value) in overrides {
                    if let Some(i) = rc.slot_of(&name) {
                        new_slots[i] = value;
                    }
                }
                Ok(Value::Object(Rc::new(crate::ObjectValue::new_reflected(
                    def, new_slots, reflect,
                ))))
            }
            // Defensive: the taken value is not a matching object (cannot happen for a check-clean
            // self-update). Rebuild via the ordinary constructor with it as the spread base — the
            // copying path the VM also falls back to on a shape mismatch.
            other => {
                let spread = other.map(|v| (v, span));
                let field_values = overrides
                    .into_iter()
                    .map(|(name, value)| (name, span, value))
                    .collect();
                self.construct_object(
                    type_name,
                    type_name_span,
                    field_values,
                    spread,
                    reflect,
                    span,
                )
            }
        }
    }

    /// Set field `field` of an object to `new_value` (`x.f = v`, object-model slice 2b). Semantics
    /// are **kind-dependent**: a reference `class` mutates the shared instance **in place** — through
    /// the `RefCell`, regardless of refcount — so the change is visible through every alias; a value
    /// `struct` keeps copy-on-write — it mutates in place only when uniquely owned, else copies so
    /// the other owner keeps its view. Either way the displaced old value fires its destructor now.
    /// The IR-interpreter analogue of the VM's `Op::SetField`, agreeing with it by construction.
    fn set_field_in_place(
        &mut self,
        recv: Value,
        field: &str,
        new_value: Value,
        span: Span,
    ) -> Eval<Value> {
        match recv {
            Value::Object(rc) => {
                if !rc.has_field_value(field) {
                    return Err(self.runtime_error(
                        DiagnosticCode::UnknownName,
                        span,
                        format!("type `{}` has no field `{field}`", rc.def.name()),
                    ));
                }
                // A class always mutates in place (reference semantics); a uniquely-owned struct may
                // too (no alias can observe it). Both go through the shared `RefCell`.
                if !rc.def.is_struct || Rc::strong_count(&rc) == 1 {
                    if let Some(old) = rc.set_field_value(field, new_value) {
                        self.destroy_value(old);
                    }
                    // A class field-set can close a reference cycle (`a.next = b; b.next = a`);
                    // register the receiver so the exit reaper can reclaim it (slice 2c).
                    if !rc.def.is_struct {
                        crate::register_mutated_object(&rc);
                    }
                    return Ok(Value::Object(rc));
                }
                // Aliased struct: copy with the field updated, preserving the other owner's view.
                let mut new_slots = rc.fields_snapshot();
                if let Some(i) = rc.slot_of(field) {
                    new_slots[i] = new_value;
                }
                Ok(Value::Object(Rc::new(crate::ObjectValue::new(
                    Rc::clone(&rc.def),
                    new_slots,
                ))))
            }
            other => Err(self.runtime_error(
                DiagnosticCode::UnknownName,
                span,
                format!("cannot assign field `{field}` on {}", other.type_name()),
            )),
        }
    }

    /// Resolve a list of atoms to values, left-to-right.
    fn eval_ir_atoms(&mut self, atoms: &[noeta_ir::Atom], frame: &mut Frame) -> Eval<Vec<Value>> {
        let mut values = Vec::with_capacity(atoms.len());
        for atom in atoms {
            values.push(self.eval_ir_atom(atom, frame)?);
        }
        Ok(values)
    }

    /// Evaluate `left && right` / `left || right` with the tree-walker's exact laziness and
    /// bool-operand checks. The `left` atom is already resolved; the `right` block runs only
    /// when `left` does not short-circuit.
    fn eval_ir_logical(
        &mut self,
        op: noeta_ir::BinaryOp,
        left: &noeta_ir::Atom,
        right: &noeta_ir::Block,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Value> {
        use noeta_ir::BinaryOp;
        let left = self.eval_ir_atom(left, frame)?;
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
            _ => unreachable!("eval_ir_logical only handles && and ||"),
        };
        if short_circuit {
            return Ok(Value::Bool(left));
        }
        let right_value = self.eval_ir_block_value(right, frame, span)?;
        match right_value {
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

    /// Run a value-position block (its statements, then its tail atom) in the current scope,
    /// returning the tail value. Used for the lazy right operand of `&&`/`||` and (later) for
    /// other expression-position blocks.
    fn eval_ir_block_value(
        &mut self,
        block: &noeta_ir::Block,
        frame: &mut Frame,
        span: Span,
    ) -> Eval<Value> {
        match self.exec_ir_stmts(&block.stmts, frame)? {
            Flow::Normal => {}
            // A value-position block contains no loop or function boundary of its own, so a
            // non-local flow can only have propagated from a nested construct; it is carried by
            // the `Eval` error channel (`?`/return), never as a `Flow` here.
            Flow::Return(_) | Flow::Break | Flow::Continue => {
                unreachable!("value-position block produced non-local flow (lowering bug)")
            }
        }
        match &block.tail {
            Some(atom) => self.eval_ir_atom(atom, frame),
            None => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                "value-position block has no tail expression (lowering bug)".to_string(),
            )),
        }
    }
}

/// Materialize a literal IR constant as a runtime value.
fn const_value(c: &noeta_ir::Const) -> Value {
    match c {
        noeta_ir::Const::Unit => Value::Unit,
        noeta_ir::Const::Bool(b) => Value::Bool(*b),
        noeta_ir::Const::Int(i) => Value::Int(*i),
        noeta_ir::Const::Float(f) => Value::Float(*f),
        noeta_ir::Const::F32(f) => Value::F32(*f),
        noeta_ir::Const::Str(s) => Value::Str(s.clone()),
    }
}
