//! The **Core-IR tree-interpreter** — a second evaluation path that walks the lowered
//! [`lang_ir`] instead of the AST, while reusing the AST walker's exact value model, scope
//! model, and leaf semantics.
//!
//! # Why it shares the tree-walker's machinery
//!
//! This interpreter is the faithfulness reference for the IR: it must produce byte-identical
//! [`RunResult`]s to [`super::Interpreter::run_with_sites`] on every program. The cheapest
//! way to *guarantee* that is to reuse the same code for everything below the orchestration
//! layer — operator semantics (`apply_binary_op`, `eval_unary`), indexing (`eval_index`),
//! display (`display_value`), method dispatch, object/enum construction, the leak-counted
//! [`Value`] model, the lexical [`Scope`], and end-of-program destruction
//! (`destroy_globals`). Only the *orchestration* differs: where the AST walker recursively
//! evaluates nested expressions, this interpreter reads pre-computed **atoms** and walks the
//! `let`-sequenced [`lang_ir::Stmt`]s.
//!
//! # Two storage classes
//!
//! Source variables live in [`Scope`] exactly as before (so captures, reassignment, and
//! `destroy_globals` are unchanged). ANF temporaries live in a [`Frame`] — a flat per-
//! activation store that drops at activation end. Because destructors fire **only** at
//! global teardown (never on a local or temporary drop), a temporary's lifetime is invisible
//! to observable behavior, so the `Frame` model needs no last-use analysis to stay faithful
//! in this phase.

use std::collections::BTreeMap;
use std::rc::Rc;

use lang_ast::Program;
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::Span;

use crate::{
    Closure, DefaultThunk, Eval, FieldSpec, Flow, FnBody, Interpreter, RunResult, TreeWalkBackend,
    TypeDef, Unwind, Value, compare_primitive,
};

/// The flat temporary store for one function activation (or the top level). Indexed by
/// [`lang_ir::Temp`]; a slot is `None` until its defining `let` runs.
/// Whether an atom is an ANF temporary (vs a named source variable or a constant). A temp receiver
/// is *owned* — single-use by the ANF invariant, holding no live binding — so after an access
/// consumes it its destructor must fire at last use (Phase 4.4). A `Var` receiver is borrowed (its
/// binding fires at its own drop), so it is left alone.
fn is_temp(atom: &lang_ir::Atom) -> bool {
    matches!(atom, lang_ir::Atom::Temp(_))
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
    fn take(&mut self, t: lang_ir::Temp) -> Value {
        self.temps[t.index()]
            .take()
            .expect("Core-IR temporary read before write or read twice (lowering bug)")
    }

    fn set(&mut self, t: lang_ir::Temp, value: Value) {
        self.temps[t.index()] = Some(value);
    }
}

impl TreeWalkBackend {
    /// Run a program through the Core-IR interpreter. `ast` supplies the reflection manifest
    /// (built identically to the AST walker, so attributes/roles/type facts match);
    /// `ir` is the lowered program to execute; `type_of_sites` is the checker's `type_of`
    /// map, threaded exactly as for the AST path.
    pub fn run_ir(
        &self,
        ast: &Program,
        ir: &lang_ir::Program,
        type_of_sites: std::collections::HashMap<Span, lang_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::new(self.seed).run_ir(ast, ir, type_of_sites)
    }

    /// As [`TreeWalkBackend::run_ir`], but against a caller-provided [`lang_stdlib::Host`]
    /// (the real host) instead of the deterministic sandbox — the IR analogue of
    /// [`TreeWalkBackend::run_with_host_sites`]. `lang run` uses this so its user-facing
    /// execution goes through the same Core-IR reference (with last-use destruction) the
    /// conformance oracle pins, rather than the superseded AST-walk path.
    pub fn run_ir_with_host(
        &self,
        ast: &Program,
        ir: &lang_ir::Program,
        host: Box<dyn lang_stdlib::Host>,
        type_of_sites: std::collections::HashMap<Span, lang_ast::reflect::TypeRepr>,
    ) -> RunResult {
        Interpreter::with_host(self.seed, host).run_ir(ast, ir, type_of_sites)
    }
}

