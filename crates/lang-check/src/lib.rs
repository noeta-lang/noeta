//! The type checker: the static front-end between parsing and compilation.
//!
//! [`check`] walks a [`Program`] and returns the type diagnostics it finds. It is exposed to
//! the pipeline as the `checked` salsa query (`lang-db`), slotted between `ast` and `bytecode`.
//!
//! ## Bidirectional, with local inference
//!
//! The checker is **bidirectional** (the inferred-static engine; not Hindley–Milner — subtyping
//! via `dyn`/records is load-bearing and defeats HM's unification core). It runs two mutually
//! recursive judgments:
//!
//! - [`Checker::synth`] — *synthesis* mode: produce a [`Type`] for an expression bottom-up
//!   (literals, operators, calls, members). The recursion among subexpressions is synthesis.
//! - [`Checker::check`] — *checking* mode: check an expression against an `expected` type. Forms
//!   that can absorb an expectation (a list against `List<T>`, a closure against a function type)
//!   propagate it inward; everything else synthesizes and is then **subsumed**
//!   ([`Checker::subsume`]: require `actual <: expected` via [`Type::subtype`]). Statement and
//!   boundary positions enter through `check`.
//!
//! ## Gradual tolerance — being removed across this track
//!
//! Today the checker is still *gradual*: wherever it cannot infer a precise type — an unannotated
//! parameter, a prelude call, a method result — it falls back to the inference hole
//! [`Type::Unknown`], and [`Type::subtype`] treats a hole as compatible in both directions, so
//! **subsumption never fires on missing information**. The consequence is the property M1.7
//! established: every program the M0 tree-walker runs still type-checks. A diagnostic fires only
//! when types are *concretely* known and unambiguously wrong — which is what lets the checker run
//! as a shared front-end for both backends without ever diverging the differential oracle (a
//! rejected program is rejected identically by both; a gradual gap is an error in neither).
//!
//! The inferred-static track tightens this in stages: the engine swap (synth/check) is
//! behavior-preserving — statement positions enter `check` with an open (`Unknown`) expectation,
//! so subsumption is a no-op and verdicts are identical — and later slices supply real
//! expectations (declared returns, required signatures) and remove the hole fallback, at which
//! point an un-inferable type becomes a compile error rather than a silent pass.
//!
//! ## What it checks
//!
//! - **Exhaustive `match`** (`E0011`) — a `match` on a concretely-typed enum (or `Result`/
//!   `Option`) that omits a variant and has no catch-all. This promotes M1.5's *runtime*
//!   non-exhaustive error to a compile-time one; the runtime `MatchFail` becomes unreachable
//!   for checked programs.
//! - **`?` on a non-fallible value** (`E0012`) — `expr?` where `expr` is concretely neither a
//!   `Result` nor an `Option`.
//! - **Operator type mismatch** (`E0007`) — arithmetic (`+ - * / %`) on a concretely
//!   non-numeric operand (e.g. `1 + true`). Reuses the existing runtime `TypeMismatch` code at
//!   the same span, so the static error reads identically to the old runtime one.
//!
//! - **Unknown type (`E0013`)** — a type annotation (a parameter, return, field, enum backing,
//!   or generic argument) naming a type that resolves to nothing: not a built-in, not a declared
//!   record/class/enum, not a name brought in by a `use`, and not a generic type parameter in
//!   scope. This was deferred until M1.9 for a reason — before module resolution, "undeclared"
//!   could not be told apart from "valid but imported", so flagging it risked a false positive on
//!   e.g. a `?User` annotation whose `User` came from a `use`. Now that the loader merges resolved
//!   imports into the program and leaves opaque-stub `use`s in place, both referents are visible
//!   to [`collect`], so an unresolvable name is genuinely unknown.
//! - **Missing signature (`E0022`)** — a named function or method lacking a type on a parameter
//!   or a return type. Inferred-static typing makes signatures mandatory at named boundaries
//!   (only closures and local bindings stay inferred). Each `return <value>` is then checked
//!   against the declared return type (an `E0007` mismatch on a concrete violation).
//!
//! The engine is bidirectional with local inference (see above), deliberately **not** classical
//! Hindley–Milner: subtyping (`dyn` widening, directional method resolution, record width) is
//! load-bearing and defeats HM's symmetric unification. The remaining gradual fallback to
//! [`Type::Unknown`] is being removed in stages across the inferred-static track; until then an
//! un-inferable interior type is tolerated rather than an error.

