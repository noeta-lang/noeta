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

use crate::{Eval, Flow, Interpreter, RunResult, TreeWalkBackend, Unwind, Value};

/// The flat temporary store for one function activation (or the top level). Indexed by
/// [`lang_ir::Temp`]; a slot is `None` until its defining `let` runs.
struct Frame {
    temps: Vec<Option<Value>>,
}

impl Frame {
    fn new(count: u32) -> Frame {
        Frame {
            temps: vec![None; count as usize],
        }
    }

    /// Read a temporary. A read before its write is a lowering bug, not a user error.
    fn get(&self, t: lang_ir::Temp) -> Value {
        self.temps[t.index()]
            .clone()
            .expect("Core-IR temporary read before it was written (lowering bug)")
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
        let exit_code = if self.diagnostics.is_empty() { 0 } else { 1 };
        RunResult {
            stdout: self.stdout,
            exit_code,
            diagnostics: self.diagnostics,
        }
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
            // Lowered only in later slices; the lowering gates them out until then.
            other => unreachable!("Core-IR statement not yet interpreted: {other:?}"),
        }
    }

    /// Resolve an atom to a value: a constant, a frame temporary, or a lexical lookup. The
    /// `Var` not-found path reproduces the AST walker's `Ident` diagnostic exactly.
    fn eval_ir_atom(&mut self, atom: &lang_ir::Atom, frame: &Frame) -> Eval<Value> {
        match atom {
            lang_ir::Atom::Const(c) => Ok(const_value(c)),
            lang_ir::Atom::Temp(t) => Ok(frame.get(*t)),
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
            lang_ir::Rvalue::Binary { op, lhs, rhs, span } => {
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
                let receiver = self.eval_ir_atom(receiver, frame)?;
                let index = self.eval_ir_atom(index, frame)?;
                self.eval_index(receiver, index, *span)
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
            // Lowered only in later slices.
            other => unreachable!("Core-IR rvalue not yet interpreted: {other:?}"),
        }
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
