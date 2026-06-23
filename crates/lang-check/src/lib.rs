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

mod stdlib;

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

/// A callable signature, as far as annotations reveal it: the parameter types (for arity +
/// argument checking) and the return type. Used for both top-level functions and user methods.
#[derive(Clone, Default)]
struct FnSig {
    params: Vec<Type>,
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
    /// User-defined methods: (type name, method name) → signature. Populated from class methods and
    /// `impl`-block methods so a method call on a user object resolves to a real type, with the
    /// owning class's generic parameters erased to `dyn` (they accept any argument).
    methods: HashMap<(String, String), FnSig>,
    /// Names bound to a Ring 2 stdlib module by a `use std.{…}` import (`json`, `fs`, …). A call
    /// `m.f(args)` on such a name resolves through [`stdlib::module_return`].
    modules: HashSet<String>,
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
                    // Record each method's signature (class methods and impl-block methods alike),
                    // so `obj.method(...)` resolves to a concrete type and its arguments are
                    // checked. The class's generic parameters are erased to `dyn` (erased at
                    // runtime, they accept any argument).
                    let tps: HashSet<String> = c.type_params.iter().cloned().collect();
                    let methods = c
                        .methods
                        .iter()
                        .chain(c.impls.iter().flat_map(|b| b.methods.iter()));
                    for m in methods {
                        let params = m
                            .params
                            .iter()
                            .map(|p| erase_type_params(param_type(p), &tps))
                            .collect();
                        let ret = erase_type_params(
                            m.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown),
                            &tps,
                        );
                        self.methods
                            .insert((c.name.clone(), m.name.clone()), FnSig { params, ret });
                    }
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
                    let params = f.params.iter().map(param_type).collect();
                    let ret = f.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown);
                    self.functions.insert(f.name.clone(), FnSig { params, ret });
                }
                // A `use std.{json, …}` import binds a Ring 2 module value (tracked in `modules`);
                // any other imported name (whether the linker merged its declaration or left an
                // opaque stub) is a legal referent for an annotation — registered as a known type.
                Stmt::Use { path, names, .. } => {
                    let is_std = path.len() == 1 && path[0] == "std";
                    for name in names {
                        if is_std && stdlib::STD_MODULES.contains(&name.name.as_str()) {
                            self.modules.insert(name.name.clone());
                        } else {
                            self.types.insert(name.name.clone());
                        }
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
                let arg_types: Vec<Type> = args.iter().map(|a| self.synth(a, env)).collect();
                self.synth_call(callee, &arg_types, env)
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
                // `left |> right` threads `left` as the first argument of `right`.
                let piped = self.synth(left, env);
                self.synth_piped(right, piped, env)
            }
            Expr::List { items, span } => {
                // Synthesize a single element type by unifying the items. Concretely incompatible
                // elements (e.g. `[1, "two"]`) are a static error here in *synthesis* position;
                // a mixed list is written explicitly as `List<dyn>` (in which case the checker
                // arrives through `check`, element-by-element against `dyn`, not here).
                let mut elem = Type::Unknown;
                let mut heterogeneous = false;
                for item in items {
                    let t = self.synth(item, env);
                    match unify_element(&elem, &t) {
                        Some(u) => elem = u,
                        None => heterogeneous = true,
                    }
                }
                if heterogeneous {
                    self.diags.push(
                        Diagnostic::error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "list elements have differing types",
                        )
                        .with_help("make the elements one type, or annotate a `List<dyn>` for a mixed list"),
                    );
                    elem = Type::Dyn; // recover as a mixed list
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
                receiver,
                index,
                span,
            } => {
                // Index into the receiver: a list element, a map value, a string char, or `dyn`.
                let recv = self.synth(receiver, env);
                self.synth(index, env);
                match stdlib::index_return(&recv) {
                    Some(t) => t,
                    None => {
                        // A concrete primitive cannot be indexed (`42[0]`). A `Named` type may
                        // implement `Index`, and a hole/`dyn` defers — neither errors here.
                        if matches!(recv, Type::Int | Type::Float | Type::Bool | Type::Unit) {
                            self.diags.push(Diagnostic::error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot index into `{recv}`"),
                            ));
                        }
                        Type::Unknown
                    }
                }
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
                    // A hole carries no info; `dyn` defers to runtime — both accept `?` without a
                    // diagnostic, yielding the same deferred type.
                    t if t.defers_to_runtime() => t.clone(),
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
                // A `dyn` operand defers to runtime dispatch (its sanctioned semantics), so it is
                // accepted like an inference hole — only a concretely non-numeric operand errors.
                let bad = |t: &Type| !t.is_numeric() && !t.defers_to_runtime();
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

    /// Synthesize a pipeline right-hand side `left |> right`, where `piped` is the type of `left`,
    /// threaded as `right`'s first argument. `right` may be a call (`add(10)` → `add(left, 10)`)
    /// or a bare callee (`inc` → `inc(left)`).
    fn synth_piped(&mut self, right: &Expr, piped: Type, env: &mut Env) -> Type {
        match right {
            Expr::Call { callee, args, .. } => {
                let mut arg_types = vec![piped];
                arg_types.extend(args.iter().map(|a| self.synth(a, env)));
                self.synth_call(callee, &arg_types, env)
            }
            Expr::Ident { .. } | Expr::Member { .. } => self.synth_call(right, &[piped], env),
            other => {
                self.synth(other, env);
                Type::Unknown
            }
        }
    }

    fn synth_call(&mut self, callee: &Expr, args: &[Type], env: &mut Env) -> Type {
        let span = callee.span();
        match callee {
            // A plain `name(args)` call: a user function, else a prelude free function.
            Expr::Ident { name, .. } => {
                if let Some(sig) = self.functions.get(name) {
                    let params = sig.params.clone();
                    let ret = sig.ret.clone();
                    self.check_args(&params, args, span, name);
                    return ret;
                }
                // Prelude functions are polymorphic/variadic — their result is typed, but their
                // arguments are not arity-checked here.
                stdlib::prelude_return(name, args).unwrap_or(Type::Unknown)
            }
            Expr::Member { receiver, name, .. } => {
                // `Type.Variant(args)` — an algebraic enum constructor applied to its data.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && self.is_enum_variant(tn, name)
                {
                    return Type::Named(tn.clone());
                }
                // `module.func(args)` — a Ring 2 stdlib module call.
                if let Expr::Ident { name: m, .. } = receiver.as_ref()
                    && self.modules.contains(m)
                {
                    if let Some(params) = stdlib::module_params(m, name) {
                        self.check_args(&params, args, span, name);
                    }
                    return stdlib::module_return(m, name, args).unwrap_or(Type::Unknown);
                }
                // `receiver.method(args)` — a built-in method, a user method, or (on a `dyn`/hole
                // receiver) a runtime-dispatched call that stays deferred.
                let recv = self.synth(receiver, env);
                self.check_method_args(&recv, name, args, span);
                self.method_call_return(&recv, name)
            }
            _ => {
                self.synth(callee, env);
                Type::Unknown
            }
        }
    }

    /// Arity- and type-check a method call's arguments against the resolved parameter signature
    /// (a built-in method or a user method); a deferred receiver or an unknown method is not
    /// checked.
    fn check_method_args(&mut self, recv: &Type, name: &str, args: &[Type], span: Span) {
        if let Some(params) = stdlib::method_params(recv, name) {
            self.check_args(&params, args, span, name);
        } else if let Type::Named(n) = recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            let params = sig.params.clone();
            self.check_args(&params, args, span, name);
        }
    }

    /// Check a call's argument count and types against the callable's parameter types, reporting
    /// at `span`. Lenient where either side defers to runtime (`dyn`/hole) and on numeric widening
    /// (`int` where `float` is expected), so polymorphic and numeric calls are not false positives.
    fn check_args(&mut self, params: &[Type], args: &[Type], span: Span, callee: &str) {
        if params.len() != args.len() {
            self.diags.push(Diagnostic::error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{callee}` expects {} argument(s), found {}",
                    params.len(),
                    args.len()
                ),
            ));
            return;
        }
        for (param, arg) in params.iter().zip(args) {
            if !arg_compatible(arg, param) {
                self.diags.push(Diagnostic::error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{param}`"),
                ));
            }
        }
    }

    /// The return type of a method call `recv.name(...)`: a built-in method, a user-declared
    /// method, or — when the receiver defers to runtime (`dyn`/hole) — the deferred type itself.
    fn method_call_return(&self, recv: &Type, name: &str) -> Type {
        if let Some(t) = stdlib::method_return(recv, name) {
            return t;
        }
        if let Type::Named(n) = recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            return sig.ret.clone();
        }
        if recv.defers_to_runtime() {
            return recv.clone();
        }
        Type::Unknown
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
        // A field/member access on a `dyn` (or hole) receiver stays deferred.
        if recv.defers_to_runtime() {
            return recv;
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
/// [`Type::is_builtin_name`]): the prelude `Ordering` enum that `compare` returns and `Comparable`
/// maps to a bool. It resolves to a [`Type::Named`] but is a legal annotation, so the unknown-type
/// check (`E0013`) accepts it. (The bare `list`/`map`/`set` spellings are now lattice built-ins —
/// they desugar to collections of `dyn`.)
const PRELUDE_TYPES: &[&str] = &["Ordering"];

/// Whether an argument of type `arg` may be passed where `param` is expected. Subtyping, plus two
/// leniencies that keep dynamic and numeric calls free of false positives: a `dyn`/hole on either
/// side defers, and a numeric argument is accepted for a numeric parameter (`int` widens to
/// `float`).
fn arg_compatible(arg: &Type, param: &Type) -> bool {
    Type::subtype(arg, param)
        || arg.defers_to_runtime()
        || param.defers_to_runtime()
        || (arg.is_numeric() && param.is_numeric())
}

/// Replace each generic type parameter (a `Named` whose name is in `params`) with `dyn`, deeply.
/// Generic parameters are erased at runtime, so a method like `set(v: T)` accepts any argument —
/// erasing `T` to `dyn` keeps argument checking from a false positive against the erased name.
fn erase_type_params(ty: Type, params: &HashSet<String>) -> Type {
    let erase = |t: Type| erase_type_params(t, params);
    match ty {
        Type::Named(n) if params.contains(&n) => Type::Dyn,
        Type::List(t) => Type::List(Box::new(erase(*t))),
        Type::Set(t) => Type::Set(Box::new(erase(*t))),
        Type::Map(k, v) => Type::Map(Box::new(erase(*k)), Box::new(erase(*v))),
        Type::Option(t) => Type::Option(Box::new(erase(*t))),
        Type::Result(t, e) => Type::Result(Box::new(erase(*t)), Box::new(erase(*e))),
        Type::Fn { params: ps, ret } => Type::Fn {
            params: ps.into_iter().map(erase).collect(),
            ret: Box::new(erase(*ret)),
        },
        other => other,
    }
}

/// Unify a running element type with the next element's type, for synthesizing a list literal's
/// element type. Returns the unified type, or `None` if the two are concretely incompatible (a
/// heterogeneous list). A deferred type (hole / `dyn`) is compatible with anything; two numeric
/// types unify to `float` (the int/float promotion the runtime performs).
fn unify_element(acc: &Type, next: &Type) -> Option<Type> {
    if acc.defers_to_runtime() {
        return Some(next.clone());
    }
    if next.defers_to_runtime() {
        return Some(acc.clone());
    }
    if Type::subtype(next, acc) {
        return Some(acc.clone());
    }
    if Type::subtype(acc, next) {
        return Some(next.clone());
    }
    if acc.is_numeric() && next.is_numeric() {
        return Some(Type::Float);
    }
    None
}

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
