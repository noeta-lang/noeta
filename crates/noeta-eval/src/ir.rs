//! The **Core-IR tree-interpreter** — the reference evaluation backend. It walks the lowered
//! [`noeta_ir`] (not the surface AST: the original AST walker was retired once it was neither a
//! production path nor the differential oracle — the oracle is this interpreter vs. the bytecode
//! VM), and is what the conformance reference executes. Production (`noeta run`) is the VM;
//! nothing in the shipped toolchain runs this path.
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
//! activation store that drops at activation end. Last-use destruction is real (the RC
//! migration's whole point): the interpreter honors the IR's inserted `drop`s, so destructors
//! fire at last use exactly as the VM's do — the differential asserts the two agree. A
//! temporary's `Frame` slot needs no separate last-use analysis because the drops are already
//! explicit in the lowered IR it walks.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use noeta_ast::Program;
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

use crate::{
    ChannelId, Closure, EnumDef, Eval, FieldSpec, Flow, Interpreter, IrRefBackend, IterState,
    ListRepr, PackedList, RunResult, ScopeId, TaskId, TypeDef, Unwind, Value, VariantInfo,
    compare_primitive,
};

/// The outcome of materializing one recipe node: a built value, or a validation
/// rejection carrying the path-rich [`noeta_stdlib::json::JsonError`] the failing `Validate::validate`
/// produced. A rejection propagates up through containers until a `Result`-wrapped door recovers it
/// into a `Result.Err` or the aborting door raises it — so `validate` fires bottom-up.
enum MatOut {
    Value(Value),
    Rejected(noeta_stdlib::json::JsonError),
}

/// The `@derive(Deserialize<Json>)` decode registry (L2.2 DI) an [`Interpreter`] runs
/// `json.decode_typed` against — a runtime type-name → recipe map lifted from
/// `noeta_check::Sites::deserialize_recipes`. Threaded through the `run_ir` entry points beside
/// `type_of_sites`, so the reference backend decodes by runtime type identically to the VM.
pub type DeserializeRecipes = std::collections::HashMap<String, noeta_stdlib::TypeRecipe>;

/// Every `@packed` struct's flat layout by type name (native type-declaration unification),
/// lifted from `noeta_check::Sites::packed_type_layouts`. Threaded through the `run_ir` entry points
/// beside `deserialize_recipes` so the from-scratch producer [`NativeCtx::make_packed`] can resolve a
/// produced `List<packed>`'s element schema BY (qualified) name — the tree-walker twin of the VM's
/// interned `packed_schemas` by-name scan. Absent (empty) on the checkerless REPL session path, where
/// no `@packed` layout is known.
pub type PackedTypeLayouts = std::collections::HashMap<String, noeta_ast::reflect::PackedLayout>;

/// The flat temporary store for one function activation (or the top level). Indexed by
/// [`noeta_ir::Temp`]; a slot is `None` until its defining `let` runs.
/// Whether an atom is an ANF temporary (vs a named source variable or a constant). A temp receiver
/// is *owned* — single-use by the ANF invariant, holding no live binding — so after an access
/// consumes it its destructor must fire at last use. A `Var` receiver is borrowed (its
/// binding fires at its own drop), so it is left alone.
fn is_temp(atom: &noeta_ir::Atom) -> bool {
    matches!(atom, noeta_ir::Atom::Temp(_))
}

/// The owned temporaries among a call's evaluated arguments, cloned so they can be destroyed after
/// the call — the argument twin of the temp-receiver rule above, and the mirror of the bytecode
/// backend's `drop_temp_args`.
///
/// Noeta is call-by-value, so `held.get_or("k", Res.new("d"))` builds the default whichever branch
/// the callee takes; on the branch that discards it, nothing else ever owns that object. The call
/// consumes `values`, so a copy is held across it and destroyed afterward — last-reference-gated, so
/// an argument the callee *kept* (stored in a collection, or handed back as the result) correctly
/// defers destruction to its real last owner. Returned in **reverse argument order** (reverse
/// construction, spec §3), which is the order the VM emits its drops in.
///
/// The bytecode backend additionally skips a temporary whose rvalue it proved destructor-free; that
/// exclusion is an optimization, not a rule, and it is not mirrored here — what it saves there is a
/// call out of native code, and what it would save here is one refcount test.
fn temp_arg_copies(args: &[noeta_ir::Atom], values: &[Value]) -> Vec<Value> {
    if !args.iter().any(is_temp) {
        return Vec::new();
    }
    args.iter()
        .zip(values)
        .rev()
        .filter(|(atom, _)| is_temp(atom))
        .map(|(_, value)| value.clone())
        .collect()
}

