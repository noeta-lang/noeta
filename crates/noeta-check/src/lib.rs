//! The type checker: the static front-end between parsing and compilation.
//!
//! [`check`] walks a [`Program`] and returns the type diagnostics it finds. It is exposed to
//! the pipeline as the `checked` salsa query (`noeta-db`), slotted between `ast` and `bytecode`.
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
//! ## Hole tolerance — eliminated at boundaries, residual in the interior
//!
//! Where the checker cannot infer a precise type it falls back to the inference hole
//! [`Type::Unknown`], and [`Type::subtype`] treats a hole as compatible in both directions, so
//! **subsumption never fires on missing information**. The inferred-static track removes that hole
//! at every *typed boundary*: a named `fn`/method must carry signatures (`E0022`), each argument is
//! checked against its parameter type and each `return` against the declared return, and a hole
//! that reaches a binding with nothing to determine it is `E0023`. What remains tolerated is an
//! *interior* hole — an un-typed prelude result, a numeric hole — where flagging it would risk a
//! false positive; that residual leniency is deliberate and recorded (see the `noeta-types` module
//! docs and the README's "known gaps").
//!
//! This posture is also what lets the checker run as a single shared front-end for both backends
//! without diverging the differential oracle: a rejected program is rejected identically by both,
//! and an interior-hole gap is an error in neither.
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
//!   struct/class/enum, not a name brought in by a `use`, and not a generic type parameter in
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
//! Hindley–Milner: subtyping (`dyn` widening, directional method resolution, struct width) is
//! load-bearing and defeats HM's symmetric unification. The fallback to [`Type::Unknown`] is gone
//! at every typed boundary; only an un-inferable *interior* type stays tolerated, by design.

use std::collections::{HashMap, HashSet};

use noeta_ast::{
    AttrValue, Attribute, BinaryOp, ClassDecl, DeriveSpec, EnumDecl, Expr, FieldDecl, FnDecl,
    ForPattern, ImplBlock, ImplDecl, MatchArm, PackedDirective, Param, Pattern, Program, Stmt,
    StrPart, StructDecl, TypeParam, TypeRef, UnaryOp,
};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_edition::{Edition, EditionMap};
use noeta_span::Span;
use noeta_types::{BuiltinTrait, Type};

mod attributes;
mod collect;
mod decls;
mod effects;
mod env;
mod packed;
mod prelude;
mod relevance;
mod sites;
mod stdlib;
pub mod tiers;
mod traits;

pub use tiers::{
    Activated, DeclaredTier, DocTarget, ResolvedProvider, TextBlock, TierFn, activate_tiers,
    activate_tiers_with, dedent_doc, extend_reflection, resolve_docs, resolve_texts,
};

use effects::*;
use env::*;
use sites::SiteMaps;
pub use sites::{DestructorRelevance, Sites};

/// The full output of one checker run: the diagnostics **and** the resolved-type map both
/// backends need. The two were once harvested by separate public entry points ([`check`] and
/// [`resolve_type_of_sites`]), each re-running the whole checker; a CLI `run` therefore
/// type-checked the program two-to-three times (the gate plus one re-derivation per backend).
/// [`check_all`] runs the checker **once** and hands back both, so an orchestrator can gate on
/// `diagnostics` and thread `type_of_sites` into the backends without re-checking. Because the
/// map is a pure function of the program, this only changes *how many times* the checker runs,
/// never the result — the differential oracle is unaffected.
#[derive(Debug, Clone)]
pub struct Checked {
    /// Every diagnostic found, in source order. Empty ⇒ well-typed (modulo the documented
    /// interior-hole tolerance).
    pub diagnostics: Vec<Diagnostic>,
    /// Every expression's inferred static type, keyed by span — the hover index. Empty unless the
    /// checker ran via [`check_all_with_types`] (the IDE path); the compile path leaves it empty.
    /// An IDE read-side index, not a compile input — which is why it lives beside [`Sites`], not
    /// inside it.
    pub expr_types: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// The compile-input bundle both backends consume — see [`Sites`].
    pub sites: Sites,
    /// Method-bundle bindings by target type name (kernel-methods K4): each
    /// `impl <module>.<Bundle> for T {}` as `(module qualified identity, bundle name)`. The IDE
    /// reads it to offer bound methods in member completion; a handful of entries, so it is
    /// populated on every run.
    pub bundle_bindings: HashMap<String, Vec<(String, String)>>,
    /// Every `@packed` struct's flat layout, by type name — the IDE's storage-fact index (hover and
    /// inlay hints read it to say "this type is packed, this list is flat/column-major"). An IDE
    /// read-side index like [`Checked::expr_types`], not a compile input — which is why it lives
    /// beside [`Sites`], not inside it; a handful of entries, so it is populated on every run, like
    /// [`Checked::bundle_bindings`].
    pub packed_layouts: HashMap<String, noeta_ast::reflect::PackedLayout>,
}

/// Everything that varies a whole-program check, so callers configure one entry point
/// ([`check_all_with`]) instead of the checker growing a `_with_types_and_editions_and_registry`
/// combinatorial family. `Default` is an ordinary compile-path check (no type index, process-global
/// registry, every declaration at [`Edition::DEFAULT`]) — identical to [`check_all`].
#[derive(Default)]
pub struct CheckOptions {
    /// Record every expression's inferred type into [`Checked::expr_types`] — the span→type index the
    /// IDE hover path reads. Off on the compile path (it pays nothing for the index).
    pub record_expr_types: bool,
    /// A per-session extension [`Registry`] (instance-registry F2) to resolve native
    /// modules/functions/extern types/bundles against, instead of the process-global default. `None`
    /// routes every lookup through the default registry — identical to an ordinary check.
    pub registry: Option<&'static noeta_stdlib::registry::Registry>,
    /// Which language [`Edition`] governs each source, keyed by `SourceId` (editions arc): the loader
    /// builds it per merged program. Empty means every declaration is [`Edition::DEFAULT`].
    pub editions: EditionMap,
}

impl std::fmt::Debug for CheckOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckOptions")
            .field("record_expr_types", &self.record_expr_types)
            // The registry is a `&'static` handle whose contents aren't `Debug`; report only presence.
            .field("registry", &self.registry.is_some())
            .field("editions", &self.editions)
            .finish()
    }
}

/// Type-check a program once against explicit [`CheckOptions`] — the single configurable entry every
/// other `check_all*` is a thin preset of. Edition-, type-index-, and registry-aware in one call, so
/// a tool that has (say) both a per-package [`EditionMap`] and the IDE type index asks for both
/// without a bespoke entry point.
pub fn check_all_with(program: &Program, opts: CheckOptions) -> Checked {
    check_all_impl(
        program,
        opts.record_expr_types,
        opts.registry,
        opts.editions,
    )
}

/// Type-check a program once, returning both its diagnostics and its resolved-type map. This is
/// the single-pass entry point the hot paths (the CLI, the conformance/differential harnesses,
/// the `noeta-db` `bytecode` query) use so the checker runs exactly once per program.
pub fn check_all(program: &Program) -> Checked {
    check_all_with(program, CheckOptions::default())
}

/// Like [`check_all`], but additionally records every expression's inferred type into
/// [`Checked::expr_types`] — the span→type index the IDE hover feature reads. Diagnostics are
/// identical either way — recording types is a pure side-channel.
pub fn check_all_with_types(program: &Program) -> Checked {
    check_all_with(
        program,
        CheckOptions {
            record_expr_types: true,
            ..CheckOptions::default()
        },
    )
}

/// [`check_all`] against the per-source [`EditionMap`] the loader built for a merged program, so the
/// checker can recover each declaration's own language [`Edition`] from its span (editions compiler
/// arc). Passing an empty map is identical to [`check_all`]. The whole-program compile/run and tool
/// paths call this with `Linked::editions`; today, with one edition, the result is byte-identical to
/// [`check_all`], but the per-package edition is now carried into the checker.
pub fn check_all_with_editions(program: &Program, editions: EditionMap) -> Checked {
    check_all_with(
        program,
        CheckOptions {
            editions,
            ..CheckOptions::default()
        },
    )
}

/// [`check_all`] against an explicit per-session extension [`Registry`] (instance-registry F2)
/// instead of the process-global default: the checker resolves every native module/function/extern
/// type/bundle against `registry`, so an embedding host that assembled its own extension set gets a
/// check that sees exactly those extensions — the same set its paired VM will run against. Passing
/// the process-global default here is identical to [`check_all`]. The registry is `&'static`
/// because a [`Registry`]'s lookups already return `&'static` data.
pub fn check_all_with_registry(
    program: &Program,
    registry: &'static noeta_stdlib::registry::Registry,
) -> Checked {
    check_all_with(
        program,
        CheckOptions {
            registry: Some(registry),
            ..CheckOptions::default()
        },
    )
}

fn check_all_impl(
    program: &Program,
    record_expr_types: bool,
    registry: Option<&'static noeta_stdlib::registry::Registry>,
    editions: EditionMap,
) -> Checked {
    let mut checker = Checker {
        record_expr_types,
        registry,
        editions,
        ..Checker::default()
    };
    checker.register_prelude();
    checker.collect_imports(program);
    checker.collect(program);
    // Compute destruct-reachability + parameter relevance before checking bodies (local-binding
    // relevance is recorded inline during `check_program`, and needs the reachable set ready).
    checker.compute_relevance(program);
    checker.check_semantic_roles(program);
    checker.check_tier_decls(program);
    checker.check_program(program);
    checker.into_checked()
}

/// [`check_all`], but the checker **stays alive as a [`SessionChecker`]** — the debug console's
/// seed (session-checker C3): console fragments then check against everything the program
/// declared and bound, exactly as later REPL entries check against earlier ones. The returned
/// [`Checked`] is identical to [`check_all`]'s (same phases, same env — merely kept instead of
/// dropped, the same move `compile_with_sites_session` made for the compiler).
pub fn check_all_session(program: &Program) -> (Checked, SessionChecker) {
    check_all_session_with(program, EditionMap::default())
}

/// [`check_all_session`] against the per-source [`EditionMap`] the loader built for the program under
/// debug/REPL (editions arc): the seeded whole-program check — and the session that outlives it —
/// resolve each declaration's edition from its span, exactly as the batch checker does. Passing an
/// empty map is identical to [`check_all_session`].
pub fn check_all_session_with(
    program: &Program,
    editions: EditionMap,
) -> (Checked, SessionChecker) {
    let mut checker = Checker {
        editions,
        ..Checker::default()
    };
    checker.register_prelude();
    checker.collect_imports(program);
    checker.collect(program);
    checker.compute_relevance(program);
    checker.check_semantic_roles(program);
    checker.check_tier_decls(program);
    let mut env: Env = vec![HashMap::new()];
    checker.check_program_in(program, &mut env);
    let checked = Checked {
        diagnostics: std::mem::take(&mut checker.diags),
        expr_types: checker.sites.expr_types.clone(),
        sites: checker.sites.clone().into_sites(checker.relevance.clone()),
        bundle_bindings: checker.bundle_bindings_public(),
        packed_layouts: checker.packed_layouts_public(),
    };
    // The whole-program check above ran strict (file mode) — unknown names in a debugged program
    // are real errors. But the returned session, over which console fragments and later entries
    // are checked, defers unknown names (F1): a fragment may reference a name a *later* fragment
    // defines, and frame locals a fragment reads are seeded per-evaluation, not in this env.
    checker.session_mode = true;
    (checked, SessionChecker { checker, env })
}

/// A persistent, incremental checker for a REPL / debug-console session (session-checker C0/C1):
/// entry *N* type-checks against the environment and registries entries *1..N-1* accumulated, then
/// commits its own declarations and bindings for entry *N+1*. It wraps the ordinary [`Checker`] —
/// the same phases, the same language rules, no fork — with the session adaptations:
///
/// - the **global scope persists** across entries (via [`Checker::check_program_in`]), so a later
///   entry sees earlier bindings and the mut-stability rules (E0006/E0007) apply across entries
///   exactly as they would across statements;
/// - **`collect` runs per entry**, appending to the persistent registries — an entry may
///   forward-reference within itself (as any program can) but not into a *future* entry: that is
///   prompt semantics, and it surfaces as the ordinary unknown-name diagnostic;
/// - **destruct-reachability re-fixpoints over the accumulated registries** each entry, so an
///   entry's `destruct` class makes an earlier entry's containing type reachable for everything
///   checked from now on (an earlier entry's already-computed relevance is inherently stale —
///   the same staleness any prompt redefinition has);
/// - **diagnostics drain per entry**; the span-keyed site maps accumulate (per-entry `SourceId`s
///   make collisions impossible) and can be snapshotted for a checked compile.
///
/// Rebinding needs no REPL-specific policy: the language already allows re-`mut` (even retyped),
/// reassignment under the stability rules, and type redefinition; only the reserved prelude /
/// native names (E0046 / E0049) refuse — so a session entry is checked by exactly the rules a
/// file is.
pub struct SessionChecker {
    checker: Checker,
    /// The persistent global scope — one frame, never popped; entries push/pop inner scopes on
    /// top of it as any block would.
    env: Env,
}

impl Default for SessionChecker {
    fn default() -> SessionChecker {
        SessionChecker::new()
    }
}

impl std::fmt::Debug for SessionChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionChecker")
            .field("globals", &self.env.first().map_or(0, |frame| frame.len()))
            .finish_non_exhaustive()
    }
}

impl SessionChecker {
    /// A fresh session: prelude registered, empty registries, an empty persistent global scope.
    pub fn new() -> SessionChecker {
        Self::with_registry_opt(None)
    }

    /// A fresh session bound to an explicit per-session extension [`Registry`] (instance-registry
    /// F2): every native name the session's entries reference resolves against `registry` rather
    /// than the process-global default — the session-mode counterpart of [`check_all_with_registry`],
    /// so an embedding host's REPL/debug console sees exactly the host's extension set.
    pub fn with_registry(registry: &'static noeta_stdlib::registry::Registry) -> SessionChecker {
        Self::with_registry_opt(Some(registry))
    }

    fn with_registry_opt(
        registry: Option<&'static noeta_stdlib::registry::Registry>,
    ) -> SessionChecker {
        let mut checker = Checker {
            session_mode: true,
            registry,
            ..Checker::default()
        };
        checker.register_prelude();
        SessionChecker {
            checker,
            env: vec![HashMap::new()],
        }
    }

    /// Type-check one entry against the accumulated session and commit its declarations and
    /// bindings. Returns **this entry's** diagnostics (no errors ⇒ the entry is well-typed against
    /// everything the session knows). The checker's per-entry lexical scratch is reset first, so
    /// one entry's context can never leak into the next.
    ///
    /// **Transactional:** an entry that ERRORS commits nothing — the prompt skips running such an
    /// entry, so the checker must not remember its declarations or bindings either (later entries
    /// would otherwise check against functions/types/bindings the runtime never bound). The whole
    /// session state is cloned before the entry and restored on error — prompt-scale registries,
    /// so the clone is cheap insurance, never a hot path. Warning-only entries commit (the prompt
    /// runs them).
    pub fn check_entry(&mut self, entry: &Program) -> Vec<Diagnostic> {
        let backup_checker = self.checker.clone();
        let backup_env = self.env.clone();
        self.reset_scratch();
        let diag_mark = self.checker.diags.len();
        self.checker.collect_imports(entry);
        self.checker.collect(entry);
        // Re-run the reachability fixpoint over the ACCUMULATED registries (this entry's
        // `destruct` class can make an earlier entry's type reachable), and record this entry's
        // parameter relevance.
        self.checker.compute_relevance(entry);
        self.checker.check_semantic_roles(entry);
        self.checker.check_tier_decls(entry);
        self.checker.check_program_in(entry, &mut self.env);
        let diagnostics = self.checker.diags.split_off(diag_mark);
        if diagnostics
            .iter()
            .any(|d| d.severity == noeta_diagnostics::Severity::Error)
        {
            self.checker = backup_checker;
            self.env = backup_env;
        }
        diagnostics
    }

    /// Type-check a debug-console **fragment** against the session: the fragment is wrapped as a
    /// bare closure expression whose parameters are `params` — the paused frame's in-scope local
    /// names, the same shape the VM compiles it as — and checked as one entry (session-checker C3,
    /// shared by `noeta dap` and `noeta mcp`). Closure parameters are inference-typed, so frame
    /// locals check as unconstrained (never a false positive); everything the fragment touches in
    /// the PROGRAM — functions, methods, types, globals — checks precisely. The bare-closure
    /// wrapper commits nothing to the session: its bindings are closure-locals to the checker, so
    /// cross-fragment console bindings stay runtime-deferred — under-constrained, never wrong.
    pub fn check_closure_fragment(
        &mut self,
        body: &noeta_ast::Program,
        params: &[String],
    ) -> Vec<Diagnostic> {
        use noeta_ast::{ClosureBody, Expr, Param, Program, Stmt};
        let span = body.span;
        let params = params
            .iter()
            .map(|name| Param {
                name: name.clone(),
                name_span: span,
                ty: None,
                default: None,
                span,
            })
            .collect();
        let wrapper = Program {
            stmts: vec![Stmt::Expr {
                expr: Expr::Closure {
                    params,
                    ret: None,
                    body: ClosureBody::Block(body.stmts.clone()),
                    span,
                },
                span,
            }],
            span,
        };
        self.check_entry(&wrapper)
    }

    /// A snapshot of the accumulated compile-input bundle — every entry's site maps so far
    /// (span-keyed by per-entry `SourceId`s, so a consumer's lookups only ever hit the right
    /// entry). What a checked session compile (C5) threads into `compile_with_sites`.
    pub fn sites_snapshot(&self) -> Sites {
        self.checker
            .sites
            .clone()
            .into_sites(self.checker.relevance.clone())
    }

    /// Reset the per-entry lexical scratch to its neutral state. The whole-program phases leave
    /// these neutral on a *clean* pass, but an entry that errored mid-body may not — and a session
    /// must isolate entries regardless.
    fn reset_scratch(&mut self) {
        self.checker.current_type = None;
        self.checker.in_dev_tier = false;
        self.checker.type_params.clear();
        self.checker.current_ret = Type::Unknown;
        self.checker.collected_returns = None;
        self.checker.current_yield = None;
        self.checker.current_async = false;
        self.checker.concurrent_depth = 0;
        self.checker.loop_depth = 0;
        self.checker.index_on_list.clear();
    }
}

/// Type-check a program and return every diagnostic found, in source order. An empty result
/// means the program is well-typed (modulo the deliberate interior-hole tolerance documented
/// in the module docs). A thin projection of [`check_all`] for callers that need only the
/// diagnostics; the hot paths use [`check_all`] to avoid a second checker run.
pub fn check(program: &Program) -> Vec<Diagnostic> {
    check_all(program).diagnostics
}

/// Resolve the precise static type of every `type_of(value)` whose operand is concretely typed,
/// keyed by the `Expr::TypeOf` span — the input both backends use to bake a full-fidelity `Type`
/// constant (`type_of([1])` ⇒ `Type.List(Type.Int)`) instead of the erased runtime head constructor
/// (P2.3 fidelity A). Runs the same inference as [`check`] (diagnostics discarded) and is **pure**,
/// so both backends harvest identical maps on the same program — the differential holds. A
/// `dyn`/union/un-inferred operand is omitted, leaving that site on the runtime path (fidelity B).
///
/// A thin projection of [`check_all`] for callers (a backend's self-deriving default) that have
/// no precomputed map to thread; orchestrators that already gate with [`check_all`] reuse its
/// `type_of_sites` instead of calling this.
pub fn resolve_type_of_sites(program: &Program) -> HashMap<Span, noeta_ast::reflect::TypeRepr> {
    check_all(program).sites.type_of_sites
}

/// Resolve every list-construction site whose element type is a `@packed` struct, keyed by the
/// constructing expression's span → the element's flat [`PackedLayout`] (P-PACK Phase 2). Both
/// backends consult this to lay out a `List<packed>` as one contiguous raw-primitive buffer rather
/// than N boxed objects + N pointers. Runs the same inference as [`check`] (diagnostics discarded)
/// and is **pure**, so both backends harvest identical maps on the same program — the flat layout
/// stays invisible to `RunResult` and the differential holds. A thin projection of [`check_all`] for
/// a backend with no precomputed map to thread.
pub fn resolve_packed_list_sites(
    program: &Program,
) -> HashMap<Span, noeta_ast::reflect::PackedLayout> {
    check_all(program).sites.packed_list_sites
}

/// Project a checker [`Type`] onto the reflection [`TypeRepr`] for a **concrete** `type_of` operand,
/// or `None` when the site must stay on the runtime path: a `dyn`/union/un-inferred (`Unknown`) top
/// type carries no fixed head constructor to bake (a union's runtime value is more precise — its
/// actual member — than `Type.Union` would be).
/// Whether `repr` is a **non-generic nominal** type — a declared `struct`/`class`/`enum` (or an
/// unknown-kind `Named`) with no type arguments (R2). The runtime head-only classification recovers
/// such a type in full (its shape name), so a construction tag would be redundant; only generic
/// instantiations and the collections need one. See [`Checker::note_construction`].
fn is_nongeneric_nominal(repr: &noeta_ast::reflect::TypeRepr) -> bool {
    use noeta_ast::reflect::TypeRepr;
    matches!(
        repr,
        TypeRepr::Struct(_, args)
            | TypeRepr::Class(_, args)
            | TypeRepr::Enum(_, args)
            | TypeRepr::Named(_, args)
        if args.is_empty()
    )
}

fn type_to_repr_top(
    ty: &Type,
    kinds: &HashMap<String, noeta_types::TypeKind>,
) -> Option<noeta_ast::reflect::TypeRepr> {
    match ty {
        // An abstract kind-type (`Enum`/`Struct`/`Class`) has no precise static head — the runtime
        // value is a concrete enum/struct/class — so it defers to the runtime `type_of` path.
        Type::Dyn | Type::Unknown | Type::Union(_) | Type::Kind(_) => None,
        concrete => Some(type_to_repr(concrete, kinds)),
    }
}

/// Total projection of a checker [`Type`] onto a reflection [`TypeRepr`], used for the nested
/// element/argument types once a concrete head is committed. A nested hole, `dyn`, or union erases
/// to [`TypeRepr::Dyn`] (the runtime erases generics anyway; nested-union fidelity is out of scope).
/// A nominal type is classified into its kind variant (`Enum`/`Struct`/`Class`) via `kinds`,
/// matching the runtime classification; an unknown-kind name falls back to `Named`.
fn type_to_repr(
    ty: &Type,
    kinds: &HashMap<String, noeta_types::TypeKind>,
) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    let rec = |t: &Type| type_to_repr(t, kinds);
    match ty {
        Type::Int => TypeRepr::Int,
        Type::Float => TypeRepr::Float,
        Type::F32 => TypeRepr::F32,
        // `f64` is bit-identical to `float` at runtime (P-NUM-SYM), so reflection reports `Float` —
        // consistent with the shared value, just as the fixed-width integers report `Int`.
        Type::F64 => TypeRepr::Float,
        // Fixed-width integers are **erased to `int`** at runtime (Tier W), so runtime reflection
        // (`type_of`) cannot recover the width — it reports `Int`, consistent with the erased value.
        Type::IntN { .. } => TypeRepr::Int,
        Type::Bool => TypeRepr::Bool,
        Type::String => TypeRepr::Str,
        Type::Bytes => TypeRepr::Bytes,
        Type::Unit => TypeRepr::Unit,
        Type::List(e) => TypeRepr::List(Box::new(rec(e))),
        Type::Set(e) => TypeRepr::Set(Box::new(rec(e))),
        Type::Option(e) => TypeRepr::Option(Box::new(rec(e))),
        Type::Map(k, v) => TypeRepr::Map(Box::new(rec(k)), Box::new(rec(v))),
        Type::Result(o, e) => TypeRepr::Result(Box::new(rec(o)), Box::new(rec(e))),
        Type::Named(n, args) => {
            let args = args.iter().map(rec).collect();
            match kinds.get(n) {
                Some(noeta_types::TypeKind::Enum) => TypeRepr::Enum(n.clone(), args),
                Some(noeta_types::TypeKind::Struct) => TypeRepr::Struct(n.clone(), args),
                Some(noeta_types::TypeKind::Class) => TypeRepr::Class(n.clone(), args),
                None => TypeRepr::Named(n.clone(), args),
            }
        }
        Type::Fn { params, ret } => {
            TypeRepr::Fn(params.iter().map(rec).collect(), Box::new(rec(ret)))
        }
        // A tuple has no reflection descriptor today (like a union); it erases to `dyn` in `type_of`.
        Type::Tuple(_) | Type::Union(_) | Type::Dyn | Type::Unknown | Type::Kind(_) => {
            TypeRepr::Dyn
        }
    }
}

/// A declaration **kind** an attribute may attach to — the closed vocabulary of `@attribute(...)`
/// and the axis `#[Foo(...)]` placement is checked on (P2.5). One per declaration site the attribute
/// system reaches. These are source positions, not runtime value types (a `Field`/`Variant` is not a
/// value), so they live only in the checker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Struct,
    Class,
    Enum,
    Function,
    Method,
    Field,
    Variant,
}

