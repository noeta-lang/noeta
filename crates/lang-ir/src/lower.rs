//! Lowering — `AST → Core IR`.
//!
//! A **pure, total** translation of a parsed program into the A-normal-form [`Program`].
//! "Total" in the sense the migration needs: every construct the IR interpreter supports
//! lowers; anything not yet covered returns [`Unsupported`] (never a panic, never a partial
//! tree), so the transitional differential can *skip* exactly the programs the IR path does
//! not yet handle — the same skip discipline the VM's bytecode path uses. As coverage grows
//! the set of `Unsupported` programs shrinks to empty.
//!
//! The single invariant the lowering establishes is **explicit evaluation order**: every
//! nested sub-expression becomes a preceding `let t = …` over atoms, in exactly the order
//! the tree-walker evaluates them. Order stops being something each backend rederives and
//! becomes IR structure.
//!
//! # On type facts
//!
//! Phase 1's IR carries no reference-counting annotations yet, so the lowering is purely
//! syntactic and does **not** consult the type checker. The reference-counting phase will
//! need per-value type facts (which fields are heap-bearing) that the checker does not
//! expose today; wiring that in is that phase's prerequisite, deliberately out of scope here
//! so the Phase 1 IR stays a faithful, RC-neutral mirror of the AST.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use lang_ast::{
    BinaryOp, ClosureBody, Expr, ForPattern as AstForPattern, Param, Program as AstProgram,
    Stmt as AstStmt, StrPart,
};
use lang_span::Span;

use crate::{
    Atom, Block, ClassDef, Const, Decl, EnumDef, Func, InterpPart, Program, Rvalue, Stmt,
    StructDef, Temp, Thunk,
};

/// A construct the lowering does not yet handle. Carried back so the caller can skip the
/// program (the transitional differential's "outside the IR subset" bucket), mirroring the
/// VM's `Unsupported`.
#[derive(Debug, Clone)]
pub struct Unsupported {
    /// A short, stable description of the unhandled construct (for diagnostics/tests).
    pub feature: &'static str,
    pub span: Span,
}

impl Unsupported {
    /// Build an "outside the IR subset" marker. The lowering is now **total** over the current
    /// AST — every construct lowers — so this is unused today; it is retained as the skip path
    /// the transitional differential expects, ready for any new AST node added ahead of its
    /// lowering (and to keep the one-slice-at-a-time discipline available).
    #[allow(dead_code)]
    fn at(feature: &'static str, span: Span) -> Unsupported {
        Unsupported { feature, span }
    }
}

/// Where a function's body comes from: an arrow expression (its value is the return) or a
/// statement block (returns via `return`, else unit).
enum BodyKind<'a> {
    Arrow(&'a Expr),
    Block(&'a [AstStmt]),
}

/// Lower a whole parsed program to the Core IR, or report the first construct outside the
/// currently-supported subset. List literals lower to the boxed [`Rvalue::List`] and `list[i].f`
/// reads to the unfused [`Rvalue::Index`] + [`Rvalue::Field`]; see [`lower_with_sites`] to also
/// stream `List<packed>` literals into a flat buffer and fuse indexed field reads.
pub fn lower(program: &AstProgram) -> Result<Program, Unsupported> {
    lower_with_sites(
        program,
        &HashMap::new(),
        &HashSet::new(),
        &HashMap::new(),
        &HashSet::new(),
    )
}

/// As [`lower`], but driven by the checker's lowering-site maps (both pure functions of the program,
/// so the optimizations they enable stay invisible to `RunResult`):
///
/// * A list literal whose span appears in `packed_list_sites` (the `List<@packed struct>` map,
///   [`lang_ast::reflect::PackedLayout`]) lowers to a **streaming** flat build —
///   [`Rvalue::PackedListNew`] then one [`Rvalue::PackedListPush`] per element — instead of a boxed
///   [`Rvalue::List`].
/// * A `list[i].field` member read whose span appears in `index_field_sites` (the checker's set of
///   member accesses whose index receiver is a built-in `List`) fuses to a single
///   [`Rvalue::IndexField`], so a packed element's field is read without materializing the element.
///
/// The production execution paths (`lang run`, the conformance reference, the bytecode compiler) pass
/// the maps; the REPL and IR corpus pass empty ones and stay on the boxed/unfused path.
pub fn lower_with_sites(
    program: &AstProgram,
    packed_list_sites: &HashMap<Span, lang_ast::reflect::PackedLayout>,
    index_field_sites: &HashSet<Span>,
    ext_call_sites: &HashMap<Span, lang_stdlib::TypeRecipe>,
    for_stream_sites: &HashSet<Span>,
) -> Result<Program, Unsupported> {
    let mut lowerer = Lowerer {
        temps: 0,
        packed_list_sites,
        index_field_sites,
        ext_call_sites,
        for_stream_sites,
    };
    let top = lowerer.lower_body(&program.stmts)?;
    Ok(Program {
        top,
        temp_count: lowerer.temps,
        span: program.span,
    })
}

/// Carries the temporary counter for the function frame currently being lowered. One
/// `Lowerer` field is reused across nested frames by save/restore (see `lower_func`), so the
/// counter always reflects the innermost activation.
struct Lowerer<'a> {
    /// The next free temporary index in the current frame; also the running frame size.
    temps: u32,
    /// The checker's `List<packed>` construction-site map (keyed by list-literal span). Empty on
    /// the boxed-only paths; non-empty enables streaming flat-buffer construction in `Expr::List`.
    packed_list_sites: &'a HashMap<Span, lang_ast::reflect::PackedLayout>,
    /// The checker's fusable `list[i].field` set (keyed by the member-access span). Empty on the
    /// unfused paths; a hit enables emitting [`Rvalue::IndexField`] in the `Expr::Member` arm.
    index_field_sites: &'a HashSet<Span>,
    /// The checker's call-site-typed native-call recipes (`json.parse::<T>`), keyed by the
    /// `Expr::TypedModuleCall` span. Baked into [`Rvalue::ExtCall`]; empty on the bare `lower` path.
    ext_call_sites: &'a HashMap<Span, lang_stdlib::TypeRecipe>,
    /// The checker's streaming-`for` set (keyed by the `for` statement's span): the iterable is
    /// statically an `Iterator<T>`, so the lowered [`Stmt::For`] gets `stream: true` (Track I.2).
    for_stream_sites: &'a HashSet<Span>,
}