/// Stamp a construction's reflected type onto the freshly-built value at a **method-call** site, so
/// `type_of` recovers its type arguments after a `dyn` launder. Two producers, one field:
///
/// * a generic **enum-variant construction** (`Tree.Leaf(5)`, R2b.2) — the value was just built and
///   is uniquely owned, so its parts move into a re-tagged `EnumValue` with no clone; an
///   unexpectedly-shared value is left untagged;
/// * a generic **constructor call** (`Repo.new("todos")` at `Repo<Todo>`, generic constructor
///   reflection) — the instantiation is known at the CALL, not inside `fn new` where the literal is
///   written, so the caller stamps it. The checker only records the site when it proved every
///   `return` of the callee hands back a fresh literal of the type, so nothing else can hold this
///   object; the tag is written in place because a `class`'s `Rc` is its identity.
///
/// `reflect` is `None` for an ordinary method call, which is returned unchanged.
fn tag_call_reflect(value: Value, reflect: &Option<noeta_ast::reflect::TypeRepr>) -> Value {
    match (reflect, value) {
        (Some(repr), Value::Enum(rc)) => match Rc::try_unwrap(rc) {
            Ok(ev) => Value::Enum(Rc::new(crate::EnumValue {
                reflect: Some(Rc::new(repr.clone())),
                ..ev
            })),
            Err(rc) => Value::Enum(rc),
        },
        (Some(repr), Value::Object(rc)) => {
            rc.set_reflect(Rc::new(repr.clone()));
            Value::Object(rc)
        }
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

impl IrRefBackend {
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
        deserialize_recipes: DeserializeRecipes,
        packed_type_layouts: PackedTypeLayouts,
    ) -> RunResult {
        Interpreter::new().run_ir(
            ast,
            ir,
            type_of_sites,
            deserialize_recipes,
            packed_type_layouts,
        )
    }

    /// As [`IrRefBackend::run_ir`], plus the abort traceback (empty for a clean run) — the
    /// oracle's twin of the VM's traced entries, letting the two backends' tracebacks be compared.
    pub fn run_ir_traced(
        &self,
        ast: &Program,
        ir: &noeta_ir::Program,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
        deserialize_recipes: DeserializeRecipes,
        packed_type_layouts: PackedTypeLayouts,
    ) -> (RunResult, Vec<noeta_backend::TraceFrame>) {
        Interpreter::new().run_ir_traced(
            ast,
            ir,
            type_of_sites,
            deserialize_recipes,
            packed_type_layouts,
        )
    }

    /// As [`IrRefBackend::run_ir`], but against a caller-provided [`noeta_stdlib::Host`]
    /// (the real host) instead of the deterministic sandbox — the host-mode runner the
    /// conformance reference drives for host-dependent corpus programs. (Historical note: `run`
    /// executed through this during the RC migration, before the VM became the sole production
    /// backend; only the conformance harness calls it now.)
    pub fn run_ir_with_host(
        &self,
        ast: &Program,
        ir: &noeta_ir::Program,
        host: Box<dyn noeta_stdlib::Host>,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
        deserialize_recipes: DeserializeRecipes,
        packed_type_layouts: PackedTypeLayouts,
    ) -> RunResult {
        Interpreter::with_host(host).run_ir(
            ast,
            ir,
            type_of_sites,
            deserialize_recipes,
            packed_type_layouts,
        )
    }

    /// As [`IrRefBackend::run_ir_with_host`], but also swapping the async executor (Track A.4).
    /// The CLI pairs a real host with a real wall-clock executor so `sleep`/`concurrent` run against
    /// real time; conformance never calls this (it keeps the default [`noeta_stdlib::SandboxExecutor`]),
    /// so this path is out-of-oracle.
    // The real-host/real-executor entry threads the same site maps as the sandbox path plus its two
    // boxed capabilities; one over the lint's arg ceiling since Slice E2 added `packed_type_layouts`.
    #[allow(clippy::too_many_arguments)]
    pub fn run_ir_with_host_and_executor(
        &self,
        ast: &Program,
        ir: &noeta_ir::Program,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
        deserialize_recipes: DeserializeRecipes,
        packed_type_layouts: PackedTypeLayouts,
    ) -> RunResult {
        Interpreter::with_host_and_executor(host, executor).run_ir(
            ast,
            ir,
            type_of_sites,
            deserialize_recipes,
            packed_type_layouts,
        )
    }
}

impl Interpreter {
    /// Destroy the argument-temporary copies [`temp_arg_copies`] held across a call, in the order it
    /// collected them (reverse construction). Each destruction is last-reference-gated, so an
    /// argument the callee kept is left to its real owner.
    ///
    /// Skipped when the call **aborted**: a panic or a `?` propagation unwinds past the point where
    /// the bytecode backend's post-call drop sits, so it never runs there — and the two backends have
    /// to agree on a panicking program. What the unwind *does* reclaim on both sides is each frame's
    /// named locals (`panic_unwind.noe`); an unnamed temporary's destructor is not part of that
    /// contract, and its memory comes back through the teardown backstop either way.
    fn destroy_temp_args(&mut self, temps: Vec<Value>, aborted: bool) {
        if aborted {
            return;
        }
        for value in temps {
            self.destroy_value(value);
        }
    }

    /// Execute a lowered program — the whole-program entry point: build the reflection manifest,
    /// run the top-level statements in the global scope, then destroy the global bindings in reverse
    /// declaration order.
    fn run_ir(
        self,
        ast: &Program,
        ir: &noeta_ir::Program,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
        deserialize_recipes: DeserializeRecipes,
        packed_type_layouts: PackedTypeLayouts,
    ) -> RunResult {
        self.run_ir_traced(
            ast,
            ir,
            type_of_sites,
            deserialize_recipes,
            packed_type_layouts,
        )
        .0
    }

    /// [`Interpreter::run_ir`] plus the abort traceback (empty for a clean run) — the tree-walker
    /// twin of the VM's traced entries, so the two backends' tracebacks can be compared.
    fn run_ir_traced(
        mut self,
        ast: &Program,
        ir: &noeta_ir::Program,
        type_of_sites: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
        deserialize_recipes: DeserializeRecipes,
        packed_type_layouts: PackedTypeLayouts,
    ) -> (RunResult, Vec<noeta_backend::TraceFrame>) {
        // Arm the safepoint-GC trigger for this run (the eval mirror of the VM's arm).
        crate::leak::safepoint_arm(crate::leak::safepoint_step());
        let native_roles = self.reg().native_roles();
        // Native trait data joins the membership table (precise `is dyn Trait` / `traits_of`)
        // through the same registry projection seam as the roles — the VM's compile does the same.
        let native_traits = noeta_ir::native_trait_impls(self.reg());
        self.reflection = noeta_ast::reflect::build(ast, &native_roles, &native_traits);
        // The spellings this program may write `#[Transient]` as, from the shared projection the
        // checker and the VM's compile resolve it with — so all three agree on which fields leave
        // the serialized shape rather than each matching the name its own way.
        self.transient_names =
            noeta_ast::attribute_local_names(ast, noeta_ast::reflect::JSON_ATTR_TRANSIENT);
        // The extensions' own declarations are NOT embedded: both backends resolve a native
        // declaration through the shared lazy `ReflectionInfo` lookups (`noeta_ast::native_reflect`),
        // so the differential stays green by construction and neither pays for the whole registry.
        self.type_of_sites = type_of_sites;
        // The `@derive(Deserialize<Json>)` decode registry (L2.2 DI) `json.decode_typed` resolves
        // against — lifted from the checker's sites, identical to the VM's map by construction.
        self.deserialize_recipes = deserialize_recipes;
        // Slice E2: every `@packed` struct's layout by name, so a native fn's `make_packed` resolves a
        // produced list's element schema — the tree-walker twin of the VM's interned `packed_schemas`.
        self.packed_type_layouts = packed_type_layouts;
        // The forwarding type-argument table rides the IR itself, so both
        // backends read the same entries by construction.
        self.type_args = ir.type_args.clone();
        self.type_arg_reprs = ir.type_arg_reprs.clone();
        self.type_arg_hints = ir.type_arg_hints.clone();
        // The element-width table rides the IR itself, for the same reason: both backends read the
        // same entries by construction rather than each deriving a width from a value that has none.
        self.absorb_elem_widths(ir);
        let mut frame = Frame::new(ir.temp_count);
        // The top-level statements run directly in the global scope (no child).
        match self.exec_ir_stmts(&ir.top.stmts, &mut frame) {
            Ok(Flow::Normal) | Ok(Flow::Break) | Ok(Flow::Continue) => {}
            // A top-level `return`, a `?` short-circuit, or a runtime error stops the program.
            Ok(Flow::Return(_)) | Err(Unwind::Return(_)) | Err(Unwind::Abort) => {}
        }
        // Exit reached: disarm the safepoint trigger — the teardown below runs destructors
        // against a heap being dismantled, and the exit reapers reclaim everything a pending
        // safepoint would have.
        crate::leak::safepoint_disarm();
        // Release every value still in the extensions' retained arena —
        // destructor-aware, mirroring the VM's teardown release, so a `destruct`-bearing value
        // left in an extension (an undisposed `Cell`) fires identically on both backends.
        for value in std::mem::take(&mut self.ext_arena).into_iter().flatten() {
            self.destroy_value(value);
        }
        self.ext_arena_free.clear();
        self.destroy_globals();
        // Reap cycles left after teardown so residency reaches 0: closure-capture cycles
        // and reference-`class` field cycles.
        self.reap_captured_scope_cycles();
        self.reap_object_cycles();
        // A deliberate `os.exit(code)` wins over the diagnostic-derived code (there are no
        // diagnostics on that path — the halt is clean).
        // Derived from whether the run **aborted**, not from whether it said anything — and
        // identically to the VM's `lifecycle`, since the differential compares the two verbatim.
        let exit_code = self
            .requested_exit
            .unwrap_or(u8::from(noeta_diagnostics::has_errors(&self.diagnostics)).into());
        (
            RunResult {
                stdout: self.stdout,
                stderr: self.stderr,
                exit_code,
                diagnostics: self.diagnostics,
            },
            self.abort_trace,
        )
    }

    /// Execute one REPL batch of lowered top-level statements **in the persistent global scope**,
    /// with a fresh temporary frame sized to this batch. Unlike [`Interpreter::run_ir`] it does
    /// *not* rebuild reflection (the [`Session`](crate::Session) sets it) and does *not* destroy the
    /// global bindings afterward — the scope, its bindings, and its declarations persist across
    /// batches, exactly as the REPL requires. ANF temporaries are per-batch and do not persist, so a
    /// fresh `Frame` each call is correct. Returns the batch's terminating [`Flow`] (a top-level
    /// `return`/error stops it, mirroring the AST-walker session loop).
    pub(crate) fn run_ir_batch(&mut self, ir: &noeta_ir::Program) -> Eval<Flow> {
        // Accumulated across batches, never replaced: an earlier entry's still-live function looks
        // its own call span up when it runs again, exactly as the VM's session keeps every install's
        // entries.
        self.absorb_elem_widths(ir);
        // Arm per batch, relative to the session's current residency (persistent bindings are
        // never charged against the watermark).
        crate::leak::safepoint_arm(crate::leak::safepoint_step());
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
            // Open a structured-concurrency scope (Track A.3b): a fresh, empty task list (A.7 tombstone
            // model — `open_scope` pushes both `scopes` and `scope_closed`).
            noeta_ir::Stmt::ScopeBegin { .. } => {
                self.open_scope();
                Ok(Flow::Normal)
            }
            // Close the scope: drive every remaining task to completion (the join), then close the
            // innermost scope — releasing the tasks' futures and results (destructor-aware). The
            // synchronous (non-flattened) path is strictly LIFO, so the innermost scope is this one.
            noeta_ir::Stmt::ScopeEnd { span } => {
                self.join_scope(*span)?;
                let si = self.innermost_open();
                self.close_scope(si);
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
                order,
                order_slots,
            } => {
                let resolved = self.resolve_order_hint(order, order_slots, frame)?;
                self.note_order_hint(*span, resolved);
                self.exec_ir_for(pattern, iterable, body, *span, *stream, frame)
            }
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
                // a temp is an owner too (spec §2). Non-aggregates/aliased values no-op.
                let value = frame.take(*t);
                self.destroy_value(value);
                Ok(Flow::Normal)
            }
            // A source-variable drop: release the binding's value at its
            // last use, aligning this backend's reclamation timing with the VM's. When the drop is
            // destructor-relevant, take the value out and run `destroy_value`, firing its
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
    ///
    /// A guarded arm (`pattern if cond`) evaluates its guard **after** the pattern matches, in
    /// the arm's child scope (the pattern bindings are visible); a `false` guard abandons the arm
    /// and falls through to the next one exactly as a failed pattern would. A non-bool guard is
    /// the same runtime error as a non-bool `if` condition — the VM compiles the guard to the
    /// identical fused conditional branch (`CondBranch`), so the two backends agree byte for byte.
    fn exec_ir_match(
        &mut self,
        value: Value,
        arms: &[noeta_ir::Arm],
        dst: Option<noeta_ir::Temp>,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Flow> {
        for arm in arms {
            if let Some(bindings) = crate::match_pattern(
                &arm.pattern,
                &value,
                &self.reflection,
                &self.native_type_names,
            ) {
                let child = crate::Scope::child(&self.scope);
                for (name, bound) in bindings {
                    child.declare(name, bound, false);
                }
                let saved = std::mem::replace(&mut self.scope, child);
                // A statement-BLOCK arm may carry non-local flow — a `return` exits
                // the enclosing function, a `break`/`continue` the enclosing loop — so the arm
                // body's flow propagates instead of being a value-position invariant (the VM's
                // inline codegen gets the same behavior from its jump targets).
                let result = (|| -> Eval<Option<(Flow, Option<Value>)>> {
                    if let Some(guard) = &arm.guard {
                        let cond = self.eval_ir_block_value(&guard.block, frame, guard.span)?;
                        match cond {
                            Value::Bool(true) => {}
                            // A false guard: this arm is not taken — fall through.
                            Value::Bool(false) => return Ok(None),
                            other => {
                                return Err(self.runtime_error(
                                    DiagnosticCode::TypeMismatch,
                                    guard.span,
                                    format!(
                                        "`if` condition must be a bool, found {}",
                                        other.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    match self.exec_ir_stmts(&arm.body.stmts, frame)? {
                        Flow::Normal => {}
                        flow => return Ok(Some((flow, None))),
                    }
                    let v = match &arm.body.tail {
                        Some(atom) => Some(self.eval_ir_atom(atom, frame)?),
                        None => None,
                    };
                    Ok(Some((Flow::Normal, v)))
                })();
                if matches!(result, Err(Unwind::Abort)) {
                    self.fire_aborted_scope();
                }
                self.scope = saved;
                let Some((flow, v)) = result? else {
                    continue; // guard was false — try the next arm
                };
                if !matches!(flow, Flow::Normal) {
                    return Ok(flow);
                }
                if let (Some(dst), Some(v)) = (dst, v) {
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
    /// lexical scope — the IR analogue of `declare_fn`'s/`Expr::Closure`'s construction. The trace
    /// name comes from the IR itself (`Func::name`, set at lowering: `"f"`, `"Type.method"`, an
    /// async/generator step under its enclosing function's name, `None` for a user's anonymous
    /// closure) — the same single source the VM's synthesized-closure prototypes read.
    fn make_ir_closure(&self, func: &Rc<noeta_ir::Func>) -> Closure {
        Closure::new(
            func.params.clone(),
            func.defaults.clone(),
            Rc::clone(func),
            Rc::clone(&self.scope),
            func.name.clone(),
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
                transient: noeta_ast::has_attribute(&f.attrs, &self.transient_names),
                unsigned: f.ty.as_ref().is_some_and(noeta_ast::is_unsigned_64_type),
            })
            .collect();
        let methods = strukt
            .methods
            .iter()
            .map(|(name, func)| (name.clone(), Rc::new(self.make_ir_closure(func))))
            .collect();
        // A `@packed` struct feeds the key-capability fixpoint (stamped onto the defs
        // right after this declaration lands, below).
        let packed = noeta_ast::packed_named_fields(decl);
        if let Some(named) = packed.clone() {
            self.packed_fields.insert(decl.name.to_string(), named);
        }
        let def = TypeDef {
            name: decl.name.to_string(),
            fields,
            methods,
            destructor: None,
            is_struct: true,
            // A value kind: `==` is always structural.
            structural_eq: true,
            // Stamped below (and re-stamped by later declarations) from the fixpoint.
            key_capable: std::cell::Cell::new(false),
            // A hand-written `compare`/`to_json` takes precedence over derivation.
            derives_comparable: noeta_ast::derives_trait(&decl.decorators.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            derives_tojson: noeta_ast::derives_trait(&decl.decorators.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            opaque: false,
            field_defaults: strukt.field_defaults.clone(),
        };
        self.scope
            .declare(decl.name.to_string(), Value::Type(Rc::new(def)), false);
        // Settle the fixpoint with this declaration included and stamp every settled
        // type's (`Rc`-shared) def — a later declaration completing a forward-referenced nested
        // chain retro-marks the earlier defs, and every live instance sees it through the shared
        // `Rc`. Capability is read only at key-USE time, which follows all involved declarations.
        if packed.is_some() {
            self.key_capable_packed = noeta_ast::key_capable_packed(&self.packed_fields);
            for name in &self.key_capable_packed {
                if let Some(Value::Type(def)) = self.globals.lookup(name) {
                    def.key_capable.set(true);
                    // Register the field names so a packed key renders its display on
                    // demand (idempotent; same registry the VM fills at load).
                    noeta_stdlib::map_key::packed_names::register(
                        name,
                        def.fields.iter().map(|f| f.name.as_str()),
                    );
                }
            }
        }
    }

    /// Register an enum whose methods are IR-bodied closures. Mirrors
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
                unsigned: noeta_ast::unsigned_field_slots(v.fields.iter().map(|f| f.ty.as_ref()))
                    .into(),
                // The backing a wire→case conversion matches on, through the one `fold_const_expr`
                // the reflection manifest and the checker's decode recipes also fold with.
                backing: v
                    .backed_value
                    .as_ref()
                    .and_then(noeta_ast::reflect::fold_const_expr),
            })
            .collect();
        let methods = en
            .methods
            .iter()
            .map(|(name, func)| (name.clone(), Rc::new(self.make_ir_closure(func))))
            .collect();
        let def = EnumDef {
            name: decl.name.to_string(),
            variants,
            // A hand-written `compare`/`to_json` takes precedence over derivation — the same
            // rule `declare_ir_struct` applies.
            derives_comparable: noeta_ast::derives_trait(&decl.decorators.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            derives_tojson: noeta_ast::derives_trait(&decl.decorators.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            methods,
        };
        self.scope
            .declare(decl.name.to_string(), Value::EnumType(Rc::new(def)), false);
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
                transient: noeta_ast::has_attribute(&f.attrs, &self.transient_names),
                unsigned: f.ty.as_ref().is_some_and(noeta_ast::is_unsigned_64_type),
            })
            .collect();
        let methods = class
            .methods
            .iter()
            .map(|(name, func)| (name.clone(), Rc::new(self.make_ir_closure(func))))
            .collect();
        let def = TypeDef {
            name: decl.name.to_string(),
            fields,
            methods,
            // The lowered `destruct` block (a parameterless IR `Func`), run via `exec_ir_fn_body`
            // with fields + `self` in scope — the same IR the VM compiles, so destructor execution
            // no longer routes through the retired AST walker.
            destructor: class.destructor.clone(),
            is_struct: false,
            // A class is never a packed key.
            key_capable: std::cell::Cell::new(false),
            // A reference `class`: `==` is identity unless the class is `Equatable` (derives it or
            // hand-`impl`s `eq`) — the same rule `declare_class` applies.
            structural_eq: noeta_ast::derives_trait(&decl.decorators.derives, "Equatable")
                || decl.methods.iter().any(|m| m.name == "eq"),
            // A hand-written `compare`/`to_json` takes precedence over derivation — the same
            // rule `declare_class` applies.
            derives_comparable: noeta_ast::derives_trait(&decl.decorators.derives, "Comparable")
                && !decl.methods.iter().any(|m| m.name == "compare"),
            derives_tojson: noeta_ast::derives_trait(&decl.decorators.derives, "Serialize")
                && !decl.methods.iter().any(|m| m.name == "to_json"),
            opaque: false,
            field_defaults: class.field_defaults.clone(),
        };
        self.scope
            .declare(decl.name.to_string(), Value::Type(Rc::new(def)), false);
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
            // abort unwinds to the caller.
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
            // Safepoint-GC poll at the loop back-edge (memory-management 6.x) — the eval mirror
            // of the VM dispatch loop's backward-jump poll.
            crate::cycles::poll_safepoint();
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
                // Safepoint-GC poll per iteration — see `exec_ir_while`.
                crate::cycles::poll_safepoint();
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
            // Safepoint-GC poll per iteration — see `exec_ir_while`.
            crate::cycles::poll_safepoint();
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

    /// The narrowing target a [`noeta_ir::Rvalue::As`]/[`noeta_ir::Rvalue::TypeTest`] with a
    /// **dynamic** head-name atom resolves to, or `None` when it carries none (the ordinary
    /// statically-written target, which stays authoritative).
    ///
    /// The atom is the `TypeArgName`/`TypeSlotName` that also answers `type_name::<T>()`, so it
    /// evaluates to the instantiation's qualified name; wrapping it back into a bare
    /// [`noeta_ir::TypeRef::Named`] re-enters `runtime_matches` on that name and reuses the whole
    /// matcher unchanged — the built-in head funnel, `Option`/`Result`, a user shape name, and an
    /// extern's qualified identity all decided exactly as for a written target. The VM's
    /// `NarrowTarget::from_runtime_name` is the mirror of that re-entry.
    ///
    /// Head-only is not a simplification here: the checker records the site only for a *bare*
    /// parameter target, so there is exactly one name and no arguments to carry.
    fn runtime_narrow_target(
        &mut self,
        ty: &noeta_ir::TypeRef,
        dynamic: &Option<noeta_ir::Atom>,
        frame: &mut Frame,
        span: noeta_span::Span,
    ) -> Eval<Option<noeta_ir::TypeRef>> {
        let Some(atom) = dynamic else {
            return Ok(None);
        };
        let name = self.eval_ir_atom(atom, frame)?;
        Ok(match name {
            Value::Str(name) => Some(noeta_ir::TypeRef::Named {
                name: noeta_ast::Name::canonical(name.to_string()),
                args: Vec::new(),
                span,
            }),
            // Both producers write a string, so this is unreachable in a checked program; a narrow
            // has no failure channel, so it degrades to the baked target rather than aborting.
            _ => {
                let _ = ty;
                None
            }
        })
    }

    /// **Evaluate a reflection query** — one dispatch over [`ReflectKind`], covering the twelve
    /// kinds that reach a backend.
    ///
    /// The twin of the VM's dispatch arms and of `noeta_compiler::compile_reflect`. Being one
    /// exhaustive match is what makes "the reference interpreter and the VM answer the same query"
    /// a compile-time obligation rather than something the differential oracle discovers later.
    fn eval_reflect(
        &mut self,
        which: noeta_ast::ReflectKind,
        args: &noeta_ir::ReflectArgs,
        private_fields: bool,
        span: Span,
        frame: &mut Frame,
    ) -> Eval<Value> {
        use noeta_ast::ReflectKind as K;
        use noeta_ir::ReflectArgs as A;

        // A shape mismatch is a compiler bug — lowering builds these, and the census asserts the
        // (kind × shape) grid — so it is reported as one rather than as a program error.
        let mismatch = || -> ! {
            panic!(
                "`{}` reached the interpreter with the wrong operand shape",
                which.keyword()
            )
        };
        match which {
            // Not a runtime query: `type_name::<T>()` IS the name-resolution step the others use as
            // an operand, and lowering already turned it into a constant or a channel read.
            K::TypeName => mismatch(),
            K::TypeOf => {
                let A::One(operand) = args else { mismatch() };
                let v = self.eval_ir_atom(operand, frame)?;
                match self.type_of_sites.get(&span) {
                    Some(repr) => Ok(crate::build_type_value(repr)),
                    None => Ok(crate::build_type_value(&crate::eval_type_repr(&v))),
                }
            }
            K::FieldsOf => {
                let A::One(operand) = args else { mismatch() };
                let v = self.eval_ir_atom(operand, frame)?;
                Ok(self.materialize_fields(&v, private_fields))
            }
            K::TraitsOf => {
                let A::One(operand) = args else { mismatch() };
                let v = self.eval_ir_atom(operand, frame)?;
                Ok(self.materialize_traits(&v))
            }
            // The five **name-keyed** queries. Each takes one runtime string — folded from a written
            // turbofish, or read off a per-instantiation channel by the preceding
            // `TypeArgName`/`TypeSlotName` — and each is total on an arbitrary name, answering the
            // empty result for one it holds nothing for. That leniency is what let the turbofish and
            // dynamic surfaces converge on a single node, and it is stated once here rather than
            // five times.
            K::AttributesOf => {
                let name = self.reflect_name_operand(args, frame)?;
                Ok(self.materialize_attributes(&name))
            }
            K::RolesOf => {
                // The one query whose operand is optional: `None` is the unscoped index.
                let role_enum = match args {
                    A::Nothing => None,
                    A::One(_) => Some(self.reflect_name_operand(args, frame)?),
                    _ => mismatch(),
                };
                Ok(self.materialize_roles(role_enum.as_deref()))
            }
            K::ParamsOf => {
                let target = self.reflect_name_operand(args, frame)?;
                Ok(self.materialize_params(&target))
            }
            K::ReturnsOf => {
                let target = self.reflect_name_operand(args, frame)?;
                Ok(self.materialize_returns(&target))
            }
            K::FieldSpecsOf => {
                let name = self.reflect_name_operand(args, frame)?;
                Ok(self.materialize_field_specs(&name))
            }
            K::VariantsOf => {
                let name = self.reflect_name_operand(args, frame)?;
                Ok(self.materialize_variant_specs(&name))
            }
            K::Construct => {
                let A::Two { name, arg } = args else {
                    mismatch()
                };
                let name_val = self.eval_ir_atom(name, frame)?;
                let fields_val = self.eval_ir_atom(arg, frame)?;
                self.construct_dynamic(name_val, fields_val, span)
            }
            K::Invoke => {
                let A::Dispatch { recv, name, args } = args else {
                    mismatch()
                };
                let receiver = match recv {
                    Some(recv) => Some(self.eval_ir_atom(recv, frame)?),
                    None => None,
                };
                let name_val = self.eval_ir_atom(name, frame)?;
                let args_val = self.eval_ir_atom(args, frame)?;
                self.invoke_dynamic(receiver, name_val, args_val, span)
            }
            K::FromBytes => {
                let A::Bytes {
                    blob,
                    layout,
                    validate,
                } = args
                else {
                    mismatch()
                };

                // Deserialize a `bytes` buffer into a flat `List<T>`: resolve T's schema,
                // then wrap the raw bytes as a packed list — the inverse of `to_bytes`, an O(n) copy.
                let blob_val = self.eval_ir_atom(blob, frame)?;
                let Value::Bytes(bytes) = blob_val else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
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
                            span,
                            "`from_bytes` requires a packable element type — a `@packed` struct or a sub-8-byte fixed-width numeric (`i32`/`u8`/`f32`, …)"
                                .to_string(),
                        )
                    })?;
                // The buffer must be a whole number of elements; a partial blob is corrupt input.
                if schema.byte_size == 0 || bytes.len() % schema.byte_size != 0 {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`from_bytes` buffer of {} bytes is not a whole number of {}-byte elements",
                            bytes.len(),
                            schema.byte_size
                        ),
                    ));
                }
                let list = Value::packed_list_from(schema, (*bytes).clone());
                // `from_bytes` is an abort door — run each decoded element's
                // `validate()` (materialized boxed for the re-entry) and abort at `[i]` on the first
                // rejection, consistent with a length/shape mismatch. Closes the hole a `@validated`
                // packed type would otherwise have here.
                if *validate && let Value::List(repr) = &list {
                    for (i, elem) in repr.to_rc_vec().iter().enumerate() {
                        if let Some(message) = self.validate_message(elem.clone(), span)? {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!("from_bytes: [{i}]: {message}"),
                            ));
                        }
                    }
                }
                Ok(list)
            }
        }
    }

    /// The **name operand** of a name-keyed query, as a runtime string.
    ///
    /// A non-string value answers the empty name, which the manifest and registry lookups all treat
    /// as "nothing registered" — the same total-on-any-name contract that lets the turbofish and
    /// runtime-string surfaces of one query converge on a single node. It was written out at each of
    /// the five call sites; one of them differing would have been a silent wrong answer, not a
    /// crash.
    fn reflect_name_operand(
        &mut self,
        args: &noeta_ir::ReflectArgs,
        frame: &mut Frame,
    ) -> Eval<String> {
        let noeta_ir::ReflectArgs::One(atom) = args else {
            panic!("a name-keyed reflection query carries exactly one operand")
        };
        Ok(match self.eval_ir_atom(atom, frame)? {
            Value::Str(s) => s,
            _ => String::new(),
        })
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
                    // (member access is explicit: `self.field`), so a miss
                    // here is a plain unknown name, exactly as the VM reports it.
                    self.record_abort_trace(*span);
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
                    // A bare `from` names no single conversion; say which ones exist.
                    noeta_ast::conversion::missing_from_message(
                        def.name(),
                        name,
                        def.methods.keys().map(String::as_str),
                    )
                    .unwrap_or_else(|| {
                        format!("type `{}` has no static function `{name}`", def.name())
                    }),
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
            // A native module-fn reference as a value — the same `Value::ModuleFn`
            // an imported `use std.mod.fn` binding holds, so a `Call` on it dispatches to the native
            // function identically. Mirrors the VM's `Const::ModuleFn`.
            noeta_ir::Rvalue::ModuleFn { module, func, .. } => {
                Ok(Value::ModuleFn(module.clone(), func.clone()))
            }
            // A native module value resolved from a namespace group (`http.client`) — the same
            // `Value::NativeModule` a direct `use std.http.client` binding holds, so a method call
            // on it dispatches through the identical native-module path. Mirrors the VM's
            // `Const::NativeModule`.
            noeta_ir::Rvalue::NativeModule { module, .. } => {
                Ok(Value::NativeModule(module.clone()))
            }
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
            noeta_ir::Rvalue::Render {
                operand,
                hint,
                slots,
                span,
            } => {
                // Render a display-site value whose static type carries an unsigned 64-bit integer.
                // The VM runs the identical hinted walk on its own value model, so the differential
                // pins the two renderings equal.
                let resolved =
                    self.resolve_render_hint(hint, slots, noeta_stdlib::HintDoor::Display, frame)?;
                let value = self.eval_ir_atom(operand, frame)?;
                match resolved.as_deref() {
                    Some(hint) => Ok(Value::Str(value.display_hinted(hint))),
                    // No hint after the splice: this door is an ORDINARY display door and must
                    // behave as one, `Display` dispatch included. That is the whole meaning of the
                    // outermost `Display` exemption — a concrete-typed door at such a type records
                    // no hint at all and keeps its dispatch, so a door that names a parameter
                    // instantiated at that type has to arrive at the same place.
                    None => Ok(Value::Str(self.display_value(&value, *span)?)),
                }
            }
            noeta_ir::Rvalue::JsonRender {
                operand,
                hint,
                slots,
                ..
            } => {
                // Serialize a JSON-site value whose static type carries an unsigned 64-bit integer.
                // The marshal is this backend's own; the walk over the marshalled tree is the shared
                // one the VM runs, so the two encodings agree by construction.
                let resolved =
                    self.resolve_render_hint(hint, slots, noeta_stdlib::HintDoor::Json, frame)?;
                let value = self.eval_ir_atom(operand, frame)?;
                Ok(Value::Str(noeta_ast::json_stringify(
                    &crate::value_to_native_deep(&value),
                    resolved.as_deref(),
                )))
            }
            noeta_ir::Rvalue::Binary {
                op,
                lhs,
                rhs,
                reuse,
                span,
            } => {
                // In-place self-append: a marked `acc = acc ~ rhs` moves the accumulator
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
                // An int method needing the receiver's static width (Tier W5): the twin of the VM's
                // `Op::WidthIntMethod`, routed through the shared `int_method_outcome` — a bit
                // intrinsic computes within `bits`, a range-checked conversion answers an option.
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
                Ok(
                    match noeta_stdlib::int_method_outcome(recv_int, *method, amount, Some(*bits)) {
                        noeta_stdlib::IntOutcome::Word(word) => Value::Int(word),
                        noeta_stdlib::IntOutcome::Checked(Some(word)) => {
                            crate::builtin_enum("Option", "some", vec![Value::Int(word)])
                        }
                        noeta_stdlib::IntOutcome::Checked(None) => {
                            crate::builtin_enum("Option", "none", Vec::new())
                        }
                    },
                )
            }
            noeta_ir::Rvalue::List { items, reflect, .. } => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval_ir_atom(item, frame)?);
                }
                // Stamp the checker-resolved element type onto the list so `type_of` recovers it
                // after a `dyn` launder — the tree-walker twin of the VM's node tag, agreeing by
                // construction. `None` → an untagged list, reflecting head-only.
                let repr =
                    ListRepr::boxed(Rc::new(values)).with_reflect(reflect.clone().map(Rc::new));
                Ok(Value::List(repr))
            }
            noeta_ir::Rvalue::PackedListNew { layout, .. } => {
                // Start a streaming flat build: an empty `List<packed>` buffer, or an
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
                    let key_value = self.eval_ir_atom(key_atom, frame)?;
                    // A string, or a key-capable extern value.
                    let Some(key) = crate::value_map_key(&key_value) else {
                        let error = noeta_stdlib::map_key::map_key_error(key_value.type_name());
                        return Err(self.runtime_error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            error.message,
                        ));
                    };
                    let value = self.eval_ir_atom(value_atom, frame)?;
                    map.insert(key, value);
                }
                // Stamp the checker-resolved `Map(K, V)` type so `type_of` recovers it after a
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
                    // destructor after. `eval_index` consumes the receiver, so clone for
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
            noeta_ir::Rvalue::Call {
                callee,
                args,
                // A forwarding generic's type arguments, in slot order — empty
                // for every call that forwards nothing. Their own channel, which is why `supplied`
                // below still indexes the VALUE parameters alone.
                type_args,
                // `None` for a pure reordering — lowering already permuted `args`, so there is
                // nothing left to say. `Some` only when the call skips a defaulted parameter.
                supplied,
                span,
            } => {
                let callee = self.eval_ir_atom(callee, frame)?;
                let tys = self.eval_ir_atoms(type_args, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                let temps = temp_arg_copies(args, &values);
                let result = self.call_masked(callee, values, *span, &tys, *supplied);
                self.destroy_temp_args(temps, result.is_err());
                result
            }
            noeta_ir::Rvalue::Method {
                receiver,
                name,
                args,
                reuse,
                reflect,
                reflect_slot,
                // A forwarding generic METHOD's type arguments (Axis A), in slot order — empty for
                // the overwhelming majority of method calls. The reuse fast paths below are all
                // built-in collection updates, which declare no slots, so they never carry any.
                type_args,
                span,
                supplied,
                order,
                order_slots,
                push,
                push_slots,
                name_span: _,
            } => {
                // A method whose result reveals an order the program can see, on a `u64`-carrying
                // receiver: register the hint by span, so the collection method below reads it the
                // way the VM reads its own span-keyed table. Resolved against this frame's render
                // slots first — inside a generic body the answer is the caller's, not the site's.
                let resolved = self.resolve_order_hint(order, order_slots, frame)?;
                self.note_order_hint(*span, resolved);
                // A native call that BINDS a value it serializes on a later tick: register its push
                // hint by span, so the ctx built for the dispatch reads it the way the VM reads its
                // own span-keyed table — spliced against this frame's render slots first, because
                // the tick that serializes will have none.
                let pushed = self.resolve_push_hint(push, push_slots, frame)?;
                self.note_binding_hint(*span, &pushed);
                // In-place collection self-update: a marked `m = m.set(k,v)` moves the
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
                    let tys = self.eval_ir_atoms(type_args, frame)?;
                    let values = self.eval_ir_atoms(args, frame)?;
                    let temps = temp_arg_copies(args, &values);
                    let recv = match self.scope.take_mut(recv_name) {
                        Some(v) => v,
                        None => self.eval_ir_atom(receiver, frame)?,
                    };
                    let result = if matches!(&recv, Value::Map(..))
                        && name == "set"
                        && values.len() == 2
                    {
                        self.map_set_in_place(recv, values, *span)
                    } else if matches!(&recv, Value::Map(..))
                        && name == "remove"
                        && values.len() == 1
                    {
                        self.map_remove_in_place(recv, values, *span)
                    } else if matches!(&recv, Value::List(_)) && name == "set" && values.len() == 2
                    {
                        self.list_set_in_place(recv, values, *span)
                    } else if matches!(&recv, Value::Set(..)) && values.len() == 1 && name == "add"
                    {
                        self.set_add_in_place(recv, values, *span)
                    } else if matches!(&recv, Value::Set(..))
                        && values.len() == 1
                        && name == "remove"
                    {
                        self.set_remove_in_place(recv, values, *span)
                    } else {
                        // The non-collection fall-through — a user method that happens to be named
                        // `set`/`add`/`remove` — is an ordinary consuming call, so it must carry both
                        // channels: the VM's reuse arm gates on the runtime receiver KIND and reaches
                        // its ordinary dispatch with them intact, and reuse is supposed to be
                        // observationally invisible.
                        self.call_method_masked(recv, name, values, *span, &tys, *supplied)
                    };
                    self.destroy_temp_args(temps, result.is_err());
                    return result;
                }
                let recv = self.eval_ir_atom(receiver, frame)?;
                let tys = self.eval_ir_atoms(type_args, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                let temps = temp_arg_copies(args, &values);
                let result = if is_temp(receiver) {
                    // An owned temp receiver (`Resource.new().use()`): fire its destructor after the
                    // call. `call_method` consumes the receiver, so clone for the call
                    // and destroy the held copy — last-reference-gated, so a method that returns
                    // `self` (the result aliases it) correctly defers destruction.
                    let result =
                        self.call_method_masked(recv.clone(), name, values, *span, &tys, *supplied);
                    // Reverse construction: the argument temporaries were built after the receiver,
                    // so they are reclaimed first (and the VM's drops are emitted in that order).
                    self.destroy_temp_args(temps, result.is_err());
                    self.destroy_value(recv);
                    result
                } else {
                    let result =
                        self.call_method_masked(recv, name, values, *span, &tys, *supplied);
                    self.destroy_temp_args(temps, result.is_err());
                    result
                };
                // When this "method call" was a generic enum-variant construction, stamp the reflected
                // type onto the freshly-built value (R2b.2) — the tree-walker twin of the VM's node tag.
                // …or a generic-in-generic construction, whose tag the hidden slot names: resolve the
                // slot's table index through the same `type_arg_reprs` table the VM's
                // `Op::RetagDynamic` reads, so both backends stamp the identical interned repr.
                let dynamic = match reflect_slot {
                    Some(slot) => self.dynamic_construction_tag(slot, frame)?,
                    None => None,
                };
                result.map(|v| tag_call_reflect(tag_call_reflect(v, reflect), &dynamic))
            }
            // A trait method call (a native default body, or a kernel-trait method): the route
            // was baked at the call site — straight to
            // the trait's shared ctx dispatch, receiver as slot 0.
            noeta_ir::Rvalue::TraitMethod {
                receiver,
                trait_name,
                name,
                args,
                span,
            } => {
                let recv = self.eval_ir_atom(receiver, frame)?;
                let values = self.eval_ir_atoms(args, frame)?;
                self.call_trait_method(trait_name, name, recv, &values, *span)
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
                // of a container in `b.inner.tag` — is destroyed afterward, firing its
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
                // Fused `list[i].field`. A packed list decodes the one field directly,
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
            // A narrow whose target is an enclosing generic's type parameter reads the head name out
            // of the `dynamic` atom — the very `TypeArgName`/`TypeSlotName` that answers
            // `type_name::<T>()` — and re-enters the matcher on that name, so the two surfaces
            // resolve one `T` and built-in instantiations (`T = int`) go through the same
            // `BuiltinTy` funnel a written `int` does. The VM's `from_runtime_name` mirrors it.
            noeta_ir::Rvalue::As {
                operand,
                ty,
                dynamic,
                span,
            } => {
                let value = self.eval_ir_atom(operand, frame)?;
                let target = self.runtime_narrow_target(ty, dynamic, frame, *span)?;
                if crate::runtime_matches(&value, target.as_ref().unwrap_or(ty), &self.reflection) {
                    Ok(crate::builtin_enum("Option", "some", vec![value]))
                } else {
                    Ok(crate::builtin_enum("Option", "none", vec![]))
                }
            }
            noeta_ir::Rvalue::TypeTest {
                operand,
                ty,
                dynamic,
                span,
            } => {
                let value = self.eval_ir_atom(operand, frame)?;
                let target = self.runtime_narrow_target(ty, dynamic, frame, *span)?;
                Ok(Value::Bool(crate::runtime_matches(
                    &value,
                    target.as_ref().unwrap_or(ty),
                    &self.reflection,
                )))
            }
            // **The reflection surface, one arm.** All twelve queries that reach a backend
            // dispatch through `eval_reflect`, which matches exhaustively on `ReflectKind` — so a
            // thirteenth cannot be added here and forgotten in the VM, or the reverse.
            noeta_ir::Rvalue::Reflect {
                which,
                args,
                private_fields,
                span,
            } => self.eval_reflect(*which, args, *private_fields, *span, frame),
            // `type_name::<T>()` where `T` is a parameter of the enclosing generic type: read
            // argument `index` off the receiver's reflected type tag. A receiver with no such
            // argument aborts — a plausible-looking wrong name would travel silently.
            noeta_ir::Rvalue::TypeArgName {
                operand,
                index,
                type_name,
                param,
                span,
            } => {
                let v = self.eval_ir_atom(operand, frame)?;
                match crate::eval_type_repr(&v).type_arg_name(*index as usize) {
                    Some(name) => Ok(Value::Str(name)),
                    None => Err(self.runtime_error(
                        DiagnosticCode::InvalidTypeArguments,
                        *span,
                        noeta_ast::reflect::missing_type_arg_message(type_name, param),
                    )),
                }
            }
            // The render-slot twin of the arm above: the same tag and the same argument position,
            // answering with the type-argument table index a door's hint resolves through. It
            // degrades where that one aborts — an argument the tag does not carry, or one no
            // construction site interned, is `NO_TYPE_ARG`, so the value renders as its erased
            // word. The VM reads its own table's reprs through the same helper.
            noeta_ir::Rvalue::SelfRenderSlot { operand, index, .. } => {
                let v = self.eval_ir_atom(operand, frame)?;
                Ok(Value::Int(crate::eval_type_repr(&v).render_slot_arg(
                    *index as usize,
                    self.type_arg_reprs.iter().map(Option::as_ref),
                )))
            }
            // A render slot the enclosing body composes out of its own leaf slots, because the
            // instantiation is one the body BUILT (`wrap([v])` inside `fn built<T>(v: T)`) and no
            // slot of it carries that type whole. The lookup is the shared one, so the VM cannot
            // compose a different entry from the same leaves; a combination no case names is
            // `NO_TYPE_ARG`, and the value renders as its erased word.
            noeta_ir::Rvalue::ComposeTypeArg { slots, cases, .. } => {
                let mut leaves = Vec::with_capacity(slots.len());
                for slot in slots {
                    leaves.push(match self.eval_ir_atom(slot, frame)? {
                        Value::Int(i) => i,
                        _ => noeta_stdlib::NO_TYPE_ARG,
                    });
                }
                Ok(Value::Int(noeta_stdlib::compose_type_arg(
                    cases,
                    &self.type_arg_hints,
                    &leaves,
                )))
            }
            // `type_name::<T>()` where `T` is a FORWARDED parameter of the enclosing generic fn:
            // the instantiation's qualified name, read off the hidden slot's table entry. The same
            // slot, entry and field the dynamic `attributes_of` arm above reads, so a forwarded
            // name and a forwarded manifest can never disagree about what `T` is.
            // The other slot reader is not an `Rvalue` of its own but a field on the associated-call
            // one — see `Self::dynamic_construction_tag`.
            noeta_ir::Rvalue::TypeSlotName { slot, span } => {
                let idx = self.eval_ir_atom(slot, frame)?;
                let Value::Int(i) = idx else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "corrupt hidden type-argument slot".to_string(),
                    ));
                };
                Ok(Value::Str(
                    self.type_args
                        .get(i as usize)
                        .map(|e| e.name.clone())
                        .unwrap_or_default(),
                ))
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
                // The checker-resolved reflected type for a generic instantiation; `None` for a
                // non-generic type → the value reflects head-only. Wrapped in an `Rc` to match the tag
                // stored on the object (a cheap refcount bump on construction).
                let reflect = reflect.clone().map(Rc::new);
                // In-place reuse: a marked self-update `acc = Type { ...acc, … }` moves the
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
                    // A cancelled handle awaited inside an `async fn` body would otherwise suspend
                    // forever on a `none`; fail loudly (Track A.8, E0056) instead — the same
                    // contract top-level `.await` enforces. The VM's `Op::PollFuture` mirror.
                    None if self.handle_cancelled(&future) => {
                        return Err(self.runtime_error(
                            DiagnosticCode::AwaitCancelled,
                            *span,
                            "cannot await a cancelled task; use `.join()` to observe the cancelled \
                             outcome"
                                .to_string(),
                        ));
                    }
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
                // Senders captured in the spawned future are producer holds (isolates I.4c).
                let holds = crate::collect_producer_channels(&future);
                for &cid in &holds {
                    self.add_producer_hold(cid);
                }
                let scope_idx = self.innermost_open();
                let task_idx = self.scopes[scope_idx].len();
                // The child inherits a snapshot of the spawner's task-local context.
                let context = self.ctx_current.clone();
                self.scopes[scope_idx].push(crate::Task {
                    future,
                    result: None,
                    cancelled: false,
                    polling: false,
                    context,
                    holds,
                });
                Ok(Value::Handle(
                    ScopeId::from_index(scope_idx),
                    TaskId::from_index(task_idx),
                ))
            }
            // `$scope_begin()` (Track A.7): open a structured-concurrency scope and yield its index, so
            // the async desugar's split `concurrent { }` can thread that index to its join poll-state.
            // The value form of `Stmt::ScopeBegin`; mirrors the VM's `Op::ScopeBeginValue`.
            noeta_ir::Rvalue::ScopeBegin { .. } => Ok(Value::Int(self.open_scope() as i64)),
            // `$scope_ready(scope)` (Track A.7): whether every task in the scope at index `scope` has
            // completed or been cancelled — the boolean the split `concurrent { }`'s join poll-state
            // tests each poll. A stale/out-of-range index reads ready (defensive; unreachable for a
            // clean program). Mirrors the VM's `Op::ScopeReady`.
            noeta_ir::Rvalue::ScopeReady { scope, span } => {
                let scope = self.eval_ir_atom(scope, frame)?;
                let Value::Int(idx) = scope else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "internal: $scope_ready expects a scope index".to_string(),
                    ));
                };
                let ready = self
                    .scopes
                    .get(idx as usize)
                    .is_none_or(|s| s.iter().all(|t| t.result.is_some() || t.cancelled));
                Ok(Value::Bool(ready))
            }
            // `$scope_end(scope)` (Track A.7): close the drained scope at index `scope` — release its
            // tasks (destructor-aware) and tombstone the slot. Closes by index, not innermost, so a
            // sibling's still-open scope above it survives. Mirrors the VM's `Op::ScopeEndAt`.
            noeta_ir::Rvalue::ScopeEndAt { scope, span } => {
                let scope = self.eval_ir_atom(scope, frame)?;
                let Value::Int(idx) = scope else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "internal: $scope_end expects a scope index".to_string(),
                    ));
                };
                if (idx as usize) < self.scopes.len() {
                    self.close_scope(idx as usize);
                }
                Ok(Value::Unit)
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
                // Scan the built future for captured senders — the same producer holds the VM's
                // `isolate`-lowering (`Call`+`Spawn`) counts from the equivalent future (isolates I.4c).
                let holds = crate::collect_producer_channels(&future);
                for &cid in &holds {
                    self.add_producer_hold(cid);
                }
                let scope_idx = self.innermost_open();
                let task_idx = self.scopes[scope_idx].len();
                // The child inherits a snapshot of the spawner's task-local context.
                let context = self.ctx_current.clone();
                self.scopes[scope_idx].push(crate::Task {
                    future,
                    result: None,
                    cancelled: false,
                    polling: false,
                    context,
                    holds,
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
                    producers: 0,
                });
                Ok(Value::Tuple(Rc::new(vec![
                    Value::Sender(ChannelId::from_index(id)),
                    Value::Receiver(ChannelId::from_index(id)),
                ])))
            }
            noeta_ir::Rvalue::TypedModuleCall {
                module,
                func,
                args,
                recipe,
                dynamic,
                span,
            } => {
                let arg_vals: Vec<Value> = args
                    .iter()
                    .map(|a| self.eval_ir_atom(a, frame))
                    .collect::<Result<_, _>>()?;
                // The recipe: baked at the call site, or resolved per-instantiation
                // through the enclosing forwarding fn's hidden slot — an index into the
                // program's type-argument table. A table entry without a recipe is statically
                // rejected at the instantiating call; the runtime check is the safety net.
                let recipe = match dynamic {
                    Some(slot) => {
                        let idx = self.eval_ir_atom(slot, frame)?;
                        let Value::Int(i) = idx else {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                "corrupt hidden type-argument slot".to_string(),
                            ));
                        };
                        self.type_args
                            .get(i as usize)
                            .and_then(|e| e.recipe.clone())
                    }
                    None => recipe.clone(),
                };
                // The recipe is required; its absence was already reported by the checker.
                let Some(recipe) = recipe else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{module}.{func}::<T>(...)` has no resolved result type"),
                    ));
                };
                // Route through the registry's call-site-typed seam: the module's `typed_dispatch`,
                // threaded the recipe, builds the whole `NativeOut` tree (already carrying its
                // declared wrapper — `Ok`/`Err` for a `Result` shape, `Some`/`None` for `Option`),
                // which materializes to a value of `T`. No function name is special-cased —
                // `json.parse`/`try_parse` are registered like any extension's typed functions.
                // Mirrors the VM, so the two backends agree by construction.
                let reg = self.reg();
                let ext_mod = reg
                    .find_module(module)
                    .filter(|_| reg.find_typed_function(module, func).is_some());
                let Some(typed_dispatch) = ext_mod.and_then(|m| m.typed_dispatch) else {
                    return Err(self.runtime_error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!(
                            "`{module}.{func}::<T>(...)` is not a call-site-typed native function"
                        ),
                    ));
                };
                let deep = ext_mod.is_some_and(|m| m.deep_marshal);
                let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
                    arg_vals.iter().map(crate::value_to_native_deep).collect()
                } else {
                    arg_vals
                        .iter()
                        .map(|a| crate::marshal_native_arg(a, reg))
                        .collect()
                };
                match typed_dispatch(func, &mut *self.host, &nargs, &recipe) {
                    Ok(out) => {
                        // The aborting door (`json.parse::<T>`): a validation rejection that reaches
                        // the top (not recovered by a `Result` wrapper) aborts with the same
                        // path-precise message the abort door already uses for shape failures.
                        let mut path = String::new();
                        match self.materialize_recipe(out, &mut path, *span)? {
                            MatOut::Value(v) => Ok(v),
                            MatOut::Rejected(e) => {
                                Err(self.std_dispatch_error(e.into_std_error(), *span))
                            }
                        }
                    }
                    Err(error) => Err(self.std_dispatch_error(error, *span)),
                }
            }
            noeta_ir::Rvalue::TypedMethodCall {
                recv,
                method,
                args,
                recipe,
                dynamic,
                span,
            } => {
                // The extern-METHOD twin of `TypedModuleCall` above, step for step —
                // the only differences are that the receiver's own runtime identity selects the
                // type (no module name) and the dispatch takes the receiver. Mirrors the VM.
                let recv_val = self.eval_ir_atom(recv, frame)?;
                let arg_vals: Vec<Value> = args
                    .iter()
                    .map(|a| self.eval_ir_atom(a, frame))
                    .collect::<Result<_, _>>()?;
                let recipe = match dynamic {
                    Some(slot) => {
                        let idx = self.eval_ir_atom(slot, frame)?;
                        let Value::Int(i) = idx else {
                            return Err(self.runtime_error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                "corrupt hidden type-argument slot".to_string(),
                            ));
                        };
                        self.type_args
                            .get(i as usize)
                            .and_then(|e| e.recipe.clone())
                    }
                    None => recipe.clone(),
                };
                let Some(recipe) = recipe else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{method}::<T>(...)` has no resolved result type"),
                    ));
                };
                let Value::Extern(cell) = &recv_val else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{method}::<T>(...)` needs a native receiver"),
                    ));
                };
                let identity = cell.borrow().type_identity();
                let reg = self.reg();
                let deep = reg
                    .find_type_qualified(identity)
                    .is_some_and(|t| t.deep_marshal);
                let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
                    arg_vals.iter().map(crate::value_to_native_deep).collect()
                } else {
                    arg_vals
                        .iter()
                        .map(|a| crate::marshal_native_arg(a, reg))
                        .collect()
                };
                let out = self.reg().dispatch_typed_method(
                    &mut **cell.borrow_mut(),
                    method,
                    &mut *self.host,
                    &nargs,
                    &recipe,
                );
                match out {
                    Ok(out) => {
                        let mut path = String::new();
                        match self.materialize_recipe(out, &mut path, *span)? {
                            MatOut::Value(v) => Ok(v),
                            MatOut::Rejected(e) => {
                                Err(self.std_dispatch_error(e.into_std_error(), *span))
                            }
                        }
                    }
                    Err(error) => Err(self.std_dispatch_error(error, *span)),
                }
            }
            noeta_ir::Rvalue::DecodeTyped { name, text, span } => {
                // The router-facing runtime decode (L2.2 DI). Fully recoverable — an unknown type
                // name, a non-string operand, or a malformed body all become `Result.Err` wrapping
                // a path-carrying `JsonError` (the same error story as `json.try_parse::<T>`).
                // Mirrors the recoverable `try_parse` branch above, but the recipe is looked up by
                // runtime type name rather than baked at the call site.
                let name_val = self.eval_ir_atom(name, frame)?;
                let text_val = self.eval_ir_atom(text, frame)?;
                let (Value::Str(type_name), Value::Str(text)) = (&name_val, &text_val) else {
                    return Err(self.runtime_error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "`json.decode_typed` expects two `string` arguments".to_string(),
                    ));
                };
                let err = |error: noeta_stdlib::json::JsonError| {
                    crate::builtin_enum(
                        "Result",
                        "Err",
                        vec![Value::Extern(Rc::new(RefCell::new(
                            noeta_stdlib::ExternBox::new(error),
                        )))],
                    )
                };
                match self.deserialize_recipes.get(type_name).cloned() {
                    None => Ok(err(noeta_stdlib::json::JsonError::unknown_type(type_name))),
                    Some(recipe) => match noeta_stdlib::json::try_parse_typed(text, &recipe) {
                        Ok(out) => {
                            // The recoverable router door: a validation rejection is threaded into
                            // the `Result.Err(JsonError)`, exactly like a shape failure.
                            let mut path = String::new();
                            match self.materialize_recipe(out, &mut path, *span)? {
                                MatOut::Value(value) => {
                                    Ok(crate::builtin_enum("Result", "Ok", vec![value]))
                                }
                                MatOut::Rejected(e) => Ok(err(e)),
                            }
                        }
                        Err(error) => Ok(err(error)),
                    },
                }
            }
        }
    }

    /// Materialize a `json.parse::<T>` result tree ([`noeta_stdlib::NativeOut`]) into a value of `T`,
    /// running any `Validate::validate` **bottom-up**. A struct is built through
    /// [`Self::construct_object`] — its real registered definition, so the instance has its
    /// methods/defaults exactly like a literal; the VM builds a matching same-name shape, so both
    /// backends agree.
    ///
    /// `path` mirrors the decode walk's path stack (`items[2].price`) so a validation rejection
    /// names its exact location. A rejection is returned as [`MatOut::Rejected`] — it propagates up
    /// through containers (short-circuiting a container *before* its own `validate` runs, so a
    /// container only ever validates already-valid fields) until a `Result`-wrapped door recovers
    /// it into a `Result.Err` or the aborting door raises it.
    fn materialize_recipe(
        &mut self,
        out: noeta_stdlib::NativeOut,
        path: &mut String,
        span: Span,
    ) -> Eval<MatOut> {
        use noeta_stdlib::json::{push_index, push_member};
        use noeta_stdlib::{NativeOut, Scalar};
        Ok(match out {
            NativeOut::Scalar(Scalar::Int(n)) => MatOut::Value(Value::Int(n)),
            NativeOut::Scalar(Scalar::Float(f)) => MatOut::Value(Value::Float(f)),
            NativeOut::Scalar(Scalar::F32(f)) => MatOut::Value(Value::F32(f)),
            NativeOut::Scalar(Scalar::Bool(b)) => MatOut::Value(Value::Bool(b)),
            NativeOut::Str(s) => MatOut::Value(Value::Str(s)),
            NativeOut::Bytes(b) => MatOut::Value(Value::Bytes(Rc::new(b))),
            NativeOut::Unit => MatOut::Value(Value::Unit),
            // An extern value — the error arm of a `Result`-wrapped door (`json.try_parse::<T>` →
            // `Result.Err(JsonError)`) carries a path-rich extern; a recipe decode of `T` itself
            // never yields one, only a wrapper's `Err` does.
            NativeOut::Extern(e) => MatOut::Value(Value::Extern(Rc::new(RefCell::new(e)))),
            // An enum value: decoded from a `TypeRecipe::Enum`, or carried by
            // a native `Result`/`Option` wrapper. Both build through the
            // ordinary `materialize_native` path, so a decoded case is indistinguishable from a
            // source-written one. Only a recipe door sets `has_validator`, and it re-enters exactly
            // as a struct's does — the case is built first, then `validate()` runs, so a rejection
            // short-circuits before this node becomes a `Value`.
            out @ NativeOut::Variant {
                has_validator: true,
                ..
            } => {
                let value = crate::materialize_native(out);
                if let Some(rejection) = self.run_validator(value.clone(), path, span)? {
                    return Ok(MatOut::Rejected(rejection));
                }
                MatOut::Value(value)
            }
            out @ NativeOut::Variant { .. } => MatOut::Value(crate::materialize_native(out)),
            // A native class instance — like a `Variant`, never decoded
            // from a JSON recipe, but a native `Result`/`Option` wrapper may carry one.
            out @ NativeOut::Instance { .. } => MatOut::Value(crate::materialize_native(out)),
            // A `TypeRecipe` names only JSON shapes; async work, bulk scalar vectors (a packed
            // reduction's result), and an in-place instance mutation (boundary 1, only a class
            // method returns it) can never decode from one.
            NativeOut::Spawn(_) | NativeOut::Scalars(_) | NativeOut::InstanceUpdate { .. } => {
                unreachable!("json recipes never produce spawn/bulk-scalar/update results")
            }
            NativeOut::None => MatOut::Value(crate::builtin_enum("Option", "none", vec![])),
            NativeOut::Some(inner) => match self.materialize_recipe(*inner, path, span)? {
                MatOut::Rejected(e) => MatOut::Rejected(e),
                MatOut::Value(v) => MatOut::Value(crate::builtin_enum("Option", "some", vec![v])),
            },
            // A `Result`-wrapped call-site-typed door (`json.try_parse::<T>`) hands back its whole
            // `Result` tree — success as `Ok`, a decode failure as `Err` (a path-carrying extern).
            // This is the **recovery point**: a validation rejection under this wrapper becomes the
            // door's `Result.Err(JsonError)` rather than an abort.
            NativeOut::Ok(inner) => match self.materialize_recipe(*inner, path, span)? {
                MatOut::Rejected(e) => MatOut::Value(crate::builtin_enum(
                    "Result",
                    "Err",
                    vec![crate::json_error_value(e)],
                )),
                MatOut::Value(v) => MatOut::Value(crate::builtin_enum("Result", "Ok", vec![v])),
            },
            NativeOut::Err(inner) => match self.materialize_recipe(*inner, path, span)? {
                MatOut::Rejected(e) => MatOut::Rejected(e),
                MatOut::Value(v) => MatOut::Value(crate::builtin_enum("Result", "Err", vec![v])),
            },
            NativeOut::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for (i, item) in items.into_iter().enumerate() {
                    let mark = push_index(path, i);
                    let outcome = self.materialize_recipe(item, path, span)?;
                    path.truncate(mark);
                    match outcome {
                        MatOut::Rejected(e) => return Ok(MatOut::Rejected(e)),
                        MatOut::Value(v) => values.push(v),
                    }
                }
                MatOut::Value(Value::list(values))
            }
            NativeOut::Map(entries) => {
                let mut map = std::collections::BTreeMap::new();
                for (key, value) in entries {
                    let mark = push_member(path, &key);
                    let outcome = self.materialize_recipe(value, path, span)?;
                    path.truncate(mark);
                    match outcome {
                        MatOut::Rejected(e) => return Ok(MatOut::Rejected(e)),
                        MatOut::Value(v) => {
                            map.insert(noeta_stdlib::MapKey::from(key), v);
                        }
                    }
                }
                MatOut::Value(Value::map_value(Rc::new(map)))
            }
            NativeOut::Fielded {
                name,
                fields,
                kind,
                has_validator,
            } => {
                let mut field_values = Vec::with_capacity(fields.len());
                for (fname, fout) in fields {
                    let mark = push_member(path, &fname);
                    let outcome = self.materialize_recipe(fout, path, span)?;
                    path.truncate(mark);
                    match outcome {
                        MatOut::Rejected(e) => return Ok(MatOut::Rejected(e)),
                        MatOut::Value(v) => field_values.push((fname, span, v)),
                    }
                }
                // A `json.parse::<T>` result carries no reflected tag — its concrete type is
                // recovered head-only from the shape; untagged.
                //
                // A NATIVE fielded struct (native type-declaration unification) has no `.noe` def in
                // scope — it is scope-bound only under its short name, while the recipe carries the
                // QUALIFIED identity `type_to_recipe` keyed against `symbols.records` — so
                // `construct_object` (a scope lookup) cannot build it. Build the native struct-kind
                // Object directly instead, the same value-`TypeDef` shape `materialize_native` gives a
                // `NativeOut::Instance{kind:Struct}` (reused via `fielded_object`), keyed by the
                // qualified identity so the `has_validator` re-entry below dispatches `validate` to the
                // type's native `dispatch` (`call_method` → `find_class_method` → the fielded seam).
                let value = if self.reg().resolve_fielded(&name).is_some() {
                    let fields = field_values.into_iter().map(|(n, _, v)| (n, v)).collect();
                    // The recipe's kind, not an assumed `struct`: a native fielded type may be
                    // either, and building a class as a value would give it structural equality
                    // and copy-on-assign that its declaration does not have. A `.noe` type takes
                    // the branch below, where `construct_object` reads the kind off the
                    // declaration itself and so was never able to get this wrong.
                    let is_struct = matches!(kind, noeta_stdlib::FieldedKind::Struct);
                    crate::fielded_object(name.clone(), is_struct, fields)
                } else {
                    self.construct_object(&name, span, field_values, None, None, span)?
                };
                // Bottom-up: every field is materialized and validated above, so the type's own
                // `validate` sees an already-valid value. A rejection short-circuits before this
                // node becomes a `Value`.
                if has_validator
                    && let Some(rejection) = self.run_validator(value.clone(), path, span)?
                {
                    return Ok(MatOut::Rejected(rejection));
                }
                MatOut::Value(value)
            }
            // `Object` (shape-from-argument) is never produced by a recipe decode.
            NativeOut::Object(_) => {
                unreachable!("json.parse recipe decode never yields an Object result")
            }
        })
    }

    /// Run `value`'s `Validate::validate` — ordinary Noeta code re-entered
    /// mid-materialize. Returns `Some(message)` (the validator's own error message) when the
    /// validator's `Result` is an `Err`, `None` when it is `Ok`. The message is a `string`-typed
    /// error's bare string, or an `Error`-typed error's `message()`. Shared by the JSON recipe doors
    /// (which wrap it in a path-carrying `JsonError`), `from_bytes` (its own error channel), and the
    /// reflective `construct` door (whose channel is its own `Result<dyn, string>`).
    pub(crate) fn validate_message(&mut self, value: Value, span: Span) -> Eval<Option<String>> {
        let result = self.call_method(value, "validate", vec![], span)?;
        match crate::result_err_payload(&result) {
            Some(payload) => Ok(Some(self.validation_message(payload, span)?)),
            None => Ok(None),
        }
    }

    /// The JSON-recipe wrapper over [`Self::validate_message`]: a rejection becomes a path-carrying
    /// [`noeta_stdlib::json::JsonError`] naming `path`.
    fn run_validator(
        &mut self,
        value: Value,
        path: &str,
        span: Span,
    ) -> Eval<Option<noeta_stdlib::json::JsonError>> {
        Ok(self
            .validate_message(value, span)?
            .map(|message| noeta_stdlib::json::JsonError::validation(path, message)))
    }

    /// The message string of a validator's `Err` payload: a `string` payload directly, or an
    /// `Error`-implementing payload's `message()` (both guaranteed by the checker's `Validate`
    /// return-shape rule).
    fn validation_message(&mut self, payload: Value, span: Span) -> Eval<String> {
        if let Value::Str(s) = &payload {
            return Ok(s.clone());
        }
        let rendered = self.call_method(payload, "message", vec![], span)?;
        Ok(match rendered {
            Value::Str(s) => s,
            other => other.display(),
        })
    }

    /// How an **unhandled** `Err` payload describes itself for the E0069 abort: a `string` payload as
    /// itself, an `Error`-implementing payload through its `message()`, anything else through its
    /// ordinary display — so the abort always names what went wrong, whatever was put in the `Err`.
    ///
    /// The `Error` test is the shared trait-membership table (the same one `is dyn Error` consults,
    /// carrying declared `impl`s, `@derive`s, and native ABI declarations), so it agrees with "would
    /// a `message()` call resolve" by construction and the VM's twin decides identically.
    pub(crate) fn unhandled_error_message(&mut self, payload: Value, span: Span) -> Eval<String> {
        if let Value::Str(s) = &payload {
            return Ok(s.clone());
        }
        let implements_error = crate::value_nominal_name(&payload)
            .is_some_and(|name| self.reflection.type_implements(&name, "Error"));
        if !implements_error {
            return Ok(payload.display());
        }
        let rendered = self.call_method(payload, "message", vec![], span)?;
        Ok(match rendered {
            Value::Str(s) => s,
            other => other.display(),
        })
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
        // straight through (so a uniquely-owned list still mutates in place); a packed list has no
        // specialized `set`, so it materializes to a fresh boxed vector — correct,
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

    /// In-place map `set` for a marked self-update `m = m.set(k, v)`. The receiver has
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
        let Some(key) = crate::value_map_key(&key_value) else {
            // Defensive: a non-key value cannot occur for a checked map `set`; rebuild via the
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

    /// In-place map `remove` for a marked self-update `m = m.remove(k)`, the companion to
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
        let Some(key) = crate::value_map_key(&key_value) else {
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

    /// In-place struct reuse for a marked self-update `acc = Type { ...acc, f: v }` (the
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
                    // Reuse keeps the accumulator's existing reflected type — a self-update
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
                // literal's reflected type, matching the VM's copy branch.
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

    /// Set field `field` of an object to `new_value` (`x.f = v`). Semantics
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
                    // register the receiver so the exit reaper can reclaim it.
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

    /// Splice a door's hint against this frame's **render slots** — the tree-walker half of the one
    /// resolution, whose other half the VM runs on its own registers.
    ///
    /// `slots` is empty for every hint that mentions no type parameter, which is every door outside
    /// a generic body: the hint is handed back borrowed and nothing is evaluated. Otherwise each
    /// slot's `$ty<i>` local is read and its table entry consulted through
    /// [`noeta_ext_abi::resolve_hint`], the same function and the same table the VM reads — so a
    /// generic door cannot render one way compiled and another interpreted.
    fn resolve_render_hint<'h>(
        &mut self,
        hint: &'h noeta_ast::RenderHint,
        slots: &[noeta_ir::Atom],
        door: noeta_stdlib::HintDoor,
        frame: &mut Frame,
    ) -> Eval<Option<std::borrow::Cow<'h, noeta_ast::RenderHint>>> {
        if slots.is_empty() {
            return Ok(Some(std::borrow::Cow::Borrowed(hint)));
        }
        let mut values: Vec<i64> = Vec::with_capacity(slots.len());
        for slot in slots {
            values.push(match self.eval_ir_atom(slot, frame)? {
                Value::Int(i) => i,
                _ => noeta_stdlib::NO_TYPE_ARG,
            });
        }
        Ok(noeta_stdlib::resolve_hint(
            hint,
            &values,
            &self.type_arg_hints,
            door,
        ))
    }

    /// [`Self::resolve_render_hint`] for an ordering door, whose hint is optional and whose result
    /// is registered by span for the collection method that reads it.
    fn resolve_order_hint(
        &mut self,
        hint: &Option<Rc<noeta_ast::RenderHint>>,
        slots: &[noeta_ir::Atom],
        frame: &mut Frame,
    ) -> Eval<Option<Rc<noeta_ast::RenderHint>>> {
        let Some(hint) = hint else {
            return Ok(None);
        };
        if slots.is_empty() {
            return Ok(Some(Rc::clone(hint)));
        }
        let resolved =
            self.resolve_render_hint(hint, slots, noeta_stdlib::HintDoor::Order, frame)?;
        Ok(resolved.map(|h| Rc::new(h.into_owned())))
    }

    /// [`Self::resolve_order_hint`] for a **kept** hint — same splice, the JSON door's answer, and
    /// resolved at the call that binds the value rather than at the walk that reads it. The tick
    /// that serializes has no frame; this call does, and it is the one that knows the
    /// instantiation.
    fn resolve_push_hint(
        &mut self,
        hint: &Option<Rc<noeta_ast::RenderHint>>,
        slots: &[noeta_ir::Atom],
        frame: &mut Frame,
    ) -> Eval<Option<Rc<noeta_ast::RenderHint>>> {
        let Some(hint) = hint else {
            return Ok(None);
        };
        if slots.is_empty() {
            return Ok(Some(Rc::clone(hint)));
        }
        let resolved =
            self.resolve_render_hint(hint, slots, noeta_stdlib::HintDoor::Json, frame)?;
        Ok(resolved.map(|h| Rc::new(h.into_owned())))
    }

    /// The reflected type a **dynamic construction site** stamps (generic-in-generic construction):
    /// read the hidden type-argument slot `slot` and project its table entry through
    /// [`Self::type_arg_reprs`].
    ///
    /// The tree-walker twin of the VM's `Op::RetagDynamic`, resolving the same index in the same
    /// table — so the tag both backends write is the identical interned repr, not two independent
    /// reconstructions. A non-integer or out-of-range slot, or an entry with no reflection
    /// projection, answers `None`: the value stays untagged and `type_of` falls back to the head-only
    /// classification, exactly as an unrecorded construction site does.
    fn dynamic_construction_tag(
        &mut self,
        slot: &noeta_ir::Atom,
        frame: &mut Frame,
    ) -> Eval<Option<noeta_ast::reflect::TypeRepr>> {
        let Value::Int(i) = self.eval_ir_atom(slot, frame)? else {
            return Ok(None);
        };
        if i < 0 {
            return Ok(None);
        }
        Ok(self.type_arg_reprs.get(i as usize).cloned().flatten())
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