impl TargetKind {
    /// The directive spelling (`@attribute(Method, …)`) ⇄ kind, also used in diagnostics.
    fn from_name(name: &str) -> Option<TargetKind> {
        Some(match name {
            "Struct" => TargetKind::Struct,
            "Class" => TargetKind::Class,
            "Enum" => TargetKind::Enum,
            "Function" => TargetKind::Function,
            "Method" => TargetKind::Method,
            "Field" => TargetKind::Field,
            "Variant" => TargetKind::Variant,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            TargetKind::Struct => "Struct",
            TargetKind::Class => "Class",
            TargetKind::Enum => "Enum",
            TargetKind::Function => "Function",
            TargetKind::Method => "Method",
            TargetKind::Field => "Field",
            TargetKind::Variant => "Variant",
        }
    }
}

/// One method-bundle binding (kernel-methods K1): which registered bundle a type acquired via
/// `impl <module>.<Bundle> for T {}`, and through which module identity runtime dispatch routes.
#[derive(Clone)]
struct BoundBundle {
    /// The owning module's root-qualified identity (`"std.vec"`).
    module: String,
    bundle: &'static noeta_stdlib::ExtBundle,
    /// The binding's trait span — conflict reporting orders by it (the textually-later binding
    /// carries the diagnostic).
    span: Span,
}

// `Clone` so a [`SessionChecker`] entry is transactional (clone-before, restore-on-error) —
// prompt-scale state, so the per-entry clone is cheap insurance, never a hot path.
#[derive(Clone, Default)]
struct Checker {
    /// User-declared enums: name → variants (each with its **accurate** payload types, like a
    /// struct's fields in [`Self::records`]).
    enums: HashMap<String, Vec<VariantInfo>>,
    /// Top-level functions: name → signature.
    functions: HashMap<String, FnSig>,
    /// Records/classes: name → declared fields (name, type).
    records: HashMap<String, Vec<(String, Type)>>,
    /// Class name → the set of its fields declared `mut`. Drives the `x.f = v` field-assignment
    /// check (Phase 5.2): only a `mut` field may be assigned in place (else E0033). Records never
    /// have `mut` fields, so they never appear here.
    mut_fields: HashMap<String, HashSet<String>>,
    /// Type name → the set of its **private** fields (object-model slice 2d). A value `struct`'s
    /// fields are always public (it never appears here); a reference `class`'s fields default
    /// private, so this holds every field *not* declared `pub`. A private field is visible only
    /// inside the declaring type's own methods ([`Checker::current_type`]); read/write/construction
    /// elsewhere is E0035.
    private_fields: HashMap<String, HashSet<String>>,
    /// While checking a type's own methods/destructor, the name of that type — so a private-field
    /// access on `self` *or* any same-type value is permitted (the type-scoped privacy rule). `None`
    /// at top level and inside free functions.
    current_type: Option<String>,
    /// While checking the body of a fn lifted from a **dev-tier block** (`@test`/…, slice 6d), the
    /// type-scoped field-privacy gate is relaxed to white-box access: co-located developer tooling
    /// may read/write/construct its module's private fields (the Rust `#[cfg(test)]` model). `false`
    /// for ordinary fns and methods. Set from [`FnDecl::is_dev_tier`] in [`Checker::check_fn`].
    in_dev_tier: bool,
    /// When set, [`Checker::synth`] records every expression's inferred type into
    /// [`SiteMaps::expr_types`] for the IDE hover path. Off by default so the compile path is
    /// unaffected; enabled by [`check_all_with_types`].
    record_expr_types: bool,
    /// A REPL / debug-console session (set only by [`SessionChecker`]). Relaxes the unknown-name
    /// gate (F1): a name undefined *this* entry may be defined in a *later* one, so an unresolved
    /// reference stays deferred to the runtime rather than being a static `E0005` — the
    /// cross-entry forward reference the prompt relies on. A whole-file check (the default, and
    /// the hot-reload transactional gate) has no future entry, so an unknown name is an error.
    session_mode: bool,
    /// Every top-level value binding's name, collected in the pre-pass (F1). Top-level globals are
    /// **hoisted** — a function body may reference one declared textually later — so the
    /// unknown-name gate treats them all as known regardless of order. (A top-level *direct*
    /// reference to a not-yet-bound global still fails at runtime; this gate does not try to catch
    /// that ordering case, only genuine typos.)
    global_binding_names: HashSet<String>,
    /// Declared type → its kind (`Enum`/`Struct`/`Class`). Drives the abstract kind-type
    /// membership rule (`Named(n) <: Enum` iff `n` is an enum) — the registry-dependent piece the
    /// pure lattice cannot decide, consulted by [`Checker::assignable`].
    type_kinds: HashMap<String, noeta_types::TypeKind>,
    /// User-defined methods: (type name, method name) → signature. Populated from class methods and
    /// `impl`-block methods so a method call on a user object resolves to a real type, with the
    /// owning class's generic parameters erased to `dyn` (they accept any argument).
    methods: HashMap<(String, String), FnSig>,
    /// Whether each `(type, method)` is an **instance** method (its body references `self`) or an
    /// associated function (never touches `self`) — DERIVED at collection time (prelude-redesign
    /// EX.2; well-defined because member access is explicit, EX.1). Drives the wrong-way-call check
    /// (E0047) and the associated-vs-instance shape of a `Type.method` handle.
    method_instance: HashMap<(String, String), bool>,
    /// Which built-in traits each user type satisfies: type name → set of trait names it `@derive`s
    /// or `impl`s. The basis (with the built-in-type table in [`Self::satisfies`]) for enforcing a
    /// generic call's trait bounds (S4.2).
    trait_impls: HashMap<String, HashSet<BuiltinTrait>>,
    /// The subset of [`Self::trait_impls`] that came from `@derive(...)` (not a hand-written
    /// `impl`). A **generic** type's derive is conditional on its instantiated fields
    /// (derive-soundness S4); a hand-written impl is unconditional. Keyed like `trait_impls`.
    derived_traits: HashMap<String, HashSet<BuiltinTrait>>,
    /// Each generic user type's type-parameter **names**, in order — so a field/method access can
    /// map an instance's type arguments (`Box<int>`) back onto the declaration's parameters (`T`)
    /// and read a field/return as `int` rather than the bare parameter or `dyn` (S4.5).
    generic_types: HashMap<String, Vec<String>>,
    /// Names bound to a native module by a `use std.{…}` import (`json`, `fs`, …) or a nested
    /// import (`use std.http.client` → `client`), each mapped to the module's **root-qualified
    /// identity** (`"std.json"`, `"std.http.client"`). A call `m.f(args)` on the bound name resolves
    /// through [`stdlib::module_return`] against that identity.
    modules: HashMap<String, String>,
    /// Names bound to a **namespace group** by `use std.http` — each mapped to the group's
    /// **root-qualified prefix** (`http` → `"std.http"`). A member access `http.client` resolves one
    /// hop through [`noeta_stdlib::registry::Registry::resolve_namespace_child`] against this prefix;
    /// a landing module identity is recorded in `namespace_module_sites` for lowering. The handle is
    /// not a value on its own — a bare reference is an error (a group must be dotted into).
    namespaces: HashMap<String, String>,
    /// Names brought into scope bare by a selective member import (`use std.math.sqrt` → `sqrt`),
    /// each mapped to its `(module, func)`. A bare call `sqrt(args)` types through
    /// [`stdlib::module_return`] exactly like the qualified `math.sqrt(args)`.
    imported_fns: HashMap<String, (String, String)>,
    /// Local names bound to a **registered extern type** by a `use std.<ns>.<Type> [as Alias]`
    /// import, each mapped to that type's **qualified identity** (`Uuid` → `std.id.Uuid`,
    /// `Metric` → `std.metrics.Counter`). An extern annotation resolves through this map — so a
    /// native type must be imported to be named (like a user type), an alias renames it, and a
    /// user-declared type of the same short name shadows it (user names in [`Self::types`] take
    /// precedence). This is what lets a file pull in two same-short-named types from different
    /// namespaces, and a native `Counter` coexist with a user's own.
    extern_types: HashMap<String, String>,
    /// Every name a type annotation may legally resolve to: declared records/classes/enums plus
    /// names brought in by a `use` (whether merged in by the linker or left as an opaque stub).
    /// Built-in names and in-scope generic parameters are *not* stored here — they are checked
    /// separately (a built-in via [`Type::is_builtin_name`], a parameter via [`Self::type_params`]).
    types: HashSet<String>,
    /// Standalone `impl Trait for T {}` declarations, grouped by target type name, as
    /// `(trait_name, trait_span)` occurrences. Collected in pass 1 so each type's coherence check
    /// (`check_coherence`) counts standalone impls alongside its `@derive`s and in-body `impl`s.
    standalone_impls: HashMap<String, Vec<(String, Span)>>,
    /// Method-bundle bindings (kernel-methods K1): target type name → the native bundles it
    /// explicitly bound via `impl <module>.<Bundle> for T {}`, each with the module's
    /// root-qualified identity (the runtime dispatch key). Resolved in a post-collect sweep so a
    /// binding is visible to method typing regardless of statement order; validated (packed
    /// target, constraint, conflicts) at the impl site in pass 2.
    bundle_impls: HashMap<String, Vec<BoundBundle>>,
    /// Every struct marked `@attribute` — the names usable in `#[...]` annotation position (P2.5,
    /// the opt-in that replaced the `Attribute` marker trait). The E0029 capability gate and
    /// `attributes_of::<T>()` both consult this set. Attributes are **structs only**.
    attributes: HashSet<String>,
    /// Every enum marked `@semantic` (plus the built-in `Semantic`) — the enums whose fieldless
    /// variants may be named by a `@role(Enum.Variant)` tag. The role-validation pass consults this
    /// set, so it runs after `collect` has registered every declaration (a struct's `@role` may name
    /// a `@semantic` enum declared later in the file).
    semantic_enums: HashSet<String>,
    /// The tier name-space (tier-providers T2): built-ins ∪ this program's `@tier` declarations.
    /// Built by [`Self::check_tier_decls`] (which also validates each declaration, E0051); the
    /// in-place `TierBlock` arm resolves names and config attributes against it.
    tier_registry: tiers::TierRegistry,
    /// Every struct marked `@packed` (P-PACK) — the value structs laid out unboxed and contiguous.
    /// Collected in pass 1 so a packed struct's field-type validation (a field may be another packed
    /// struct declared later) sees the full set, and so `List<Packed>` specialization can consult it.
    packed_structs: HashSet<String>,
    /// Every `@packed(layout: column)` struct (P-SIMD C2) — a subset of [`Self::packed_structs`]
    /// whose lists are stored column-major. Collected alongside `packed_structs` so `packed_layout`
    /// can flag the runtime schema; layout is a performance-only property (behaviour-invisible).
    column_structs: HashSet<String>,
    /// An attribute's optional placement restriction from `@attribute(Method, Function, …)`:
    /// attribute name → the declaration kinds a `#[ThisType(...)]` use may attach to. An attribute
    /// *absent* from this map (bare `@attribute`) is unrestricted. Enforced per use site (E0030);
    /// kind names are validated when this is built.
    attachable: HashMap<String, Vec<TargetKind>>,
    /// Per struct/class, the fields that carry a default (`name: T = …`) and so are **optional** in a
    /// `#[Foo(...)]` attribute construction (object-model slice 6i): such a field may be omitted, the
    /// default supplies it. Keyed by type name → optional field names. The construction gate consults
    /// this to suppress the missing-field error (E0009) for a defaulted field.
    attribute_optional_fields: HashMap<String, HashSet<String>>,
    /// The generic type parameters in scope while checking the current declaration, each mapped to
    /// its declared trait **bounds** (`<T: Comparable>` → `{"T": ["Comparable"]}`). Empty at top
    /// level; saved and restored around each generic declaration. The bounds drive body-side
    /// enforcement (S4.3c — an operation on `T` is only allowed if a bound licenses it).
    type_params: HashMap<String, Vec<String>>,
    /// The declared return type of the function whose body is currently being checked — the
    /// expectation each `return <value>` is checked against. `Unknown` at top level and inside a
    /// function with no return annotation (so the check is a no-op there). Saved and restored
    /// around each function so nested declarations do not clobber the enclosing one.
    current_ret: Type,
    /// When `Some`, the checker is inferring a block-bodied closure's return type: each
    /// `return <value>` records its value's type here (instead of only being checked against a
    /// declared return). The closure joins these into its inferred return. `None` everywhere else
    /// (a named function declares its return, so its `return`s are checked, not collected). Saved and
    /// restored around each closure so nesting is correct.
    collected_returns: Option<Vec<Type>>,
    /// When `Some(T)`, the checker is inside a **generator** body (a function containing `yield`)
    /// whose element type is `T`: each `yield e` is checked `e <: T` (Track G). `None` outside a
    /// generator, so a stray `yield` is `E0039`. Saved/restored around each function and reset to
    /// `None` when entering a closure (so `yield` cannot cross a closure boundary — the coloring rule).
    current_yield: Option<Type>,
    /// Whether the checker is inside an **async context** (Track A): the body of an `async fn`, or the
    /// implicitly-async module top level (a top-level body containing a `.await`). Each `expr.await`
    /// is only valid when this is `true`; otherwise it is `E0040` (the coloring rule). Saved/restored
    /// around each function and reset to `false` when entering a closure (so `.await` cannot cross a
    /// closure boundary — the same coloring rule as `yield`).
    current_async: bool,
    /// The number of enclosing `concurrent { }` scopes around the statement being checked (Track A.3b).
    /// A `spawn` is only valid when this is non-zero; otherwise it is an orphan task (E0041). Reset at a
    /// closure boundary (a closure is a fresh callable — a `concurrent` scope does not cross into it).
    concurrent_depth: u32,
    /// The number of enclosing `for`/`while` loops around the statement being checked. A `break`
    /// or `continue` is only valid when this is non-zero; otherwise it is `E0024`.
    loop_depth: usize,
    /// `Expr::Index` spans whose receiver typed as a built-in `List` — recorded as each index is
    /// synthesized so that [`Checker::synth_member`] can recognize a `list[i].field` read without
    /// re-synthesizing (and re-diagnosing) the inner receiver. Internal scratch, not exported (so it
    /// stays a plain `Checker` field, not part of [`SiteMaps`]).
    index_on_list: HashSet<Span>,
    /// The span-keyed **codegen site maps** the checker produces for the backends and lowering — its
    /// codegen-hint output, grouped apart from the checker's own type-environment/coloring state. See
    /// [`SiteMaps`].
    sites: SiteMaps,
    /// Class names that declare a `destruct { ... }` block — the seeds of destruct-reachability.
    destructor_classes: HashSet<String>,
    /// Type names whose value, when dropped, could run *some* `destruct` block — transitively,
    /// through the type's own block, its fields, or its collection elements (the fixpoint
    /// [`compute_destruct_reachable`] computes). The input to per-binding destructor-relevance.
    destruct_reachable: HashSet<String>,
    /// The destructor-relevance of each binding (memory-management migration, Phase 3.2b): the
    /// drop-insertion pass reads it to mark a `DropVar`'s `relevant` bit, which Phase 4 uses to skip
    /// the destructor check for a value whose type can run no destructor.
    relevance: DestructorRelevance,
    /// The **extension registry** this checker resolves native modules, functions, extern types,
    /// tiers, and attributes against (instance-registry F2). `None` — the default — routes every
    /// lookup through the process-global default registry (via [`Checker::reg`]), so an ordinary
    /// whole-program check is unchanged. An embedding host that assembled a *per-session* extension
    /// set threads its own [`Registry`] here, and this checker then sees exactly those extensions —
    /// the same set its paired VM runs against. `&'static` because a [`Registry`]'s lookups already
    /// return `&'static` (its units are static); the handle is `Copy`, so `Clone` (the transactional
    /// session snapshot) stays cheap.
    registry: Option<&'static noeta_stdlib::registry::Registry>,
    /// Which language [`Edition`] governs each source of the merged program, keyed by `SourceId`
    /// (editions compiler arc). The loader builds this from each package's own edition; the checker
    /// recovers a declaration's edition from its span via [`Checker::edition_at`]. Empty — the
    /// default — means every declaration is [`Edition::DEFAULT`] (a single-file check, or the
    /// one-edition world), so an ordinary check is unchanged. The first rule to branch on it is the
    /// editions arc's S3 (the first edition-gated behaviour); until then this is threaded and
    /// per-span-queryable but consulted by no rule.
    editions: EditionMap,
    diags: Vec<Diagnostic>,
}

impl Checker {
    /// The extension [`Registry`] this checker resolves native names against (instance-registry F2):
    /// the per-session registry when one was threaded in ([`Checker::registry`]), otherwise the
    /// process-global default. `&'static` because a registry's lookups already yield `&'static`
    /// data. Every stdlib/extern/tier lookup in the checker goes through here, so pointing a session
    /// at a different extension set is a single field — no lookup site knows which registry it holds.
    fn reg(&self) -> &'static noeta_stdlib::registry::Registry {
        self.registry
            .unwrap_or_else(noeta_stdlib::registry::default_seeded)
    }

    /// The language [`Edition`] governing the declaration a `span` belongs to — resolved from the
    /// per-source [`EditionMap`] the loader threaded in ([`Checker::editions`]) via the span's
    /// `SourceId`, defaulting to [`Edition::DEFAULT`] for any source without a recorded edition (a
    /// single-file check, a synthetic span, or the one-edition world).
    ///
    /// This is the per-declaration edition switch for a merged program: every rule that will diverge
    /// by edition reads it here rather than assuming the root's edition. No rule branches on it yet —
    /// the first is the editions arc's S3 — so it is `allow(dead_code)` until then; it is wired and
    /// unit-tested now so that slice adds only the divergent behaviour, not the plumbing.
    #[allow(dead_code)]
    fn edition_at(&self, span: Span) -> Edition {
        self.editions.at(span)
    }

    /// Record an error diagnostic, returning `&mut` to the just-pushed diagnostic so a help line can
    /// be chained onto it in place (`self.error(code, span, msg).help("…")`). The single place the
    /// checker constructs an error — every diagnostic site funnels through here rather than repeating
    /// `self.diags.push(Diagnostic::error(…))`.
    fn error(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.diags.push(Diagnostic::error(code, span, message));
        self.diags.last_mut().expect("just pushed a diagnostic")
    }

    /// Reject a declaration that binds a **reserved prelude name** (E0046, prelude-redesign P3).
    /// The always-global prelude is deliberately tiny — `Ok`/`Err`/`some`/`none`/`panic`/`assert` —
    /// and those names cannot be bound by ANY form (binding, `mut`, param, `fn`, type, `for`/match
    /// binder): the tree-walker pre-declares them as immutable globals while the VM would resolve a
    /// shadow as a fresh local, so allowing a binding meant the backends diverged. Rejecting it
    /// statically closes that divergence by construction. Methods and enum variants are exempt —
    /// they are always receiver-/type-qualified, so a bare prelude name never resolves to them.
    /// Reject a type declaration that binds a **reserved language-level type name** (E0049): the
    /// checker-native `Iterator`/`Future`/`Sender`/`Receiver` (produced by `iter()`/`async`/`.await`/
    /// `channel()`), whose values are backend builtins dispatched by name-match — a same-name user
    /// type would be silently shadowed. **Registered extern types (`Uuid`, `Response`, …) are no
    /// longer reserved**: they are namespace-scoped and `use`-imported like user types, so a user may
    /// freely declare `class Response` — the two carry distinct qualified identities and never
    /// conflate. A collision with a *specific* extern the file also imported is caught separately as
    /// E0020 ([`Self::check_extern_import_collision`]).
    fn check_reserved_type_name(&mut self, name: &str, span: Span) {
        if stdlib::NATIVE_TYPE_NAMES.contains(&name) {
            self.error(
                DiagnosticCode::ReservedTypeName,
                span,
                format!("cannot declare `{name}`: it is a reserved language type name"),
            )
            .help(
                "rename the type — the built-in `Iterator`/`Future`/`Sender`/`Receiver` cannot \
                 be shadowed",
            );
        }
        self.check_extern_import_collision(name, span);
    }