impl Lowerer<'_> {
    /// Allocate a fresh frame-local temporary.
    fn fresh(&mut self) -> Temp {
        let t = Temp(self.temps);
        self.temps += 1;
        t
    }

    /// Lower a statement-position block of statements (no value), in the current frame.
    fn lower_body(&mut self, stmts: &[AstStmt]) -> Result<Block, Unsupported> {
        let mut out = Vec::new();
        for stmt in stmts {
            self.lower_stmt(stmt, &mut out)?;
        }
        Ok(Block::stmts(out))
    }

    fn lower_stmt(&mut self, stmt: &AstStmt, out: &mut Vec<Stmt>) -> Result<(), Unsupported> {
        match stmt {
            AstStmt::Echo { value, span } => {
                let atom = self.lower_expr(value, out)?;
                out.push(Stmt::Echo {
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                span,
                ..
            } => {
                // A `x.f = v` field-set is parsed as a reassignment of `x` whose value is an
                // `Expr::FieldSet`; flag it so the backends skip the immutable-reassignment check
                // (object-model slice 2b′ — the checker enforces the `struct` case statically).
                let field_assign = matches!(value, Expr::FieldSet { .. });
                let atom = self.lower_expr(value, out)?;
                out.push(Stmt::Bind {
                    mut_decl: *mut_decl,
                    name: name.clone(),
                    name_span: *name_span,
                    value: atom,
                    field_assign,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Expr { expr, .. } => {
                // Evaluate for effect; the result atom is discarded. If it landed in a temp,
                // release that temp here so its reference does not outlive the statement (the
                // tree-walker drops the corresponding intermediate at the same point).
                let atom = self.lower_expr(expr, out)?;
                if let Atom::Temp(t) = atom {
                    out.push(Stmt::Drop(t));
                }
                Ok(())
            }
            AstStmt::Return { value, span } => {
                let atom = match value {
                    Some(expr) => Some(self.lower_expr(expr, out)?),
                    None => None,
                };
                out.push(Stmt::Return {
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            // `yield` (Track G) is desugared into the generator state machine in a dedicated pass
            // (Track G.1b). Until that lands, every generator is gated as a checker error (E0039 "not
            // yet executable"), so a `yield` never reaches a *run* path through a clean program. To
            // keep lowering **total** (the `lower(...).expect(...)` invariant the eval backend and the
            // determinism property test rely on — both lower regardless of diagnostics), the interim
            // lowering evaluates the operand for effect and discards it, like an expression statement.
            // Replaced by the real state-machine desugar in G.1b.
            AstStmt::Yield { value, .. } => {
                let atom = self.lower_expr(value, out)?;
                if let Atom::Temp(t) = atom {
                    out.push(Stmt::Drop(t));
                }
                Ok(())
            }
            AstStmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                let cond = self.lower_expr(cond, out)?;
                let then_block = self.lower_body(then_body)?;
                let else_block = match else_body {
                    Some(body) => Some(self.lower_body(body)?),
                    None => None,
                };
                out.push(Stmt::If {
                    cond,
                    then_block,
                    else_block,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::While { cond, body, span } => {
                let cond = self.lower_value_block(cond)?;
                let body = self.lower_body(body)?;
                out.push(Stmt::While {
                    cond,
                    body,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                let stream = self.for_stream_sites.contains(span);
                let iterable = self.lower_expr(iterable, out)?;
                match pattern {
                    AstForPattern::Single { .. } => {
                        let body = self.lower_body(body)?;
                        out.push(Stmt::For {
                            pattern: pattern.clone(),
                            iterable,
                            body,
                            span: *span,
                            stream,
                        });
                    }
                    // A tuple destructure `for (a, b, …) in …` (object-model slice 4b) desugars to a
                    // single hidden element var plus per-position `.N` projections at the top of the
                    // body — so the IR for-loop only ever carries a `Single` pattern and reuses the
                    // existing `TupleIndex` machinery (no new runtime op).
                    AstForPattern::Tuple { names, .. } => {
                        let elem = format!("$for{}", self.fresh().0);
                        let mut body_stmts = Vec::new();
                        self.destructure_into(&elem, names, &mut body_stmts);
                        for s in body {
                            self.lower_stmt(s, &mut body_stmts)?;
                        }
                        out.push(Stmt::For {
                            pattern: AstForPattern::Single {
                                name: elem,
                                name_span: *span,
                            },
                            iterable,
                            body: Block::stmts(body_stmts),
                            span: *span,
                            stream,
                        });
                    }
                }
                Ok(())
            }
            AstStmt::Destructure {
                mut_decl,
                targets,
                value,
                span,
            } => {
                // Evaluate the value once, bind it to a hidden holder var (so its lifetime spans all
                // projections — a bare temp would be consumed by the first read), then bind each
                // target to its tuple position. Object-model slice 4b.
                let value_atom = self.lower_expr(value, out)?;
                let holder = format!("$destr{}", self.fresh().0);
                out.push(Stmt::Bind {
                    mut_decl: false,
                    name: holder.clone(),
                    name_span: *span,
                    value: value_atom,
                    field_assign: false,
                    span: *span,
                });
                for (i, (name, name_span)) in targets.iter().enumerate() {
                    let proj = self.emit(
                        out,
                        Rvalue::TupleIndex {
                            receiver: Atom::Var {
                                name: holder.clone(),
                                span: *name_span,
                            },
                            index: i as u32,
                            span: *name_span,
                        },
                        *name_span,
                    );
                    out.push(Stmt::Bind {
                        mut_decl: *mut_decl,
                        name: name.clone(),
                        name_span: *name_span,
                        value: proj,
                        field_assign: false,
                        span: *name_span,
                    });
                }
                Ok(())
            }
            AstStmt::Break { span } => {
                out.push(Stmt::Break { span: *span });
                Ok(())
            }
            AstStmt::Continue { span } => {
                out.push(Stmt::Continue { span: *span });
                Ok(())
            }
            AstStmt::Fn(decl) => {
                let func =
                    self.lower_func(&decl.params, BodyKind::Block(&decl.body), decl.span, true)?;
                out.push(Stmt::Decl(Decl::Fn {
                    name: decl.name.clone(),
                    func: Rc::new(func),
                    span: decl.span,
                }));
                Ok(())
            }
            AstStmt::Class(decl) => {
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func =
                        self.lower_func(&m.params, BodyKind::Block(&m.body), m.span, true)?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                // The `destruct` block lowers to a parameterless block [`Func`] (fields resolve
                // against the receiver, like a method), so the VM can compile it to a prototype.
                let destructor = match &decl.destructor {
                    Some(body) => Some(Rc::new(self.lower_func(
                        &[],
                        BodyKind::Block(body),
                        decl.span,
                        false,
                    )?)),
                    None => None,
                };
                let field_defaults = self.lower_field_defaults(&decl.fields)?;
                out.push(Stmt::Decl(Decl::Class(ClassDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    field_defaults,
                    destructor,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Enum(decl) => {
                // An enum carries inherent methods and `impl`-block methods (the unified body,
                // object-model slice 3), lowered to IR funcs exactly like a struct's. Variant/derive
                // data stays on the surface `decl`.
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func =
                        self.lower_func(&m.params, BodyKind::Block(&m.body), m.span, true)?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                out.push(Stmt::Decl(Decl::Enum(EnumDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Struct(decl) => {
                // A struct carries inherent methods and `impl`-block methods (the unified body),
                // lowered to IR funcs exactly like a class's — minus any `destruct` (structs have
                // none). Field/derive data stays on the surface `decl`.
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func =
                        self.lower_func(&m.params, BodyKind::Block(&m.body), m.span, true)?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                let field_defaults = self.lower_field_defaults(&decl.fields)?;
                out.push(Stmt::Decl(Decl::Struct(StructDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    field_defaults,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Use { path, names, span } => {
                out.push(Stmt::Decl(Decl::Use {
                    path: path.clone(),
                    names: names.clone(),
                    span: *span,
                }));
                Ok(())
            }
            // A standalone `impl` and a `namespace` have no runtime effect in the tree-walker
            // (both are `Ok(Flow::Normal)` no-ops), so they lower to nothing.
            AstStmt::Impl(_) | AstStmt::Namespace { .. } => Ok(()),
            // A dev-tier block reaching lowering is an *inactive* residual (object-model slice 6):
            // the tier-strip pass already spliced any *active* block's items into the statement
            // stream and dropped the inactive ones, so an inactive block lowers to nothing (stripped
            // from the build, identically on both backends since both lower the same program).
            AstStmt::TierBlock { .. } => Ok(()),
        }
    }

    /// Lower a function/method/closure into an IR [`Func`] with its own temporary frame. The
    /// enclosing frame's temporary counter is saved and restored, so a nested function (a
    /// closure inside a function body) is numbered independently and the outer numbering
    /// continues afterward.
    fn lower_func(
        &mut self,
        params: &[Param],
        body: BodyKind<'_>,
        span: Span,
        generator: bool,
    ) -> Result<Func, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        let param_names = params.iter().map(|p| p.name.clone()).collect();
        // Defaults are evaluated in the captured scope at call time, each in its own frame, so
        // lower each as a self-contained thunk (this also restores `self.temps` to 0 between
        // thunks, keeping the body's numbering independent).
        let mut defaults = Vec::with_capacity(params.len());
        for p in params {
            match &p.default {
                Some(expr) => defaults.push(Some(self.lower_thunk(expr)?)),
                None => defaults.push(None),
            }
        }
        let body = match body {
            BodyKind::Arrow(expr) => self.lower_value_block(expr)?,
            // A generator (a function whose body contains `yield`, Track G) lowers to a state-machine
            // step closure wrapped in `make_gen` — not the body's statements directly. `generator` is
            // set only at the call sites where a generator is legal (named `fn`/methods), never for a
            // closure or the synthesized step closure itself, so the desugar applies exactly once.
            BodyKind::Block(stmts) if generator && body_has_yield(stmts) => {
                self.lower_generator(stmts, span)?
            }
            BodyKind::Block(stmts) => self.lower_body(stmts)?,
        };
        let temp_count = self.temps;
        self.temps = outer;
        Ok(Func {
            params: param_names,
            defaults,
            body,
            temp_count,
            span,
        })
    }

    /// Lower a **generator** body (Track G.1b) — a function whose top-level statements include
    /// `yield` — into the state-machine representation: a step closure (the desugared dispatch) wrapped
    /// in [`Rvalue::MakeGen`]. The body becomes
    ///
    /// ```text
    /// mut $state = 0                    // dispatch discriminant
    /// mut <local> = none               // every top-level local, hoisted to a cell
    /// ...
    /// let $step = ($resume) => { <dispatch> }   // captures $state + the hoisted cells
    /// return make_gen($step)
    /// ```
    ///
    /// where the dispatch is an if-chain over `$state`: state *k* runs segment *k* (the statements up
    /// to the *k*-th top-level `yield`), advances `$state`, and `return some(<yielded>)`; the final
    /// segment runs the trailing statements and `return none`. The hoisted `mut` locals become
    /// captured cells (the original `let x = …` inside the closure reassigns the outer binding rather
    /// than declaring a closure-local — the language's bare-assignment rule), so a value computed in
    /// one segment survives into the next. Straight-line only: a `yield` nested in control flow is
    /// rejected by the checker (E0039, "not yet supported — Track G.2") and never reaches a *run*; if
    /// one survives in a check-failed program it stays a `Stmt::Yield` inside a segment and lowers
    /// through the interim discard arm, keeping lowering total.
    fn lower_generator(&mut self, stmts: &[AstStmt], span: Span) -> Result<Block, Unsupported> {
        let desugar = desugar_generator(stmts, span);
        let mut out = Vec::new();
        for stmt in &desugar.prelude {
            self.lower_stmt(stmt, &mut out)?;
        }
        let step = self.lower_expr(&desugar.step, &mut out)?;
        let generator = self.emit(&mut out, Rvalue::MakeGen { step, span }, span);
        out.push(Stmt::Return {
            value: Some(generator),
            span,
        });
        Ok(Block::stmts(out))
    }

    /// Lower each field carrying a default (`x: T = expr`) to a parameterless value [`Thunk`]
    /// (object-model slice 5), keyed by field name. A defaulted field's thunk is run in the type's
    /// definition scope at construction when a literal omits it — the same self-contained-thunk
    /// machinery as a defaulted parameter. A mandatory field contributes nothing.
    fn lower_field_defaults(
        &mut self,
        fields: &[lang_ast::FieldDecl],
    ) -> Result<Vec<(String, Thunk)>, Unsupported> {
        let mut defaults = Vec::new();
        for f in fields {
            if let Some(expr) = &f.default {
                defaults.push((f.name.clone(), self.lower_thunk(expr)?));
            }
        }
        Ok(defaults)
    }

    /// Lower a defaulted-parameter expression into a self-contained value-producing [`Thunk`]
    /// with its own temporary frame (defaults run independently in the captured scope).
    fn lower_thunk(&mut self, expr: &Expr) -> Result<Thunk, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        let body = self.lower_value_block(expr)?;
        let temp_count = self.temps;
        self.temps = outer;
        Ok(Thunk { body, temp_count })
    }

    /// Lower an expression into a fresh value-position [`Block`] (its computed `let`s plus a
    /// tail atom). Used where an expression is re-evaluated or evaluated lazily — a `while`
    /// condition, a defaulted parameter — so it cannot be hoisted into the surrounding
    /// straight-line sequence.
    fn lower_value_block(&mut self, expr: &Expr) -> Result<Block, Unsupported> {
        let mut stmts = Vec::new();
        let atom = self.lower_expr(expr, &mut stmts)?;
        Ok(Block {
            stmts,
            tail: Some(atom),
        })
    }

    /// Lower an expression to an [`Atom`], emitting the `let`s that compute any
    /// sub-expressions into `out` first (A-normal form). Literals and identifiers reduce
    /// directly to an atom with no `let`.
    fn lower_expr(&mut self, expr: &Expr, out: &mut Vec<Stmt>) -> Result<Atom, Unsupported> {
        match expr {
            Expr::Str { value, .. } => Ok(Atom::Const(Const::Str(value.clone()))),
            Expr::Int { value, .. } => Ok(Atom::Const(Const::Int(*value))),
            Expr::Float { value, .. } => Ok(Atom::Const(Const::Float(*value))),
            Expr::F32 { value, .. } => Ok(Atom::Const(Const::F32(*value))),
            Expr::Bool { value, .. } => Ok(Atom::Const(Const::Bool(*value))),
            Expr::Ident { name, span } => Ok(Atom::Var {
                name: name.clone(),
                span: *span,
            }),
            Expr::Unary { op, operand, span } => {
                let operand = self.lower_expr(operand, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Unary {
                        op: *op,
                        operand,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Binary { op, lhs, rhs, span } if matches!(op, BinaryOp::And | BinaryOp::Or) => {
                self.lower_logical(*op, lhs, rhs, *span, out)
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let lhs = self.lower_expr(lhs, out)?;
                let rhs = self.lower_expr(rhs, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Binary {
                        op: *op,
                        lhs,
                        rhs,
                        reuse: false,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::List { items, span } => {
                // A `List<@packed struct>` literal (its span recorded by the checker) streams into a
                // flat buffer: allocate, then build-and-push each element in turn so only one element
                // object is ever live (P-PACK 2.5). Any other list builds the boxed `Rvalue::List`,
                // materializing all element atoms first.
                if let Some(layout) = self.packed_list_sites.get(span) {
                    let mut acc = self.emit(
                        out,
                        Rvalue::PackedListNew {
                            layout: layout.clone(),
                            span: *span,
                        },
                        *span,
                    );
                    for item in items {
                        let item_span = item.span();
                        let value = self.lower_expr(item, out)?;
                        acc = self.emit(
                            out,
                            Rvalue::PackedListPush {
                                list: acc,
                                value,
                                span: item_span,
                            },
                            item_span,
                        );
                    }
                    return Ok(acc);
                }
                let mut atoms = Vec::with_capacity(items.len());
                for item in items {
                    atoms.push(self.lower_expr(item, out)?);
                }
                Ok(self.emit(
                    out,
                    Rvalue::List {
                        items: atoms,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Tuple { items, span } => {
                let mut atoms = Vec::with_capacity(items.len());
                for item in items {
                    atoms.push(self.lower_expr(item, out)?);
                }
                Ok(self.emit(
                    out,
                    Rvalue::Tuple {
                        items: atoms,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TupleIndex {
                        receiver,
                        index: *index,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let start = self.lower_expr(start, out)?;
                let end = self.lower_expr(end, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Range {
                        start,
                        end,
                        inclusive: *inclusive,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Map { entries, span } => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let key = self.lower_expr(k, out)?;
                    let value = self.lower_expr(v, out)?;
                    pairs.push((key, value));
                }
                Ok(self.emit(
                    out,
                    Rvalue::Map {
                        entries: pairs,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                let index = self.lower_expr(index, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Index {
                        receiver,
                        index,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Interp { parts, span } => {
                let mut ir_parts = Vec::with_capacity(parts.len());
                for part in parts {
                    match part {
                        StrPart::Literal(text) => ir_parts.push(InterpPart::Literal(text.clone())),
                        StrPart::Hole(e) => {
                            let atom = self.lower_expr(e, out)?;
                            ir_parts.push(InterpPart::Hole {
                                atom,
                                span: e.span(),
                            });
                        }
                    }
                }
                Ok(self.emit(
                    out,
                    Rvalue::Interp {
                        parts: ir_parts,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Call { callee, args, span } => {
                // A call whose callee is a member access is a method call; otherwise an
                // ordinary call. Evaluation order matches the tree-walker's `eval_call`:
                // receiver/callee first, then arguments left-to-right.
                if let Expr::Member {
                    receiver,
                    name,
                    name_span,
                    ..
                } = callee.as_ref()
                {
                    let receiver = self.lower_expr(receiver, out)?;
                    let arg_atoms = self.lower_args(args, out)?;
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: name.clone(),
                            name_span: *name_span,
                            args: arg_atoms,
                            reuse: false,
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let arg_atoms = self.lower_args(args, out)?;
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                // A `list[i].field` read the checker proved fusable (its index receiver is a built-in
                // `List`) lowers to one [`Rvalue::IndexField`] over the list and index atoms, so a
                // packed element's field is read without materializing the element (P-PACK 2.5+). Any
                // other member access lowers to the ordinary field load.
                if self.index_field_sites.contains(span)
                    && let Expr::Index {
                        receiver: list,
                        index,
                        ..
                    } = receiver.as_ref()
                {
                    let list = self.lower_expr(list, out)?;
                    let index = self.lower_expr(index, out)?;
                    return Ok(self.emit(
                        out,
                        Rvalue::IndexField {
                            receiver: list,
                            index,
                            field: name.clone(),
                            field_span: *name_span,
                            span: *span,
                        },
                        *span,
                    ));
                }
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Field {
                        receiver,
                        name: name.clone(),
                        name_span: *name_span,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                span,
            } => {
                // Lower receiver then value (left-to-right), matching the tree-walker's order.
                let receiver = self.lower_expr(receiver, out)?;
                let value = self.lower_expr(value, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::SetField {
                        receiver,
                        name: field.clone(),
                        name_span: *field_span,
                        value,
                        reuse: false,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                let scrut = self.lower_expr(scrutinee, out)?;
                let mut ir_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let body = self.lower_value_block(&arm.body)?;
                    ir_arms.push(crate::Arm {
                        pattern: arm.pattern.clone(),
                        body,
                        span: arm.span,
                    });
                }
                let dst = self.fresh();
                out.push(Stmt::Match {
                    scrutinee: scrut,
                    arms: ir_arms,
                    dst: Some(dst),
                    span: *span,
                });
                Ok(Atom::Temp(dst))
            }
            Expr::Try { expr, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Try {
                        operand,
                        // Filled by the drop-insertion pass; lowering emits none.
                        on_error: Vec::new(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => {
                let value = self.lower_expr(value, out)?;
                let fallback = self.lower_value_block(fallback)?;
                let dst = self.fresh();
                out.push(Stmt::Coalesce {
                    dst: Some(dst),
                    value,
                    fallback,
                    span: *span,
                });
                Ok(Atom::Temp(dst))
            }
            Expr::As { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::As {
                        operand,
                        ty: ty.clone(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeTest { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TypeTest {
                        operand,
                        ty: ty.clone(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeOf { value, span } => {
                let operand = self.lower_expr(value, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TypeOf {
                        operand,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::FromBytes { blob, span, .. } => {
                let blob = self.lower_expr(blob, out)?;
                // The element layout was recorded by the checker at this span in the same channel
                // list literals use (`packed_list_sites`); `None` means T was not packable (already
                // a checker error), and the backend then fails cleanly rather than mis-decoding.
                let layout = self.packed_list_sites.get(span).cloned();
                Ok(self.emit(
                    out,
                    Rvalue::FromBytes {
                        blob,
                        layout,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypedModuleCall {
                recv,
                func,
                args,
                span,
                ..
            } => {
                let module = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.clone(),
                    _ => String::new(),
                };
                let args = args
                    .iter()
                    .map(|a| self.lower_expr(a, out))
                    .collect::<Result<Vec<_>, _>>()?;
                // The recipe was resolved by the checker at this span (the same channel the other
                // typed sites use); `None` means `T` had no decoding (already a checker error).
                let recipe = self.ext_call_sites.get(span).cloned();
                Ok(self.emit(
                    out,
                    Rvalue::ExtCall {
                        module,
                        func: func.clone(),
                        args,
                        recipe,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::AttributesOf { ty, span } => Ok(self.emit(
                out,
                Rvalue::AttributesOf {
                    ty: ty.clone(),
                    span: *span,
                },
                *span,
            )),
            Expr::RolesOf { span } => Ok(self.emit(out, Rvalue::RolesOf { span: *span }, *span)),
            Expr::Invoke {
                recv,
                name,
                args,
                span,
            } => {
                let recv = self.lower_expr(recv, out)?;
                let name = self.lower_expr(name, out)?;
                let args = self.lower_expr(args, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Invoke {
                        recv,
                        name,
                        args,
                        span: *span,
                    },
                    *span,
                ))
            }
            // The return annotation is runtime-erased (the checker has already used it); lowering
            // ignores it, exactly as it ignores parameter type annotations. An arrow body lowers like
            // a value-returning expression; a block body lowers exactly like a named function's body.
            Expr::Closure {
                params, body, span, ..
            } => {
                let body_kind = match body {
                    lang_ast::ClosureBody::Expr(e) => BodyKind::Arrow(e),
                    lang_ast::ClosureBody::Block(stmts) => BodyKind::Block(stmts),
                };
                // A closure is never a generator: `yield` resets at a callable boundary (the checker
                // already rejects a `yield` inside a closure), and the generator desugar's own step
                // closure must not be re-desugared. So `generator` is `false` here.
                let func = self.lower_func(params, body_kind, *span, false)?;
                Ok(self.emit(
                    out,
                    Rvalue::Closure {
                        func: Rc::new(func),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Object(lit) => {
                // Evaluation order matches `eval_object`: the `..` spread first, then named
                // initializers left-to-right.
                let spread = match &lit.spread {
                    Some(s) => Some((self.lower_expr(s, out)?, s.span())),
                    None => None,
                };
                let mut fields = Vec::with_capacity(lit.fields.len());
                for init in &lit.fields {
                    let value = self.lower_expr(&init.value, out)?;
                    fields.push(crate::ObjectFieldInit {
                        name: init.name.clone(),
                        name_span: init.name_span,
                        value,
                    });
                }
                Ok(self.emit(
                    out,
                    Rvalue::Object {
                        type_name: lit.type_name.clone(),
                        type_name_span: lit.type_name_span,
                        fields,
                        spread,
                        // The reuse-analysis pass (Phase 5) sets this when it recognizes a self-update;
                        // lowering is reuse-neutral.
                        reuse: false,
                        span: lit.span,
                    },
                    lit.span,
                ))
            }
            Expr::Pipeline { left, right, span } => self.lower_pipeline(left, right, *span, out),
        }
    }

    /// Lower `left |> right`, desugaring to a call/method with `left` threaded as the leading
    /// argument — mirroring `eval_pipeline`. `left` is evaluated first (matching the
    /// tree-walker), then the callee/receiver, then any remaining arguments.
    fn lower_pipeline(
        &mut self,
        left: &Expr,
        right: &Expr,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let left_atom = self.lower_expr(left, out)?;
        match right {
            // `x |> f(a)` ⟶ `f(x, a)`; `x |> obj.m(a)` ⟶ `obj.m(x, a)`.
            Expr::Call { callee, args, span } => {
                if let Expr::Member {
                    receiver,
                    name,
                    name_span,
                    ..
                } = callee.as_ref()
                {
                    let receiver = self.lower_expr(receiver, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(a, out)?);
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: name.clone(),
                            name_span: *name_span,
                            args: arg_atoms,
                            reuse: false,
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(a, out)?);
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            // `x |> obj.m` ⟶ `obj.m(x)`.
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Method {
                        receiver,
                        name: name.clone(),
                        name_span: *name_span,
                        args: vec![left_atom],
                        reuse: false,
                        span: *span,
                    },
                    *span,
                ))
            }
            // `x |> f` ⟶ `f(x)`.
            _ => {
                let callee = self.lower_expr(right, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Call {
                        callee,
                        args: vec![left_atom],
                        span,
                    },
                    span,
                ))
            }
        }
    }

    /// Lower a call's argument list left-to-right (the tree-walker's order).
    fn lower_args(&mut self, args: &[Expr], out: &mut Vec<Stmt>) -> Result<Vec<Atom>, Unsupported> {
        let mut atoms = Vec::with_capacity(args.len());
        for arg in args {
            atoms.push(self.lower_expr(arg, out)?);
        }
        Ok(atoms)
    }

    /// Lower `a && b` / `a || b` to a [`Stmt::Logical`] writing into a fresh temp, so the
    /// right operand is evaluated lazily (a [`Block`]) rather than up-front.
    fn lower_logical(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let left = self.lower_expr(lhs, out)?;
        let mut right_stmts = Vec::new();
        let right_atom = self.lower_expr(rhs, &mut right_stmts)?;
        let dst = self.fresh();
        out.push(Stmt::Logical {
            dst: Some(dst),
            op,
            left,
            right: Block {
                stmts: right_stmts,
                tail: Some(right_atom),
            },
            span,
        });
        Ok(Atom::Temp(dst))
    }

    /// Emit `let t = rvalue` into `out` and return the new temp as an atom.
    fn emit(&mut self, out: &mut Vec<Stmt>, rvalue: Rvalue, span: Span) -> Atom {
        let dst = self.fresh();
        out.push(Stmt::Let { dst, rvalue, span });
        Atom::Temp(dst)
    }

    /// Emit, into `out`, the per-position `.N` projections that destructure a tuple held by the
    /// variable `holder` into `names` — `name_i = holder.i` (object-model slice 4b). Shared by the
    /// for-loop tuple pattern's body prologue (the binding-statement destructure inlines its own,
    /// since it carries a `mut` flag).
    fn destructure_into(&mut self, holder: &str, names: &[(String, Span)], out: &mut Vec<Stmt>) {
        for (i, (name, name_span)) in names.iter().enumerate() {
            let proj = self.emit(
                out,
                Rvalue::TupleIndex {
                    receiver: Atom::Var {
                        name: holder.to_string(),
                        span: *name_span,
                    },
                    index: i as u32,
                    span: *name_span,
                },
                *name_span,
            );
            out.push(Stmt::Bind {
                mut_decl: false,
                name: name.clone(),
                name_span: *name_span,
                value: proj,
                field_assign: false,
                span: *name_span,
            });
        }
    }
}

/// The synthetic dispatch discriminant cell of a desugared generator. `$`-prefixed, so it can never
/// collide with a source name (the lexer forbids `$` in identifiers).
const STATE_VAR: &str = "$state";
/// The ignored resume parameter of the step closure (one argument, forward-compatible with Track A's
/// real resume value).
const RESUME_PARAM: &str = "$resume";

/// The AST product of the generator desugar ([`Lowerer::lower_generator`]): the hoisted-local prelude
/// and the state-machine step closure. Kept as ordinary AST so the existing lowering paths produce the
/// IR — only the final `make_gen` wrapper is a dedicated rvalue.
struct GeneratorDesugar {
    prelude: Vec<AstStmt>,
    step: Expr,
}

/// Build the [`GeneratorDesugar`] for a generator body (Track G.1b straight-line + G.2 control flow).
///
/// The body is flattened into a **control-flow graph of states** — a stackless state machine. Each
/// `yield` is a suspend point; `if`/`while` carrying a `yield` (and any `break`/`continue` reaching an
/// enclosing flattened loop) become state transitions. The step closure is a `while true` dispatch
/// loop over `$state`: state *k* runs its statements and then either **returns** (a `yield` → `some`,
/// or exhaustion → `none`) or **jumps** (`$state = j; continue`) to re-enter the loop at another
/// state. A construct with no `yield` and no escaping control flow is emitted **verbatim** (a `match`,
/// a self-contained `for`/`while`), so it runs whole within one step. A `yield` inside a `for` is
/// rejected by the checker (E0039) and never reaches here through a clean program.
///
/// Every top-level / flattened-level local is conservatively hoisted to a captured `mut` cell (the
/// prelude declares it; its in-body `let x = …` is rewritten to a bare assignment that reassigns the
/// cell), so a value computed before a `yield` survives into the next state. G.3 narrows this with
/// real liveness. All synthesized nodes carry the generator's `span`; reused source nodes keep theirs.
fn desugar_generator(body: &[AstStmt], span: Span) -> GeneratorDesugar {
    let mut flat = Flattener {
        blocks: Vec::new(),
        hoisted: Vec::new(),
    };
    let entry = flat.new_block();
    let exit = flat.lower_seq(body, entry, None);
    flat.blocks[exit].term = Term::Done;

    // Render each state as `if $state == idx { <stmts> <terminator> }`, wrapped in a `while true`
    // dispatch loop; the trailing `return none` fires once `$state` reaches the terminal sentinel.
    let terminal = flat.blocks.len();
    let mut chain: Vec<AstStmt> = Vec::with_capacity(flat.blocks.len() + 1);
    for (idx, block) in flat.blocks.iter().enumerate() {
        let mut stmts = block.stmts.clone();
        block.term.render(&mut stmts, terminal, span);
        chain.push(state_arm(idx as i64, stmts, span));
    }
    chain.push(AstStmt::Return {
        value: Some(none_expr(span)),
        span,
    });
    let dispatch = AstStmt::While {
        cond: Expr::Bool { value: true, span },
        body: chain,
        span,
    };

    let step = Expr::Closure {
        params: vec![Param {
            name: RESUME_PARAM.to_string(),
            name_span: span,
            ty: None,
            default: None,
            span,
        }],
        ret: None,
        body: ClosureBody::Block(vec![dispatch]),
        span,
    };

    let mut prelude = vec![AstStmt::Binding {
        mut_decl: true,
        name: STATE_VAR.to_string(),
        name_span: span,
        ty: None,
        value: Expr::Int { value: 0, span },
        span,
    }];
    for name in &flat.hoisted {
        prelude.push(AstStmt::Binding {
            mut_decl: true,
            name: name.clone(),
            name_span: span,
            ty: None,
            value: none_expr(span),
            span,
        });
    }

    GeneratorDesugar { prelude, step }
}

/// A state's terminator in the flattened generator CFG — how control leaves the state.
enum Term {
    /// Jump to another state and re-enter the dispatch loop (`$state = k; continue`).
    Goto(usize),
    /// Suspend: `$state = k; return some(<expr>)` — the next step resumes at state `k`.
    Yield(Expr, usize),
    /// Two-way branch on a condition (`if cond { $state = t } else { $state = e } continue`).
    Branch(Expr, usize, usize),
    /// End of iteration: `$state = <terminal>; return none` (every later step also returns `none`).
    Done,
}

impl Term {
    /// Emit the statements that realize this terminator, appended after a state's own statements.
    fn render(&self, out: &mut Vec<AstStmt>, terminal: usize, span: Span) {
        match self {
            Term::Goto(k) => {
                out.push(assign_state(*k as i64, span));
                out.push(AstStmt::Continue { span });
            }
            Term::Yield(value, k) => {
                out.push(assign_state(*k as i64, span));
                out.push(AstStmt::Return {
                    value: Some(call_some(value.clone(), span)),
                    span,
                });
            }
            Term::Branch(cond, then_state, else_state) => {
                out.push(AstStmt::If {
                    cond: cond.clone(),
                    then_body: vec![assign_state(*then_state as i64, span)],
                    else_body: Some(vec![assign_state(*else_state as i64, span)]),
                    span,
                });
                out.push(AstStmt::Continue { span });
            }
            Term::Done => {
                out.push(assign_state(terminal as i64, span));
                out.push(AstStmt::Return {
                    value: Some(none_expr(span)),
                    span,
                });
            }
        }
    }
}

/// One state of the flattened generator: the statements it runs and how it leaves.
struct BlockBuf {
    stmts: Vec<AstStmt>,
    term: Term,
}

/// Flattens a generator body into a CFG of [`BlockBuf`] states (Track G.2). See
/// [`desugar_generator`].
struct Flattener {
    blocks: Vec<BlockBuf>,
    /// Flattened-level locals to hoist into captured cells (deduped, in first-seen order).
    hoisted: Vec<String>,
}

impl Flattener {
    /// Allocate a fresh empty state (terminator filled in by the caller; `Done` is a safe default for
    /// an unreachable continuation).
    fn new_block(&mut self) -> usize {
        let idx = self.blocks.len();
        self.blocks.push(BlockBuf {
            stmts: Vec::new(),
            term: Term::Done,
        });
        idx
    }

    fn hoist(&mut self, name: &str) {
        if !self.hoisted.iter().any(|n| n == name) {
            self.hoisted.push(name.to_string());
        }
    }

    /// Lower a statement sequence starting at state `entry`, returning the state where control
    /// continues afterward. `loop_ctx` is `(continue_target, break_target)` of the innermost
    /// enclosing **flattened** loop (`None` at the top level), routing `continue`/`break`.
    fn lower_seq(
        &mut self,
        stmts: &[AstStmt],
        entry: usize,
        loop_ctx: Option<(usize, usize)>,
    ) -> usize {
        let mut cur = entry;
        for stmt in stmts {
            cur = self.lower_one(stmt, cur, loop_ctx);
        }
        cur
    }

    /// Lower one statement into the CFG at state `cur`, returning the state control continues at.
    fn lower_one(&mut self, stmt: &AstStmt, cur: usize, loop_ctx: Option<(usize, usize)>) -> usize {
        match stmt {
            AstStmt::Yield { value, .. } => {
                let next = self.new_block();
                self.blocks[cur].term = Term::Yield(value.clone(), next);
                next
            }
            // Bare `return;` ends iteration; `return e;` is checker-rejected (E0039) — treat both as
            // end-of-iteration so lowering stays total. Statements after it are unreachable.
            AstStmt::Return { .. } => {
                self.blocks[cur].term = Term::Done;
                self.new_block()
            }
            AstStmt::Break { .. } => {
                // `break` to the enclosing flattened loop's exit (or end iteration if there is none —
                // a `break` outside a loop is already a checker error E0024, so this never runs).
                self.blocks[cur].term = match loop_ctx {
                    Some((_, brk)) => Term::Goto(brk),
                    None => Term::Done,
                };
                self.new_block()
            }
            AstStmt::Continue { .. } => {
                self.blocks[cur].term = match loop_ctx {
                    Some((cont, _)) => Term::Goto(cont),
                    None => Term::Done,
                };
                self.new_block()
            }
            // A flattened-level local: hoist it to a captured cell and rewrite its declaration to a
            // bare assignment (the prelude already declares the cell, so this reassigns it — including
            // a `mut`-declared local, which must not re-shadow the cell).
            AstStmt::Binding {
                name, value, span, ..
            } => {
                self.hoist(name);
                self.blocks[cur].stmts.push(AstStmt::Binding {
                    mut_decl: false,
                    name: name.clone(),
                    name_span: *span,
                    ty: None,
                    value: value.clone(),
                    span: *span,
                });
                cur
            }
            AstStmt::Destructure {
                targets,
                value,
                span,
                ..
            } => {
                for (name, _) in targets {
                    self.hoist(name);
                }
                self.blocks[cur].stmts.push(AstStmt::Destructure {
                    mut_decl: false,
                    targets: targets.clone(),
                    value: value.clone(),
                    span: *span,
                });
                cur
            }
            AstStmt::If {
                cond,
                then_body,
                else_body,
                ..
            } if needs_flatten_if(then_body, else_body.as_deref()) => {
                let then_entry = self.new_block();
                let else_entry = self.new_block();
                let join = self.new_block();
                self.blocks[cur].term = Term::Branch(cond.clone(), then_entry, else_entry);
                let then_exit = self.lower_seq(then_body, then_entry, loop_ctx);
                self.blocks[then_exit].term = Term::Goto(join);
                let else_exit =
                    self.lower_seq(else_body.as_deref().unwrap_or(&[]), else_entry, loop_ctx);
                self.blocks[else_exit].term = Term::Goto(join);
                join
            }
            AstStmt::While { cond, body, .. } if body_has_yield(body) => {
                let head = self.new_block();
                self.blocks[cur].term = Term::Goto(head);
                let body_entry = self.new_block();
                let after = self.new_block();
                self.blocks[head].term = Term::Branch(cond.clone(), body_entry, after);
                // The loop body's `break`/`continue` route to `after`/`head`; the body falls back to
                // `head` (the back-edge).
                let body_exit = self.lower_seq(body, body_entry, Some((head, after)));
                self.blocks[body_exit].term = Term::Goto(head);
                after
            }
            // No `yield` and no escaping control flow: emit verbatim — it runs whole within this state
            // (a `match`, a self-contained `for`/`while`/`if`). Its own `break`/`continue` target
            // itself, so it needs no state interaction.
            other => {
                self.blocks[cur].stmts.push(other.clone());
                cur
            }
        }
    }
}

/// Whether an `if` must be flattened into the state machine: it carries a `yield`, or a `break`/
/// `continue` that **escapes** the `if` (targeting an enclosing flattened loop) — emitting it verbatim
/// would make that jump a real `break`/`continue` of the dispatch loop.
fn needs_flatten_if(then_body: &[AstStmt], else_body: Option<&[AstStmt]>) -> bool {
    let yields = body_has_yield(then_body) || else_body.is_some_and(body_has_yield);
    let escapes =
        body_has_escaping_ctrl(then_body) || else_body.is_some_and(body_has_escaping_ctrl);
    yields || escapes
}

/// Whether a statement sequence contains a `break`/`continue` that escapes to an **enclosing** loop —
/// i.e. one not absorbed by a `while`/`for` within the sequence. An `if` is transparent to control
/// flow (its `break` targets the outer loop); a `while`/`for` absorbs its own (no labels); a `match`
/// arm is an expression (no statements); nested callables are separate scopes.
fn body_has_escaping_ctrl(stmts: &[AstStmt]) -> bool {
    stmts.iter().any(stmt_has_escaping_ctrl)
}

fn stmt_has_escaping_ctrl(stmt: &AstStmt) -> bool {
    match stmt {
        AstStmt::Break { .. } | AstStmt::Continue { .. } => true,
        AstStmt::If {
            then_body,
            else_body,
            ..
        } => {
            body_has_escaping_ctrl(then_body)
                || else_body.as_deref().is_some_and(body_has_escaping_ctrl)
        }
        _ => false,
    }
}

/// `if $state == k { body }` — one state's dispatch arm inside the `while true` step loop.
fn state_arm(k: i64, body: Vec<AstStmt>, span: Span) -> AstStmt {
    AstStmt::If {
        cond: Expr::Binary {
            op: BinaryOp::Eq,
            lhs: Box::new(Expr::Ident {
                name: STATE_VAR.to_string(),
                span,
            }),
            rhs: Box::new(Expr::Int { value: k, span }),
            span,
        },
        then_body: body,
        else_body: None,
        span,
    }
}

/// `$state = k` — advance the dispatch discriminant (a bare assignment, so it reassigns the captured
/// cell rather than declaring a closure-local).
fn assign_state(k: i64, span: Span) -> AstStmt {
    AstStmt::Binding {
        mut_decl: false,
        name: STATE_VAR.to_string(),
        name_span: span,
        ty: None,
        value: Expr::Int { value: k, span },
        span,
    }
}

/// `some(value)` — the per-element constructor of a generator step's `?T` result.
fn call_some(value: Expr, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::Ident {
            name: "some".to_string(),
            span,
        }),
        args: vec![value],
        span,
    }
}

/// `none` — the end-of-iteration result and the placeholder a hoisted local is initialized to (it is
/// always reassigned before it is read in a well-formed generator).
fn none_expr(span: Span) -> Expr {
    Expr::Ident {
        name: "none".to_string(),
        span,
    }
}

/// Whether a statement sequence contains a `yield` (Track G), descending into control-flow bodies but
/// **not** into nested callables (a closure/`fn` resets the generator context). Mirrors the checker's
/// generator detection so the lowering desugars exactly the bodies the checker treats as generators.
fn body_has_yield(stmts: &[AstStmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

fn stmt_has_yield(stmt: &AstStmt) -> bool {
    match stmt {
        AstStmt::Yield { .. } => true,
        AstStmt::If {
            then_body,
            else_body,
            ..
        } => body_has_yield(then_body) || else_body.as_deref().is_some_and(body_has_yield),
        AstStmt::For { body, .. } | AstStmt::While { body, .. } => body_has_yield(body),
        _ => false,
    }
}