impl Interpreter {
    /// Execute a lowered program, mirroring [`Interpreter::run_with_sites`]: build the
    /// reflection manifest, run the top-level statements in the global scope, then destroy
    /// the global bindings in reverse declaration order.
    fn run_ir(
        mut self,
        ast: &Program,
        ir: &lang_ir::Program,
        type_of_sites: std::collections::HashMap<Span, lang_ast::reflect::TypeRepr>,
    ) -> RunResult {
        self.reflection = lang_ast::reflect::build(ast);
        self.type_of_sites = type_of_sites;
        let mut frame = Frame::new(ir.temp_count);
        // The top-level statements run directly in the global scope (no child), exactly as
        // `run_with_sites` runs `program.stmts` in `self.scope`.
        match self.exec_ir_stmts(&ir.top.stmts, &mut frame) {
            Ok(Flow::Normal) | Ok(Flow::Break) | Ok(Flow::Continue) => {}
            // A top-level `return`, a `?` short-circuit, or a runtime error stops the program.
            Ok(Flow::Return(_)) | Err(Unwind::Return(_)) | Err(Unwind::Abort) => {}
        }
        self.destroy_globals();
        // Reap any closure-capture cycle left after teardown (Phase 6.3), so residency reaches 0.
        self.reap_captured_scope_cycles();
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
    pub(crate) fn run_ir_batch(&mut self, ir: &lang_ir::Program) -> Eval<Flow> {
        let mut frame = Frame::new(ir.temp_count);
        self.exec_ir_stmts(&ir.top.stmts, &mut frame)
    }

    /// Execute a statement sequence in the current scope, stopping at the first non-local
    /// flow (`return`/`break`/`continue`) — the IR analogue of `exec_stmts`.
    fn exec_ir_stmts(&mut self, stmts: &[lang_ir::Stmt], frame: &mut Frame) -> Eval<Flow> {
        for stmt in stmts {
            let flow = self.exec_ir_stmt(stmt, frame)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_ir_stmt(&mut self, stmt: &lang_ir::Stmt, frame: &mut Frame) -> Eval<Flow> {
        match stmt {
            lang_ir::Stmt::Let { dst, rvalue, .. } => {
                let value = self.eval_ir_rvalue(rvalue, frame)?;
                frame.set(*dst, value);
                Ok(Flow::Normal)
            }
            lang_ir::Stmt::Eval { rvalue, .. } => {
                self.eval_ir_rvalue(rvalue, frame)?;
                Ok(Flow::Normal)
            }
            lang_ir::Stmt::Bind {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => {
                let value = self.eval_ir_atom(value, frame)?;
                self.bind(*mut_decl, name, *name_span, value)?;
                Ok(Flow::Normal)
            }
            lang_ir::Stmt::Echo { value, span } => {
                let v = self.eval_ir_atom(value, frame)?;
                let text = self.display_value(&v, *span)?;
                self.stdout.push_str(&text);
                self.stdout.push('\n');
                Ok(Flow::Normal)
            }
            lang_ir::Stmt::Logical {
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
            lang_ir::Stmt::Return { value, .. } => {
                let value = match value {
                    Some(atom) => self.eval_ir_atom(atom, frame)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(value))
            }
            lang_ir::Stmt::Break { .. } => Ok(Flow::Break),
            lang_ir::Stmt::Continue { .. } => Ok(Flow::Continue),
            lang_ir::Stmt::If {
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
            lang_ir::Stmt::While {
                cond, body, span, ..
            } => self.exec_ir_while(cond, body, *span, frame),
            lang_ir::Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => self.exec_ir_for(pattern, iterable, body, *span, frame),
            lang_ir::Stmt::Match {
                scrutinee,
                arms,
                dst,
                span,
            } => {
                let value = self.eval_ir_atom(scrutinee, frame)?;
                self.exec_ir_match(value, arms, *dst, *span, frame)
            }
            lang_ir::Stmt::Decl(decl) => {
                self.exec_ir_decl(decl);
                Ok(Flow::Normal)
            }
            lang_ir::Stmt::Drop(t) => {
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
            lang_ir::Stmt::DropVar { name, relevant, .. } => {
                if *relevant {
                    if let Some(value) = self.scope.take_for_drop(name) {
                        self.destroy_value(value);
                    }
                } else {
                    self.scope.release_binding(name);
                }
                Ok(Flow::Normal)
            }
            lang_ir::Stmt::Coalesce {
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
        arms: &[lang_ir::Arm],
        dst: Option<lang_ir::Temp>,
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

    /// Register a declaration. `fn`/`class` build IR-bodied closures; `enum`/`record`/`use`
    /// carry no executable body, so they reuse the tree-walker's registration unchanged.
    fn exec_ir_decl(&mut self, decl: &lang_ir::Decl) {
        match decl {
            lang_ir::Decl::Fn { name, func, .. } => {
                let closure = self.make_ir_closure(func);
                self.scope
                    .declare(name.clone(), Value::Function(Rc::new(closure)), false);
            }
            lang_ir::Decl::Class(class) => self.declare_ir_class(class),
            lang_ir::Decl::Enum(decl) => self.declare_enum(decl),
            lang_ir::Decl::Record(decl) => self.declare_record(decl),
            lang_ir::Decl::Use { path, names, .. } => self.declare_use(path, names),
        }
    }

    /// Build a closure value from a lowered IR function template, capturing the current
    /// lexical scope — the IR analogue of `declare_fn`'s/`Expr::Closure`'s construction.
    fn make_ir_closure(&self, func: &Rc<lang_ir::Func>) -> Closure {
        Closure::new(
            func.params.clone(),
            func.defaults
                .iter()
                .map(|d| d.clone().map(DefaultThunk::Ir))
                .collect(),
            FnBody::Ir(Rc::clone(func)),
            Rc::clone(&self.scope),
        )
    }

    /// Register a class whose methods are IR-bodied closures. Mirrors `declare_class`: fields,
    /// derives, and the (still-surface) destructor come from the carried declaration; the
    /// methods are the lowered IR funcs.
    fn declare_ir_class(&mut self, class: &lang_ir::ClassDef) {
        let decl = &class.decl;
        let fields = decl
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.name.clone(),
                mutable: f.mut_field,
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
            destructor: decl.destructor.clone().map(Rc::new),
            is_record: false,
            // A hand-written `compare`/`to_json` takes precedence over derivation — the same
            // rule `declare_class` applies.
            derives_comparable: lang_ast::derives_trait(&decl.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            derives_tojson: lang_ast::derives_trait(&decl.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            opaque: false,
        };
        self.scope
            .declare(decl.name.clone(), Value::Type(Rc::new(def)), false);
    }

    /// Run a lowered function body as a call: allocate its temporary frame, run its
    /// statements, and yield the explicit `return` value, else the arrow tail, else unit.
    /// Mirrors `exec_fn_body` (block) and arrow-body evaluation. Called from the shared call
    /// machinery (`call_closure`/`call_method_on`) when a closure has an IR body.
    pub(crate) fn exec_ir_fn_body(&mut self, func: &lang_ir::Func) -> Eval<Value> {
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
    pub(crate) fn exec_ir_thunk(&mut self, thunk: &lang_ir::Thunk) -> Eval<Value> {
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
    fn exec_ir_block_scoped(&mut self, block: &lang_ir::Block, frame: &mut Frame) -> Eval<Flow> {
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
        cond: &lang_ir::Block,
        body: &lang_ir::Block,
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
        pattern: &lang_ir::ForPattern,
        iterable: &lang_ir::Atom,
        body: &lang_ir::Block,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Flow> {
        let iterable_value = self.eval_ir_atom(iterable, frame)?;
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
    fn eval_ir_atom(&mut self, atom: &lang_ir::Atom, frame: &mut Frame) -> Eval<Value> {
        match atom {
            lang_ir::Atom::Const(c) => Ok(const_value(c)),
            lang_ir::Atom::Temp(t) => Ok(frame.take(*t)),
            lang_ir::Atom::Var { name, span } => match self.scope.lookup(name) {
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
        }
    }

    /// Compute a primitive operation over already-resolved atoms. Each arm mirrors the
    /// matching `eval_expr` arm, delegating to the shared leaf helpers so behavior is
    /// identical by construction.
    fn eval_ir_rvalue(&mut self, rvalue: &lang_ir::Rvalue, frame: &mut Frame) -> Eval<Value> {
        match rvalue {
            lang_ir::Rvalue::Use(atom) => self.eval_ir_atom(atom, frame),
            lang_ir::Rvalue::Unary { op, operand, span } => {
                let value = self.eval_ir_atom(operand, frame)?;
                self.eval_unary(*op, value, *span)
            }
            lang_ir::Rvalue::Binary {
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
                    && *op == lang_ir::BinaryOp::Concat
                    && let lang_ir::Atom::Var { name, .. } = lhs
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
            lang_ir::Rvalue::List { items, .. } => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_ir_atom(item, frame)?);
                }
                Ok(Value::List(Rc::new(values)))
            }
            lang_ir::Rvalue::Range {
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
            lang_ir::Rvalue::Map { entries, span } => {
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
                Ok(Value::Map(Rc::new(map)))
            }
            lang_ir::Rvalue::Index {
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
            lang_ir::Rvalue::Interp { parts, .. } => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        lang_ir::InterpPart::Literal(text) => out.push_str(text),
                        lang_ir::InterpPart::Hole { atom, span } => {
                            let v = self.eval_ir_atom(atom, frame)?;
                            out.push_str(&self.display_value(&v, *span)?);
                        }
                    }
                }
                Ok(Value::Str(out))
            }
            lang_ir::Rvalue::Call { callee, args, span } => {
                let callee = self.eval_ir_atom(callee, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                self.call(callee, values, *span)
            }
            lang_ir::Rvalue::Method {
                receiver,
                name,
                args,
                reuse,
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
                    && let lang_ir::Atom::Var {
                        name: recv_name, ..
                    } = receiver
                {
                    let values = self.eval_ir_atoms(args, frame)?;
                    let recv = match self.scope.take_mut(recv_name) {
                        Some(v) => v,
                        None => self.eval_ir_atom(receiver, frame)?,
                    };
                    if matches!(&recv, Value::Map(_)) {
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
                    if matches!(&recv, Value::Set(_)) && values.len() == 1 {
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
                if is_temp(receiver) {
                    // An owned temp receiver (`Resource.new().use()`): fire its destructor after the
                    // call (Phase 4.4). `call_method` consumes the receiver, so clone for the call
                    // and destroy the held copy — last-reference-gated, so a method that returns
                    // `self` (the result aliases it) correctly defers destruction.
                    let result = self.call_method(recv.clone(), name, values, *span);
                    self.destroy_value(recv);
                    result
                } else {
                    self.call_method(recv, name, values, *span)
                }
            }
            lang_ir::Rvalue::Field {
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
                let result = match &recv {
                    Value::EnumType(def) => self.make_variant(def, name, vec![], *span),
                    Value::Object(object) => match object.fields.get(name) {
                        Some(value) => Ok(value.clone()),
                        None => Err(self.runtime_error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("type `{}` has no field `{name}`", object.def.name()),
                        )),
                    },
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
                };
                if is_temp(receiver) {
                    self.destroy_value(recv);
                }
                result
            }
            lang_ir::Rvalue::SetField {
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
                    && let lang_ir::Atom::Var {
                        name: recv_name, ..
                    } = receiver
                    && let Some(recv) = self.scope.take_mut(recv_name)
                {
                    return self.set_field_in_place(recv, name, new_value, *span);
                }
                let recv = self.eval_ir_atom(receiver, frame)?;
                self.set_field_in_place(recv, name, new_value, *span)
            }
            lang_ir::Rvalue::Try {
                operand,
                on_error,
                span,
            } => {
                let value = self.eval_ir_atom(operand, frame)?;
                self.eval_try_ir(value, on_error, *span)
            }
            lang_ir::Rvalue::As { operand, ty, .. } => {
                let value = self.eval_ir_atom(operand, frame)?;
                if crate::runtime_matches(&value, ty) {
                    Ok(crate::builtin_enum("Option", "some", vec![value]))
                } else {
                    Ok(crate::builtin_enum("Option", "none", vec![]))
                }
            }
            lang_ir::Rvalue::TypeTest { operand, ty, .. } => {
                let value = self.eval_ir_atom(operand, frame)?;
                Ok(Value::Bool(crate::runtime_matches(&value, ty)))
            }
            lang_ir::Rvalue::TypeOf { operand, span } => {
                let v = self.eval_ir_atom(operand, frame)?;
                match self.type_of_sites.get(span) {
                    Some(repr) => Ok(crate::build_type_value(repr)),
                    None => Ok(crate::build_type_value(&crate::eval_type_repr(&v))),
                }
            }
            lang_ir::Rvalue::AttributesOf { ty, .. } => {
                let type_name = match ty {
                    lang_ir::TypeRef::Named { name, .. } => name.as_str(),
                    _ => "",
                };
                Ok(self.materialize_attributes(type_name))
            }
            lang_ir::Rvalue::Object {
                type_name,
                type_name_span,
                fields,
                spread,
                reuse,
                span,
            } => {
                // In-place reuse (Phase 5): a marked self-update `acc = Type { ...acc, … }` moves the
                // accumulator out of its (reassigned) binding and mutates it in place when uniquely
                // owned, mirroring the VM's `MakeRecordInPlace`. The token guarantees the spread is the
                // reassigned base; both backends gate on the runtime refcount so an alias copies.
                if *reuse && let Some((lang_ir::Atom::Var { name, .. }, _)) = spread {
                    return self.construct_object_reuse(
                        type_name,
                        *type_name_span,
                        fields,
                        name,
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
                self.construct_object(type_name, *type_name_span, field_values, spread, *span)
            }
            lang_ir::Rvalue::Closure { func, .. } => {
                Ok(Value::Function(Rc::new(self.make_ir_closure(func))))
            }
            lang_ir::Rvalue::RolesOf { .. } => Ok(self.materialize_roles()),
            lang_ir::Rvalue::Invoke {
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
        let Value::List(mut rc) = recv else {
            unreachable!("caller checked the receiver is a list")
        };
        let new_value = values.pop().expect("set takes two args");
        let index_value = values.pop().expect("set takes two args");
        let Value::Int(i) = index_value else {
            return self.call_method(Value::List(rc), "set", vec![index_value, new_value], span);
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
                Ok(Value::List(rc))
            }
            None => {
                let mut new = (*rc).clone();
                new[i] = new_value;
                Ok(Value::List(Rc::new(new)))
            }
        }
    }

    /// In-place map `set` for a marked self-update `m = m.set(k, v)` (Phase 5.1c). The receiver has
    /// already been moved out of its binding by the caller. When uniquely owned its backing map is
    /// mutated in place (the displaced value, if any, fires its destructor now — matching the
    /// copy-and-reassign baseline, which releases it when the old map dies); an aliased map copies,
    /// preserving the other owner's view. `values` is `[key, value]`.
    fn map_set_in_place(&mut self, recv: Value, mut values: Vec<Value>, span: Span) -> Eval<Value> {
        let Value::Map(mut rc) = recv else {
            unreachable!("caller checked the receiver is a map")
        };
        let new_value = values.pop().expect("set takes two args");
        let key_value = values.pop().expect("set takes two args");
        let Value::Str(key) = key_value else {
            // Defensive: a non-string key cannot occur for a checked map `set`; rebuild via the
            // ordinary path so the error (if any) matches.
            return self.call_method(Value::Map(rc), "set", vec![key_value, new_value], span);
        };
        match Rc::get_mut(&mut rc) {
            Some(map) => {
                if let Some(old) = map.insert(key, new_value) {
                    self.destroy_value(old);
                }
                Ok(Value::Map(rc))
            }
            None => {
                let mut new = (*rc).clone();
                new.insert(key, new_value);
                Ok(Value::Map(Rc::new(new)))
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
        let Value::Map(mut rc) = recv else {
            unreachable!("caller checked the receiver is a map")
        };
        let key_value = values.pop().expect("remove takes one arg");
        let Value::Str(key) = key_value else {
            return self.call_method(Value::Map(rc), "remove", vec![key_value], span);
        };
        match Rc::get_mut(&mut rc) {
            Some(map) => {
                if let Some(old) = map.remove(&key) {
                    self.destroy_value(old);
                }
                Ok(Value::Map(rc))
            }
            None => {
                let mut new = (*rc).clone();
                new.remove(&key);
                Ok(Value::Map(Rc::new(new)))
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
        let Value::Set(mut rc) = recv else {
            unreachable!("caller checked the receiver is a set")
        };
        let value = values.pop().expect("add takes one arg");
        // A set is homogeneous in its orderability class, so comparing against its first element
        // settles whether `value` is orderable at all.
        let orderable = rc
            .first()
            .is_none_or(|first| compare_primitive(first, &value).is_some());
        if !orderable {
            return self.call_method(Value::Set(rc), "add", vec![value], span);
        }
        match Rc::get_mut(&mut rc) {
            Some(items) => {
                if let Err(pos) = items.binary_search_by(|item| {
                    compare_primitive(item, &value).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    items.insert(pos, value);
                }
                Ok(Value::Set(rc))
            }
            None => self.call_method(Value::Set(rc), "add", vec![value], span),
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
        let Value::Set(mut rc) = recv else {
            unreachable!("caller checked the receiver is a set")
        };
        let target = values.pop().expect("remove takes one arg");
        let orderable = rc
            .first()
            .is_none_or(|first| compare_primitive(first, &target).is_some());
        if !orderable {
            return Ok(Value::Set(rc));
        }
        match Rc::get_mut(&mut rc) {
            Some(items) => {
                if let Ok(pos) = items.binary_search_by(|item| {
                    compare_primitive(item, &target).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    let old = items.remove(pos);
                    self.destroy_value(old);
                }
                Ok(Value::Set(rc))
            }
            None => self.call_method(Value::Set(rc), "remove", vec![target], span),
        }
    }

    /// In-place record reuse for a marked self-update `acc = Type { ...acc, f: v }` (Phase 5, the
    /// IR-interpreter analogue of the VM's `Op::MakeRecordInPlace`). The reuse pass guarantees the
    /// spread base `base_name` is the very binding the result is reassigned to (so moving it out is
    /// sound) and that `Type` has no own `destruct` (so reusing the allocation never skips a
    /// container destructor). The accumulator is moved out of its binding; if it is uniquely owned
    /// (no alias) its field map is mutated in place — only the overridden keys change, each
    /// displacing its old value through `destroy_value` so a replaced field's `destruct` fires at the
    /// right time (spec §4/§5); an aliased base copies, preserving the other owner's view. Both paths
    /// gate on the runtime refcount exactly as the VM does, so the two backends agree.
    fn construct_object_reuse(
        &mut self,
        type_name: &str,
        type_name_span: Span,
        fields: &[lang_ir::ObjectFieldInit],
        base_name: &str,
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
            Some(Value::Object(mut rc)) if rc.def.name() == type_name && !rc.def.opaque => {
                let def = Rc::clone(&rc.def);
                // We hold the sole scope reference now; unique iff no other live alias.
                if Rc::strong_count(&rc) == 1
                    && let Some(obj) = Rc::get_mut(&mut rc)
                {
                    for (name, value) in overrides {
                        if let Some(old) = obj.fields.insert(name, value) {
                            self.destroy_value(old);
                        }
                    }
                    return Ok(Value::Object(rc));
                }
                // Aliased (or a `get_mut` miss): copy, preserving the alias's view. The displaced
                // fields live in `rc`, whose `Rc` drops at the end of this statement (releasing the
                // scope's old reference); the alias keeps the original object.
                let mut new_fields = rc.fields.clone();
                for (name, value) in overrides {
                    new_fields.insert(name, value);
                }
                Ok(Value::Object(Rc::new(crate::ObjectValue::new(
                    def, new_fields,
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
                self.construct_object(type_name, type_name_span, field_values, spread, span)
            }
        }
    }

    /// Set field `field` of an object to `new_value` with **value semantics** (`x.f = v`, Phase 5.2):
    /// a uniquely-owned instance is mutated in place (the displaced old value fires its destructor now,
    /// matching the copy baseline which would release it when the old object dies); an aliased instance
    /// is copied with the field updated, so the other owner keeps its view. The IR-interpreter analogue
    /// of the VM's `Op::SetField`; gated on the runtime refcount exactly as the VM is, so they agree.
    fn set_field_in_place(
        &mut self,
        recv: Value,
        field: &str,
        new_value: Value,
        span: Span,
    ) -> Eval<Value> {
        match recv {
            Value::Object(mut rc) => {
                if !rc.fields.contains_key(field) {
                    return Err(self.runtime_error(
                        DiagnosticCode::UnknownName,
                        span,
                        format!("type `{}` has no field `{field}`", rc.def.name()),
                    ));
                }
                if Rc::strong_count(&rc) == 1
                    && let Some(obj) = Rc::get_mut(&mut rc)
                {
                    if let Some(old) = obj.fields.insert(field.to_string(), new_value) {
                        self.destroy_value(old);
                    }
                    return Ok(Value::Object(rc));
                }
                let mut new_fields = rc.fields.clone();
                new_fields.insert(field.to_string(), new_value);
                Ok(Value::Object(Rc::new(crate::ObjectValue::new(
                    Rc::clone(&rc.def),
                    new_fields,
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
    fn eval_ir_atoms(&mut self, atoms: &[lang_ir::Atom], frame: &mut Frame) -> Eval<Vec<Value>> {
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
        op: lang_ir::BinaryOp,
        left: &lang_ir::Atom,
        right: &lang_ir::Block,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Value> {
        use lang_ir::BinaryOp;
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
        block: &lang_ir::Block,
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
fn const_value(c: &lang_ir::Const) -> Value {
    match c {
        lang_ir::Const::Unit => Value::Unit,
        lang_ir::Const::Bool(b) => Value::Bool(*b),
        lang_ir::Const::Int(i) => Value::Int(*i),
        lang_ir::Const::Float(f) => Value::Float(*f),
        lang_ir::Const::Str(s) => Value::Str(s.clone()),
    }
}