    /// The registered extern type a source annotation name resolves to **in this file's scope**, via
    /// the `use`-import map (`Uuid` → `std.id.Uuid`, an alias → its target) — or `None` if the name
    /// is not an imported native type. The scope-aware counterpart to a bare `registry::find_type`.
    fn imported_extern(&self, name: &str) -> Option<&'static noeta_stdlib::registry::ExtType> {
        self.extern_types
            .get(name)
            .and_then(|q| self.reg().find_type_qualified(q))
    }

    /// Reject declaring a type whose name a `use std.<ns>.<Type> [as Alias]` in this file already
    /// bound (E0020): the local name would be ambiguous between the imported native type and the
    /// local declaration. Mirrors the linker's user-import collision rule — the reason a user type
    /// and a same-named native type can safely coexist is that they can never both be in scope.
    fn check_extern_import_collision(&mut self, name: &str, span: Span) {
        if let Some(qualified) = self.extern_types.get(name) {
            self.error(
                DiagnosticCode::NameCollision,
                span,
                format!("`{name}` is already imported from `{qualified}`"),
            )
            .help("rename the local type, or import the native type under an alias (`as …`)");
        }
    }

    fn check_reserved_name(&mut self, name: &str, span: Span) {
        if RESERVED_PRELUDE.contains(&name) {
            self.error(
                DiagnosticCode::ReservedName,
                span,
                format!("cannot bind `{name}`: it is a reserved prelude name"),
            )
            .help(
                "rename the binding — the prelude's `Ok`/`Err`/`some`/`none`/`panic`/`assert` \
                 cannot be shadowed",
            );
        }
    }

    /// Consume the checker into the public [`Checked`] result — the whole-program finisher
    /// (`check_all` moves; the session flavor clones and keeps the checker alive instead).
    fn into_checked(self) -> Checked {
        let bundle_bindings = self.bundle_bindings_public();
        let packed_layouts = self.packed_layouts_public();
        let relevance = self.relevance;
        let mut sites = self.sites;
        let expr_types = std::mem::take(&mut sites.expr_types);
        Checked {
            diagnostics: self.diags,
            expr_types,
            sites: sites.into_sites(relevance),
            bundle_bindings,
            packed_layouts,
        }
    }

    /// Every `@packed` struct's flat layout, by type name — the IDE storage-fact index
    /// ([`Checked::packed_layouts`]). A malformed packed struct (a field E0038 already diagnosed)
    /// yields no layout and is simply absent.
    fn packed_layouts_public(&self) -> HashMap<String, noeta_ast::reflect::PackedLayout> {
        self.packed_structs
            .iter()
            .filter_map(|name| {
                let ty = Type::Named(name.clone(), Vec::new());
                Some((name.clone(), self.packed_layout(&ty)?))
            })
            .collect()
    }

    /// The bundle bindings as the public `(module, bundle)` form (kernel-methods K4) — what the
    /// IDE reads to offer bound methods in member completion.
    fn bundle_bindings_public(&self) -> HashMap<String, Vec<(String, String)>> {
        self.bundle_impls
            .iter()
            .map(|(ty, bindings)| {
                (
                    ty.clone(),
                    bindings
                        .iter()
                        .map(|b| (b.module.clone(), b.bundle.name.to_string()))
                        .collect(),
                )
            })
            .collect()
    }

    /// Pass 2: check every top-level statement with a fresh global scope.
    fn check_program(&mut self, program: &Program) {
        let mut env: Env = vec![HashMap::new()];
        self.check_program_in(program, &mut env);
    }

    /// [`Checker::check_program`] against a **caller-owned** environment — the seam the
    /// [`SessionChecker`] rides (session-checker C0): a REPL/console session passes its persistent
    /// global scope, so an entry sees the bindings earlier entries committed and the session keeps
    /// whatever this entry binds. The whole-program path passes a fresh one-frame env
    /// (behavior-identical).
    fn check_program_in(&mut self, program: &Program, env: &mut Env) {
        // Implicit async top level (Track A): if the module body contains a top-level `.await` (one
        // not inside a nested `fn`/closure), the top level is itself an async context, so its awaits
        // are legal (executable since A.1 — a top-level `.await` runs its future to completion).
        self.current_async = block_has_await(&program.stmts);
        for stmt in &program.stmts {
            self.check_stmt(stmt, env);
        }
        self.current_async = false;
        self.check_unrefined_muts(&program.stmts);
    }

    /// Flag a `mut` binding to a context-free polymorphic literal (`mut x = []`/`{}`/`none`/
    /// `Ok(_)`/`Err(_)`) that is *never reassigned* in its lexical scope: its type stays an
    /// undeterminable hole, so it is the `mut` analogue of the immutable `E0023` (which fires at
    /// the binding site). The `mut` exemption exists so an accumulator's later writes can supply
    /// the element type — when no such write exists, the exemption does not apply. Purely
    /// syntactic (reachability + nesting), so it runs as a standalone pass over the merged AST.
    fn check_unrefined_muts(&mut self, stmts: &[Stmt]) {
        for (i, stmt) in stmts.iter().enumerate() {
            if let Stmt::Binding {
                mut_decl: true,
                ty: None,
                name,
                value,
                ..
            } = stmt
                && is_uninferable_literal(value)
                && !reassigns(&stmts[i + 1..], name)
            {
                self.error(
                    DiagnosticCode::CannotInfer,
                    value.span(),
                    format!("cannot infer the type of `{name}`"),
                )
                .help(
                    "this `mut` binding is never assigned after its empty initializer, so its \
                         type stays undeterminable — annotate it (e.g. `mut x: List<int> = []`) \
                         or remove it",
                );
            }
            // Recurse into nested statement bodies for `mut` bindings declared there.
            for body in child_stmt_bodies(stmt) {
                self.check_unrefined_muts(body);
            }
        }
    }

    fn check_block(&mut self, stmts: &[Stmt], env: &mut Env) {
        env.push(HashMap::new());
        self.bind_nested_fns(stmts, env);
        for stmt in stmts {
            self.check_stmt(stmt, env);
        }
        env.pop();
    }

    /// Pre-register a block's **nested `fn` declarations** into the current scope frame (F1): a
    /// nested function is not in [`Self::functions`] (top-level only), so a sibling, forward, or
    /// recursive call to one must resolve here — otherwise the unknown-name gate would flag it.
    /// Bound as its (erased) `Fn` type so a bare reference is precise too; the call's own argument
    /// checking stays deferred (a nested-fn call routes through the prelude fallback), unchanged.
    fn bind_nested_fns(&self, stmts: &[Stmt], env: &mut Env) {
        for stmt in stmts {
            if let Stmt::Fn(decl) = stmt {
                let params = decl
                    .params
                    .iter()
                    .map(|p| param_type(p, &self.extern_types))
                    .collect();
                let ret = decl
                    .ret
                    .as_ref()
                    .map(|t| from_ref_q(t, &self.extern_types))
                    .unwrap_or(Type::Unknown);
                bind(
                    env,
                    &decl.name,
                    Type::Fn {
                        params,
                        ret: Box::new(ret),
                    },
                );
            }
        }
    }

    /// Check a closure body (arrow or block) and return the closure's return type. `expected` is the
    /// type the body must produce — the explicit annotation, or the context's expected return — or
    /// `None` to infer it. The caller has already pushed the parameter frame onto `env`.
    ///
    /// An arrow body is the expression's type (checked against `expected` when given). A block body
    /// runs as a fresh control-flow context (`break`/`continue` cannot target an enclosing loop, like
    /// a named function body); with an `expected` type its `return`s are checked against it, otherwise
    /// they are collected and joined into the inferred return (plus `void` if the block can fall
    /// through). This inference is purely local — no cross-function propagation — so it does not
    /// reintroduce the cost the required-boundary-signature rule avoids.
    fn closure_body_type(
        &mut self,
        body: &noeta_ast::ClosureBody,
        expected: Option<&Type>,
        env: &mut Env,
    ) -> Type {
        // A closure is a fresh callable: an enclosing generator's `yield` context does not cross into
        // it (a `yield` inside a closure is E0039 — the coloring rule), and neither does an enclosing
        // async context (a `.await` inside a closure is E0040 — the same coloring rule). Restored
        // after the body.
        let saved_yield = self.current_yield.take();
        let saved_async = std::mem::replace(&mut self.current_async, false);
        // A `concurrent` scope likewise does not cross into a closure — a `spawn` inside a closure
        // passed to a builtin is an orphan (E0041), the same coloring rule.
        let saved_concurrent = std::mem::replace(&mut self.concurrent_depth, 0);
        let result = self.closure_body_type_inner(body, expected, env);
        self.concurrent_depth = saved_concurrent;
        self.current_async = saved_async;
        self.current_yield = saved_yield;
        result
    }

    fn closure_body_type_inner(
        &mut self,
        body: &noeta_ast::ClosureBody,
        expected: Option<&Type>,
        env: &mut Env,
    ) -> Type {
        match body {
            noeta_ast::ClosureBody::Expr(e) => match expected {
                Some(exp) => self.check(e, exp, env),
                None => self.synth(e, env),
            },
            noeta_ast::ClosureBody::Block(stmts) => {
                let saved_loop = std::mem::replace(&mut self.loop_depth, 0);
                let ret = match expected {
                    Some(exp) => {
                        // Check each `return` against `exp`; the closure's return type is `exp`.
                        let saved_ret = std::mem::replace(&mut self.current_ret, exp.clone());
                        let saved_col = self.collected_returns.take();
                        self.check_block(stmts, env);
                        self.collected_returns = saved_col;
                        self.current_ret = saved_ret;
                        exp.clone()
                    }
                    None => {
                        // Infer: collect the `return` types and join them.
                        let saved_ret = std::mem::replace(&mut self.current_ret, Type::Unknown);
                        let saved_col = self.collected_returns.replace(Vec::new());
                        self.check_block(stmts, env);
                        let collected = std::mem::replace(&mut self.collected_returns, saved_col)
                            .unwrap_or_default();
                        self.current_ret = saved_ret;
                        join_closure_returns(stmts, collected)
                    }
                };
                self.loop_depth = saved_loop;
                ret
            }
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt, env: &mut Env) {
        match stmt {
            // `echo` accepts any value, so it enters checking mode with a genuinely open
            // (`Unknown`) expectation — subsumption is a no-op here. (Other statement positions,
            // such as `return`, do supply a real expectation; see `check_stmt`'s `Return` arm.)
            Stmt::Echo { value, .. } => {
                self.check(value, &Type::Unknown, env);
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                ty,
                value,
                ..
            } => {
                self.check_reserved_name(name, *name_span);
                // An annotated binding (`x: T = …`) is checked against `T` and bound at `T`; the
                // annotation is the boundary the value must satisfy and the way to fix an otherwise
                // un-inferable value. Un-annotated bindings stay inference-only (open expectation).
                match ty {
                    Some(ty) => {
                        self.check_type_ref(ty);
                        let expected = from_ref_q(ty, &self.extern_types);
                        self.check(value, &expected, env);
                        // Record destructor-relevance of this binding for the drop-insertion pass.
                        if self.type_relevant(&expected) {
                            self.relevance.locals.insert(*name_span);
                        }
                        // Annotated = a fresh declaration; carry its `mut`-ness for the field-set rule.
                        if *mut_decl {
                            bind_mut(env, name, expected);
                        } else {
                            bind(env, name, expected);
                        }
                    }
                    None => {
                        let vty = self.check(value, &Type::Unknown, env);
                        if self.type_relevant(&vty) {
                            self.relevance.locals.insert(*name_span);
                        }
                        // An *immutable* binding to a context-free polymorphic literal (`x = []`,
                        // `m = {}`, `x = none`) can never be reassigned (that would be `E0006`), so
                        // its element/payload type is fixed yet undeterminable — `E0023`, fixable
                        // with an annotation. A `mut` binding is exempt: it is an accumulator whose
                        // later writes supply the type (L3).
                        if !*mut_decl && is_uninferable_literal(value) {
                            self.error(
                                DiagnosticCode::CannotInfer,
                                value.span(),
                                format!("cannot infer the type of `{name}`"),
                            )
                            .help(
                                "annotate it (e.g. `x: List<int> = []`), or use a `mut` binding \
                                     whose later writes determine the type",
                            );
                        }
                        // `mut x = …` is a fresh declaration (innermost frame, even if it shadows).
                        if *mut_decl {
                            bind_mut(env, name, vty);
                        } else if matches!(value, Expr::FieldSet { .. } | Expr::Coalesce { .. }) {
                            // Two desugars of compound assignment carry an *intended* type change and
                            // so bypass the plain-variable reassignment checks below:
                            //  - `x.f = v` → `x = FieldSet{…}`: a receiver rebind whose mutability is
                            //    class-aware (a value `struct` rebinds and needs `mut x`, E0006; a
                            //    reference `class` mutates in place) and whose type is checked, both
                            //    inside `synth_field_set` — so the checks below would double-report on
                            //    a struct and false-positive on a class.
                            //  - `x ??= y` → `x = x ?? y`: the coalesce **unwraps** an optional, so it
                            //    deliberately narrows the binding (`Option<int>` → `int`). This is the
                            //    one place a bare reassignment legitimately changes a *resolved* type.
                            // Update the binding's type as before; each desugar's own checks ran.
                            assign(env, name, vty);
                        } else {
                            // A bare `x = …` reassigns an existing binding, or introduces a fresh
                            // immutable one. Reassignment is now enforced **statically** — the
                            // tree-walker deferred both of these to the runtime:
                            match lookup(env, name) {
                                Some(existing) => {
                                    if !lookup_mutable(env, name) {
                                        // (1) Mutability: an immutable binding cannot be reassigned.
                                        self.error(
                                            DiagnosticCode::ImmutableAssignment,
                                            *name_span,
                                            format!(
                                                "cannot assign to `{name}`, which is immutable"
                                            ),
                                        )
                                        .help(format!(
                                            "declare it `mut {name} = …` to allow reassignment"
                                        ));
                                    } else if existing.contains_unknown() {
                                        // (2) A still-unresolved inferred type (`mut acc = []`) — this
                                        // write completes / refines it (the accumulator pattern).
                                        assign(env, name, vty);
                                    } else if !self.assignable(&vty, &existing) {
                                        // (3) Type stability: a resolved `mut` binding keeps its type;
                                        // a value that is not assignable to it — a different type, or a
                                        // widening of a resolved type — is rejected. Use a declared
                                        // union or `dyn` for a genuinely multi-type binding.
                                        self.error(
                                            DiagnosticCode::TypeMismatch,
                                            value.span(),
                                            format!(
                                                "cannot assign `{vty}` to `{name}`, which has type `{existing}`"
                                            ),
                                        )
                                        .help(format!(
                                            "a reassignment must match the binding's type — declare \
                                             `mut {name}: {existing} | {vty}` for a union, or \
                                             `mut {name}: dyn` to opt out of a fixed type"
                                        ));
                                    }
                                    // else: assignable (subtype / same / union member) — the binding
                                    // keeps its established type, so its shown type stays stable.
                                }
                                // Not in scope — a fresh immutable binding in the innermost frame.
                                None => bind(env, name, vty),
                            }
                        }
                    }
                }
            }
            // `(a, b, …) = expr` — a tuple-destructuring binding (object-model slice 4b). The value
            // must be a tuple of matching arity; each target binds to its element type (a `dyn`/hole
            // value defers, binding every target `dyn`).
            Stmt::Destructure {
                targets,
                value,
                span,
                ..
            } => {
                let vty = self.check(value, &Type::Unknown, env);
                let elem_types: Vec<Type> = match &vty {
                    Type::Tuple(els) => {
                        if els.len() != targets.len() {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "cannot destructure a {}-tuple into {} names",
                                    els.len(),
                                    targets.len()
                                ),
                            );
                        }
                        targets
                            .iter()
                            .enumerate()
                            .map(|(i, _)| els.get(i).cloned().unwrap_or(Type::Unknown))
                            .collect()
                    }
                    _ if vty.defers_to_runtime() => vec![Type::Unknown; targets.len()],
                    _ => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            value.span(),
                            format!("cannot destructure `{vty}` — expected a tuple"),
                        );
                        vec![Type::Unknown; targets.len()]
                    }
                };
                for ((name, name_span), t) in targets.iter().zip(elem_types) {
                    self.check_reserved_name(name, *name_span);
                    if self.type_relevant(&t) {
                        self.relevance.locals.insert(*name_span);
                    }
                    bind(env, name, t);
                }
            }
            Stmt::Expr { expr, .. } => {
                self.check(expr, &Type::Unknown, env);
            }
            Stmt::Return { value, span } => {
                // In a generator, only bare `return;` is allowed (it ends iteration); a value has no
                // place under pure-pull `next() -> ?T` (no completion type) → E0039.
                if self.current_yield.is_some() {
                    if value.is_some() {
                        self.error(
                            DiagnosticCode::GeneratorMisuse,
                            *span,
                            "a generator's `return` cannot carry a value; use bare `return;` to end \
                             iteration (the elements come from `yield`)"
                                .to_string(),
                        );
                    }
                    return;
                }
                // Check the returned value against the enclosing function's declared return
                // (`current_ret` is `Unknown` when inferring a closure, so the check is a no-op
                // there), and — when inferring a block-bodied closure's return — record its type so
                // the closure can join all `return`s into its inferred return.
                let ty = match value {
                    Some(value) => {
                        let expected = self.current_ret.clone();
                        self.check(value, &expected, env)
                    }
                    None => Type::Unit,
                };
                if let Some(returns) = &mut self.collected_returns {
                    returns.push(ty);
                }
            }
            Stmt::Yield { value, span } => {
                // `yield e` is valid only inside a generator (a function containing `yield`), where it
                // is checked against the element type `T` of the declared `Iterator<T>` return.
                match self.current_yield.clone() {
                    Some(elem) => {
                        self.check(value, &elem, env);
                    }
                    None => {
                        self.synth(value, env); // still type the operand for nested checks
                        self.error(
                            DiagnosticCode::GeneratorMisuse,
                            *span,
                            "`yield` is only valid inside a generator (a function whose body \
                             contains `yield`, returning `Iterator<T>`)"
                                .to_string(),
                        );
                    }
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.synth(cond, env);
                // Flow-narrowing: `if ident is T { … }` sees `ident` as `T` in the then-body —
                // but only when the body never reassigns it (a write could invalidate the
                // narrowing). The else-body keeps the declared type (negative narrowing is not
                // done). Mirrors the per-arm narrowing in `synth_match`.
                if let Expr::TypeTest { expr, ty, .. } = cond
                    && let Expr::Ident { name, .. } = expr.as_ref()
                    && !reassigns(then_body, name)
                {
                    env.push(HashMap::new());
                    bind(env, name, from_ref_q(ty, &self.extern_types));
                    self.check_block(then_body, env);
                    env.pop();
                } else {
                    self.check_block(then_body, env);
                }
                if let Some(else_body) = else_body {
                    self.check_block(else_body, env);
                }
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                let iter_ty = self.synth(iterable, env);
                // A `for` over a statically-known `Iterator<T>` streams via `next()` (Track I.2); the
                // lowering reads this set to set `Stmt::For.stream`. Collections / `dyn` keep the
                // snapshot fast path.
                if matches!(&iter_ty, Type::Named(n, _) if n == stdlib::ITERATOR) {
                    self.sites.for_stream_sites.insert(*span);
                }
                env.push(HashMap::new());
                self.bind_for_pattern(pattern, &iter_ty, env);
                self.loop_depth += 1;
                for stmt in body {
                    self.check_stmt(stmt, env);
                }
                self.loop_depth -= 1;
                env.pop();
            }
            Stmt::While { cond, body, .. } => {
                // Like `if`, the condition's bool-ness is enforced at runtime (`RequireCondBool`,
                // identical on both backends); synth it for nested checks and check the body.
                self.synth(cond, env);
                self.loop_depth += 1;
                self.check_block(body, env);
                self.loop_depth -= 1;
            }
            Stmt::Concurrent { body, span } => {
                // `concurrent { }` is a structured-concurrency scope (Track A.3b). It is async-only —
                // joining spawned tasks needs suspend machinery — so it is illegal in a sync context
                // (the coloring rule, E0040), exactly like `.await`.
                if !self.current_async {
                    self.error(
                        DiagnosticCode::AsyncMisuse,
                        *span,
                        "`concurrent { }` is only allowed inside an `async fn` (or the async top \
                             level)"
                            .to_string(),
                    )
                    .help(
                        "mark the enclosing function `async fn`; structured concurrency needs an \
                             async context to join its tasks",
                    );
                }
                // Inside the scope, `spawn` is legal; check the body with the depth raised.
                // `concurrent { }` is a **transparent** scope at runtime — a binding made inside it
                // (`w = race([a, b])`) leaks to the enclosing function, exactly like an `if` body's
                // bindings do not but a concurrent block's do. So check the body *in the current
                // frame* rather than pushing one (F1: the unknown-name gate would otherwise flag a
                // later reference to such a binding, which the tolerated-unknown behavior masked).
                self.concurrent_depth += 1;
                self.bind_nested_fns(body, env);
                for stmt in body {
                    self.check_stmt(stmt, env);
                }
                self.concurrent_depth -= 1;
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                // A loop-control statement is only meaningful inside a `for`/`while` body.
                if self.loop_depth == 0 {
                    let kw = if matches!(stmt, Stmt::Break { .. }) {
                        "break"
                    } else {
                        "continue"
                    };
                    self.error(
                        DiagnosticCode::LoopControlOutsideLoop,
                        *span,
                        format!("`{kw}` outside of a loop"),
                    );
                }
            }
            Stmt::Fn(decl) => {
                self.check_reserved_name(&decl.name, decl.name_span);
                self.check_fn(decl, env, &[], TargetKind::Function)
            }
            Stmt::Struct(r) => {
                self.check_reserved_name(&r.name, r.name_span);
                self.check_reserved_type_name(&r.name, r.name_span);
                self.check_struct(r, env)
            }
            Stmt::Class(c) => {
                self.check_reserved_name(&c.name, c.name_span);
                self.check_reserved_type_name(&c.name, c.name_span);
                self.check_class(c, env)
            }
            Stmt::Enum(e) => {
                self.check_reserved_name(&e.name, e.name_span);
                self.check_reserved_type_name(&e.name, e.name_span);
                self.check_enum(e, env)
            }
            Stmt::Impl(decl) => self.check_standalone_impl(decl),
            Stmt::Namespace { .. } | Stmt::Use { .. } => {}
            // A dev-tier block reaching the checker is an *inactive* residual (object-model
            // slice 6): the strip pass already spliced any *active* block's items into the
            // statement stream (where they are checked as ordinary declarations) and dropped the
            // inactive ones. So we validate only the tier name — a typo must not silently vanish
            // (E0036) — and do not type-check the (stripped) items.
            Stmt::TierBlock {
                tier,
                tier_span,
                args,
                ..
            } => {
                if !self.tier_registry.is_known(tier) {
                    self.diags
                        .push(tiers::unknown_tier_diagnostic(self.reg(), tier, *tier_span));
                } else if self.tier_registry.is_expr_tier(tier) {
                    // An expression tier's block in *statement* position (expr-tiers arc): its
                    // value would be silently discarded — and it never activates/strips, so a
                    // bare block would otherwise just vanish. Shared E0052 with activation.
                    self.diags
                        .push(tiers::expr_tier_statement_diagnostic(tier, *tier_span));
                } else if let Some(d) = self.tier_registry.knobless_args_diagnostic(tier, args) {
                    // Args on a knob-less tier (`@test(x)`) — E0037.
                    self.diags.push(d);
                } else if !args.is_empty()
                    && let Some(attr_name) = self
                        .tier_registry
                        .config_attribute(tier)
                        .map(str::to_string)
                {
                    // Args on a knob-carrying tier construct its config attribute
                    // (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`) — validate that
                    // construction through the ordinary attribute gate, so this default path
                    // rejects exactly what the activated path's stamped attributes would.
                    let synth = tiers::synthesized_config_attr(&attr_name, args, *tier_span);
                    self.check_attrs(std::slice::from_ref(&synth), TargetKind::Function);
                }
            }
        }
    }

    /// Check a function (or method) body. `extra` seeds the body scope with additional bindings
    /// (a class's fields, when checking a method).
    fn check_fn(
        &mut self,
        decl: &FnDecl,
        env: &mut Env,
        extra: &[(String, Type)],
        target: TargetKind,
    ) {
        self.require_signature(decl);
        // A function/method's `#[...]` attributes are validated like a type's: each names an
        // `Attribute` capability (E0029) and constructs it from its literal args (E0009/E0007/E0005).
        // `target` distinguishes a top-level `Function` from a `Method` for placement checks (P2.5).
        self.check_attrs(&decl.attrs, target);
        // Bring the function's own generic parameters into scope for its body (a free function may
        // be generic; a method is generic over its class's parameters, already in scope, and
        // carries none of its own). Union with the current set so a method does not lose the
        // class's parameters; restored after the body. Bounds are validated here too.
        self.check_type_param_bounds(&decl.type_params);
        let saved_type_params = self.type_params.clone();
        self.type_params.extend(
            decl.type_params
                .iter()
                .map(|p| (p.name.clone(), p.bounds.clone())),
        );
        for p in &decl.params {
            self.check_type_opt(&p.ty);
        }
        self.check_type_opt(&decl.ret);
        // Validate parameter defaults: trailing-only (`E0026`) and each default's type against its
        // parameter (`E0007`). Checked here, before the parameter frame is pushed, so a default is
        // evaluated against the definition scope — for a named function/method that is globals only
        // (mirroring how both backends evaluate it). `self.type_params` already includes this
        // function's own.
        self.validate_param_defaults(&decl.params, env);
        // The body's `return`s are checked against the declared return type; `Unknown` when
        // unannotated (already an `E0022`), so the check stays a no-op there. Saved/restored so a
        // nested function does not clobber the enclosing one's expectation.
        let ret = decl
            .ret
            .as_ref()
            .map(|t| from_ref_q(t, &self.extern_types))
            .unwrap_or(Type::Unknown);
        // A function whose body contains `yield` is a generator (Track G): its declared return must
        // be `Iterator<T>`, and its body's `yield e` are checked against the element type `T`. The
        // yield context is reset for a non-generator (so an enclosing generator's context does not
        // leak into a nested ordinary function) and saved/restored around the body.
        let is_generator = body_has_yield(&decl.body);
        let yield_elem = if is_generator {
            match &ret {
                Type::Named(n, args) if n == stdlib::ITERATOR => {
                    Some(args.first().cloned().unwrap_or(Type::Unknown))
                }
                _ => {
                    self.error(
                        DiagnosticCode::GeneratorMisuse,
                        decl.name_span,
                        format!(
                            "a generator (a function that uses `yield`) must declare its return \
                             type as `Iterator<T>`, found `{ret}`"
                        ),
                    );
                    Some(Type::Unknown)
                }
            }
        } else {
            None
        };
        // E0048 inputs, captured before `ret` is moved into `current_ret` below. A function must
        // produce its declared return on every path; only a type that *admits* `unit` — `void`
        // itself, `dyn`, or a union containing `void` — may fall off the end (falling through returns
        // `unit`). A generator produces its `Iterator<T>` through `yield`s and exhaustion, not a value
        // return, so it is exempt; an unannotated return is already `E0022`, so `Unknown` is skipped.
        let must_return_value =
            !is_generator && !matches!(ret, Type::Unknown) && !Type::subtype(&Type::Unit, &ret);
        let declared_ret = ret.clone();
        let saved_yield = std::mem::replace(&mut self.current_yield, yield_elem);
        // An `async fn` body is an async context: its `.await`s are legal (Track A). `current_ret`
        // stays the *inner* declared type `T` (the body writes `return t`); a call site sees the
        // wrapped `Future<T>` via the signature. Reset for a non-async function so an enclosing async
        // context does not leak into a nested ordinary function.
        let saved_async = std::mem::replace(&mut self.current_async, decl.is_async);
        let saved_ret = std::mem::replace(&mut self.current_ret, ret);
        // A function body is a fresh control-flow context: `break`/`continue` inside it cannot
        // target a loop the *enclosing* code is in, so reset the depth (restored after).
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        // White-box field privacy inside a dev-tier fn (slice 6d). Sticky: a nested fn declared in a
        // dev-tier body stays white-box too (co-located tooling). Restored after the body.
        let saved_dev_tier = self.in_dev_tier;
        self.in_dev_tier = decl.is_dev_tier || saved_dev_tier;
        env.push(HashMap::new());
        for (name, ty) in extra {
            bind(env, name, ty.clone());
        }
        for p in &decl.params {
            self.check_reserved_name(&p.name, p.name_span);
            bind(env, &p.name, param_type(p, &self.extern_types));
        }
        self.bind_nested_fns(&decl.body, env);
        for stmt in &decl.body {
            self.check_stmt(stmt, env);
        }
        // E0048: a non-`void` function must return a value on every path. If control can reach the end
        // of the body — it falls off the end, or an `if` without an `else` leaves a path open — the
        // function would implicitly return `unit` where its signature promised another type, and a
        // caller would silently bind that type to `unit`. (`return`s inside are already checked
        // against the declared type above; this is the complementary "did every path return" check.)
        if must_return_value && !block_diverges(&decl.body) {
            self.error(
                DiagnosticCode::MissingReturn,
                decl.name_span,
                format!(
                    "function `{}` can reach the end of its body without returning `{declared_ret}`",
                    decl.name
                ),
            )
            .help(
                "every path must `return` a value; only a `void` function may fall off the end",
            );
        }
        // An `async fn` body compiles to the async state machine (Track A.3a), which supports `.await`
        // only in statement position. Reject an `.await` buried in a sub-expression (E0040) rather than
        // silently driving it to completion — which would fail to yield to a sibling under concurrency.
        if decl.is_async {
            self.check_await_positions(&decl.body);
        }
        // A generator desugars into a full state machine (Track G): `yield` runs at the top level and
        // inside any nesting of `if`/`while`/`for` — a `for x in src { … yield … }` lowers to the
        // iterator protocol with the source cursor held as machine state (G.4), so no control-flow
        // context around a `yield` is rejected here.
        env.pop();
        self.in_dev_tier = saved_dev_tier;
        self.current_ret = saved_ret;
        self.current_async = saved_async;
        self.current_yield = saved_yield;
        self.loop_depth = saved_loop_depth;
        self.type_params = saved_type_params;
    }

    // ----- bidirectional judgments -----

    /// *Checking* mode: check `expr` against the `expected` type, returning the expression's
    /// actual type. Forms that can absorb an expectation propagate it inward (a list against
    /// `List<T>` checks each element against `T`; a closure against a function type adopts the
    /// expected parameter/return types); every other form synthesizes and is then subsumed.
    ///
    /// Callers pass real expectations here — a declared return at `return`, a parameter type at a
    /// call argument, a declared element type into a list/map literal — so the propagation arms
    /// below adopt the concrete type and [`Self::subsume`] enforces `actual <: expected`. Only a
    /// genuinely open position (e.g. `echo`) passes `Unknown`, where `check` reduces to bare
    /// [`Self::synth`].
    /// Check `expr` against `expected` (bidirectional position). Thin wrapper over
    /// [`Self::check_inner`] that, on the IDE path, records the result into the `expr_types`
    /// index — check-position expressions (an absorbed closure, an annotation-driven literal)
    /// previously never recorded, so hover and inlay hints missed them.
    fn check(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        let ty = self.check_inner(expr, expected, env);
        if self.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
    }

    fn check_inner(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        match expr {
            // A list literal absorbs an expected `List<T>`: check each element against `T`.
            Expr::List { items, span } if matches!(expected, Type::List(_)) => {
                let Type::List(elem) = expected else {
                    unreachable!()
                };
                for item in items {
                    self.check(item, elem, env);
                }
                self.note_packed_list(elem, *span);
                // Annotation-driven: record the *expected* element type (so `List<dyn> = [1,2,3]`
                // tags `List(Dyn)`, not the inferred `List(int)`).
                let ty = Type::List(elem.clone());
                self.note_construction(&ty, *span);
                ty
            }
            // An empty map literal absorbs an expected `Map<K, V>` (the map analogue of the list
            // arm); a non-empty map synthesizes its own element types and is then subsumed.
            Expr::Map { entries, span }
                if entries.is_empty() && matches!(expected, Type::Map(..)) =>
            {
                // Annotation-driven: record the *expected* map type (R1) so `Map<string, dyn> = {}`
                // tags `Map(String, Dyn)`, the map analogue of the list arm above.
                self.note_construction(expected, *span);
                expected.clone()
            }
            // A non-empty map literal absorbs an expected `Map<K, V>`: check each key against `K`
            // and each value against `V`, so heterogeneous values that are each a member of `V` (a
            // union, or `dyn`) are accepted instead of being cross-unified into a single element
            // type (`{"route": "/x", "status": 200}` against `Map<string, string|int|float|bool>`).
            // The map analogue of the list arm; the empty case is the preceding arm.
            Expr::Map { entries, span } if matches!(expected, Type::Map(..)) => {
                let Type::Map(kty, vty) = expected else {
                    unreachable!()
                };
                for (k, v) in entries {
                    self.check(k, kty, env);
                    self.check(v, vty, env);
                }
                let ty = Type::Map(kty.clone(), vty.clone());
                self.note_construction(&ty, *span);
                ty
            }
            // `none` absorbs an expected `Option<T>` (`?T`): it carries no payload, so it simply
            // adopts the expectation instead of leaking an inference hole.
            Expr::Ident { name, .. } if name == "none" && matches!(expected, Type::Option(_)) => {
                expected.clone()
            }
            // The polymorphic constructors absorb their expected algebraic type and check their
            // payload against the corresponding slot — so `some("x")` against `Option<int>` or
            // `Ok("x")` against `Result<int, _>` is now caught instead of deferring to a hole.
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "some")
                    && args.len() == 1
                    && matches!(expected, Type::Option(_)) =>
            {
                let Type::Option(inner) = expected else {
                    unreachable!()
                };
                self.check(&args[0], inner, env);
                expected.clone()
            }
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Ok")
                    && args.len() <= 1
                    && matches!(expected, Type::Result(..)) =>
            {
                let Type::Result(ok, _) = expected else {
                    unreachable!()
                };
                match args.first() {
                    Some(arg) => {
                        self.check(arg, ok, env);
                    }
                    // `Ok()` carries a unit payload (`Result<void, E>`).
                    None => self.subsume(&Type::Unit, ok, expr.span()),
                }
                expected.clone()
            }
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Err")
                    && args.len() == 1
                    && matches!(expected, Type::Result(..)) =>
            {
                let Type::Result(_, err) = expected else {
                    unreachable!()
                };
                self.check(&args[0], err, env);
                expected.clone()
            }
            // A closure absorbs an expected function type: an explicit parameter annotation wins,
            // otherwise the parameter adopts the expected type; the body is checked against the
            // expected return.
            Expr::Closure {
                params,
                ret: ann,
                body,
                span: closure_span,
            } if matches!(expected, Type::Fn { .. }) => {
                let Type::Fn {
                    params: expected_params,
                    ret,
                } = expected
                else {
                    unreachable!()
                };
                // A closure default is evaluated in the captured (enclosing) scope, so validate it
                // against `env` before the parameter frame is pushed.
                self.validate_param_defaults(params, env);
                env.push(HashMap::new());
                // Each parameter's bound type: an explicit annotation wins, else the expectation.
                // KEPT for the closure's own type below — returning `param_type` here used to
                // forget the absorption, leaving the recorded closure `(dyn) -> R` even when the
                // parameters were known (the dyn-closure gap's second half).
                let bound: Vec<Type> = params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        p.ty.as_ref()
                            .map(|t| from_ref_q(t, &self.extern_types))
                            .or_else(|| expected_params.get(i).cloned())
                            .unwrap_or(Type::Unknown)
                    })
                    .collect();
                for (p, pty) in params.iter().zip(&bound) {
                    self.check_reserved_name(&p.name, p.name_span);
                    bind(env, &p.name, pty.clone());
                }
                // An explicit return annotation is the body's expected type and the closure's return
                // type; it must also satisfy the context's expected return. Without one the expected
                // return drives the body — UNLESS that expectation is `dyn`, the builtin "any
                // result" shape (`map` expects `(T) -> dyn`): checking against `dyn` would erase the
                // body's real type and starve the call-site refinements (`xs.map(f) → List<R>`), so
                // the body is inferred instead; `dyn` accepts whatever comes out.
                let declared = ann.as_ref().map(|t| from_ref_q(t, &self.extern_types));
                let body_expected = declared
                    .clone()
                    .or_else(|| (!matches!(**ret, Type::Dyn)).then(|| (**ret).clone()));
                let body_ty = self.closure_body_type(body, body_expected.as_ref(), env);
                env.pop();
                if let Some(declared) = &declared {
                    self.subsume(declared, ret, *closure_span);
                }
                Type::Fn {
                    params: bound,
                    ret: Box::new(declared.unwrap_or(body_ty)),
                }
            }
            // A bare numeric literal adapts into a fixed-width context — `x: u8 = 200`, `y: i8 = -5`,
            // `z: f32 = 1.5`, `w: f64 = 1.5` (P-NUM-SYM). Shared with call-argument checking via
            // `try_adapt_literal`; a non-adapting pair falls through to synthesize-and-check.
            _ => {
                if let Some(adapted) = self.try_adapt_literal(expr, expected) {
                    return adapted;
                }
                let actual = self.synth(expr, env);
                self.subsume(&actual, expected, expr.span());
                actual
            }
        }
    }

    /// If `expr` is a bare numeric literal that adapts into the fixed-width `expected` type — an
    /// integer literal (optionally negated) into an in-range [`Type::IntN`], or a float literal into
    /// [`Type::F32`]/[`Type::F64`] — perform the adaptation and return the adapted type. Range-checks
    /// an `IntN` (E0044 out of range) and records the `f32` narrowing site so lowering emits a
    /// `Const::F32`. Returns `None` for any non-adapting pair. Shared by binding checks (`mut x: T =
    /// …`) and call-argument checks (`f(…)`) so a bare `5`/`1.5` flows into an `i64`/`f32`/`f64`
    /// identically in both positions. (A *suffixed* literal like `200u8`/`1.5f32` is its own
    /// `Expr::IntN`/`Expr::F32`, already the fixed-width type — it never reaches here.)
    fn try_adapt_literal(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        match expected {
            Type::IntN { signed, bits } => {
                let is_int_literal = matches!(expr, Expr::Int { .. })
                    || matches!(
                        expr,
                        Expr::Unary {
                            op: UnaryOp::Neg,
                            ..
                        }
                    );
                if !is_int_literal {
                    return None;
                }
                let value = int_literal_value(expr)?;
                let (lo, hi) = Self::int_width_range(*signed, *bits);
                if value < lo || value > hi {
                    self.error(
                        DiagnosticCode::FixedWidthOutOfRange,
                        expr.span(),
                        format!(
                            "literal `{value}` is out of range for `{expected}` (valid range {lo}..={hi})"
                        ),
                    );
                }
                Some(expected.clone())
            }
            // `f64` is bit-identical to `float`, so no narrowing is needed — only the static type.
            Type::F64 if matches!(expr, Expr::Float { .. }) => Some(Type::F64),
            // `f32` is a distinct 32-bit representation; record the site so lowering narrows it.
            Type::F32 if matches!(expr, Expr::Float { .. }) => {
                if let Expr::Float { span, .. } = expr {
                    self.sites.f32_literal_sites.insert(*span);
                }
                Some(Type::F32)
            }
            _ => None,
        }
    }

    /// Subsumption: require `actual <: expected`. A violation is a type mismatch (`E0007`, the
    /// same code the arithmetic/runtime mismatch path uses). An inference hole on either side
    /// makes [`Type::subtype`] hold, so a not-yet-inferred interior type never produces a false
    /// positive — the deliberate residual tolerance (holes are removed at typed boundaries, not
    /// here).
    /// Whether `name` is a declared (or prelude) type of `kind` — the registry-dependent half of the
    /// abstract kind-type membership rule the pure lattice cannot decide.
    fn is_of_kind(&self, name: &str, kind: noeta_types::TypeKind) -> bool {
        self.type_kinds.get(name) == Some(&kind)
    }

    /// Kind-aware assignability: `actual <: expected`, extending [`Type::subtype`] with the one rule
    /// it cannot decide on its own — a concrete `Named(n)` widens into an abstract `Kind(k)` when
    /// `n` is a declared type of kind `k`. Recurses through the covariant containers and unions so
    /// the rule composes (`List<WebRole> <: List<Enum>`); every non-kind case delegates to the pure
    /// lattice. This is the single funnel for assignment, argument, return, and field checks.
    fn assignable(&self, actual: &Type, expected: &Type) -> bool {
        // The pure subtype lattice, plus the one registry-dependent rule it defers: whether a
        // `Named(n)` is a member of an abstract `Kind(k)`. Threading it through [`Type::subtype_with`]
        // reaches every nested covariant position without re-implementing the variance walk here.
        Type::subtype_with(actual, expected, &|n, k| self.is_of_kind(n, k))
    }

    /// Whether an argument of type `arg` may be passed where `param` is expected — the kind-aware
    /// counterpart of the free [`arg_compatible`]. A `dyn`/hole on either side defers to the runtime;
    /// otherwise the argument must be assignable to the parameter under the strict subtype lattice.
    /// There is **no** numeric-widening leniency: an `int` is not accepted where a `float` is expected
    /// (write `f(2.0)`, not `f(2)`), matching every other typed boundary — a binding, a return, a list
    /// element — where `int → float` is already rejected, and so an inlay-hinted parameter type is a
    /// promise the caller must meet.
    fn arg_assignable(&self, arg: &Type, param: &Type) -> bool {
        self.assignable(arg, param) || arg.defers_to_runtime() || param.defers_to_runtime()
    }

    fn subsume(&mut self, actual: &Type, expected: &Type, span: Span) {
        if !self.assignable(actual, expected) {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("expected `{expected}`, found `{actual}`"),
            );
        }
    }

    // ----- synthesis -----

    /// Synthesize an expression's type. Thin wrapper over [`Self::synth_inner`] that, on the IDE
    /// path ([`Self::record_expr_types`]), records the result into the `expr_types` index for hover.
    /// Every expression — and every subexpression, since the checker recurses through here — flows
    /// through this one choke point, so the index covers the whole tree with a single insertion site.
    fn synth(&mut self, expr: &Expr, env: &mut Env) -> Type {
        let ty = self.synth_inner(expr, env);
        if self.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
    }

    fn synth_inner(&mut self, expr: &Expr, env: &mut Env) -> Type {
        match expr {
            // A resolved native-fn reference as a *value* — a loose `Fn` type, like a
            // selectively-imported module function referenced bare (the precise per-call signature
            // is applied in the `Call` callee arm). The desugar only ever uses it as a callee.
            Expr::NativeFnRef { .. } => Type::Fn {
                params: Vec::new(),
                ret: Box::new(Type::Dyn),
            },
            Expr::Str { .. } => Type::String,
            Expr::Int { .. } => Type::Int,
            Expr::Float { .. } => Type::Float,
            Expr::F32 { .. } => Type::F32,
            Expr::F64 { .. } => Type::F64,
            Expr::IntN {
                magnitude,
                signed,
                bits,
                span,
            } => self.check_intn_literal(*magnitude, *signed, *bits, false, *span),
            Expr::Bool { .. } => Type::Bool,
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let StrPart::Hole(e) = part {
                        self.synth(e, env);
                    }
                }
                Type::String
            }
            // An expression-tier block types as the handler call it desugars to (`Try`/`Await`
            // architecture: the node is kept, the checker types it, IR lowering rewrites it
            // through the same [`noeta_ast::desugar`] constructor). Checking the constructed
            // call is the whole typing rule: each hole closure checks against the handler's
            // `List<() -> U>` — so a hole-type error lands on the hole's real span — and the
            // block's type is the handler's declared return. A block whose tier is not
            // `expr:`-declared (`x = @doc { … }`) is E0052; its holes still synth for IDE
            // coverage inside the body.
            Expr::TierExpr {
                tier,
                tier_span,
                statics,
                holes,
                span,
            } => {
                let handler = self.tier_registry.expr_tier_handler(tier);
                match handler {
                    Some(handler) => {
                        let call = noeta_ast::desugar::tier_expr_call(
                            &handler, *tier_span, statics, holes, *span,
                        );
                        self.synth(&call, env)
                    }
                    None => {
                        for hole in holes {
                            self.synth(hole, env);
                        }
                        self.error(
                            DiagnosticCode::InvalidTierExpression,
                            *tier_span,
                            format!(
                                "`@{tier}` is not an expression tier — its blocks are not values"
                            ),
                        )
                        .help(
                            "only a tier declared `@tier(name, …, expr: Type)` yields a value \
                             from `@name { … }`; a text tier's blocks are runner input, not \
                             expressions",
                        );
                        Type::Unknown
                    }
                }
            }
            Expr::Ident { name, span } => match lookup(env, name)
                // A bare user-function reference is a first-class value of its **full** signature
                // type — parameters included, so passing it where a `Fn(A) -> B` is declared
                // (`map_bounded(items, n, dbl)`, `xs.map(inc)`) checks like the equivalent
                // closure. A generic function's erased params are `dyn`, which defers per
                // position. (Was params-erased until higher-order-abi H2 made module signatures
                // carry declared `Fn` params, which an erased handle could never satisfy.)
                .or_else(|| {
                    self.functions.get(name).map(|sig| Type::Fn {
                        params: sig.params.clone(),
                        ret: Box::new(sig.ret.clone()),
                    })
                })
                // A selectively-imported module function referenced as a value (`let f = sqrt`).
                .or_else(|| {
                    self.imported_fns.contains_key(name).then(|| Type::Fn {
                        params: Vec::new(),
                        ret: Box::new(Type::Dyn),
                    })
                }) {
                Some(t) => t,
                None => {
                    // A bare name inside a type's own body that names one of its FIELDS is a
                    // targeted static error (prelude-redesign EX.1): member access is explicit, so
                    // the field is only reachable as `self.name`. Any other unknown ident stays
                    // tolerated here (deferred to the runtime E0005, as before).
                    if let Some(ct) = self.current_type.clone()
                        && self
                            .records
                            .get(&ct)
                            .is_some_and(|fs| fs.iter().any(|(f, _)| f == name))
                    {
                        self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("cannot find `{name}` in this scope"),
                        )
                        .help(format!(
                            "member access is explicit — the field is `self.{name}`"
                        ));
                    } else if !self.session_mode && !self.is_known_name(name, env) {
                        // A bare reference to a name that resolves to nothing — a genuinely
                        // undefined value (F1), the same static `E0005` as an unknown callee. A
                        // session defers (a later entry may define it).
                        self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("cannot find `{name}` in this scope"),
                        );
                    }
                    Type::Unknown
                }
            },
            Expr::Unary { op, operand, span } => {
                // A negated fixed-width literal (`-128i8`, `-1i32`): check against the *signed*
                // negative range here, so the inner literal's positive-range check does not fire a
                // false positive on the boundary value `128i8` that only `-128i8` may reach.
                if let (
                    UnaryOp::Neg,
                    Expr::IntN {
                        magnitude,
                        signed,
                        bits,
                        span: lit_span,
                    },
                ) = (op, operand.as_ref())
                {
                    return self.check_intn_literal(*magnitude, *signed, *bits, true, *lit_span);
                }
                let t = self.synth(operand, env);
                // A list spread `...xs` (the marker the L2 desugar wraps spread operands in) must
                // spread a list — otherwise the desugared `~` would silently fall through to
                // display-concatenation. It always types list-shaped so the surrounding literal
                // stays a list: a list passes through; a `dyn`/hole spread contributes `dyn`
                // elements; a concrete non-list is an error (and still resolves to `List<dyn>`,
                // suppressing a second diagnostic from the desugared concat).
                if matches!(op, UnaryOp::Spread) {
                    return match &t {
                        Type::List(_) => t,
                        _ if t.defers_to_runtime() => Type::List(Box::new(Type::Dyn)),
                        _ => {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot spread `{t}` — `...` expects a list"),
                            );
                            Type::List(Box::new(Type::Dyn))
                        }
                    };
                }
                // Unary `-` on a fixed-width integer (Tier W): the result is the same width, masked so
                // `-i8::MIN` wraps back to `i8::MIN`; negating an *unsigned* width has no meaning →
                // E0044. (A negated fixed-width *literal* is handled by the intercept above.)
                if let (UnaryOp::Neg, Type::IntN { signed, bits }) = (op, &t) {
                    if *signed {
                        self.sites.width_sites.insert(*span, (*signed, *bits));
                    } else {
                        self.error(
                            DiagnosticCode::FixedWidthOutOfRange,
                            *span,
                            format!("cannot negate `u{bits}`: unary `-` requires a signed type"),
                        );
                    }
                    return t;
                }
                // Other unary type errors have no corpus case and the operand is often gradual;
                // infer for nested checks but do not promote (kept conservative).
                t
            }
            Expr::Binary { op, lhs, rhs, span } => self.synth_binary(*op, lhs, rhs, *span, env),
            Expr::Call {
                callee, args, span, ..
            } => {
                // Bidirectional literal arguments: a closure's parameter types — and a container
                // literal's expected element/value type — come from the CALLEE's resolved signature,
                // so both are deferred (placeholder `Unknown`) and typed by `synth_call` once the
                // signature is known (a `{"route": "/x", "status": 200}` map literal then absorbs a
                // `Map<string, string|int|float|bool>` parameter, checking each value against the
                // union instead of cross-unifying them). Everything else synthesizes as before.
                let arg_types: Vec<Type> = args
                    .iter()
                    .map(|a| {
                        if is_deferred_literal_arg(a) {
                            Type::Unknown
                        } else {
                            self.synth(a, env)
                        }
                    })
                    .collect();
                self.synth_call(callee, &arg_types, args, *span, env)
            }
            Expr::Closure {
                params,
                ret: ann,
                body,
                ..
            } => {
                self.validate_param_defaults(params, env);
                env.push(HashMap::new());
                for p in params {
                    self.check_reserved_name(&p.name, p.name_span);
                    bind(env, &p.name, param_type(p, &self.extern_types));
                }
                // With an explicit return annotation, check the body against it (and adopt it as the
                // closure's return type); otherwise infer it from the body (the arrow expression's
                // type, or a block's joined `return`s).
                let declared = ann.as_ref().map(|t| from_ref_q(t, &self.extern_types));
                let ret = self.closure_body_type(body, declared.as_ref(), env);
                env.pop();
                Type::Fn {
                    params: params
                        .iter()
                        .map(|p| param_type(p, &self.extern_types))
                        .collect(),
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
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "list elements have differing types",
                    )
                    .help("make the elements one type, or annotate a `List<dyn>` for a mixed list");
                    elem = Type::Dyn; // recover as a mixed list
                }
                self.note_packed_list(&elem, *span);
                let ty = Type::List(Box::new(elem));
                self.note_construction(&ty, *span);
                ty
            }
            // A tuple literal `(a, b, …)` synthesizes a `Type::Tuple` of its elements' types,
            // positionally — heterogeneity is the point (no unification, unlike a list).
            Expr::Tuple { items, .. } => {
                Type::Tuple(items.iter().map(|item| self.synth(item, env)).collect())
            }
            // Tuple projection `receiver.N`: the Nth element type of a tuple receiver. An out-of-range
            // index is `E0007`; a `.N` on a non-tuple concrete type is rejected; a `dyn`/hole defers.
            Expr::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let recv = self.synth(receiver, env);
                match &recv {
                    Type::Tuple(elements) => match elements.get(*index as usize) {
                        Some(t) => t.clone(),
                        None => {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "tuple index `{index}` is out of range for `{recv}` ({} element(s))",
                                    elements.len()
                                ),
                            );
                            Type::Unknown
                        }
                    },
                    _ if recv.defers_to_runtime() => Type::Unknown,
                    _ => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("cannot apply tuple index `.{index}` to non-tuple `{recv}`"),
                        );
                        Type::Unknown
                    }
                }
            }
            Expr::Range {
                start, end, span, ..
            } => {
                // A range builds a `List<int>`; both bounds must be `int` (a `dyn`/hole defers).
                let st = self.synth(start, env);
                let en = self.synth(end, env);
                let bad = |t: &Type| !matches!(t, Type::Int) && !t.defers_to_runtime();
                if bad(&st) || bad(&en) {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("range bounds must be `int`, found `{st}` and `{en}`"),
                    );
                }
                Type::List(Box::new(Type::Int))
            }
            Expr::Map { entries, span } => {
                // Synthesize key/value types by unifying the entries (mirroring the list path).
                // Runtime map keys are always strings, so keys unify trivially in practice; values
                // that concretely disagree (`{"a": 1, "b": "two"}`) are a static error, recovering
                // as a `Map<_, dyn>`. An empty `{}` leaves both unspecified (an inference hole).
                let mut key_ty = Type::Unknown;
                let mut val_ty = Type::Unknown;
                let mut heterogeneous = false;
                for (k, v) in entries {
                    let kt = self.synth(k, env);
                    let vt = self.synth(v, env);
                    key_ty = unify_element(&key_ty, &kt).unwrap_or(Type::Dyn);
                    match unify_element(&val_ty, &vt) {
                        Some(u) => val_ty = u,
                        None => heterogeneous = true,
                    }
                }
                if heterogeneous {
                    self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "map values have differing types",
                        )
                        .help(
                            "make the values one type, or annotate a `Map<string, dyn>` for a mixed map",
                        );
                    val_ty = Type::Dyn; // recover as a mixed map
                }
                // A literal keyed by a type without a runtime key form is rejected statically
                // (extern-types X4 / P-PKEY S3), matching the `Map<K, _>` formation gate.
                if let Type::Named(key_name, _) = &key_ty
                    && self.named_key_capable(key_name, false) == Some(false)
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{key_ty}` cannot key a map: it is not a key-capable type"),
                    )
                    .help(
                        "key-capable types are strings, key-capable extern types (e.g. `Uuid`), \
                         and `@packed` structs of int/bool fields",
                    );
                }
                let ty = Type::Map(Box::new(key_ty), Box::new(val_ty));
                self.note_construction(&ty, *span);
                ty
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => self.synth_member(receiver, name, *name_span, *span, env),
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                // Index into the receiver: a list element, a map value, a string char, or `dyn`.
                let recv = self.synth(receiver, env);
                self.synth(index, env);
                // Note a list-typed index so a `list[i].field` member access can fuse (P-PACK 2.5+).
                // Recorded here — where the receiver's type is already in hand — so `synth_member`
                // need not re-synthesize the inner receiver.
                if matches!(recv, Type::List(_)) {
                    self.index_on_list.insert(*span);
                }
                match stdlib::index_return(&recv) {
                    Some(t) => t,
                    None => {
                        // A concrete primitive cannot be indexed (`42[0]`). A `Named` type may
                        // implement `Index`, and a hole/`dyn` defers — neither errors here.
                        if matches!(recv, Type::Int | Type::Float | Type::Bool | Type::Unit) {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot index into `{recv}`"),
                            );
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
                // Infer the type's arguments from the field values: match each field's declared
                // type (which may be a type parameter) against the value's type, then read the
                // parameters off in declaration order. `Box { value: 1 }` → `Box<int>`. With no
                // generic parameters the result is the bare name; if nothing constrained any
                // parameter the arguments stay empty (a wildcard, compatible with any instantiation).
                let params = self
                    .generic_types
                    .get(&lit.type_name)
                    .cloned()
                    .unwrap_or_default();
                let decls = self
                    .records
                    .get(&lit.type_name)
                    .cloned()
                    .unwrap_or_default();
                let pset: HashSet<String> = params.iter().cloned().collect();
                let mut subst: HashMap<String, Type> = HashMap::new();
                for f in &lit.fields {
                    let vty = self.synth(&f.value, env);
                    // A literal that sets a private field is only valid inside the declaring type's
                    // own methods (slice 2d) — a `class` with private fields is built externally
                    // through an associated `fn`/constructor, not a bare literal.
                    if !self.field_visible(&lit.type_name, &f.name) {
                        self.report_private_field(
                            &lit.type_name,
                            &f.name,
                            FieldAccess::Set,
                            f.name_span,
                        );
                    }
                    if let Some((_, declared)) = decls.iter().find(|(n, _)| n == &f.name) {
                        if !pset.is_empty() {
                            bind_type_params(declared, &vty, &pset, &mut subst);
                        }
                        // The field value must be assignable to the declared field type (`E0007`),
                        // mirroring the field-default check. The type's own parameters are erased to
                        // `dyn` (they are inferred from this very value above), so a generic field
                        // accepts any value while a concrete field type is enforced.
                        let expected = erase_type_params(declared.clone(), &pset);
                        if !self.arg_assignable(&vty, &expected) {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                f.value.span(),
                                format!(
                                    "field `{}` expects type `{expected}`, found `{vty}`",
                                    f.name
                                ),
                            );
                        }
                    }
                }
                let args = if subst.is_empty() {
                    Vec::new()
                } else {
                    params
                        .iter()
                        .map(|p| subst.get(p).cloned().unwrap_or(Type::Dyn))
                        .collect()
                };
                let ty = Type::Named(lit.type_name.clone(), args);
                self.note_construction(&ty, lit.span);
                ty
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
                        self.error(
                            DiagnosticCode::InvalidTry,
                            *span,
                            format!("`?` expects a `Result` or `Option`, found `{other}`"),
                        )
                        .help("`?` only propagates `Result`/`Option`; this value is neither");
                        Type::Unknown
                    }
                }
            }
            Expr::Await { expr, span } => {
                let inner = self.synth(expr, env);
                // Coloring (Track A): `.await` is legal only inside an async context (an `async fn`
                // body or the implicitly-async top level). A `.await` in a sync `fn` — or in a closure
                // passed to a builtin, where `current_async` was reset at the boundary — is E0040.
                if !self.current_async {
                    self.error(
                        DiagnosticCode::AsyncMisuse,
                        *span,
                        "`.await` is only allowed inside an `async fn` (or the async top level)"
                            .to_string(),
                    )
                    .help(
                        "mark the enclosing function `async fn`; `.await` cannot be used in a \
                             synchronous function or in a closure passed to a builtin",
                    );
                }
                // `Future<T>.await` yields `T`; a hole/`dyn` defers to runtime; anything else is a
                // `.await` on a non-future.
                match &inner {
                    Type::Named(n, args) if n == stdlib::FUTURE => {
                        args.first().cloned().unwrap_or(Type::Unknown)
                    }
                    t if t.defers_to_runtime() => t.clone(),
                    other => {
                        self.error(
                            DiagnosticCode::AsyncMisuse,
                            *span,
                            format!("`.await` expects a `Future`, found `{other}`"),
                        )
                        .help("`.await` unwraps a `Future<T>` produced by an `async fn`");
                        Type::Unknown
                    }
                }
            }
            Expr::Spawn {
                future,
                isolate,
                span,
            } => {
                let kw = if *isolate { "isolate" } else { "spawn" };
                let inner = self.synth(future, env);
                // Structured concurrency (Track A.3b): `spawn`/`isolate` are legal only inside a
                // `concurrent { }` scope. An orphan one (no enclosing scope — incl. one in a closure,
                // where the depth was reset) is E0041 by construction, so a spawned unit can never
                // outlive a scope.
                if self.concurrent_depth == 0 {
                    self.error(
                        DiagnosticCode::OrphanSpawn,
                        *span,
                        format!("`{kw}` is only allowed inside a `concurrent {{ }}` scope"),
                    )
                    .help(format!(
                        "wrap the `{kw}` in a `concurrent {{ }}` block; a task must have an owning \
                             scope that joins it"
                    ));
                }
                // `spawn e`/`isolate f(args)` take a `Future<T>` (an `async fn` call) and yield a handle
                // that is itself a `Future<T>` — so `spawn f().await` produces the result. A non-future
                // operand is E0041 (a hole/`dyn` defers to runtime).
                let result = match &inner {
                    Type::Named(n, _) if n == stdlib::FUTURE => inner.clone(),
                    t if t.defers_to_runtime() => {
                        Type::Named(stdlib::FUTURE.to_string(), vec![t.clone()])
                    }
                    other => {
                        self.error(
                            DiagnosticCode::OrphanSpawn,
                            *span,
                            format!("`{kw}` expects a `Future`, found `{other}`"),
                        )
                        .help(format!("`{kw}` an `async fn` call, e.g. `{kw} fetch(url)`"));
                        Type::Named(stdlib::FUTURE.to_string(), vec![Type::Unknown])
                    }
                };
                // `isolate` runs in a fresh heap, so its arguments and result must be `Send` (E0042) —
                // the check the object-model arc parked here. `spawn` (same heap) has no such limit.
                if *isolate {
                    self.check_isolate_send(future, &result, *span);
                }
                result
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
            Expr::As { expr, ty, span } => {
                let src = self.synth(expr, env);
                self.check_type_ref(ty);
                let target = from_ref_q(ty, &self.extern_types);
                // Narrowing is the explicit way *out* of an open type: the dynamic top `dyn`, an
                // un-inferred hole (which defers), a **union** (a *closed* `dyn`), or an abstract
                // **kind-type** (`Enum`/`Struct`/`Class` — narrow to a concrete member). A value
                // whose static type is already a single concrete type has nothing dynamic to narrow
                // — that is an `E0028`.
                if !src.defers_to_runtime() && !matches!(src, Type::Union(_) | Type::Kind(_)) {
                    self.error(
                        DiagnosticCode::InvalidNarrow,
                        *span,
                        format!(
                            "`.as<{target}>()` can only narrow a `dyn` or union value, but \
                                 this value is already `{src}`"
                        ),
                    )
                    .help(
                        "narrowing converts an open type (`dyn` or a union) to a checked `?T`; \
                             a value with a single known concrete type does not need it",
                    );
                }
                Type::Option(Box::new(target))
            }
            Expr::TypeTest { expr, ty, .. } => {
                // A type *test* is always well-formed on any source — even a concrete one (it is
                // simply a constant `true`/`false`), unlike `.as<T>()` whose narrowing of a known
                // concrete value is an `E0028`. We only validate the target type names something.
                self.synth(expr, env);
                self.check_type_ref(ty);
                Type::Bool
            }
            Expr::AttributesOf { ty, span } => {
                self.check_type_ref(ty);
                let target = from_ref_q(ty, &self.extern_types);
                // The type argument must itself be an attribute — a struct marked `@attribute` (the
                // same capability gate as a `#[T(...)]` use). Otherwise the manifest holds no `T` to
                // materialize.
                let is_attribute = matches!(&target, Type::Named(n, _)
                    if self.attributes.contains(n));
                if !is_attribute {
                    self.error(
                        DiagnosticCode::NotAnAttribute,
                        *span,
                        format!(
                            "`attributes_of` requires an attribute type, but `{target}` is not one"
                        ),
                    )
                    .help("name a record marked `@attribute`");
                    return Type::List(Box::new(Type::Dyn));
                }
                Type::List(Box::new(Type::Named(
                    "Attributed".to_string(),
                    vec![target],
                )))
            }
            Expr::TypeOf { value, span } => {
                // Synthesize the operand's static type; the result of `type_of` is always the
                // prelude `Type` enum. When the operand is concretely typed, record the precise
                // `TypeRepr` so the backends bake a full-fidelity `Type` constant (A); otherwise the
                // site stays absent and falls back to the runtime head-constructor path (B).
                let operand = self.synth(value, env);
                if let Some(repr) = type_to_repr_top(&operand, &self.type_kinds) {
                    self.sites.type_of_sites.insert(*span, repr);
                }
                Type::Named("Type".to_string(), Vec::new())
            }
            Expr::RolesOf { ty, span } => {
                // The compiler-built role index, surfaced as `List<RoleBinding>`. The optional
                // turbofish scopes the query to one role enum, which — like `attributes_of`'s
                // `@attribute` gate — must be a `@semantic` enum (only those contribute roles).
                if let Some(ty) = ty {
                    self.check_type_ref(ty);
                    let target = from_ref_q(ty, &self.extern_types);
                    let is_semantic = matches!(&target, Type::Named(n, _)
                        if self.semantic_enums.contains(n));
                    if !is_semantic {
                        self.error(
                            DiagnosticCode::InvalidRole,
                            *span,
                            format!(
                                "`roles_of` requires a `@semantic` enum, but `{target}` is not one"
                            ),
                        )
                        .help("mark the enum `@semantic` to query its roles");
                    }
                }
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::ROLE_BINDING.to_string(),
                    Vec::new(),
                )))
            }
            Expr::FromBytes { ty, blob, span } => {
                // The operand must be a `bytes` buffer (gradual holes tolerated).
                let blob_ty = self.synth(blob, env);
                if !matches!(blob_ty, Type::Bytes) && !blob_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        blob.span(),
                        format!("`from_bytes` expects a `bytes` value, found `{blob_ty}`"),
                    );
                }
                self.check_type_ref(ty);
                let elem = from_ref_q(ty, &self.extern_types);
                // The element type must be a packable `@packed` struct — the blob is a flat packed
                // buffer. Recording the layout in `packed_list_sites` (the channel list literals use)
                // hands the backend the schema to rebuild the list. Generic over any declared packable
                // type (no hardcoded list — extension-friendly).
                match self.packed_layout(&elem) {
                    Some(layout) => {
                        self.sites.packed_list_sites.insert(*span, layout);
                    }
                    None => {
                        self.error(
                            DiagnosticCode::InvalidPackedType,
                            *span,
                            format!(
                                "`from_bytes::<{elem}>` requires a packable `@packed` struct element type"
                            ),
                        );
                    }
                }
                Type::List(Box::new(elem))
            }
            Expr::Channel {
                elem,
                capacity,
                span: _,
            } => {
                // The capacity is a buffer size — an `int` (gradual holes tolerated).
                let cap_ty = self.synth(capacity, env);
                if !matches!(cap_ty, Type::Int) && !cap_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        capacity.span(),
                        format!("`channel` expects an `int` capacity, found `{cap_ty}`"),
                    );
                }
                self.check_type_ref(elem);
                let t = from_ref_q(elem, &self.extern_types);
                // The split-endpoint pair: a `Sender<T>` and a `Receiver<T>` over the message type.
                Type::Tuple(vec![
                    Type::Named(stdlib::SENDER.to_string(), vec![t.clone()]),
                    Type::Named(stdlib::RECEIVER.to_string(), vec![t]),
                ])
            }
            Expr::TypedModuleCall {
                recv,
                func,
                func_span,
                ty,
                args,
                span,
            } => {
                let module = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.clone(),
                    _ => String::new(),
                };
                // Arguments are synthesized (checked as expressions) regardless of which function.
                let arg_types: Vec<Type> = args.iter().map(|a| self.synth(a, env)).collect();
                // The only call-site-typed native function today is `json.parse::<T>(text)`. (When
                // more land, this resolves through the registry's `RetTy::TypeArg` functions; the
                // dynamic `json.parse(s)` keeps its own path, so the shared name does not collide.)
                if module == "json" && func == "parse" {
                    if arg_types.len() != 1 {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`json.parse::<T>` takes 1 argument, found {}",
                                arg_types.len()
                            ),
                        );
                    } else if !matches!(arg_types[0], Type::String)
                        && !arg_types[0].defers_to_runtime()
                    {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            args[0].span(),
                            format!("`json.parse` expects a `string`, found `{}`", arg_types[0]),
                        );
                    }
                } else {
                    self.error(
                        DiagnosticCode::UnknownName,
                        *func_span,
                        format!(
                            "`{module}.{func}::<T>(...)` is not a call-site-typed native function"
                        ),
                    );
                }
                self.check_type_ref(ty);
                let t = from_ref_q(ty, &self.extern_types);
                // Record the build recipe; a type with no JSON decoding (an enum, class, generic, …)
                // is an error here.
                match self.type_to_recipe(&t) {
                    Some(recipe) => {
                        self.sites.typed_module_call_sites.insert(*span, recipe);
                    }
                    None => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`{t}` cannot be deserialized from JSON with `json.parse`"),
                        );
                    }
                }
                t
            }
            Expr::Invoke {
                recv, name, args, ..
            } => {
                // The receiver is either a value (→ instance method) or a bare type name (→
                // associated function). A bare type name is not an ordinary value expression, so it
                // is licensed here rather than synthesized; any other receiver is synthesized
                // normally (it must be well-typed, but its type is unconstrained — dispatch is
                // dynamic). The name (a `string`) and args (a `List`) are runtime-checked, so they
                // are synthesized leniently. By-name invocation is fallible by construction:
                // unknown name / wrong arity are runtime `Err`, never static errors.
                let recv_is_type = matches!(
                    recv.as_ref(),
                    Expr::Ident { name, .. } if self.types.contains(name)
                );
                if !recv_is_type {
                    self.synth(recv, env);
                }
                self.synth(name, env);
                self.synth(args, env);
                Type::Result(Box::new(Type::Dyn), Box::new(Type::Dyn))
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                ..
            } => self.synth_field_set(receiver, field, *field_span, value, env),
        }
    }

    /// Type-check a field assignment `x.f = v` (Phase 5.2): the receiver must be a class instance,
    /// the field must be declared `mut` (else E0033), and the value must be assignable to the
    /// field's declared type (else E0007). The result is the receiver's own type — the surrounding
    /// `Stmt::Binding` reassigns `x` to a value of the same type. A `dyn`/hole receiver defers to
    /// runtime (the field cannot be resolved statically).
    fn synth_field_set(
        &mut self,
        receiver: &Expr,
        field: &str,
        field_span: Span,
        value: &Expr,
        env: &mut Env,
    ) -> Type {
        let recv = self.synth(receiver, env);
        let vty = self.synth(value, env);
        if recv.defers_to_runtime() {
            return recv;
        }
        let Type::Named(name, recv_args) = recv.clone() else {
            self.error(
                DiagnosticCode::ImmutableField,
                field_span,
                format!("cannot assign to field `{field}`: `{recv}` is not a class instance"),
            )
            .help("only a `mut` field of a class instance can be assigned with `x.f = v`");
            return recv;
        };
        // A private field is assignable only inside its declaring type's own methods (slice 2d).
        if !self.field_visible(&name, field) {
            self.report_private_field(&name, field, FieldAccess::Assign, field_span);
        }
        // Asymmetric `mut` rule (object-model slice 2b′): a value `struct` field-set is desugared to
        // a rebind of the receiver (`x = T { ...x, f: v }`), so the receiver binding must be `mut`
        // (E0006); a reference `class` field-set mutates the shared instance in place, needing no
        // `mut` binding. (The field itself must still be declared `mut` — E0033, checked below.)
        if matches!(
            self.type_kinds.get(&name),
            Some(noeta_types::TypeKind::Struct)
        ) && let Expr::Ident {
            name: recv_name,
            span: recv_span,
        } = receiver
            && !lookup_mutable(env, recv_name)
        {
            self.error(
                DiagnosticCode::ImmutableAssignment,
                *recv_span,
                format!(
                    "cannot assign to field `{field}`: `{recv_name}` is an immutable binding, \
                         and a `struct` field-set rebinds it"
                ),
            )
            .help(format!(
                "declare it `mut {recv_name} = ...` (a value `struct` is updated by rebinding); \
                     a reference `class` field mutates in place without `mut`"
            ));
        }
        let is_mut = self
            .mut_fields
            .get(&name)
            .is_some_and(|fields| fields.contains(field));
        if !is_mut {
            let exists = self
                .records
                .get(&name)
                .is_some_and(|fs| fs.iter().any(|(n, _)| n == field));
            // Both `struct` (value) and `class` (reference) fields are immutable unless declared
            // `mut`; the unified body grammar gives them the same rule and the same diagnostic.
            if !exists {
                self.error(
                    DiagnosticCode::ImmutableField,
                    field_span,
                    format!("type `{name}` has no field `{field}`"),
                );
            } else {
                self.error(
                    DiagnosticCode::ImmutableField,
                    field_span,
                    format!("field `{field}` of `{name}` is not declared `mut`"),
                )
                .help(format!(
                    "declare it `mut {field}: ...` to allow `x.{field} = ...`, or build a new value \
                     with `{name} {{ ...x, {field}: ... }}`"
                ));
            }
            return recv;
        }
        // The field is `mut`; check the new value against its declared type, substituting the
        // class's generic parameters from the receiver's type arguments (mirroring `synth_member`).
        if let Some((_, fty)) = self
            .records
            .get(&name)
            .and_then(|fs| fs.iter().find(|(n, _)| n == field))
            .map(|(n, t)| (n.clone(), t.clone()))
        {
            let params = self.generic_types.get(&name).cloned().unwrap_or_default();
            let subst: HashMap<String, Type> = params
                .iter()
                .cloned()
                .zip(recv_args.iter().cloned())
                .collect();
            let pset: HashSet<String> = params.into_iter().collect();
            let expected = erase_type_params(apply_subst(&fty, &subst), &pset);
            if !self.assignable(&vty, &expected) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    value.span(),
                    format!("field `{field}` has type `{expected}`, but the value is `{vty}`"),
                );
            }
        }
        recv
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
            // `~` concatenates two lists (their element types unified, `dyn` on a concrete clash)
            // or display-concatenates any other operands to a string.
            BinaryOp::Concat => {
                if let (Type::List(a), Type::List(b)) = (&lt, &rt) {
                    Type::List(Box::new(unify_element(a, b).unwrap_or(Type::Dyn)))
                } else {
                    Type::String
                }
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                // Fixed-width integers (Tier W): `+ - * / %` on two same-width `IntN` yield that
                // width — `+ - *` mask the result (W2, sign-agnostic), `/ %` use the width-carrying
                // sign-aware op (W3). Mixed-width or `IntN` mixed with `int`/`float` needs an explicit
                // conversion (no implicit widening) → E0044. Intercept before the generic numeric
                // path, whose widening lattice does not model `IntN`.
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    return self.synth_intn_arith(op, &lt, &rt, span);
                }
                // Strict fixed-width floats (P-NUM-SYM): `f32`/`f64` arithmetic is same-type-only,
                // exactly like `IntN` — no implicit widening with `int`/`float` or between each other.
                if matches!(lt, Type::F32 | Type::F64) || matches!(rt, Type::F32 | Type::F64) {
                    return self.synth_fixed_float_arith(op, &lt, &rt, span);
                }
                // Arithmetic is trait-backed: `+`→`Add`, … (`%` has no trait — numerics only). An
                // operand must satisfy that trait — a built-in numeric, a user type that `impl`s it,
                // or a type parameter bounded by it; a `dyn`/hole defers. Otherwise it is rejected,
                // statically catching what the runtime would (`cannot apply` / a missing bound).
                let trait_name = required_operator_trait(op);
                let acceptable = |this: &Self, t: &Type| match trait_name {
                    Some(n) => this.operand_satisfies_operator(t, n),
                    None => t.is_numeric() || t.defers_to_runtime(),
                };
                if !acceptable(self, &lt) || !acceptable(self, &rt) {
                    self.report_operator_error(op, &lt, &rt, trait_name, span);
                    Type::Unknown
                } else if let (Some(lr), Some(rr)) = (lt.numeric_rank(), rt.numeric_rank()) {
                    // Numeric widening lattice `int < f32 < float`: the result is the higher-ranked
                    // operand (`f32 + int → f32`, `f32 + float → float`), the production widening rule.
                    if lr >= rr { lt } else { rt }
                } else {
                    Type::Unknown
                }
            }
            // Ordering comparisons require `Comparable`: a built-in scalar, a user type that derives
            // or `impl`s it, or a type parameter bounded by it. A concrete type that does not is
            // `E0007` (the runtime's "cannot compare"); an unbounded type parameter is `E0025`.
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                // Fixed-width ordering (Tier W3) is sign-dependent (unsigned `u64` ordering differs
                // from signed past bit 63), so it consults the operand width the way W2's arithmetic
                // does — same-width `IntN` only; mixed → E0044. Intercept before the generic
                // `Comparable` path (which the width-carrying `WideInt` op then implements).
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    self.synth_intn_compare(op, &lt, &rt, span);
                    return Type::Bool;
                }
                if matches!(lt, Type::F32 | Type::F64) || matches!(rt, Type::F32 | Type::F64) {
                    self.synth_fixed_float_compare(op, &lt, &rt, span);
                    return Type::Bool;
                }
                if !self.operand_satisfies_operator(&lt, BuiltinTrait::Comparable)
                    || !self.operand_satisfies_operator(&rt, BuiltinTrait::Comparable)
                {
                    self.report_operator_error(op, &lt, &rt, Some(BuiltinTrait::Comparable), span);
                }
                Type::Bool
            }
            // `==`/`!=` are universal (structural equality fallback) and the logical operators take
            // bools; none impose a trait bound, so none is checked here.
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::And | BinaryOp::Or => Type::Bool,
            // `===`/`!==` ask reference identity (*same instance*), meaningful only for the
            // reference kind `class`. A definitely-value operand (scalar, collection, struct/enum,
            // tuple, fn) has no identity → E0034; a `dyn`/hole or class (or a union of them) defers.
            BinaryOp::Identity | BinaryOp::NotIdentity => {
                if !self.is_reference_comparable(&lt) || !self.is_reference_comparable(&rt) {
                    self.error(
                        DiagnosticCode::InvalidIdentityCompare,
                        span,
                        format!(
                            "`{}` compares reference identity, which only a `class` has; \
                             `{lt}` and `{rt}` are value types — compare them with `==`",
                            op.symbol(),
                        ),
                    );
                }
                Type::Bool
            }
            // Symmetric bitwise `& | ^` (P-BITS Tier B on `int`; W5 on fixed-width). Two same-width
            // `IntN` yield that width — the erased op is already correctly extended, so no mask.
            // Mixed-width or `IntN`+`int` → E0044. Otherwise both operands must be `int` → `int`
            // (a `dyn`/hole defers); anything else is E0043 (`bool` uses `&&`/`||`).
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    return self.synth_intn_bitwise(op, &lt, &rt, span);
                }
                let ok = |t: &Type| matches!(t, Type::Int) || t.defers_to_runtime();
                if !ok(&lt) || !ok(&rt) {
                    self.report_noninteger_bitwise(op, &lt, &rt, span);
                }
                Type::Int
            }
            // Shifts `<< >>` are asymmetric: the left operand is the value (it sets the result type),
            // the right is a count (any integer — its width is irrelevant). On a fixed-width value
            // (W5) `<<` masks the result into the width (sign-agnostic, like `+ - *`), and `>>` is
            // sign-dependent — **arithmetic** (sign-fill) on a signed width, **logical** (zero-fill)
            // on an unsigned one — so it lowers to the width-carrying `WideInt`.
            BinaryOp::Shl | BinaryOp::Shr => {
                let amount_ok =
                    |t: &Type| matches!(t, Type::Int | Type::IntN { .. }) || t.defers_to_runtime();
                if let Type::IntN { signed, bits } = lt {
                    if !amount_ok(&rt) {
                        self.error(
                            DiagnosticCode::NonIntegerBitwise,
                            span,
                            format!(
                                "`{}` shift amount must be an integer, found `{rt}`",
                                op.symbol()
                            ),
                        );
                    }
                    // Both `<<` (via `MaskWidth`) and `>>` (via `WideInt`) read the width from here;
                    // lowering routes by the operator.
                    self.sites.width_sites.insert(span, (signed, bits));
                    return Type::IntN { signed, bits };
                }
                let ok = |t: &Type| matches!(t, Type::Int) || t.defers_to_runtime();
                if !ok(&lt) || !amount_ok(&rt) {
                    self.report_noninteger_bitwise(op, &lt, &rt, span);
                }
                Type::Int
            }
        }
    }

    /// Whether `ty` may be a **reference (`class`) instance**, so `===`/`!==` is meaningful on it.
    /// True for a `dyn`/inference hole (may hold a class at runtime), the `Class` kind-type, a
    /// concrete `class` (or an as-yet-unresolved named type, deferring to its own diagnostic), and a
    /// union all of whose members qualify. False for every definitely-value type (scalars,
    /// collections, `struct`/`enum`, functions) — those drive E0034.
    fn is_reference_comparable(&self, ty: &Type) -> bool {
        match ty {
            Type::Unknown | Type::Dyn => true,
            Type::Kind(noeta_types::TypeKind::Class) => true,
            Type::Named(n, _) => matches!(
                self.type_kinds.get(n),
                Some(noeta_types::TypeKind::Class) | None
            ),
            Type::Union(members) => members.iter().all(|m| self.is_reference_comparable(m)),
            _ => false,
        }
    }

    /// Whether `operand` may be used with an operator requiring `trait_name`: a `dyn`/hole defers;
    /// an in-scope **type parameter** is licensed only by its declared bounds; any other type by the
    /// satisfaction model ([`Self::satisfies`] — built-in table + `@derive`/`impl` index).
    fn operand_satisfies_operator(&self, operand: &Type, t: BuiltinTrait) -> bool {
        if operand.defers_to_runtime() {
            return true;
        }
        if let Type::Named(n, _) = operand
            && let Some(bounds) = self.type_params.get(n)
        {
            return bounds.iter().any(|b| b == t.name());
        }
        self.satisfies(operand, t)
    }

    /// The name of an in-scope type parameter (`operand`) that lacks `trait_name` among its bounds,
    /// or `None` if `operand` is not such a parameter — used to pick the diagnostic flavor.
    fn unbounded_type_param(&self, operand: &Type, t: BuiltinTrait) -> Option<String> {
        match operand {
            Type::Named(n, _) => match self.type_params.get(n) {
                Some(bounds) if !bounds.iter().any(|b| b == t.name()) => Some(n.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Report a trait-backed operator applied to an unsupported operand: an unbounded type parameter
    /// is `E0025` (a missing bound, fixable at the declaration); any other concrete mismatch is
    /// `E0007` (the same "cannot apply" the runtime raised). Reported once for the operator.
    fn report_operator_error(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        trait_name: Option<BuiltinTrait>,
        span: Span,
    ) {
        if let Some(tn) = trait_name
            && let Some(n) = self
                .unbounded_type_param(lt, tn)
                .or_else(|| self.unbounded_type_param(rt, tn))
        {
            self.error(
                DiagnosticCode::TraitBoundNotSatisfied,
                span,
                format!(
                    "operator `{}` requires `{n}: {}`, but `{n}` is an unbounded type \
                         parameter",
                    op.symbol(),
                    tn.name()
                ),
            )
            .help(format!("add the bound, e.g. `<{n}: {}>`", tn.name()));
        } else {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("cannot apply `{}` to `{lt}` and `{rt}`", op.symbol()),
            );
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
                self.synth_call(callee, &arg_types, &[], right.span(), env)
            }
            Expr::Ident { .. } | Expr::Member { .. } => {
                self.synth_call(right, &[piped], &[], right.span(), env)
            }
            other => {
                self.synth(other, env);
                Type::Unknown
            }
        }
    }

    /// Finalize the deferred closure arguments of a call once the callee's parameter types are
    /// known (the dyn-closure gap): a `Fn`-typed parameter checks the closure against it — the
    /// absorption arm adopts the parameter types and, for a `-> dyn` expectation, infers the body's
    /// real return — anything else synthesizes standalone (the pre-deferral behavior). Idempotent:
    /// a closure some earlier branch already typed (never `Unknown`) is left alone.
    fn finalize_closure_args(
        &mut self,
        params: &[Type],
        args: &mut [Type],
        arg_exprs: &[Expr],
        env: &mut Env,
    ) {
        for (i, expr) in arg_exprs.iter().enumerate() {
            if !is_deferred_literal_arg(expr) {
                continue;
            }
            let Some(slot) = args.get_mut(i) else {
                continue;
            };
            if !matches!(slot, Type::Unknown) {
                continue;
            }
            // Absorb the expected parameter type where it can guide the literal — a `Fn` for a
            // closure, a `List`/`Map` for a container literal; anything else (a mismatched param, or
            // an unknown one) synthesizes standalone, preserving the pre-deferral behavior (the
            // mismatch is then caught by `check_args`' assignability check).
            *slot = match (expr, params.get(i)) {
                (Expr::Closure { .. }, Some(expected @ Type::Fn { .. })) => {
                    self.check(expr, expected, env)
                }
                (
                    Expr::List { .. } | Expr::Map { .. },
                    Some(expected @ (Type::List(_) | Type::Map(..))),
                ) => self.check(expr, expected, env),
                _ => self.synth(expr, env),
            };
        }
    }

    /// Whether `name` resolves to **something the checker knows** — a local binding, a top-level
    /// or selectively-imported function, a bound module, a user type or enum, or a reserved
    /// prelude name. The unknown-name gate (F1) uses its negation: a name that is none of these
    /// is genuinely undefined, a static `E0005` rather than a deferral to the runtime `E0005`.
    fn is_known_name(&self, name: &str, env: &Env) -> bool {
        lookup(env, name).is_some()
            || self.functions.contains_key(name)
            || self.imported_fns.contains_key(name)
            || self.modules.contains_key(name)
            || self.types.contains(name)
            || self.enums.contains_key(name)
            || RESERVED_PRELUDE.contains(&name)
            // Built-in namable types/enums (`Ordering`, `Type`, `Semantic`, iterator types, …)
            // are legitimate bare references — `Ordering.Less` names the prelude enum's variant.
            || PRELUDE_TYPES.contains(&name)
            // A hoisted top-level global (a fn body may reference one declared later).
            || self.global_binding_names.contains(name)
    }

    fn synth_call(
        &mut self,
        callee: &Expr,
        args: &[Type],
        arg_exprs: &[Expr],
        call_span: Span,
        env: &mut Env,
    ) -> Type {
        let mut args = args.to_vec();
        let ret = self.synth_call_inner(callee, &mut args, arg_exprs, call_span, env);
        // Safety net for the deferred closure arguments: any closure no resolution branch
        // finalized (an unknown callee, a deferred receiver, a variadic prelude call) is
        // synthesized standalone here, so its body is always checked (diagnostics, hover index)
        // exactly as before the deferral existed. A closure's type is never `Unknown` once typed,
        // so the placeholder is an unambiguous marker.
        for (i, expr) in arg_exprs.iter().enumerate() {
            if is_deferred_literal_arg(expr) && matches!(args.get(i), Some(Type::Unknown)) {
                self.synth(expr, env);
            }
        }
        ret
    }

    fn synth_call_inner(
        &mut self,
        callee: &Expr,
        args: &mut [Type],
        arg_exprs: &[Expr],
        call_span: Span,
        env: &mut Env,
    ) -> Type {
        let span = callee.span();
        match callee {
            // A **resolved native module-function** callee (expr-tiers arc): the expression-tier
            // desugar builds this for a native handler, so `handler(statics, holes)` types exactly
            // like the bare `use std.math.sqrt` call below — same params/return tables — no matter
            // that no import bound it. This is what lets a native and a Noeta handler share one
            // `Call` typing path.
            Expr::NativeFnRef { module, func, .. } => {
                if let Some(params) = stdlib::module_params(self.reg(), module, func, args) {
                    let required =
                        stdlib::module_required(self.reg(), module, func).unwrap_or(params.len());
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    self.check_args(&params, required, args, arg_exprs, span, func);
                }
                self.check_module_bounds(module, func, args, span);
                stdlib::module_return(self.reg(), module, func, args).unwrap_or(Type::Unknown)
            }
            // A plain `name(args)` call: a user function, else a prelude free function.
            Expr::Ident { name, .. } => {
                if let Some(sig) = self.functions.get(name) {
                    let required = sig.required;
                    // A generic function is instantiated per call: bind its type parameters from the
                    // argument types, check arguments against the substituted parameters, enforce
                    // the bounds (E0025), and return the substituted result type.
                    if let Some(generic) = sig.generic.clone() {
                        return self.check_generic_call(
                            name,
                            &generic,
                            required,
                            args,
                            arg_exprs,
                            span,
                            &[],
                            env,
                        );
                    }
                    let params = sig.params.clone();
                    let ret = sig.ret.clone();
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    self.check_args(&params, required, args, arg_exprs, span, name);
                    return ret;
                }
                // A selectively-imported module function (`use std.math.sqrt`) called bare — typed
                // exactly like the qualified `math.sqrt(args)` (same params/return tables). A local
                // binding of the same name shadows it (checked first, in the arms above via `env`).
                if let Some((module, func)) = self.imported_fns.get(name).cloned()
                    && lookup(env, name).is_none()
                {
                    if let Some(params) = stdlib::module_params(self.reg(), &module, &func, args) {
                        let required = stdlib::module_required(self.reg(), &module, &func)
                            .unwrap_or(params.len());
                        self.finalize_closure_args(&params, args, arg_exprs, env);
                        self.check_args(&params, required, args, arg_exprs, span, &func);
                    }
                    self.check_module_bounds(&module, &func, args, span);
                    return stdlib::module_return(self.reg(), &module, &func, args)
                        .unwrap_or(Type::Unknown);
                }
                // Prelude functions are polymorphic/variadic — their result is typed, but their
                // arguments are not arity-checked here. (The packed-result note the free `map`
                // recorded here moved to the list-method `map` arm in `synth_call`'s Member case —
                // the free form left the prelude, P1.2.) Closure arguments synthesize standalone
                // first, so a payload-typed result (`some(fn…)`) sees the real closure type.
                self.finalize_closure_args(&[], args, arg_exprs, env);
                if let Some(t) = stdlib::prelude_return(name, args) {
                    return t;
                }
                // Not a user fn, import, or prelude free function. A local (a closure value) or a
                // module/type name called here stays deferred to the runtime (a local closure's
                // args are not statically checked, unchanged); a name that resolves to *nothing*
                // is a genuinely undefined callee — a static `E0005` (F1), so a typo is caught at
                // check time instead of failing at runtime. A session defers (a later entry may
                // define it).
                if !self.session_mode && !self.is_known_name(name, env) {
                    self.error(
                        DiagnosticCode::UnknownName,
                        span,
                        format!("cannot find `{name}` in this scope"),
                    );
                }
                Type::Unknown
            }
            Expr::Member { receiver, name, .. } => {
                // `Enum.try_from(s)` → `?Enum` / `Enum.from(s)` → `Enum` — the built-in string→case
                // conversions (PHP `tryFrom`/`from`), reserved on every enum type. Checked before the
                // variant constructor so the names cannot be captured by a same-named variant.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && (name == "try_from" || name == "from")
                    && lookup(env, tn).is_none()
                    && self.enums.contains_key(tn)
                {
                    self.check_args(&[Type::String], 1, args, arg_exprs, span, name);
                    let ty = Type::Named(tn.clone(), Vec::new());
                    return if name == "from" {
                        ty
                    } else {
                        Type::Option(Box::new(ty))
                    };
                }
                // `Type.Variant(args)` — an algebraic enum constructor applied to its data. Infer the
                // enum's type arguments from the payload (R2b), so `Tree.Leaf(5)` is `Tree<int>`.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && self.is_enum_variant(tn, name)
                {
                    // Payload types bind the enum's generics, so a closure payload must be real.
                    self.finalize_closure_args(&[], args, arg_exprs, env);
                    return self.enum_construction_type(tn, name, args, call_span);
                }
                // `module.func(args)` — a native module call. The module identity comes from either a
                // bare module binding (`client.get`, `client` from `use std.http.client`) or a
                // namespace-group member chain (`http.client.get`, `http` from `use std.http`); both
                // key the same stdlib return-type tables, and the chain form records its span so
                // lowering materializes the leaf module value (`std.http.client`).
                let module_id = match receiver.as_ref() {
                    Expr::Ident { name: m, .. } => self.modules.get(m).cloned(),
                    _ => None,
                }
                .or_else(|| self.resolve_namespace_module(receiver, env));
                if let Some(qm) = module_id {
                    if let Some(params) = stdlib::module_params(self.reg(), &qm, name, args) {
                        let required =
                            stdlib::module_required(self.reg(), &qm, name).unwrap_or(params.len());
                        self.finalize_closure_args(&params, args, arg_exprs, env);
                        self.check_args(&params, required, args, arg_exprs, span, name);
                    }
                    self.check_module_bounds(&qm, name, args, span);
                    return stdlib::module_return(self.reg(), &qm, name, args)
                        .unwrap_or(Type::Unknown);
                }
                // The receiver is a namespace group (`http` from `use std.http`) — a submodule chain
                // (`http.client.get`) already resolved above, so any member reaching here is either
                // an unknown member (`http.nope` — a hard error, a group is fully enumerable) or a
                // deferred non-module child (a sub-namespace/type used in call position). Either way
                // the group handle is not a value, so this must not fall through to the generic
                // method path (which would synthesize `http` as an unknown name).
                if let Some(prefix) = self.resolve_namespace_prefix(receiver, env) {
                    use noeta_stdlib::registry::NsChild;
                    self.finalize_closure_args(&[], args, arg_exprs, env);
                    if matches!(
                        self.reg().resolve_namespace_child(&prefix, name),
                        NsChild::None
                    ) {
                        self.namespace_member_error(&prefix, name, span);
                    }
                    return Type::Unknown;
                }
                // `Type.assoc(args)` — an associated function / static call on a known user type
                // (`Box.new(1)`). Resolve to the type's method signature so the result is precisely
                // typed (a constructor result is `Box`, not a hole) and a generic class enforces its
                // bounds at construction. Guard on the receiver naming a type that is not shadowed
                // by a local variable.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && lookup(env, tn).is_none()
                    && self.types.contains(tn)
                    && let Some(sig) = self.methods.get(&(tn.clone(), name.to_string())).cloned()
                {
                    // An INSTANCE method (its body references `self`) cannot be called
                    // associated-style — there is no receiver to become `self` (E0047,
                    // prelude-redesign EX.2). The classification is derived from the body.
                    if self
                        .method_instance
                        .get(&(tn.clone(), name.to_string()))
                        .copied()
                        .unwrap_or(false)
                    {
                        self.error(
                            DiagnosticCode::InvalidReceiver,
                            span,
                            format!("`{name}` is an instance method of `{tn}`"),
                        )
                        .help(format!(
                            "call it on a value (`x.{name}(...)`), or pass `{tn}.{name}` \
                             as a handle"
                        ));
                        return sig.ret.clone();
                    }
                    // A static call: the type arguments are not known from a bare type name, so the
                    // method's own arguments instantiate any parameters (`Box.new(1)` infers `int`).
                    return self.call_user_method(name, &sig, args, arg_exprs, span, &[], env);
                }
                // `receiver.method(args)` — a built-in method, a user method, or (on a `dyn`/hole
                // receiver) a runtime-dispatched call that stays deferred.
                let recv = self.synth(receiver, env);
                // A user-declared instance method resolves through the same path as a static call
                // (generic methods instantiate + enforce bounds); the receiver's type arguments seed
                // the instantiation so the result is precise. A built-in method or a deferred
                // receiver falls through below.
                if let Type::Named(n, recv_args) = &recv
                    && let Some(sig) = self.methods.get(&(n.clone(), name.to_string())).cloned()
                {
                    // An ASSOCIATED function (never touches `self`) is not callable on a value —
                    // the receiver would be silently discarded (E0047, prelude-redesign EX.2).
                    if !self
                        .method_instance
                        .get(&(n.clone(), name.to_string()))
                        .copied()
                        .unwrap_or(true)
                    {
                        self.error(
                            DiagnosticCode::InvalidReceiver,
                            span,
                            format!("`{name}` is an associated function of `{n}`"),
                        )
                        .help(format!("call it on the type: `{n}.{name}(...)`"));
                        return sig.ret.clone();
                    }
                    return self
                        .call_user_method(name, &sig, args, arg_exprs, span, recv_args, env);
                }
                // THE dyn-closure gap's primary site: a builtin method's parameter types carry
                // the receiver's element type (`List<int>.map` expects `(int) -> dyn`), so the
                // deferred closure argument finalizes against them here — its parameters adopt the
                // element type, its body infers a real return, and the `map` refinements below see
                // a precise `Fn` instead of a context-free one.
                let builtin_params =
                    stdlib::method_params(self.reg(), &recv, name).unwrap_or_default();
                self.finalize_closure_args(&builtin_params, args, arg_exprs, env);
                self.check_method_args(&recv, name, args, arg_exprs, span);
                // A bit intrinsic on a fixed-width receiver (Tier W5) must act within the width, not
                // the erased i64 (`(1u8).leading_zeros() == 7`), so mark the **call** span (the one
                // lowering's `Method` carries) — lowering then emits the width-carrying
                // `WidthIntMethod`. Conversions (`IntMethod::Convert`, the `to_*` names) are already
                // width-typed by name and stay ordinary methods. Signedness is irrelevant here.
                if let Type::IntN { bits, .. } = recv
                    && let Some(m) = noeta_stdlib::IntMethod::from_name(name)
                    && !matches!(m, noeta_stdlib::IntMethod::Convert { .. })
                {
                    self.sites.width_sites.insert(call_span, (false, bits));
                }
                // `it.zip(other)` → `Iterator<(A, B)>`: both element types are needed and only `recv`
                // reaches `method_return`, so the precise tuple is assembled here where the argument
                // type is in scope (A from the receiver, B from the argument iterator).
                if name == "zip"
                    && let Type::Named(rn, ra) = &recv
                    && rn == stdlib::ITERATOR
                {
                    let a = ra.first().cloned().unwrap_or(Type::Dyn);
                    let b = match args.first() {
                        Some(Type::Named(an, aa)) if an == stdlib::ITERATOR => {
                            aa.first().cloned().unwrap_or(Type::Dyn)
                        }
                        _ => Type::Dyn,
                    };
                    return Type::Named(
                        stdlib::ITERATOR.to_string(),
                        vec![Type::Tuple(vec![a, b])],
                    );
                }
                // `it.map(f)` → `Iterator<R>` where `R` is the closure's return type — known here from
                // the argument but not to `method_return` (which sees only the receiver). (Track I.1c.)
                if name == "map"
                    && let Type::Named(rn, _) = &recv
                    && rn == stdlib::ITERATOR
                {
                    let r = match args.first() {
                        Some(Type::Fn { ret, .. }) => (**ret).clone(),
                        _ => Type::Dyn,
                    };
                    return Type::Named(stdlib::ITERATOR.to_string(), vec![r]);
                }
                // `xs.map(f)` on a list → `List<R>`, `R` the closure's return type — the eager list
                // method form (prelude-redesign P1), refined here for the same reason as iterator
                // `map`. Matches the free `map(xs, f)` this replaces.
                if name == "map" && matches!(recv, Type::List(_)) {
                    let r = match args.first() {
                        Some(Type::Fn { ret, .. }) => (**ret).clone(),
                        _ => Type::Dyn,
                    };
                    // Record the packed-result note the free `map` gets (keyed by the call span), so a
                    // packed-struct element still lowers to a flat result.
                    self.note_map_packed(&r, call_span);
                    return Type::List(Box::new(r));
                }
                // A method-bundle method (kernel-methods K2): the receiver is a bound `@packed`
                // type (`Element`) or a `List<T>` of one (`Bulk`). Resolution is static: the
                // route is recorded at the call span for lowering to bake in — so dispatch is
                // call-site-resolved (an empty list receiver works) and a `dyn` receiver simply
                // never reaches here (the documented escape-hatch behavior).
                if let Some(ret) = self.bundle_method_call(&recv, name, args, span, call_span) {
                    return ret;
                }
                let ret = self.method_call_return(&recv, name);
                // A method call on a concrete primitive with no such built-in method is an error,
                // mirroring the non-indexable check (`42[0]`). `dyn`/holes defer (their result is
                // the deferred type, not `Unknown`), and a user `Named` type may resolve the call
                // through a trait at runtime — so both are left lenient; only the closed primitives
                // are flagged.
                if matches!(ret, Type::Unknown)
                    && matches!(
                        recv,
                        Type::Int | Type::IntN { .. } | Type::Float | Type::Bool | Type::Unit
                    )
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("type `{recv}` has no method `{name}`"),
                    );
                }
                ret
            }
            _ => {
                self.synth(callee, env);
                Type::Unknown
            }
        }
    }

    /// Check a call to a resolved user method or associated function (`Box.new(...)`, `obj.m(...)`).
    /// A generic one (a method of a generic class) instantiates and enforces its bounds through the
    /// shared [`Self::check_generic_call`]; a non-generic one checks arguments against its
    /// (erased) parameter types and returns its declared return type.
    /// The type of an enum-variant construction — `Tree.Leaf(5)` (payload) or `Color.Red` (nullary) —
    /// **inferring the enum's type arguments** (R2b): for a generic enum, unify the variant's declared
    /// payload types against the argument types (like a generic constructor call, reusing
    /// [`bind_type_params`]), filling any parameter the payload does not pin with `dyn`; for a
    /// non-generic enum, the empty argument list. Reuses the accurate [`VariantInfo::fields`] (the same
    /// source the `Send`/relevance analyses read). Records the construction site (`span`) so reflection
    /// can tag the value (R2b.2); the refined type also flows into the static `type_of` path.
    fn enum_construction_type(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Type],
        span: Span,
    ) -> Type {
        let params = self
            .generic_types
            .get(enum_name)
            .cloned()
            .unwrap_or_default();
        let type_args = if params.is_empty() {
            Vec::new()
        } else {
            let pset: HashSet<String> = params.iter().cloned().collect();
            let mut subst: HashMap<String, Type> = HashMap::new();
            if let Some(fields) = self
                .enums
                .get(enum_name)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| v.fields.clone())
            {
                for (decl, arg) in fields.iter().zip(args) {
                    bind_type_params(decl, arg, &pset, &mut subst);
                }
            }
            params
                .iter()
                .map(|p| subst.get(p).cloned().unwrap_or(Type::Dyn))
                .collect()
        };
        let ty = Type::Named(enum_name.to_string(), type_args);
        self.note_construction(&ty, span);
        ty
    }

    #[allow(clippy::too_many_arguments)]
    fn call_user_method(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &mut [Type],
        arg_exprs: &[Expr],
        span: Span,
        recv_args: &[Type],
        env: &mut Env,
    ) -> Type {
        if let Some(generic) = &sig.generic {
            return self.check_generic_call(
                name,
                generic,
                sig.required,
                args,
                arg_exprs,
                span,
                recv_args,
                env,
            );
        }
        let params = sig.params.clone();
        self.finalize_closure_args(&params, args, arg_exprs, env);
        self.check_args(&params, sig.required, args, arg_exprs, span, name);
        sig.ret.clone()
    }

    /// Arity- and type-check a method call's arguments against the resolved parameter signature
    /// (a built-in method or a user method); a deferred receiver or an unknown method is not
    /// checked.
    fn check_method_args(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
    ) {
        if let Some(params) = stdlib::method_params(self.reg(), recv, name) {
            let required = stdlib::method_required(self.reg(), recv, name).unwrap_or(params.len());
            self.check_args(&params, required, args, arg_exprs, span, name);
        } else if let Type::Named(n, _) = recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            let params = sig.params.clone();
            let required = sig.required;
            self.check_args(&params, required, args, arg_exprs, span, name);
        }
    }

    /// Check a call's argument count and types against the callable's parameter types, reporting
    /// at `span`. Lenient where either side defers to runtime (`dyn`/hole) and on numeric widening
    /// (`int` where `float` is expected), so polymorphic and numeric calls are not false positives.
    fn check_args(
        &mut self,
        params: &[Type],
        required: usize,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
        callee: &str,
    ) {
        if args.len() < required || args.len() > params.len() {
            let expected = if required == params.len() {
                format!("{}", params.len())
            } else {
                format!("between {required} and {}", params.len())
            };
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{callee}` expects {expected} argument(s), found {}",
                    args.len()
                ),
            );
            return;
        }
        // Only the supplied arguments are type-checked; the omitted trailing parameters are
        // filled by their defaults (already checked against their parameter types at the
        // declaration), so `zip` stopping at the shorter side is exactly right.
        for (i, (param, arg)) in params.iter().zip(args).enumerate() {
            // A bare numeric literal argument adapts into a fixed-width parameter (`f(200)` for a
            // `u8` param, `f(1.5)` for `f32`/`f64`) — exactly as it does at a binding of that type
            // (P-NUM-SYM). Try that first; a non-literal or non-adapting arg falls to `arg_assignable`
            // (which keeps the `int`/`float` widening leniency the strict fixed-width types lack).
            if let Some(expr) = arg_exprs.get(i)
                && self.try_adapt_literal(expr, param).is_some()
            {
                continue;
            }
            if !self.arg_assignable(arg, param) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{param}`"),
                );
            }
        }
    }

    /// Validate every `@semantic` directive and `@role(Enum.Variant)` tag in the program (`E0031`).
    /// Runs **after** `collect`, so the full set of `@semantic` enums is known regardless of source
    /// order. A `@semantic` on a struct/class is a misplacement (it marks enums only); a `@role`
    /// must tag a struct that is itself an attribute and must name a fieldless variant of a
    /// `@semantic` enum. Well-formed tags are surfaced purely by `reflect::build`, so nothing is
    /// stored here.
    /// Validate every `@tier` declaration (tier-providers T2, E0051) and build the program's
    /// [`tiers::TierRegistry`]. Runs after `collect`, so a `config:` type declared later in the
    /// file (or in an imported module) is visible. Four rules: the name must not collide with a
    /// built-in tier; two declarations must not claim one name; `config:` must name an
    /// `@attribute` struct; and the runner must be `fn(roots: List<TierRoot>): void` — the
    /// signature dispatch calls with the activated roots.
    fn check_tier_decls(&mut self, program: &Program) {
        // Resolve the extension-tier half of the name-space against THIS checker's registry
        // (instance-registry IR4), so an embed session whose own extension declares a `@tier`
        // validates its `@<tier>` blocks correctly. Defaults to the process-global registry.
        self.tier_registry = tiers::TierRegistry::collect_with_registry(program, self.reg());
        let mut seen: HashMap<(String, String), Span> = HashMap::new();
        for stmt in &program.stmts {
            let Stmt::Fn(f) = stmt else { continue };
            let Some(decl) = &f.tier else { continue };
            // Redeclaring an extension tier's name is legal (provider override): the declaration
            // is dormant until a target's `tiers` map selects its package as the provider
            // (`bench = "criterion"`); the extension declaration stays the default. Only a
            // duplicate within one provider — two `@tier(x)` declarations whose runners share a
            // package root — is a real collision (E0051): provider selection could not tell them
            // apart.
            let root = decl_runner_root(&f.name);
            if let Some(first) = seen.get(&(decl.name.clone(), root.clone())) {
                let first = *first;
                self.error(
                    DiagnosticCode::InvalidTierDeclaration,
                    decl.name_span,
                    format!(
                        "tier `{}` is declared more than once by one provider",
                        decl.name
                    ),
                )
                .help(format!(
                    "the first declaration is at {first:?}; a tier has exactly one runner per \
                     package"
                ));
            } else {
                seen.insert((decl.name.clone(), root), decl.name_span);
            }
            if let Some((config, config_span)) = &decl.config
                && !self.attributes.contains(config)
            {
                self.error(
                    DiagnosticCode::InvalidTierDeclaration,
                    *config_span,
                    format!("`config: {config}` does not name an `@attribute` struct"),
                )
                .help("a tier's knobs are an attribute's fields; declare the struct with `@attribute`");
            }
            // `text:` and `config:` are mutually exclusive: a text tier's body is verbatim prose,
            // so there are no contained fns to stamp knob attributes onto.
            if let (Some(_), Some((_, text_span))) = (&decl.config, &decl.text) {
                self.error(
                    DiagnosticCode::InvalidTierDeclaration,
                    *text_span,
                    format!(
                        "tier `{}` declares both `config:` and `text:` — a text tier has no knobs",
                        decl.name
                    ),
                )
                .help(
                    "a `text: \"<lang>\"` tier's `@<name> { … }` bodies are captured verbatim \
                     (no fns inside to configure); drop one of the two",
                );
            }
            if let Some((lang, text_span)) = &decl.text
                && lang.is_empty()
            {
                self.error(
                    DiagnosticCode::InvalidTierDeclaration,
                    *text_span,
                    "`text:` needs a language ID for the body, e.g. `text: \"markdown\"`",
                )
                .help(
                    "the ID tags the verbatim bodies for tooling (editor highlighting, \
                     extraction); use a lowercase language name like \"markdown\", \"xml\", \"sql\"",
                );
            }
            // An **expression tier** (expr-tiers arc): `expr: T` makes the decorated fn the
            // tier's *handler* — `fn(statics: List<string>, holes: List<() -> U>): T` — not a
            // runner. Its own rules, then skip the runner-signature branch entirely.
            if let Some((expr_ty, expr_span)) = &decl.expr {
                if decl.config.is_some() {
                    self.error(
                        DiagnosticCode::InvalidTierDeclaration,
                        *expr_span,
                        format!(
                            "tier `{}` declares both `config:` and `expr:` — an expression tier \
                             has no knobs",
                            decl.name
                        ),
                    )
                    .help(
                        "an `expr: Type` tier's `@<name> { … }` blocks are expressions (no fns \
                         inside to configure); drop one of the two",
                    );
                }
                let statics_ok = matches!(
                    f.params.first().and_then(|p| p.ty.as_ref()),
                    Some(TypeRef::Named { name, args, .. })
                        if name == "List"
                            && matches!(
                                args.as_slice(),
                                [TypeRef::Named { name: el, args: el_args, .. }]
                                    if el == "string" && el_args.is_empty()
                            )
                );
                // The hole type `U` is the handler's choice — only the thunk shape is fixed.
                let holes_ok = matches!(
                    f.params.get(1).and_then(|p| p.ty.as_ref()),
                    Some(TypeRef::Named { name, args, .. })
                        if name == "List"
                            && matches!(
                                args.as_slice(),
                                [TypeRef::Fn { params, .. }] if params.is_empty()
                            )
                );
                let ret_ok = matches!(
                    f.ret.as_ref(),
                    Some(TypeRef::Named { name, args, .. }) if name == expr_ty && args.is_empty()
                );
                if f.params.len() != 2 || !statics_ok || !holes_ok || !ret_ok {
                    self.error(
                        DiagnosticCode::InvalidTierDeclaration,
                        f.name_span,
                        format!(
                            "tier `{}`'s handler must be `fn(statics: List<string>, holes: \
                             List<() -> U>): {expr_ty}`",
                            decl.name
                        ),
                    )
                    .help(
                        "an expression tier's `@<name> { … }` block desugars to \
                         `handler(statics, holes)`: the body's literal segments (always holes + \
                         1) and one zero-param closure per `${…}` hole, typed against the `U` \
                         you choose; the return type must match the declared `expr:`",
                    );
                }
                continue;
            }
            // The runner signature: exactly one `List<TierRoot>` parameter (`List<TierText>` for
            // a text tier — its roots are verbatim bodies, not fns), returning `void`.
            let root_ty = if decl.text.is_some() {
                noeta_ast::reflect::TIER_TEXT
            } else {
                noeta_ast::reflect::TIER_ROOT
            };
            let param_ok = f.params.len() == 1
                && matches!(
                    f.params[0].ty.as_ref(),
                    Some(TypeRef::Named { name, args, .. })
                        if name == "List"
                            && matches!(
                                args.as_slice(),
                                [TypeRef::Named { name: el, args: el_args, .. }]
                                    if el == root_ty && el_args.is_empty()
                            )
                );
            let ret_ok = matches!(
                f.ret.as_ref(),
                Some(TypeRef::Named { name, args, .. }) if name == "void" && args.is_empty()
            );
            if !param_ok || !ret_ok {
                self.error(
                    DiagnosticCode::InvalidTierDeclaration,
                    f.name_span,
                    format!(
                        "tier `{}`'s runner must be `fn(roots: List<{root_ty}>): void`",
                        decl.name
                    ),
                )
                .help(if decl.text.is_some() {
                    "a text tier's runner receives one root per verbatim body — `root.target` \
                     names the adjacent declaration (`\"\"` for module/section prose), `root.text` \
                     is the body"
                } else {
                    "the runner receives one activated root per fn — `root.name` for the report, \
                     `root.run()` to invoke it; knob values come from `attributes_of::<Config>()`"
                });
            }
        }
    }

    fn check_semantic_roles(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(r) => {
                    self.check_misplaced_semantic(r.semantic, &r.name, "record");
                    self.check_role_tags(r.name_span, r.role.as_deref(), r.attribute.is_some());
                    self.check_packed_struct(r);
                }
                Stmt::Class(c) => {
                    self.check_misplaced_semantic(c.semantic, &c.name, "class");
                    self.check_misplaced_packed(c.packed, &c.name, "class");
                    // A role tags an attribute, and attributes are structs only, so `@role` on a
                    // class is an error (E0031).
                    if c.role.is_some() {
                        self.error(
                            DiagnosticCode::InvalidRole,
                            c.name_span,
                            format!(
                                "a class cannot carry a role: `{}` must be a record attribute",
                                c.name
                            ),
                        )
                        .help("declare it as an `@attribute type` and tag that with `@role`");
                    }
                }
                Stmt::Enum(e) => {
                    self.check_misplaced_packed(e.packed, &e.name, "enum");
                }
                _ => {}
            }
        }
    }

    /// Whether `ty` can be a field of a `@packed` struct (P-PACK): a primitive (`int`/`float`/`bool`)
    /// or another packed struct (a non-generic `Named` in `packed_structs`). Everything else — a
    /// string/list/map/class/enum/`dyn`/generic — is heap-shaped and cannot lay out flat.
    fn is_packable_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Int | Type::Float | Type::F32 | Type::Bool => true,
            Type::Named(name, args) if args.is_empty() => self.packed_structs.contains(name),
            _ => false,
        }
    }

    /// The flat [`PackedLayout`] of `ty` if it is a `@packed` struct, else `None` (P-PACK Phase 2).
    /// Recurses through nested packed fields, flattening them inline. `check_packed_struct` has
    /// already guaranteed every field of a packed struct is packable, so the field walk never bails on
    /// a well-typed program; the `?`s defend against a malformed registry (and an unpacked element).
    /// Resolve a checker [`Type`] into a [`noeta_stdlib::TypeRecipe`] for call-site-typed
    /// deserialization (`json.parse::<T>`), or `None` if `T` has no JSON decoding: an enum or class
    /// (a reference/identity type, or a sum with no canonical JSON form), a tuple/set/result/`dyn`,
    /// a non-string-keyed map, a generic instantiation, or a struct with any such field. A struct
    /// records its fields in **declared order** (so the decoder emits them in the order the backend's
    /// registered type expects).
    fn type_to_recipe(&self, ty: &Type) -> Option<noeta_stdlib::TypeRecipe> {
        use noeta_stdlib::TypeRecipe;
        Some(match ty {
            Type::Int => TypeRecipe::Int,
            Type::Float => TypeRecipe::Float,
            Type::F32 => TypeRecipe::F32,
            Type::Bool => TypeRecipe::Bool,
            Type::String => TypeRecipe::Str,
            Type::Unit => TypeRecipe::Unit,
            Type::Option(e) => TypeRecipe::Option(Box::new(self.type_to_recipe(e)?)),
            Type::List(e) => TypeRecipe::List(Box::new(self.type_to_recipe(e)?)),
            // JSON object keys are strings, so only string-keyed maps decode.
            Type::Map(k, v) if matches!(**k, Type::String) => {
                TypeRecipe::Map(Box::new(self.type_to_recipe(v)?))
            }
            // Only a non-generic value struct decodes (a class is reference/identity; an enum has no
            // canonical JSON shape). The field set is the declared record fields, in order.
            Type::Named(name, args)
                if args.is_empty()
                    && self.type_kinds.get(name) == Some(&noeta_types::TypeKind::Struct) =>
            {
                let fields = self
                    .records
                    .get(name)?
                    .iter()
                    .map(|(fname, fty)| Some((fname.clone(), self.type_to_recipe(fty)?)))
                    .collect::<Option<Vec<_>>>()?;
                TypeRecipe::Struct {
                    name: name.clone(),
                    fields,
                }
            }
            _ => return None,
        })
    }

    /// Flag a `@semantic` directive on a non-enum declaration (`E0031`): it marks enums role-eligible
    /// and has no meaning on a struct or class.
    fn check_misplaced_semantic(&mut self, semantic: Option<Span>, name: &str, kind: &str) {
        if let Some(span) = semantic {
            self.error(
                DiagnosticCode::InvalidRole,
                span,
                format!("`@semantic` may only mark an enum, not the {kind} `{name}`"),
            )
            .help("`@semantic` makes an enum's variants usable as `@role(Enum.Variant)`");
        }
    }

    /// Validate a struct's `@role(Enum.Variant)` tags. Each must name a **fieldless** variant of a
    /// `@semantic` enum, and may only tag a struct that is itself an attribute (`@attribute`) — the
    /// role rides on what the attribute attaches to. Multiple roles are allowed. Each violation is
    /// `E0031` at its span; `name_span` locates the declaration for the "not an attribute" case.
    fn check_role_tags(
        &mut self,
        name_span: Span,
        roles: Option<&[noeta_ast::RoleTag]>,
        is_attribute: bool,
    ) {
        let Some(roles) = roles else { return };
        if !is_attribute {
            self.error(
                DiagnosticCode::InvalidRole,
                name_span,
                "`@role(...)` may only tag an attribute".to_string(),
            )
            .help("also mark the record `@attribute`");
        }
        for tag in roles {
            // A bare `@role(Variant)` carries no enum; a role must name `Enum.Variant`.
            if tag.enum_name.is_empty() {
                self.error(
                    DiagnosticCode::InvalidRole,
                    tag.span,
                    format!(
                        "`@role` requires a qualified `Enum.Variant`, not `{}`",
                        tag.variant
                    ),
                )
                .help("name a variant of a `@semantic` enum, e.g. `@role(Semantic.EntryPoint)`");
                continue;
            }
            // The enum must be `@semantic` (the built-in `Semantic` always is).
            if !self.semantic_enums.contains(&tag.enum_name) {
                self.error(
                    DiagnosticCode::InvalidRole,
                    tag.span,
                    format!("`{}` is not a `@semantic` enum", tag.enum_name),
                )
                .help("mark the enum `@semantic` to use its variants as roles");
                continue;
            }
            // The variant must exist on that enum and be fieldless (a payload would have to be
            // built per use site — genuine comptime, the one thing roles defer).
            match self
                .enums
                .get(&tag.enum_name)
                .and_then(|vs| vs.iter().find(|v| v.name == tag.variant))
            {
                None => {
                    self.error(
                        DiagnosticCode::InvalidRole,
                        tag.span,
                        format!("`{}` has no variant `{}`", tag.enum_name, tag.variant),
                    );
                }
                Some(variant) if !variant.fields.is_empty() => {
                    self.error(
                        DiagnosticCode::InvalidRole,
                        tag.span,
                        format!(
                            "`{}.{}` carries fields, so it cannot be a role",
                            tag.enum_name, tag.variant
                        ),
                    )
                    .help("a role must be a fieldless (payload-free) variant");
                }
                Some(_) => {}
            }
        }
    }

    /// The return type of a method call `recv.name(...)`: a built-in method, a user-declared
    /// method, or — when the receiver defers to runtime (`dyn`/hole) — the deferred type itself.
    /// Type a method-bundle method call (kernel-methods K2): an `Element` method on a bound
    /// `@packed` type, or a `Bulk` method on a `List<T>` of one. On a hit: checks arity and
    /// argument types against the bundle's declared signature (nominal — the shape requirement
    /// was already verified at the impl site), records the call-site route for lowering, and
    /// returns the method's type under the receiver-at-0 convention. `None` = not a bundle
    /// method; the caller falls through to the ordinary paths.
    fn bundle_method_call(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        span: Span,
        call_span: Span,
    ) -> Option<Type> {
        use noeta_stdlib::BundleReceiver;
        let (type_name, receiver_kind) = match recv {
            Type::Named(n, targs) if targs.is_empty() => (n, BundleReceiver::Element),
            Type::List(elem) => match elem.as_ref() {
                Type::Named(n, targs) if targs.is_empty() => (n, BundleReceiver::Bulk),
                _ => return None,
            },
            _ => return None,
        };
        let bindings = self.bundle_impls.get(type_name)?;
        let (route, method) = bindings.iter().find_map(|b| {
            b.bundle
                .method(name)
                .filter(|m| m.receiver == receiver_kind)
                .map(|m| ((b.module.clone(), b.bundle.name.to_string()), m))
        })?;
        let params = stdlib::bundle_method_params(self.reg(), &method.sig, args);
        let required = noeta_stdlib::SigType::required_count(method.sig.params);
        self.check_args(&params, required, args, &[], span, name);
        self.sites.bundle_call_sites.insert(call_span, route);
        Some(stdlib::bundle_method_return(
            self.reg(),
            &method.sig,
            recv,
            args,
        ))
    }

    fn method_call_return(&self, recv: &Type, name: &str) -> Type {
        if let Some(t) = stdlib::method_return(self.reg(), recv, name) {
            return t;
        }
        if let Type::Named(n, _) = recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            return sig.ret.clone();
        }
        if recv.defers_to_runtime() {
            return recv.clone();
        }
        Type::Unknown
    }

    /// Whether field `field` of type `type_name` is accessible at the current checking context
    /// (object-model slice 2d): a public field always is; a private one (a `class` field not
    /// declared `pub`) only inside the declaring type's own methods/destructor ([`Self::current_type`]).
    fn field_visible(&self, type_name: &str, field: &str) -> bool {
        let private = self
            .private_fields
            .get(type_name)
            .is_some_and(|fs| fs.contains(field));
        // White-box for dev-tier (`@test`/…) fn bodies: co-located tooling sees its module's
        // privates (slice 6d), so a private field is visible there regardless of `current_type`.
        !private || self.in_dev_tier || self.current_type.as_deref() == Some(type_name)
    }

    /// Report an access to a private field from outside its type (E0035). `access` names the action
    /// for the message — a closed [`FieldAccess`] so a call site cannot invent a verb.
    fn report_private_field(
        &mut self,
        type_name: &str,
        field: &str,
        access: FieldAccess,
        span: Span,
    ) {
        let verb = access.verb();
        self.error(
            DiagnosticCode::PrivateField,
            span,
            format!("cannot {verb} private field `{field}` of `{type_name}` from outside it"),
        )
        .help(format!(
            "fields of a `class` are private by default; declare it `pub {field}: ...` to expose \
                 it, or go through a method"
        ));
    }

    /// The **root-qualified namespace prefix** an expression denotes, if it is a namespace group
    /// (`http` bound by `use std.http` → `"std.http"`) or a deeper namespace member chain
    /// (`http.v2` → `"std.http.v2"`). `None` for anything that is not a pure namespace path — a
    /// value, a concrete module, or a type. A local binding shadows a namespace of the same name.
    fn resolve_namespace_prefix(&self, expr: &Expr, env: &Env) -> Option<String> {
        use noeta_stdlib::registry::NsChild;
        match expr {
            Expr::Ident { name, .. } if lookup(env, name).is_none() => {
                self.namespaces.get(name).cloned()
            }
            Expr::Member { receiver, name, .. } => {
                let prefix = self.resolve_namespace_prefix(receiver, env)?;
                match self.reg().resolve_namespace_child(&prefix, name) {
                    NsChild::Namespace(sub) => Some(sub),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// If `expr` is a namespace-group member chain resolving to a concrete native **module**
    /// (`http.client` from `use std.http`), return its root-qualified identity (`std.http.client`)
    /// and record the `Member` span in `namespace_module_sites` so lowering emits an
    /// [`Rvalue::NativeModule`] carrying the leaf identity. `None` when the chain is not a namespace
    /// path or the final hop is not a module (a sub-namespace, a type, or unresolved).
    fn resolve_namespace_module(&mut self, expr: &Expr, env: &Env) -> Option<String> {
        use noeta_stdlib::registry::NsChild;
        let Expr::Member {
            receiver,
            name,
            span,
            ..
        } = expr
        else {
            return None;
        };
        let prefix = self.resolve_namespace_prefix(receiver, env)?;
        match self.reg().resolve_namespace_child(&prefix, name) {
            NsChild::Module(qm) => {
                self.sites.namespace_module_sites.insert(*span, qm.clone());
                Some(qm)
            }
            _ => None,
        }
    }

    /// Report an unresolved member on a namespace group (`http.nope`, whether read or called) — a
    /// bare-name miss (E0005). A group is fully enumerable, so an unknown member is never a forward
    /// reference; when a child name is a plausible typo we attach a "did you mean" hint. `prefix` is
    /// the group's root-qualified identity; the message names it as written in source (root stripped).
    fn namespace_member_error(&mut self, prefix: &str, name: &str, span: Span) {
        let group = prefix.split_once('.').map_or(prefix, |(_, rest)| rest);
        let candidates = self.reg().namespace_children(prefix);
        let suggestion = noeta_diagnostics::closest(name, candidates.iter().map(String::as_str))
            .map(str::to_string);
        let diag = self.error(
            DiagnosticCode::UnknownName,
            span,
            format!("namespace `{group}` has no member `{name}`"),
        );
        if let Some(s) = suggestion {
            diag.help(format!("did you mean `{s}`?"));
        }
    }

    fn synth_member(
        &mut self,
        receiver: &Expr,
        name: &str,
        name_span: Span,
        member_span: Span,
        env: &mut Env,
    ) -> Type {
        // `Type.Variant` (a nullary enum constructor like `Status.Paid`) reads as the enum type. For a
        // generic enum a payload-free variant pins no parameter, so its arguments infer to `dyn`
        // (R2b) — keeping the arity consistent with a payload variant of the same enum.
        if let Expr::Ident { name: tn, .. } = receiver
            && self.is_enum_variant(tn, name)
        {
            return self.enum_construction_type(tn, name, &[], member_span);
        }
        // `Type.method` in value position (not the callee of a call) is an unbound **method handle**:
        // a callable taking the receiver as its first argument (prelude-redesign MH). Guarded to a
        // bare type name not shadowed by a local, naming a method of a user type. Typed
        // `Fn(ReceiverType, ...method_params) -> ret`; the resolution is recorded so lowering emits an
        // `Rvalue::MethodHandle`. (Built-in-type receivers — `list.len` — land in a later slice.)
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn).is_none()
            && let Some(sig) = self.methods.get(&(tn.clone(), name.to_string()))
        {
            // The handle's shape follows the derived classification (EX.2): an INSTANCE method's
            // handle takes the receiver as its first argument (`Fn(T, ...params) -> ret`); an
            // ASSOCIATED function's handle is the function itself (`Fn(params) -> ret`) — e.g.
            // `ctor = Stack.new`.
            let instance = self
                .method_instance
                .get(&(tn.clone(), name.to_string()))
                .copied()
                .unwrap_or(true);
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            if instance {
                params.push(Type::Named(tn.clone(), Vec::new()));
            }
            params.extend(sig.params.iter().cloned());
            let ret = sig.ret.clone();
            self.sites
                .handle_sites
                .insert(member_span, (tn.clone(), name.to_string(), !instance));
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // The same for a **built-in** type receiver (`list.len`, `string.upper`): a bare built-in
        // type name (not shadowed) whose `name` is one of its built-in methods → an instance handle
        // `Fn(ReceiverType, ...method_params) -> ret` (prelude-redesign MH.2). Built-in types have no
        // associated fns, so a built-in handle is always instance.
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn).is_none()
            && let Some(recv_ty) = builtin_receiver_type(tn)
            && let Some(ret) = stdlib::method_return(self.reg(), &recv_ty, name)
        {
            let mut params = vec![recv_ty.clone()];
            params.extend(stdlib::method_params(self.reg(), &recv_ty, name).unwrap_or_default());
            self.sites
                .handle_sites
                .insert(member_span, (tn.clone(), name.to_string(), false));
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // A namespace-group member access (`http.client` from `use std.http`) in value position:
        // resolve one hop against the group prefix. A landing module records its span so lowering
        // materializes the leaf module value; a sub-namespace or extension type is a valid
        // intermediate. An unresolved member is a hard error (`http.nope`) — a group is fully
        // enumerable, so this is never a forward reference. The group handle is never a value on its
        // own, so this precedes the generic receiver synth below (which would treat `http` as an
        // unknown name).
        if let Some(prefix) = self.resolve_namespace_prefix(receiver, env) {
            use noeta_stdlib::registry::NsChild;
            match self.reg().resolve_namespace_child(&prefix, name) {
                NsChild::Module(qm) => {
                    self.sites.namespace_module_sites.insert(member_span, qm);
                }
                NsChild::None => {
                    self.namespace_member_error(&prefix, name, member_span);
                }
                // A sub-namespace or extension type reached as a value is not statically typed here
                // (associated calls resolve through the call path); no error.
                NsChild::Namespace(_) | NsChild::Type(_) => {}
            }
            return Type::Unknown;
        }
        let recv = self.synth(receiver, env);
        if let Type::Named(n, recv_args) = &recv
            && let Some(ty) = self
                .records
                .get(n)
                .and_then(|fields| fields.iter().find(|(fname, _)| fname == name))
                .map(|(_, ty)| ty.clone())
        {
            // A private field is readable only inside its declaring type's own methods (slice 2d).
            if !self.field_visible(n, name) {
                self.report_private_field(n, name, FieldAccess::Read, name_span);
            }
            // Fusable indexed field read: `list[i].field`, where the index receiver typed as a
            // built-in `List` (recorded in the `Expr::Index` arm) and the field resolved on the
            // element type `n`. Lowering reads `index_field_sites` to emit a single `Rvalue::IndexField`
            // (P-PACK 2.5+); restricting to a `List` receiver keeps the backends' fast path / boxed
            // fallback list-only (no map/string/`Index`-trait dispatch to reproduce).
            if let Expr::Index { span: idx_span, .. } = receiver
                && self.index_on_list.contains(idx_span)
            {
                self.sites.index_field_sites.insert(member_span);
            }
            // Substitute the class's type parameters from the receiver's type arguments, so a field
            // of a `Box<int>` reads as `int`. An unresolved parameter (the receiver's arguments are
            // unknown, e.g. from a literal) erases to `dyn` rather than leaking the parameter name.
            let params = self.generic_types.get(n).cloned().unwrap_or_default();
            let subst: HashMap<String, Type> = params
                .iter()
                .cloned()
                .zip(recv_args.iter().cloned())
                .collect();
            // Inside the generic type's OWN body (`self.value` in a method of `Box<T>`), `T` is in
            // scope and must stay `T` — erasing it to `dyn` would break `fn get(): T { return
            // self.value }` (prelude-redesign EX.1: this path now serves what the retired bare
            // field read did). Only parameters NOT in scope erase.
            let pset: HashSet<String> = params
                .into_iter()
                .filter(|p| !self.type_params.contains_key(p))
                .collect();
            return erase_type_params(apply_subst(&ty, &subst), &pset);
        }
        // `value.method` in value position — a **bound** method handle (EX.2b): the receiver is
        // captured at bind time; the handle is `Fn(params) -> ret` (no receiver parameter). Checked
        // AFTER the field path, so a same-named field keeps winning member access. Covers user
        // types (instance methods only — binding an associated fn through a value is the E0047
        // wrong-way shape) and built-in receivers (`xs.len`, `s.upper`).
        if let Type::Named(n, _) = &recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            let params = sig.params.clone();
            let ret = sig.ret.clone();
            let instance = self
                .method_instance
                .get(&(n.clone(), name.to_string()))
                .copied()
                .unwrap_or(true);
            // Binding an ASSOCIATED function through a value is the wrong-way shape (E0047) —
            // there is no receiver to capture; bind it off the type instead.
            if !instance {
                self.error(
                    DiagnosticCode::InvalidReceiver,
                    member_span,
                    format!("`{name}` is an associated function of `{n}`"),
                )
                .help(format!("bind it off the type: `{n}.{name}`"));
            } else {
                self.sites.bound_handle_sites.insert(member_span);
            }
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        if !matches!(recv, Type::Unknown | Type::Dyn)
            && let Some(ret) = stdlib::method_return(self.reg(), &recv, name)
        {
            let params = stdlib::method_params(self.reg(), &recv, name).unwrap_or_default();
            self.sites.bound_handle_sites.insert(member_span);
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
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
        // Flow-narrowing: an `is T` arm sees the scrutinee narrowed to `T`, but only when the
        // scrutinee is a bare identifier (there is then a name to re-type in the arm scope).
        let scrut_ident = match scrutinee {
            Expr::Ident { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let mut result = Type::Unknown;
        for arm in arms {
            env.push(HashMap::new());
            self.bind_pattern(&arm.pattern, &scrut, env);
            if let (Some(name), Pattern::IsType { ty, .. }) = (scrut_ident, &arm.pattern) {
                bind(env, name, from_ref_q(ty, &self.extern_types));
            }
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
        // A type-pattern match (`is T` arms): the domain is *types*, not variant names. A union is
        // a closed domain — exhaustive iff every member is covered by some `is` arm; `dyn` is the
        // open top — a finite set of `is` arms can never exhaust it, so it needs a `_`.
        let type_targets: Vec<Type> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::IsType { ty, .. } => Some(from_ref_q(ty, &self.extern_types)),
                _ => None,
            })
            .collect();
        if !type_targets.is_empty() {
            let missing: Vec<String> = match scrut {
                Type::Union(members) => members
                    .iter()
                    .filter(|m| !type_targets.iter().any(|t| Type::subtype(m, t)))
                    .map(|m| m.to_string())
                    .collect(),
                Type::Dyn => vec!["a `dyn` value (open type domain)".into()],
                // A concrete or gradual scrutinee with `is` arms is not exhaustiveness-checked.
                _ => return,
            };
            if !missing.is_empty() {
                self.error(
                    DiagnosticCode::NonExhaustiveMatch,
                    span,
                    format!("non-exhaustive `match`: missing {}", missing.join(", ")),
                )
                .help("add an `is T` arm for each missing type, or a `_` catch-all");
            }
            return;
        }
        let all: Vec<String> = match scrut {
            Type::Result(..) => vec!["Ok".into(), "Err".into()],
            Type::Option(..) => vec!["some".into(), "none".into()],
            Type::Named(n, _) => match self.enums.get(n) {
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
            self.error(
                DiagnosticCode::NonExhaustiveMatch,
                span,
                format!("non-exhaustive `match`: missing {}", missing.join(", ")),
            )
            .help("add an arm for each missing case, or a `_` catch-all");
        }
    }

    // ----- pattern binding -----

    fn bind_for_pattern(&mut self, pattern: &ForPattern, iter_ty: &Type, env: &mut Env) {
        // The element type a `for` loop binds: a list/set's element, a map's **value** (iteration
        // yields values, like the runtime), or an `Iterator<T>`'s element (Track I.2). Anything else
        // (a `dyn`/gradual source) binds a hole.
        let elem = match iter_ty {
            Type::List(t) | Type::Set(t) => (**t).clone(),
            Type::Map(_, v) => (**v).clone(),
            Type::Named(n, args) if n == stdlib::ITERATOR => {
                args.first().cloned().unwrap_or(Type::Unknown)
            }
            _ => Type::Unknown,
        };
        match pattern {
            ForPattern::Single { name, name_span } => {
                self.check_reserved_name(name, *name_span);
                bind(env, name, elem)
            }
            // `for (a, b, …) in …` destructures each iterated **tuple** element positionally
            // (object-model slice 4b — `.enumerate()` yields `(int, T)` tuples). Each name binds to
            // its element type when the element is a known tuple, else `dyn`.
            ForPattern::Tuple { names, .. } => {
                for (i, (name, _)) in names.iter().enumerate() {
                    let t = match &elem {
                        Type::Tuple(els) => els.get(i).cloned().unwrap_or(Type::Unknown),
                        _ => Type::Unknown,
                    };
                    bind(env, name, t);
                }
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, ty: &Type, env: &mut Env) {
        match pattern {
            Pattern::Wildcard { .. }
            | Pattern::Int { .. }
            | Pattern::Str { .. }
            | Pattern::Bool { .. }
            // `is T` binds no name here — `synth_match` narrows the scrutinee identifier instead.
            | Pattern::IsType { .. } => {}
            Pattern::Binding { name, span } => {
                // A bare `none` in pattern position is the Option-none CONSTRUCTOR pattern (it is
                // represented as a binding but matched by name), not a fresh binding — exempt it
                // from the reserved-name rule so `match o { some(v) => …, none => … }` stays legal.
                if name != "none" {
                    self.check_reserved_name(name, *span);
                }
                bind(env, name, ty.clone())
            }
            Pattern::Variant {
                variant, bindings, ..
            } => {
                let payloads = self.payload_types(ty, variant, bindings.len());
                for (sub, pty) in bindings.iter().zip(payloads) {
                    self.bind_pattern(sub, &pty, env);
                }
            }
            // A tuple pattern `(p, q, …)` binds each sub-pattern against the corresponding tuple
            // element type (object-model slice 4b); a non-tuple/gradual scrutinee binds `dyn`.
            Pattern::Tuple { elements, .. } => {
                for (i, sub) in elements.iter().enumerate() {
                    let pty = match ty {
                        Type::Tuple(els) => els.get(i).cloned().unwrap_or(Type::Unknown),
                        _ => Type::Unknown,
                    };
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
            // Substitute the enum's type arguments into the variant's declared payload types, so a
            // pattern on a generic enum binds the *instantiated* payload: `match t { Tree.Leaf(n) => … }`
            // where `t: Tree<int>` types `n` as `int`, not the abstract parameter `T`. Mirrors the
            // construction-side inference (R2b.1); the two are the same generic type-argument flow.
            Type::Named(n, args) => self
                .enums
                .get(n)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| {
                    let subst = self.type_arg_subst(n, args);
                    v.fields.iter().map(|t| apply_subst(t, &subst)).collect()
                })
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
/// The declaring package root of a link-qualified runner name (`fuzzkit` for
/// `fuzzkit.tiers.run_fuzz`; `""` for an entry-local name) — the provider identity a target's
/// `tiers` map selects. Mirrors `tiers::TierRegistry`'s collection.
fn decl_runner_root(qualified: &str) -> String {
    match qualified.rsplit_once('.') {
        Some((path, _)) => path.split('.').next().unwrap_or("").to_string(),
        None => String::new(),
    }
}

/// Map an extension attribute field's declared literal type onto the checker lattice.
fn attr_field_type(ty: noeta_stdlib::registry::AttrFieldType) -> Type {
    match ty {
        noeta_stdlib::registry::AttrFieldType::Int => Type::Int,
        noeta_stdlib::registry::AttrFieldType::Str => Type::String,
        noeta_stdlib::registry::AttrFieldType::Dyn => Type::Dyn,
    }
}

const PRELUDE_TYPES: &[&str] = &[
    "Ordering",
    "Type",
    "Semantic",
    "RoleBinding",
    // The roots-list element a declared tier's runner receives (tier-providers T2).
    "TierRoot",
    // The lazy-iterator type (Track I): a writable annotation now that `iter()`/adapters and
    // generator returns produce `Iterator<T>` values.
    "Iterator",
    // The async completion type (Track A): a writable annotation. Calling an `async fn f(): T`
    // produces a `Future<T>`; `expr.await` unwraps it back to `T`.
    "Future",
    // The channel endpoint types (isolates I.1): writable annotations. `channel::<T>(cap)` yields a
    // `(Sender<T>, Receiver<T>)`; `send`/`recv` dispatch on them.
    "Sender",
    "Receiver",
];

/// The type a **call** to an `async fn f(): T` produces: `Future<T>` (Track A). The body writes
/// `return t` (checked against the inner `T`), but a call site sees the wrapped future; `.await`
/// unwraps it again. A non-async function's return type is returned unchanged.
fn async_return(inner: Type, is_async: bool) -> Type {
    if is_async {
        Type::Named(stdlib::FUTURE.to_string(), vec![inner])
    } else {
        inner
    }
}

/// The built-in trait an operand of `op` must satisfy, for the trait-backed operators: arithmetic
/// (`+ - * /` → `Add`/`Sub`/`Mul`/`Div`) and ordering (`< <= > >=` → `Comparable`). `%` (no trait —
/// numerics only), `~`/`==`/`!=` (universal: display-concat / structural-equality fallbacks), and
/// the logical operators map to `None`, so the checker imposes no trait requirement on them.
/// The action named in an E0035 private-field diagnostic — a closed set so a call site cannot
/// invent a verb string.
#[derive(Debug, Clone, Copy)]
enum FieldAccess {
    Read,
    Assign,
    Set,
}

impl FieldAccess {
    fn verb(self) -> &'static str {
        match self {
            FieldAccess::Read => "read",
            FieldAccess::Assign => "assign",
            FieldAccess::Set => "set",
        }
    }
}

fn required_operator_trait(op: BinaryOp) -> Option<BuiltinTrait> {
    use BinaryOp::*;
    match op {
        Add => Some(BuiltinTrait::Add),
        Sub => Some(BuiltinTrait::Sub),
        Mul => Some(BuiltinTrait::Mul),
        Div => Some(BuiltinTrait::Div),
        Lt | Le | Gt | Ge => Some(BuiltinTrait::Comparable),
        _ => None,
    }
}

/// Replace each generic type parameter (a `Named` whose name is in `params`) with `dyn`, deeply.
/// Generic parameters are erased at runtime, so a method like `set(v: T)` accepts any argument —
/// erasing `T` to `dyn` keeps argument checking from a false positive against the erased name.
fn erase_type_params(ty: Type, params: &HashSet<String>) -> Type {
    let erase = |t: Type| erase_type_params(t, params);
    match ty {
        // A type parameter used directly (`T`) erases to `dyn`; a named type with arguments
        // (`Box<T>`) keeps its name but erases inside its arguments.
        Type::Named(n, _) if params.contains(&n) => Type::Dyn,
        Type::Named(n, args) => Type::Named(n, args.into_iter().map(erase).collect()),
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

/// Bind generic type parameters by structurally matching a (possibly un-erased) parameter type
/// `raw` against a concrete argument type `arg`, filling `subst`. Only **unbound** parameters are
/// filled (the first concrete argument that constrains a parameter wins); a deferred argument
/// (`dyn`/hole) never pins a parameter, so a later concrete argument can. Matching descends into
/// containers, options/results, and function arrows.
fn bind_type_params(
    raw: &Type,
    arg: &Type,
    params: &HashSet<String>,
    subst: &mut HashMap<String, Type>,
) {
    match (raw, arg) {
        // A deferred argument (`dyn`/hole) never pins a parameter, so a later concrete argument can.
        (Type::Named(n, _), _) if params.contains(n) && !arg.defers_to_runtime() => {
            subst.entry(n.clone()).or_insert_with(|| arg.clone());
        }
        // A named generic type (`Box<T>` matched against `Box<int>`): bind through the arguments.
        (Type::Named(rn, rargs), Type::Named(an, aargs)) if rn == an => {
            for (r, a) in rargs.iter().zip(aargs) {
                bind_type_params(r, a, params, subst);
            }
        }
        (Type::List(r), Type::List(a)) => bind_type_params(r, a, params, subst),
        (Type::Set(r), Type::Set(a)) => bind_type_params(r, a, params, subst),
        (Type::Option(r), Type::Option(a)) => bind_type_params(r, a, params, subst),
        (Type::Map(rk, rv), Type::Map(ak, av)) => {
            bind_type_params(rk, ak, params, subst);
            bind_type_params(rv, av, params, subst);
        }
        (Type::Result(rt, re), Type::Result(at, ae)) => {
            bind_type_params(rt, at, params, subst);
            bind_type_params(re, ae, params, subst);
        }
        (
            Type::Fn {
                params: rp,
                ret: rr,
            },
            Type::Fn {
                params: ap,
                ret: ar,
            },
        ) => {
            for (r, a) in rp.iter().zip(ap) {
                bind_type_params(r, a, params, subst);
            }
            bind_type_params(rr, ar, params, subst);
        }
        _ => {}
    }
}

/// Substitute every generic **type parameter** of a declared type with `dyn` — the conservative form
/// for destructor-relevance (a parameter could be instantiated with a destructor-bearing type, and the
/// runtime erases the argument). `dyn` is destruct-relevant, so a field mentioning a parameter (bare
/// or nested, `T` / `List<T>`) becomes relevant; a concrete field is unchanged. No-op for a
/// non-generic type (empty `params`).
fn params_to_dyn(ty: &Type, params: &[String]) -> Type {
    if params.is_empty() {
        return ty.clone();
    }
    let subst: HashMap<String, Type> = params.iter().map(|p| (p.clone(), Type::Dyn)).collect();
    apply_subst(ty, &subst)
}

/// Whether `ty` mentions one of `params` (bare `T` or nested, `List<T>`), deeply. Used by the
/// derive field constraint (E0050) to defer parameter-typed fields to the instantiation site.
fn mentions_param(ty: &Type, params: &[String]) -> bool {
    if params.is_empty() {
        return false;
    }
    match ty {
        Type::Named(n, args) => {
            params.iter().any(|p| p == n) || args.iter().any(|a| mentions_param(a, params))
        }
        Type::List(t) | Type::Set(t) | Type::Option(t) => mentions_param(t, params),
        Type::Map(k, v) | Type::Result(k, v) => {
            mentions_param(k, params) || mentions_param(v, params)
        }
        Type::Tuple(elems) | Type::Union(elems) => elems.iter().any(|e| mentions_param(e, params)),
        Type::Fn { params: ps, ret } => {
            ps.iter().any(|p| mentions_param(p, params)) || mentions_param(ret, params)
        }
        _ => false,
    }
}

/// Substitute resolved type parameters into a type, deeply. An unresolved parameter is left as its
/// `Named` form (the caller erases any residue to `dyn`).
fn apply_subst(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    match ty {
        // A type parameter (`T`) resolves to its binding; a named generic type (`Box<T>`)
        // substitutes inside its arguments.
        Type::Named(n, args) => match subst.get(n) {
            Some(t) => t.clone(),
            None => Type::Named(
                n.clone(),
                args.iter().map(|a| apply_subst(a, subst)).collect(),
            ),
        },
        Type::List(t) => Type::List(Box::new(apply_subst(t, subst))),
        Type::Set(t) => Type::Set(Box::new(apply_subst(t, subst))),
        Type::Map(k, v) => Type::Map(
            Box::new(apply_subst(k, subst)),
            Box::new(apply_subst(v, subst)),
        ),
        Type::Option(t) => Type::Option(Box::new(apply_subst(t, subst))),
        Type::Result(t, e) => Type::Result(
            Box::new(apply_subst(t, subst)),
            Box::new(apply_subst(e, subst)),
        ),
        Type::Fn { params, ret } => Type::Fn {
            params: params.iter().map(|p| apply_subst(p, subst)).collect(),
            ret: Box::new(apply_subst(ret, subst)),
        },
        other => other.clone(),
    }
}

/// The signed value of an **untyped** integer literal expression — `Int{v}` → `v`, `-Int{v}` →
/// `-v` — or `None` if it is not a plain (optionally negated) integer literal. Used to coerce an
/// untyped literal into a fixed-width context (Tier W). `i128` so no width's range overflows.
fn int_literal_value(expr: &Expr) -> Option<i128> {
    match expr {
        Expr::Int { value, .. } => Some(*value as i128),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => match operand.as_ref() {
            Expr::Int { value, .. } => Some(-(*value as i128)),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a **built-in** type satisfies a built-in trait — the static mirror of what the backends
/// actually dispatch. The scalars are ordered/equatable; both numerics are arithmetic; `string`
/// and `list` concatenate; almost everything displays. (User types satisfy traits only via an
/// explicit `@derive`/`impl`, handled in [`Checker::satisfies`].)
///
/// Fixed-width integers (Tier W) satisfy `Equatable`/`Display` here — equality and (small-value)
/// display are correct on the erased `int` word. Fixed-width arithmetic (`+ - *`, W2) and now
/// ordering/`/`/`%` (W3) are enabled: `+ - *` are sign-agnostic (masking the result suffices), while
/// `Div`/`Comparable` need the operand width+signedness, which lowering carries on the op
/// (`Rvalue::WideInt`) — so the erased op is never subtly wrong.
/// If `lt` and `rt` are the **same** fixed-width integer type, its `(signed, bits)`. Fixed-width
/// arithmetic (W2) and ordering (W3) both require identical operand types — no implicit widening —
/// so this gates them and yields the width lowering records for masking / the sign-aware op.
fn same_width_intn(lt: &Type, rt: &Type) -> Option<(bool, u8)> {
    match (lt, rt) {
        (
            Type::IntN {
                signed: s1,
                bits: b1,
            },
            Type::IntN {
                signed: s2,
                bits: b2,
            },
        ) if s1 == s2 && b1 == b2 => Some((*s1, *b1)),
        _ => None,
    }
}

fn builtin_satisfies(ty: &Type, t: BuiltinTrait) -> bool {
    use BuiltinTrait as Bt;
    use Type::*;
    match t {
        Bt::Comparable | Bt::Equatable => ty.is_arith_numeric() || matches!(ty, String | Bool),
        // Fixed-width `+ - *` are sign-agnostic (W2 — the low bits are the same read signed or
        // unsigned, so masking the result is correct); `Div` (and ordering) are sign-dependent and
        // land in W3 via the width-carrying `Rvalue::WideInt`. (`%` is numeric-only — no trait.)
        Bt::Add | Bt::Sub | Bt::Mul | Bt::Div => ty.is_arith_numeric(),
        Bt::Concat => matches!(ty, String | List(_)),
        Bt::Display => {
            ty.is_arith_numeric()
                || matches!(
                    ty,
                    String | Bool | Unit | List(_) | Map(..) | Set(_) | Option(_) | Result(..)
                )
        }
        // No built-in *primitive* type satisfies these marker/protocol traits without an explicit
        // `impl`. `Mergeable` in particular is satisfied only by the CRDT extern types, which are
        // `Type::Named` and so resolve through the seeded `trait_impls` table in `satisfies`, never
        // reaching here — no primitive is ever `Mergeable`.
        Bt::Clone
        | Bt::Serialize
        | Bt::Index
        | Bt::Length
        | Bt::Iterable
        | Bt::Callable
        | Bt::Members
        | Bt::DynamicCall
        | Bt::TryAdd
        | Bt::Mergeable => false,
    }
}

/// Unify a running element type with the next element's type, for synthesizing a list literal's
/// element type. Returns the unified type, or `None` if the two are concretely incompatible (a
/// heterogeneous list). A deferred type (hole / `dyn`) is compatible with anything; two numeric
/// types unify to `float` (the int/float promotion the runtime performs).
/// Join a block-bodied closure's collected `return` types into its inferred return type. If the
/// block does not definitely end in a value-`return` it can fall through to the end, which returns
/// `void`, so `void` is added to the join. Compatible types collapse via [`unify_element`] (the same
/// lattice join list literals use); genuinely distinct types form a closed union (e.g. a function
/// that returns `int` on one path and `string` on another is `int | string`); an empty set is `void`.
fn join_closure_returns(stmts: &[Stmt], mut types: Vec<Type>) -> Type {
    let falls_through = !matches!(stmts.last(), Some(Stmt::Return { value: Some(_), .. }));
    if falls_through {
        types.push(Type::Unit);
    }
    let Some((first, rest)) = types.split_first() else {
        return Type::Unit;
    };
    let mut acc = first.clone();
    for t in rest {
        match unify_element(&acc, t) {
            Some(joined) => acc = joined,
            // Incompatible return types form a closed union over all of them.
            None => return Type::union(types.clone()),
        }
    }
    acc
}

/// Whether a block of statements **definitely diverges** — every path through it returns from the
/// enclosing function, panics, or loops forever, so control cannot fall off the block's end. Drives
/// the non-`void` "must return a value" check (E0048). Conservative in the sound direction: any
/// construct not recognized as diverging is treated as *falling through*, so the analysis can only
/// ever *miss* a diverging path (a false negative), never invent one — it cannot reject a valid
/// function. A block diverges as soon as *one* of its statements does: everything after an
/// unconditional divergence is unreachable, so the block's end is too.
fn block_diverges(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_diverges)
}

/// Whether a single statement unconditionally transfers control away and never falls through to the
/// statement after it.
fn stmt_diverges(stmt: &Stmt) -> bool {
    match stmt {
        // `return` leaves the function. (`yield` does not — a generator resumes after it.)
        Stmt::Return { .. } => true,
        // An `if` diverges only with an `else` where *both* arms diverge; a missing or falling-through
        // arm reaches the end.
        Stmt::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => block_diverges(then_body) && block_diverges(else_body),
        // `while true { … }` with no `break` targeting this loop never exits normally.
        Stmt::While { cond, body, .. } => {
            matches!(cond, Expr::Bool { value: true, .. }) && !body_breaks(body)
        }
        // A structured-concurrency scope is a transparent block for control flow: a `return` inside it
        // still leaves the function.
        Stmt::Concurrent { body, .. } => block_diverges(body),
        // A bare `panic(...)` (or a `match` all of whose arms diverge) never returns.
        Stmt::Expr { expr, .. } => expr_diverges(expr),
        _ => false,
    }
}

/// Whether an expression in statement position unconditionally diverges: a `panic(...)` call, or a
/// `match` whose (non-empty) arms *all* diverge — an arm body is an expression, so it diverges only by
/// itself being a `panic`/all-diverging `match`, never by a `return` (a statement can't sit there).
fn expr_diverges(expr: &Expr) -> bool {
    match expr {
        Expr::Call { callee, .. } => {
            matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "panic")
        }
        Expr::Match { arms, .. } => !arms.is_empty() && arms.iter().all(|a| expr_diverges(&a.body)),
        _ => false,
    }
}

/// Whether a loop body contains a `break` that targets *this* loop — a `break` not nested inside an
/// inner `for`/`while` (which it would target instead). Distinguishes an infinite `while true` that
/// diverges from one that can exit.
fn body_breaks(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_breaks)
}

fn stmt_breaks(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break { .. } => true,
        // A `break` inside a nested loop targets *that* loop, not ours — do not descend.
        Stmt::For { .. } | Stmt::While { .. } => false,
        Stmt::If {
            then_body,
            else_body,
            ..
        } => body_breaks(then_body) || else_body.as_ref().is_some_and(|b| body_breaks(b)),
        Stmt::Concurrent { body, .. } => body_breaks(body),
        _ => false,
    }
}

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

/// Whether an expression is a **context-free polymorphic literal** — one whose type carries an
/// unconstrained hole that only context can fill: an empty list `[]`, an empty map `{}`, `none`,
/// or an `Ok(x)`/`Err(e)` constructor (one constructor fills only one `Result` slot, so the other
/// is always a hole). A non-empty list/map infers its elements and `some(x)` fully determines its
/// `Option`, so those are *not* uninferable. This is the syntactic trigger for `E0023` on an
/// immutable, un-annotated binding, so a hole inherited from an arbitrary call result is never
/// mistaken for one.
/// Whether a call argument is a **deferred literal** — a closure or a container literal whose type
/// is best driven top-down by the callee's parameter. Such arguments are placeheld as `Unknown` at
/// the call site and finalized once the signature is resolved (`finalize_closure_args` /
/// `check_generic_call`), with a standalone-synth safety net in `synth_call` for callees that never
/// resolve a matching parameter. This lets a heterogeneous map/list literal absorb an expected
/// `Map<K, V>` / `List<T>` (union or `dyn` value type) instead of being cross-unified.
fn is_deferred_literal_arg(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Closure { .. } | Expr::List { .. } | Expr::Map { .. }
    )
}

fn is_uninferable_literal(expr: &Expr) -> bool {
    match expr {
        Expr::List { items, .. } => items.is_empty(),
        Expr::Map { entries, .. } => entries.is_empty(),
        Expr::Ident { name, .. } => name == "none",
        // `Ok(x)`/`Err(e)` synthesize `Result<T, ?>` / `Result<?, E>` — the opposite slot is an
        // unfillable hole at the binding site (only context or an annotation supplies it).
        Expr::Call { callee, .. } => {
            matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Ok" || name == "Err")
        }
        _ => false,
    }
}

/// The child statement lists nested directly inside a statement — `if`/`for` bodies and a nested
/// function's body — for the recursive `mut`-refinement and reassignment walks. Class/impl method
/// bodies are included so a method-local `mut x = []` is covered too.
fn child_stmt_bodies(stmt: &Stmt) -> Vec<&[Stmt]> {
    match stmt {
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            let mut bodies = vec![then_body.as_slice()];
            if let Some(b) = else_body {
                bodies.push(b.as_slice());
            }
            bodies
        }
        Stmt::For { body, .. } => vec![body.as_slice()],
        Stmt::While { body, .. } => vec![body.as_slice()],
        Stmt::Fn(decl) => vec![decl.body.as_slice()],
        Stmt::Class(c) => c
            .methods
            .iter()
            .chain(c.impls.iter().flat_map(|b| b.methods.iter()))
            .map(|m| m.body.as_slice())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether any statement in `stmts` (or a nested `if`/`for`/`fn` body) reassigns `name` via a bare
/// `name = …` (an un-`mut` `Binding`). Distinguishes a never-refined `mut x = []` (undeterminable,
/// `E0023`) from an accumulator whose later write resolves its element type. Conservative: an inner
/// shadow's reassignment counts here, which can only *suppress* the diagnostic, never add one.
fn reassigns(stmts: &[Stmt], name: &str) -> bool {
    stmts.iter().any(|stmt| {
        matches!(stmt, Stmt::Binding { mut_decl: false, name: n, .. } if n == name)
            || child_stmt_bodies(stmt)
                .iter()
                .any(|body| reassigns(body, name))
    })
}

/// [`Type::from_ref`], but each name a `use std.<ns>.<Type> [as Alias]` import brought into scope is
/// rewritten to that extern type's **qualified identity** (`Uuid` → `std.id.Uuid`, an alias
/// `Metric` → `std.metrics.Counter`). `xt` is the importing scope's extern-import map
/// ([`Checker::extern_types`]). This is the single annotation-resolution entry point the checker
/// uses instead of the bare `Type::from_ref`, so an annotation (`x: Uuid`) and a registry-derived
/// return (`uuid()` → `Uuid`) agree on identity, and a native type is never conflated with a
/// same-short-named user type. A name absent from `xt` — a user type, a generic parameter, the
/// language-level `Future`/`Iterator`/…, or an un-imported (hence unknown) name — is left bare;
/// user-type precedence needs no check here because importing a name you also declare is an E0020
/// collision, so the two can never both be in scope.
fn from_ref_q(ty: &TypeRef, xt: &HashMap<String, String>) -> Type {
    qualify_externs(Type::from_ref(ty), xt)
}

/// Recursively rewrite imported extern-type names inside a [`Type`] to their qualified identity via
/// the import map `xt`. Idempotent: an already-qualified identity (`std.id.Uuid`) is not a local
/// import key, so it is left unchanged.
fn qualify_externs(t: Type, xt: &HashMap<String, String>) -> Type {
    let q = |t: Type| qualify_externs(t, xt);
    match t {
        Type::Named(n, args) => {
            let n = xt.get(&n).cloned().unwrap_or(n);
            Type::Named(n, args.into_iter().map(q).collect())
        }
        Type::List(e) => Type::List(Box::new(q(*e))),
        Type::Set(e) => Type::Set(Box::new(q(*e))),
        Type::Option(e) => Type::Option(Box::new(q(*e))),
        Type::Map(k, v) => Type::Map(Box::new(q(*k)), Box::new(q(*v))),
        Type::Result(t, e) => Type::Result(Box::new(q(*t)), Box::new(q(*e))),
        Type::Tuple(es) => Type::Tuple(es.into_iter().map(q).collect()),
        Type::Union(es) => Type::union(es.into_iter().map(q)),
        Type::Fn { params, ret } => Type::Fn {
            params: params.into_iter().map(q).collect(),
            ret: Box::new(q(*ret)),
        },
        other => other,
    }
}

/// The declared type of a field, or `Unknown` when unannotated.
fn field_type(ty: &Option<TypeRef>, xt: &HashMap<String, String>) -> Type {
    ty.as_ref()
        .map(|t| from_ref_q(t, xt))
        .unwrap_or(Type::Unknown)
}

/// The type of one enum-variant payload field (R2b). A **positional** payload (`Leaf(T)`, `V(int)`)
/// is parsed with its type as the `Param`'s *name* and no annotation, so its type is reconstructed
/// from the name; a **named** field (`Leaf(x: T)`) uses its annotation. Reconstructing from the name
/// routes through the same name→[`Type`] resolution `from_ref` uses, so `int` maps to [`Type::Int`]
/// and a type parameter `T` to `Type::Named("T", [])` (the form [`bind_type_params`] unifies).
fn variant_field_type(p: &Param, xt: &HashMap<String, String>) -> Type {
    match &p.ty {
        Some(tr) => from_ref_q(tr, xt),
        None => from_ref_q(
            &TypeRef::Named {
                name: p.name.clone(),
                args: Vec::new(),
                span: p.name_span,
            },
            xt,
        ),
    }
}

/// The receiver (`self`) type inside a method of `name` — `Named(name, <its own type params>)` — so
/// an explicit `self.field` resolves through [`Checker::synth_member`] to the field's declared type
/// (a concrete field keeps it precisely, e.g. `List<u64>`; a generic field erases to `dyn` via the
/// same parameter substitution as bare field access). Structs/classes bind this exactly as enums do.
/// Compare a `@packed` struct's resolved layout against a bundle's declared constraint
/// (kernel-methods K1) — the compile-time twin of the runtime `PackedView` check a raw-buffer
/// kernel performs. `None` = satisfied; `Some(message)` names exactly what disagrees.
fn constraint_mismatch(
    layout: &noeta_ast::reflect::PackedLayout,
    constraint: &noeta_stdlib::PackedConstraint,
) -> Option<String> {
    use noeta_ast::reflect::PackedKind;
    use noeta_stdlib::{ConstraintField, ConstraintLayout};
    fn render(fields: &[ConstraintField]) -> String {
        fields
            .iter()
            .map(|f| match f {
                ConstraintField::Int => "int",
                ConstraintField::Float => "float",
                ConstraintField::F32 => "f32",
                ConstraintField::Bool => "bool",
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
    let kinds: Option<Vec<ConstraintField>> = layout
        .fields
        .iter()
        .map(|f| match f.kind {
            PackedKind::Int => Some(ConstraintField::Int),
            PackedKind::Float => Some(ConstraintField::Float),
            PackedKind::F32 => Some(ConstraintField::F32),
            PackedKind::Bool => Some(ConstraintField::Bool),
            // Constraints cover primitive fields only (a bundle over nested packed structs is a
            // later, additive extension).
            PackedKind::Struct(_) => None,
        })
        .collect();
    let Some(kinds) = kinds else {
        return Some(
            "the bundle's constraint covers primitive fields only; the type has a nested packed \
             field"
                .to_string(),
        );
    };
    if kinds != constraint.fields {
        return Some(format!(
            "the bundle requires fields ({}), found ({})",
            render(constraint.fields),
            render(&kinds)
        ));
    }
    match constraint.layout {
        ConstraintLayout::Any => {}
        ConstraintLayout::Row if layout.column => {
            return Some(
                "the bundle requires row layout; the type is `@packed(layout: column)`".to_string(),
            );
        }
        ConstraintLayout::Column if !layout.column => {
            return Some(
                "the bundle requires column layout — mark the type `@packed(layout: column)`"
                    .to_string(),
            );
        }
        _ => {}
    }
    None
}

fn self_type(name: &str, type_params: &[TypeParam]) -> Type {
    Type::Named(
        name.to_string(),
        type_params
            .iter()
            .map(|p| Type::Named(p.name.clone(), vec![]))
            .collect(),
    )
}

/// The declared type of a parameter, or `Unknown` when unannotated.
fn param_type(p: &Param, xt: &HashMap<String, String>) -> Type {
    p.ty.as_ref()
        .map(|t| from_ref_q(t, xt))
        .unwrap_or(Type::Unknown)
}

/// The number of *required* parameters: the leading run with no default value. With defaults
/// enforced trailing-only (`E0026`), this is the index of the first defaulted parameter (or the
/// full length when none have defaults). A call must supply at least this many arguments.
fn required_params(params: &[Param]) -> usize {
    params
        .iter()
        .position(|p| p.default.is_some())
        .unwrap_or(params.len())
}

#[cfg(test)]
mod tests;