use std::collections::{HashMap, HashSet};

use lang_ast::{
    Attribute, BinaryOp, ClassDecl, EnumDecl, Expr, FnDecl, ForPattern, ImplBlock, MatchArm, Param,
    Pattern, Program, RecordDecl, Stmt, StrPart, TypeRef,
};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::Span;
use lang_types::{BuiltinTrait, Type};

/// Type-check a program and return every diagnostic found, in source order. An empty result
/// means the program is well-typed (as far as the gradual checker can determine).
pub fn check(program: &Program) -> Vec<Diagnostic> {
    let mut checker = Checker::default();
    checker.collect(program);
    checker.check_program(program);
    checker.diags
}

/// One enum variant: its name and the types of its positional data fields.
#[derive(Clone)]
struct VariantInfo {
    name: String,
    fields: Vec<Type>,
}

/// A top-level function signature, as far as annotations reveal it.
#[derive(Clone)]
struct FnSig {
    ret: Type,
}

/// A lexical scope stack: each frame maps a name to its inferred type. Inner frames shadow.
type Env = Vec<HashMap<String, Type>>;

fn lookup(env: &Env, name: &str) -> Option<Type> {
    env.iter().rev().find_map(|frame| frame.get(name).cloned())
}

fn bind(env: &mut Env, name: &str, ty: Type) {
    if let Some(frame) = env.last_mut() {
        frame.insert(name.to_string(), ty);
    }
}

#[derive(Default)]
struct Checker {
    /// User-declared enums: name → variants.
    enums: HashMap<String, Vec<VariantInfo>>,
    /// Top-level functions: name → signature.
    functions: HashMap<String, FnSig>,
    /// Records/classes: name → declared fields (name, type).
    records: HashMap<String, Vec<(String, Type)>>,
    /// Every name a type annotation may legally resolve to: declared records/classes/enums plus
    /// names brought in by a `use` (whether merged in by the linker or left as an opaque stub).
    /// Built-in names and in-scope generic parameters are *not* stored here — they are checked
    /// separately (a built-in via [`Type::is_builtin_name`], a parameter via [`Self::type_params`]).
    types: HashSet<String>,
    /// The generic type parameters in scope while checking the current declaration's annotations
    /// (a class/record/enum's `<T, ...>`). Empty at top level; saved and restored around each
    /// generic declaration.
    type_params: HashSet<String>,
    /// The declared return type of the function whose body is currently being checked — the
    /// expectation each `return <value>` is checked against. `Unknown` at top level and inside a
    /// function with no return annotation (so the check is a no-op there). Saved and restored
    /// around each function so nested declarations do not clobber the enclosing one.
    current_ret: Type,
    diags: Vec<Diagnostic>,
}

impl Checker {
    /// Pass 1: register every top-level declaration so forward references resolve before any
    /// body is checked. Mirrors the compiler's "register types first" pass.
    fn collect(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                Stmt::Record(r) => {
                    let fields = r
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), field_type(&f.ty)))
                        .collect();
                    self.records.insert(r.name.clone(), fields);
                    self.types.insert(r.name.clone());
                }
                Stmt::Class(c) => {
                    let fields = c
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), field_type(&f.ty)))
                        .collect();
                    self.records.insert(c.name.clone(), fields);
                    self.types.insert(c.name.clone());
                }
                Stmt::Enum(e) => {
                    let variants = e
                        .variants
                        .iter()
                        .map(|v| VariantInfo {
                            name: v.name.clone(),
                            fields: v.fields.iter().map(|p| field_type(&p.ty)).collect(),
                        })
                        .collect();
                    self.enums.insert(e.name.clone(), variants);
                    self.types.insert(e.name.clone());
                }
                Stmt::Fn(f) => {
                    let ret = f.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown);
                    self.functions.insert(f.name.clone(), FnSig { ret });
                }
                // An imported name (whether the linker merged its declaration or left an opaque
                // stub) is a legal referent for an annotation — register it as a known type.
                Stmt::Use { names, .. } => {
                    for name in names {
                        self.types.insert(name.name.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// Pass 2: check every top-level statement with a fresh global scope.
    fn check_program(&mut self, program: &Program) {
        let mut env: Env = vec![HashMap::new()];
        for stmt in &program.stmts {
            self.check_stmt(stmt, &mut env);
        }
    }

    fn check_block(&mut self, stmts: &[Stmt], env: &mut Env) {
        env.push(HashMap::new());
        for stmt in stmts {
            self.check_stmt(stmt, env);
        }
        env.pop();
    }

    fn check_stmt(&mut self, stmt: &Stmt, env: &mut Env) {
        match stmt {
            // Statement positions enter checking mode. The expectation is open (`Unknown`) until
            // later slices supply real ones (a declared return type at `Return`), so subsumption
            // is a no-op here and behavior is identical to bare synthesis — the parity guarantee.
            Stmt::Echo { value, .. } => {
                self.check(value, &Type::Unknown, env);
            }
            Stmt::Binding { name, value, .. } => {
                let ty = self.check(value, &Type::Unknown, env);
                bind(env, name, ty);
            }
            Stmt::Expr { expr, .. } => {
                self.check(expr, &Type::Unknown, env);
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    // Check the returned value against the enclosing function's declared return.
                    let expected = self.current_ret.clone();
                    self.check(value, &expected, env);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.synth(cond, env);
                self.check_block(then_body, env);
                if let Some(else_body) = else_body {
                    self.check_block(else_body, env);
                }
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                let iter_ty = self.synth(iterable, env);
                env.push(HashMap::new());
                self.bind_for_pattern(pattern, &iter_ty, env);
                for stmt in body {
                    self.check_stmt(stmt, env);
                }
                env.pop();
            }
            Stmt::Fn(decl) => self.check_fn(decl, env, &[]),
            Stmt::Record(r) => self.check_record(r),
            Stmt::Class(c) => self.check_class(c, env),
            Stmt::Enum(e) => self.check_enum(e),
            Stmt::Namespace { .. } | Stmt::Use { .. } => {}
        }
    }

    /// Check a function (or method) body. `extra` seeds the body scope with additional bindings
    /// (a class's fields, when checking a method).
    fn check_fn(&mut self, decl: &FnDecl, env: &mut Env, extra: &[(String, Type)]) {
        self.require_signature(decl);
        for p in &decl.params {
            self.check_type_opt(&p.ty);
        }
        self.check_type_opt(&decl.ret);
        // The body's `return`s are checked against the declared return type; `Unknown` when
        // unannotated (already an `E0022`), so the check stays a no-op there. Saved/restored so a
        // nested function does not clobber the enclosing one's expectation.
        let ret = decl
            .ret
            .as_ref()
            .map(Type::from_ref)
            .unwrap_or(Type::Unknown);
        let saved_ret = std::mem::replace(&mut self.current_ret, ret);
        env.push(HashMap::new());
        for (name, ty) in extra {
            bind(env, name, ty.clone());
        }
        for p in &decl.params {
            bind(env, &p.name, param_type(p));
        }
        for stmt in &decl.body {
            self.check_stmt(stmt, env);
        }
        env.pop();
        self.current_ret = saved_ret;
    }

    /// Inferred-static requires a full signature on every **named** function or method: a type on
    /// each parameter and a return type. (Closures and local bindings stay inferred — inference
    /// reconstructs them.) Each missing piece is its own `E0022`.
    fn require_signature(&mut self, decl: &FnDecl) {
        for p in &decl.params {
            if p.ty.is_none() {
                self.diags.push(
                    Diagnostic::error(
                        DiagnosticCode::MissingSignature,
                        p.name_span,
                        format!("parameter `{}` needs a type annotation", p.name),
                    )
                    .with_help(
                        "every parameter of a named function needs a type; only closures and \
                         locals are inferred",
                    ),
                );
            }
        }
        if decl.ret.is_none() {
            self.diags.push(
                Diagnostic::error(
                    DiagnosticCode::MissingSignature,
                    decl.name_span,
                    format!("function `{}` needs a return type", decl.name),
                )
                .with_help("annotate the return type after the parameters, e.g. `): int`"),
            );
        }
    }

    fn check_record(&mut self, r: &RecordDecl) {
        let saved = self.enter_type_params(&r.type_params);
        for f in &r.fields {
            self.check_type_opt(&f.ty);
        }
        self.check_derives(&r.derives);
        self.check_attrs(&r.attrs);
        self.type_params = saved;
    }

    fn check_class(&mut self, c: &ClassDecl, env: &mut Env) {
        let saved = self.enter_type_params(&c.type_params);
        let fields: Vec<(String, Type)> = c
            .fields
            .iter()
            .map(|f| (f.name.clone(), field_type(&f.ty)))
            .collect();
        for f in &c.fields {
            self.check_type_opt(&f.ty);
        }
        self.check_derives(&c.derives);
        self.check_attrs(&c.attrs);
        for block in &c.impls {
            self.check_impl(block);
        }
        for method in &c.methods {
            self.check_fn(method, env, &fields);
        }
        if let Some(destructor) = &c.destructor {
            env.push(HashMap::new());
            for (name, ty) in &fields {
                bind(env, name, ty.clone());
            }
            for stmt in destructor {
                self.check_stmt(stmt, env);
            }
            env.pop();
        }
        self.type_params = saved;
    }

    fn check_enum(&mut self, e: &EnumDecl) {
        let saved = self.enter_type_params(&e.type_params);
        self.check_type_opt(&e.backing);
        for variant in &e.variants {
            for field in &variant.fields {
                self.check_type_opt(&field.ty);
            }
        }
        self.check_derives(&e.derives);
        self.check_attrs(&e.attrs);
        self.type_params = saved;
    }

    // ----- unknown-type resolution (E0013) -----

    /// Install `params` as the in-scope generic type parameters and return the previous set (to
    /// restore once the declaration is checked). Generic parameters are erased at runtime but are
    /// legal referents for annotations within their declaration.
    fn enter_type_params(&mut self, params: &[String]) -> HashSet<String> {
        std::mem::replace(&mut self.type_params, params.iter().cloned().collect())
    }

    fn check_type_opt(&mut self, ty: &Option<TypeRef>) {
        if let Some(ty) = ty {
            self.check_type_ref(ty);
        }
    }

    /// Verify that every named type in an annotation resolves: a built-in, a declared/imported
    /// type, or a generic parameter in scope. An unresolvable name is `E0013`. Generic arguments
    /// are checked recursively, so `List<Ghost>` flags `Ghost`.
    fn check_type_ref(&mut self, ty: &TypeRef) {
        match ty {
            TypeRef::Optional { inner, .. } => self.check_type_ref(inner),
            TypeRef::Named { name, args, span } => {
                if !Type::is_builtin_name(name)
                    && !PRELUDE_TYPES.contains(&name.as_str())
                    && !self.type_params.contains(name)
                    && !self.types.contains(name)
                {
                    self.diags.push(
                        Diagnostic::error(
                            DiagnosticCode::UnknownType,
                            *span,
                            format!("unknown type `{name}`"),
                        )
                        .with_help(
                            "name a declared type, one imported with `use`, a generic parameter, \
                             or a built-in",
                        ),
                    );
                }
                for arg in args {
                    self.check_type_ref(arg);
                }
            }
        }
    }

    // ----- traits: impl coherence and derive validation (M1.8) -----

    /// Validate an `impl Trait { ... }` block: the trait must be a known built-in, and the block
    /// must provide the trait's required method with the right arity. The impl's method *bodies*
    /// are checked separately (they are flattened into `ClassDecl::methods`).
    fn check_impl(&mut self, block: &ImplBlock) {
        let Some(t) = BuiltinTrait::lookup(&block.trait_name) else {
            self.diags.push(
                Diagnostic::error(
                    DiagnosticCode::UnknownTrait,
                    block.trait_span,
                    format!("unknown trait `{}`", block.trait_name),
                )
                .with_help(
                    "only built-in traits can be implemented (e.g. `Add`, `Equatable`, `Display`)",
                ),
            );
            return;
        };
        let Some((req_name, req_arity)) = t.required_method else {
            return; // a marker trait (e.g. `Clone`) imposes no hand-written method
        };
        match block.methods.iter().find(|m| m.name == req_name) {
            None => self.diags.push(
                Diagnostic::error(
                    DiagnosticCode::InvalidImpl,
                    block.trait_span,
                    format!("`impl {}` must define `fn {req_name}`", block.trait_name),
                )
                .with_help(format!(
                    "the `{}` trait requires the `{req_name}` method",
                    block.trait_name
                )),
            ),
            Some(m) if m.params.len() != req_arity => self.diags.push(Diagnostic::error(
                DiagnosticCode::InvalidImpl,
                m.name_span,
                format!(
                    "`{req_name}` must take {req_arity} parameter(s), found {}",
                    m.params.len()
                ),
            )),
            Some(_) => {}
        }
    }

    /// Validate the `@derive(...)` directives on a declaration: every named trait must be a known
    /// *derivable* built-in. The compiler synthesizes the listed impls from the type's fields.
    fn check_derives(&mut self, derives: &[(String, Span)]) {
        for (trait_name, span) in derives {
            match BuiltinTrait::lookup(trait_name) {
                Some(t) if t.derivable => {}
                Some(_) => self.diags.push(
                    Diagnostic::error(
                        DiagnosticCode::UnknownTrait,
                        *span,
                        format!("`{trait_name}` is not a derivable trait"),
                    )
                    .with_help(
                        "derivable traits are `Equatable`, `Comparable`, `Display`, `Clone`, \
                         `ToJson`, `Serialize`; mark attribute records with `impl Attribute for X {}`",
                    ),
                ),
                None => self.diags.push(Diagnostic::error(
                    DiagnosticCode::UnknownTrait,
                    *span,
                    format!("unknown trait `{trait_name}` in `@derive(...)`"),
                )),
            }
        }
    }

    /// Validate the `#[...]` data attributes on a declaration. These reduce to records in the
    /// manifest (M1.8b) and carry no codegen meaning. The one error checked now is the migration
    /// case: `#[derive(...)]` is the old codegen spelling, which is now the `@derive(...)`
    /// directive. (The `Attribute`-trait gate on `#[Foo(...)]` usage lands with the manifest.)
    fn check_attrs(&mut self, attrs: &[Attribute]) {
        for attr in attrs {
            if attr.name == "derive" {
                self.diags.push(
                    Diagnostic::error(
                        DiagnosticCode::InvalidAttribute,
                        attr.span,
                        "`#[derive(...)]` is not a data attribute",
                    )
                    .with_help(
                        "code generation now uses the `@derive(...)` directive; `#[...]` is for \
                         data attributes only",
                    ),
                );
            }
        }
    }

    // ----- bidirectional judgments -----

    /// *Checking* mode: check `expr` against the `expected` type, returning the expression's
    /// actual type. Forms that can absorb an expectation propagate it inward (a list against
    /// `List<T>` checks each element against `T`; a closure against a function type adopts the
    /// expected parameter/return types); every other form synthesizes and is then subsumed.
    ///
    /// In this slice every caller passes an open (`Unknown`) expectation, so the propagation
    /// arms below adopt no concrete type and [`Self::subsume`] never fires — `check` is
    /// behavior-identical to [`Self::synth`]. Later slices pass real expectations (declared
    /// returns, parameter types) and this is where they take effect.
    fn check(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        match expr {
            // A list literal absorbs an expected `List<T>`: check each element against `T`.
            Expr::List { items, .. } if matches!(expected, Type::List(_)) => {
                let Type::List(elem) = expected else {
                    unreachable!()
                };
                for item in items {
                    self.check(item, elem, env);
                }
                Type::List(elem.clone())
            }
            // A closure absorbs an expected function type: an explicit parameter annotation wins,
            // otherwise the parameter adopts the expected type; the body is checked against the
            // expected return.
            Expr::Closure { params, body, .. } if matches!(expected, Type::Fn { .. }) => {
                let Type::Fn {
                    params: expected_params,
                    ret,
                } = expected
                else {
                    unreachable!()
                };
                env.push(HashMap::new());
                for (i, p) in params.iter().enumerate() {
                    let pty = p.ty.as_ref().map(Type::from_ref).unwrap_or_else(|| {
                        expected_params.get(i).cloned().unwrap_or(Type::Unknown)
                    });
                    bind(env, &p.name, pty);
                }
                let body_ty = self.check(body, ret, env);
                env.pop();
                Type::Fn {
                    params: params.iter().map(param_type).collect(),
                    ret: Box::new(body_ty),
                }
            }
            // Default: synthesize the actual type, then require it to be a subtype of the
            // expectation.
            _ => {
                let actual = self.synth(expr, env);
                self.subsume(&actual, expected, expr.span());
                actual
            }
        }
    }

    /// Subsumption: require `actual <: expected`. A violation is a type mismatch (`E0007`, the
    /// same code the arithmetic/runtime mismatch path uses). An inference hole on either side
    /// makes [`Type::subtype`] hold, so a not-yet-inferred type never produces a false positive —
    /// the gradual-tolerance invariant this slice preserves.
    fn subsume(&mut self, actual: &Type, expected: &Type, span: Span) {
        if !Type::subtype(actual, expected) {
            self.diags.push(Diagnostic::error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("expected `{expected}`, found `{actual}`"),
            ));
        }
    }

    // ----- synthesis -----

    fn synth(&mut self, expr: &Expr, env: &mut Env) -> Type {
        match expr {
            Expr::Str { .. } => Type::String,
            Expr::Int { .. } => Type::Int,
            Expr::Float { .. } => Type::Float,
            Expr::Bool { .. } => Type::Bool,
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let StrPart::Hole(e) = part {
                        self.synth(e, env);
                    }
                }
                Type::String
            }
            Expr::Ident { name, .. } => lookup(env, name)
                .or_else(|| {
                    self.functions.get(name).map(|sig| Type::Fn {
                        params: Vec::new(),
                        ret: Box::new(sig.ret.clone()),
                    })
                })
                .unwrap_or(Type::Unknown),
            Expr::Unary { operand, .. } => {
                // Unary type errors have no corpus case and the operand is often gradual; infer
                // for nested checks but do not promote (kept conservative).
                self.synth(operand, env)
            }
            Expr::Binary { op, lhs, rhs, span } => self.synth_binary(*op, lhs, rhs, *span, env),
            Expr::Call { callee, args, .. } => {
                for arg in args {
                    self.synth(arg, env);
                }
                self.synth_call(callee, env)
            }
            Expr::Closure { params, body, .. } => {
                env.push(HashMap::new());
                for p in params {
                    bind(env, &p.name, param_type(p));
                }
                let ret = self.synth(body, env);
                env.pop();
                Type::Fn {
                    params: params.iter().map(param_type).collect(),
                    ret: Box::new(ret),
                }
            }
            Expr::Pipeline { left, right, .. } => {
                self.synth(left, env);
                self.synth(right, env);
                Type::Unknown
            }
            Expr::List { items, .. } => {
                let mut elem = Type::Unknown;
                for item in items {
                    let t = self.synth(item, env);
                    if elem.is_gradual() {
                        elem = t;
                    }
                }
                Type::List(Box::new(elem))
            }
            Expr::Map { entries, .. } => {
                for (k, v) in entries {
                    self.synth(k, env);
                    self.synth(v, env);
                }
                Type::Map(Box::new(Type::Unknown), Box::new(Type::Unknown))
            }
            Expr::Member { receiver, name, .. } => self.synth_member(receiver, name, env),
            Expr::Index {
                receiver, index, ..
            } => {
                // Recurse so nested checks (exhaustiveness, `?`-typing) still fire inside an
                // index expression. The element type is gradual: a list element or an `Index`
                // impl's return are not statically tracked yet.
                self.synth(receiver, env);
                self.synth(index, env);
                Type::Unknown
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => self.synth_match(scrutinee, arms, *span, env),
            Expr::Object(lit) => {
                if let Some(spread) = &lit.spread {
                    self.synth(spread, env);
                }
                for f in &lit.fields {
                    self.synth(&f.value, env);
                }
                Type::Named(lit.type_name.clone())
            }
            Expr::Try { expr, span } => {
                let inner = self.synth(expr, env);
                match &inner {
                    Type::Result(ok, _) => (**ok).clone(),
                    Type::Option(some) => (**some).clone(),
                    t if t.is_gradual() => Type::Unknown,
                    other => {
                        self.diags.push(
                            Diagnostic::error(
                                DiagnosticCode::InvalidTry,
                                *span,
                                format!("`?` expects a `Result` or `Option`, found `{other}`"),
                            )
                            .with_help(
                                "`?` only propagates `Result`/`Option`; this value is neither",
                            ),
                        );
                        Type::Unknown
                    }
                }
            }
            Expr::Coalesce {
                value, fallback, ..
            } => {
                let v = self.synth(value, env);
                self.synth(fallback, env);
                match v {
                    Type::Result(ok, _) => *ok,
                    Type::Option(some) => *some,
                    _ => Type::Unknown,
                }
            }
        }
    }

    fn synth_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        env: &mut Env,
    ) -> Type {
        let lt = self.synth(lhs, env);
        let rt = self.synth(rhs, env);
        match op {
            BinaryOp::Concat => Type::String,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                let bad = |t: &Type| !t.is_numeric() && !t.is_gradual();
                if bad(&lt) || bad(&rt) {
                    // A concretely non-numeric operand: the same error the M0 runtime raised,
                    // now caught statically. Span is the binary expression (matches the runtime
                    // report site), so the diagnostic reads identically.
                    self.diags.push(Diagnostic::error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("cannot apply `{}` to `{lt}` and `{rt}`", op.symbol()),
                    ));
                    Type::Unknown
                } else if lt == Type::Float || rt == Type::Float {
                    Type::Float
                } else if lt == Type::Int && rt == Type::Int {
                    Type::Int
                } else {
                    Type::Unknown
                }
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::And
            | BinaryOp::Or => Type::Bool,
        }
    }

    fn synth_call(&mut self, callee: &Expr, env: &mut Env) -> Type {
        match callee {
            Expr::Ident { name, .. } => match name.as_str() {
                "Ok" => Type::Result(Box::new(Type::Unknown), Box::new(Type::Unknown)),
                "Err" => Type::Result(Box::new(Type::Unknown), Box::new(Type::Unknown)),
                "some" => Type::Option(Box::new(Type::Unknown)),
                _ => self
                    .functions
                    .get(name)
                    .map(|sig| sig.ret.clone())
                    .unwrap_or(Type::Unknown),
            },
            // `Type.Variant(args)` — an algebraic enum constructor applied to its data.
            Expr::Member { receiver, name, .. } => {
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && self.is_enum_variant(tn, name)
                {
                    return Type::Named(tn.clone());
                }
                Type::Unknown
            }
            _ => {
                self.synth(callee, env);
                Type::Unknown
            }
        }
    }

    fn synth_member(&mut self, receiver: &Expr, name: &str, env: &mut Env) -> Type {
        // `Type.Variant` (a nullary enum constructor like `Status.Paid`) reads as the enum type.
        if let Expr::Ident { name: tn, .. } = receiver
            && self.is_enum_variant(tn, name)
        {
            return Type::Named(tn.clone());
        }
        let recv = self.synth(receiver, env);
        if let Type::Named(n) = &recv
            && let Some(fields) = self.records.get(n)
            && let Some((_, ty)) = fields.iter().find(|(fname, _)| fname == name)
        {
            return ty.clone();
        }
        Type::Unknown
    }

    fn synth_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        env: &mut Env,
    ) -> Type {
        let scrut = self.synth(scrutinee, env);
        self.check_exhaustive(&scrut, arms, span);
        let mut result = Type::Unknown;
        for arm in arms {
            env.push(HashMap::new());
            self.bind_pattern(&arm.pattern, &scrut, env);
            let t = self.synth(&arm.body, env);
            env.pop();
            if result.is_gradual() {
                result = t;
            }
        }
        result
    }

    /// Promote a non-exhaustive `match` to a compile error (`E0011`), but only when the
    /// scrutinee's type is a concretely-known enum / `Result` / `Option`. Anything else (an
    /// `int`/`string`/`bool` scrutinee, or a gradual type) has an open or unknown domain and is
    /// left to the runtime backstop — keeping the check free of false positives.
    fn check_exhaustive(&mut self, scrut: &Type, arms: &[MatchArm], span: Span) {
        // A wildcard or bare binding arm catches everything.
        if arms.iter().any(|a| {
            matches!(
                a.pattern,
                Pattern::Wildcard { .. } | Pattern::Binding { .. }
            )
        }) {
            return;
        }
        let all: Vec<String> = match scrut {
            Type::Result(..) => vec!["Ok".into(), "Err".into()],
            Type::Option(..) => vec!["some".into(), "none".into()],
            Type::Named(n) => match self.enums.get(n) {
                Some(variants) => variants.iter().map(|v| v.name.clone()).collect(),
                None => return,
            },
            _ => return,
        };
        let covered: HashSet<&str> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::Variant { variant, .. } => Some(variant.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = all
            .into_iter()
            .filter(|v| !covered.contains(v.as_str()))
            .collect();
        if !missing.is_empty() {
            self.diags.push(
                Diagnostic::error(
                    DiagnosticCode::NonExhaustiveMatch,
                    span,
                    format!("non-exhaustive `match`: missing {}", missing.join(", ")),
                )
                .with_help("add an arm for each missing case, or a `_` catch-all"),
            );
        }
    }

    // ----- pattern binding -----

    fn bind_for_pattern(&mut self, pattern: &ForPattern, iter_ty: &Type, env: &mut Env) {
        let elem = match iter_ty {
            Type::List(t) => (**t).clone(),
            _ => Type::Unknown,
        };
        match pattern {
            ForPattern::Single { name, .. } => bind(env, name, elem),
            ForPattern::Pair { first, second, .. } => {
                bind(env, first, Type::Int);
                bind(env, second, elem);
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, ty: &Type, env: &mut Env) {
        match pattern {
            Pattern::Wildcard { .. }
            | Pattern::Int { .. }
            | Pattern::Str { .. }
            | Pattern::Bool { .. } => {}
            Pattern::Binding { name, .. } => bind(env, name, ty.clone()),
            Pattern::Variant {
                variant, bindings, ..
            } => {
                let payloads = self.payload_types(ty, variant, bindings.len());
                for (sub, pty) in bindings.iter().zip(payloads) {
                    self.bind_pattern(sub, &pty, env);
                }
            }
        }
    }

    /// The data-field types a variant pattern binds, given the scrutinee type. Falls back to
    /// `Unknown` per position when the type is gradual or the variant is unknown.
    fn payload_types(&self, ty: &Type, variant: &str, arity: usize) -> Vec<Type> {
        let known = match ty {
            Type::Result(ok, err) => match variant {
                "Ok" => vec![(**ok).clone()],
                "Err" => vec![(**err).clone()],
                _ => Vec::new(),
            },
            Type::Option(some) => match variant {
                "some" => vec![(**some).clone()],
                _ => Vec::new(),
            },
            Type::Named(n) => self
                .enums
                .get(n)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| v.fields.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if known.len() == arity {
            known
        } else {
            vec![Type::Unknown; arity]
        }
    }

    fn is_enum_variant(&self, type_name: &str, variant: &str) -> bool {
        self.enums
            .get(type_name)
            .is_some_and(|vs| vs.iter().any(|v| v.name == variant))
    }
}

/// Surface type names the language provides that are *not* lattice built-ins (so they are not in
/// [`Type::is_builtin_name`]): the bare, untyped collection spellings and the prelude `Ordering`
/// enum that `compare` returns and `Comparable` maps to a bool. They resolve to no distinct
/// [`Type`] variant but are legal annotations, so the unknown-type check (`E0013`) accepts them.
const PRELUDE_TYPES: &[&str] = &["list", "map", "set", "Ordering"];

/// The declared type of a field, or `Unknown` when unannotated.
fn field_type(ty: &Option<TypeRef>) -> Type {
    ty.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown)
}

/// The declared type of a parameter, or `Unknown` when unannotated.
fn param_type(p: &Param) -> Type {
    p.ty.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown)
}

#[cfg(test)]
mod tests;
