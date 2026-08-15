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
//!   `Result` nor an `Option`. Same code for the **absence-position** rule: `?` on an `Option`
//!   early-returns `none`, so the enclosing function must be declared to return one — the twin of
//!   the `Result` error-position rule (`E0057`).
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

use std::collections::{BTreeSet, HashMap, HashSet};

use noeta_ast::{
    AttrValue, Attribute, BinaryOp, BuiltinTy, ClassDecl, DeriveSpec, EnumDecl, Expr, FieldDecl,
    FnDecl, ForPattern, ImplBlock, ImplDecl, MatchArm, MethodDirective, Param, Pattern, Program,
    Stmt, StrPart, StructDecl, TypeParam, TypeRef, UnaryOp,
};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
pub use noeta_edition::Provenance;
use noeta_edition::{Edition, EditionMap};
use noeta_ext_abi::NominalType;
use noeta_span::{PackageOrigin, Span};
use noeta_types::{BuiltinTrait, ParamId, ParamRef, Type};

mod args;
mod attributes;
mod collect;
mod constructors;
mod decls;
pub mod directives;
mod effects;
mod env;
mod expr;
mod forwarding;
mod packed;
mod prelude;
mod relevance;
pub mod setup;
mod sites;
mod stdlib;
mod subst;
pub mod tiers;
mod traits;

pub use setup::{SetupDrop, SetupWarning, dropped_setup_warnings, is_tier_setup, setup_drop};
pub use tiers::{
    Activated, DeclaredTier, DocTarget, ResolvedProvider, ResolvedTier, TextBlock, TierFn, TierId,
    activate_tiers, code_tiers_in, dedent_doc, resolve_docs, resolve_texts,
};

use collect::TraitSuppliedMethod;
use constructors::compute_fresh_constructors;
use effects::*;
/// The derived receiver discipline, published because it is a fact about a *declaration* that the
/// source never spells — the IDE renders it as an inlay hint ([`Checked::method_receivers`]). It is
/// re-exported rather than mirrored so there is exactly one enum, derived in exactly one place: a
/// second copy of "does the body mention `self`" in the IDE crate would drift the first time the
/// rule gains a case (an explicitly declared self-less method, say).
pub use env::Receiver;
use env::*;
use forwarding::*;
use sites::SiteMaps;
pub use sites::{DestructorRelevance, Sites, intern_type_arg_entry};
use subst::*;

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
    /// The spans of **statement-expressions that do not return** — every `Stmt::Expr` whose
    /// expression's inferred type is [`noeta_types::Type::Never`] (`os.exit(1)`,
    /// `server.serve(8080, fetch)`).
    ///
    /// This is what makes divergence a *question the language can answer* rather than one a tool
    /// has to guess. The tier runners (`noeta test` / `noeta bench`, and the MCP execute path)
    /// build each case as `<shared setup> + <call the test fn>`, and they must not put a statement
    /// that exits the process or blocks forever into that setup. They used to decide by statement
    /// **form** — dropping every `Stmt::Expr` — which also dropped `conn.migrate("…")` and handed
    /// every test a live, empty database with no diagnostic anywhere. Now they ask this set.
    ///
    /// Recorded on **every** check, not just the IDE path: it is one insert on a statement that
    /// diverges, and the whole point is that a runner can rely on it being there.
    ///
    /// Deliberately statement-level rather than expression-level. A diverging call nested inside a
    /// larger expression (`x = pick(a, os.exit(1))`) is not recorded, because a *statement* is the
    /// granularity the runners include or exclude — recording sub-expressions would invite a
    /// consumer to conclude something about a statement it cannot conclude.
    pub diverging_stmts: HashSet<Span>,
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
    /// Every user method's derived [`Receiver`] discipline, keyed by the method's **name span** —
    /// one entry per declaration. Receiver-ness is never written in source (it is derived from the
    /// body plus the trait context, EX.2), so this is the only channel by which a reader can be
    /// told which call form a method admits; the IDE renders it as an inlay hint at the declaration.
    ///
    /// An IDE read-side index like [`Checked::packed_layouts`], and populated on every run for the
    /// same reason: it is one hash insert per method declaration, and a consumer must be able to
    /// rely on it being there.
    pub method_receivers: HashMap<Span, Receiver>,
}

/// Everything that varies a whole-program check, so callers configure one entry point
/// ([`check_all_with`]) instead of the checker growing a `_with_types_and_editions_and_registry`
/// combinatorial family.
///
/// `#[non_exhaustive]` and the absence of `Default` are load-bearing, not ceremony: outside this
/// crate the only way to make one is a constructor, so provenance arrives as an *argument* and a
/// future field cannot be silently defaulted at a dozen call sites the way `package_uses` was.
/// `Clone` so a driver can carry one configured value alongside the program it describes — the CLI's
/// tier verbs re-check *synthesized* programs (a per-test case, a bench loop) against the parent
/// workspace's options, and pairing the fields by hand at each of those call sites is exactly how
/// editions and provenance would drift apart.
#[derive(Clone)]
#[non_exhaustive]
pub struct CheckOptions {
    /// Record every expression's inferred type into [`Checked::expr_types`] — the span→type index the
    /// IDE hover path reads. Off on the compile path (it pays nothing for the index).
    pub record_expr_types: bool,
    /// A per-session extension [`Registry`] (instance-registry F2) to resolve native
    /// modules/functions/extern types/bundles against, instead of the process-global default. `None`
    /// routes every lookup through the default registry — identical to an ordinary check.
    pub registry: Option<&'static noeta_ext_abi::registry::Registry>,
    /// Where this program's sources came from — see [`Provenance`], which exists because these three
    /// facts were once three fields here and two of them kept getting left out.
    pub provenance: Provenance,
}

impl CheckOptions {
    /// A program with no package graph: a lone file, a synthetic program, a REPL entry. The
    /// compile-path default in every other respect — no type index, process-global registry.
    pub fn for_program() -> CheckOptions {
        CheckOptions {
            record_expr_types: false,
            registry: None,
            provenance: Provenance::unattributed(),
        }
    }

    /// A program whose sources carry per-source editions but no package graph.
    pub fn for_sources(editions: EditionMap) -> CheckOptions {
        CheckOptions::for_workspace(Provenance::of_sources(editions))
    }

    /// A linked workspace program, checked under the provenance the loader resolved for it. This is
    /// the constructor every whole-project surface wants: the CLI's `check`/`run`/tier verbs, the
    /// LSP's workspace diagnostics, MCP's tools, the conformance runner.
    pub fn for_workspace(provenance: Provenance) -> CheckOptions {
        CheckOptions {
            record_expr_types: false,
            registry: None,
            provenance,
        }
    }

    /// Also record every expression's inferred type into [`Checked::expr_types`] — the span→type
    /// index the IDE hover path reads.
    #[must_use]
    pub fn with_expr_types(mut self) -> CheckOptions {
        self.record_expr_types = true;
        self
    }

    /// Resolve native modules, functions, extern types and bundles against a per-session extension
    /// [`Registry`] (instance-registry F2) rather than the process-global default.
    #[must_use]
    pub fn with_registry(
        mut self,
        registry: &'static noeta_ext_abi::registry::Registry,
    ) -> CheckOptions {
        self.registry = Some(registry);
        self
    }
}

impl std::fmt::Debug for CheckOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckOptions")
            .field("record_expr_types", &self.record_expr_types)
            // The registry is a `&'static` handle whose contents aren't `Debug`; report only presence.
            .field("registry", &self.registry.is_some())
            .field("provenance", &self.provenance)
            .finish()
    }
}

/// Type-check a program once against explicit [`CheckOptions`] — the single configurable entry every
/// other `check_all*` is a thin preset of. Edition-, type-index-, and registry-aware in one call, so
/// a tool that has (say) both a per-package [`EditionMap`] and the IDE type index asks for both
/// without a bespoke entry point.
pub fn check_all_with(program: &Program, opts: CheckOptions) -> Checked {
    // The batch/tool entry never cancels: cancellation is a salsa-incremental concern, wired only by
    // [`check_all_cancellable`] (which the `checked` query calls with salsa's revision poll).
    check_all_impl(program, opts, &|| {})
}

/// Type-check a program once, returning both its diagnostics and its resolved-type map. This is
/// the single-pass entry point the hot paths (the CLI, the conformance/differential harnesses,
/// the `noeta-db` `bytecode` query) use so the checker runs exactly once per program.
pub fn check_all(program: &Program) -> Checked {
    check_all_with(program, CheckOptions::for_program())
}

/// Like [`check_all`], but additionally records every expression's inferred type into
/// [`Checked::expr_types`] — the span→type index the IDE hover feature reads. Diagnostics are
/// identical either way — recording types is a pure side-channel.
pub fn check_all_with_types(program: &Program) -> Checked {
    check_all_with(program, CheckOptions::for_program().with_expr_types())
}

/// [`check_all`] against the per-source [`EditionMap`] the loader built for a merged program, so the
/// checker can recover each declaration's own language [`Edition`] from its span (editions compiler
/// arc). Passing an empty map is identical to [`check_all`]. The whole-program compile/run and tool
/// paths call this with `Linked::editions`; today, with one edition, the result is byte-identical to
/// [`check_all`], but the per-package edition is now carried into the checker.
pub fn check_all_with_editions(program: &Program, editions: EditionMap) -> Checked {
    check_all_with(program, CheckOptions::for_sources(editions))
}

/// [`check_all`] against an explicit per-session extension [`Registry`] (instance-registry F2)
/// instead of the process-global default: the checker resolves every native module/function/extern
/// type/bundle against `registry`, so an embedding host that assembled its own extension set gets a
/// check that sees exactly those extensions — the same set its paired VM will run against. Passing
/// the process-global default here is identical to [`check_all`]. The registry is `&'static`
/// because a [`Registry`]'s lookups already return `&'static` data.
pub fn check_all_with_registry(
    program: &Program,
    registry: &'static noeta_ext_abi::registry::Registry,
) -> Checked {
    check_all_with(program, CheckOptions::for_program().with_registry(registry))
}

/// [`check_all_with_editions`], but polling `cancel` once per top-level declaration during body
/// checking (audit F9 residual b). The salsa `checked`/`linked_checked_*` queries pass salsa's own
/// revision-cancellation poll (`db.unwind_if_revision_cancelled()`) so a pending input write aborts
/// a long check of a large module promptly — mid-module — rather than only between queries (a
/// whole-program check is one salsa query). `cancel` signals cancellation by unwinding
/// (`salsa::Cancelled`), which the checker lets propagate. `record_expr_types` selects the IDE hover
/// index exactly as [`CheckOptions::record_expr_types`] does, so the ide-flavored linked query wires
/// the same poll. Every non-salsa caller uses the plain entries and never cancels.
pub fn check_all_cancellable(program: &Program, opts: CheckOptions, cancel: &dyn Fn()) -> Checked {
    check_all_impl(program, opts, cancel)
}

fn check_all_impl(program: &Program, opts: CheckOptions, cancel: &dyn Fn()) -> Checked {
    let mut checker = Checker::new(Config::of(opts, false));
    checker.register_prelude();
    checker.collect_imports(program);
    checker.collect(program);
    // The fresh-constructor pre-pass (generic constructor reflection) must precede body checking: a
    // `Repo.new(...)` call site stamps its instantiation onto the result whether it is written
    // before or after `Repo`'s declaration. It runs FIRST because the forwarding pre-pass below
    // consumes it: a fresh constructor in a checked position is one of the sites that forwards.
    checker.symbols.fresh_constructors = compute_fresh_constructors(program);
    // The type-param forwarding pre-pass (poly-values F2b) must precede body checking too, for the
    // same reason: a call site of a forwarding fn records hidden arguments, whether it appears
    // before or after the callee's declaration.
    let fwd = compute_forwarding(
        program,
        &checker.imports.extern_types,
        &checker.symbols.fresh_constructors,
    );
    checker.symbols.forwarding = fwd.map;
    checker.symbols.forwarding_poisoned = fwd.poisoned;
    checker.symbols.reflective_generic_types = fwd.reflective;
    // Compute destruct-reachability + parameter relevance before checking bodies (local-binding
    // relevance is recorded inline during `check_program`, and needs the reachable set ready).
    checker.compute_relevance(program);
    // Tier declarations FIRST: they build the tier registry, and the directive placement check
    // resolves a declaration's unrecognized `@name` against the whole name-space (which includes
    // the tiers). Running the placement check first resolved every foreign directive against an
    // empty registry, so an extension's directive read as unknown.
    checker.check_tier_decls(program);
    checker.check_semantic_roles(program);
    checker.check_program(program, cancel);
    checker.into_checked()
}

/// A `@packed` layout name→layout map projected into a deterministically-ordered vector for the
/// compiler's [`Sites::packed_type_layouts`] schema-availability channel (Slice E2). Sorted by type
/// name so the interned `packed_schemas` table — and thus the `.noeb` startup cache — is order-stable
/// regardless of the `HashMap`'s iteration order (the same determinism the `map_packed_sites` sort
/// buys).
fn sorted_packed_layouts(
    layouts: &HashMap<String, noeta_ast::reflect::PackedLayout>,
) -> Vec<noeta_ast::reflect::PackedLayout> {
    let mut out: Vec<noeta_ast::reflect::PackedLayout> = layouts.values().cloned().collect();
    out.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    out
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
    check_all_session_opts(program, CheckOptions::for_sources(editions))
}

/// [`check_all_session`] against explicit [`CheckOptions`] — the session-mode counterpart of
/// [`check_all_with`], so the session path can express everything the batch path can (editions,
/// per-session registry, the IDE type index) without a bespoke constructor per combination
/// (audit-3 finding 9).
pub fn check_all_session_opts(program: &Program, opts: CheckOptions) -> (Checked, SessionChecker) {
    // Session mode, and — since 2026-08-01 — the caller's *whole* provenance. This site used to
    // spell the fields out and stop after `editions`, so `Loaded::check_session` threaded the package
    // map and the `@name` tables in and the session dropped both: no orphan rule, and a spurious
    // `E0036` in the REPL and the debug console on any project that renames a directive. That is the
    // bug `Provenance` and `Config::of` exist to make unwriteable.
    let mut checker = Checker::new(Config::of(opts, true));
    checker.register_prelude();
    checker.collect_imports(program);
    checker.collect(program);
    checker.symbols.fresh_constructors = compute_fresh_constructors(program);
    let fwd = compute_forwarding(
        program,
        &checker.imports.extern_types,
        &checker.symbols.fresh_constructors,
    );
    checker.symbols.forwarding = fwd.map;
    checker.symbols.forwarding_poisoned = fwd.poisoned;
    checker.symbols.reflective_generic_types = fwd.reflective;
    checker.compute_relevance(program);
    // Tier declarations FIRST: they build the tier registry, and the directive placement check
    // resolves a declaration's unrecognized `@name` against the whole name-space (which includes
    // the tiers). Running the placement check first resolved every foreign directive against an
    // empty registry, so an extension's directive read as unknown.
    checker.check_tier_decls(program);
    checker.check_semantic_roles(program);
    let mut env: Env = vec![HashMap::new()];
    checker.check_program_in(program, &mut env);
    let checked = Checked {
        diagnostics: std::mem::take(&mut checker.diags),
        expr_types: checker.sites.expr_types.clone(),
        diverging_stmts: checker.sites.diverging_stmts.clone(),
        sites: {
            let mut sites = checker.sites.clone().into_sites(checker.relevance.clone());
            // Slice E2: seed the from-scratch producer's schema-availability channel (the
            // accumulator projection above cannot — the layouts read `symbols`).
            sites.packed_type_layouts = sorted_packed_layouts(&checker.packed_layouts_public());
            sites
        },
        bundle_bindings: checker.bundle_bindings_public(),
        packed_layouts: checker.packed_layouts_public(),
        method_receivers: checker.symbols.method_receiver_spans.clone(),
    };
    // The whole-program check above ran strict (file mode) — unknown names in a debugged program
    // are real errors. But the returned session, over which console fragments and later entries
    // are checked, defers unknown names (F1): a fragment may reference a name a *later* fragment
    // defines, and frame locals a fragment reads are seeded per-evaluation, not in this env.
    checker.config.session_mode = true;
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
        Self::with_options(CheckOptions::for_program())
    }

    /// A fresh session bound to an explicit per-session extension [`Registry`] (instance-registry
    /// F2): every native name the session's entries reference resolves against `registry` rather
    /// than the process-global default — the session-mode counterpart of [`check_all_with_registry`],
    /// so an embedding host's REPL/debug console sees exactly the host's extension set.
    pub fn with_registry(registry: &'static noeta_ext_abi::registry::Registry) -> SessionChecker {
        Self::with_options(CheckOptions::for_program().with_registry(registry))
    }

    /// A fresh session against explicit [`CheckOptions`] — the constructor the presets above are
    /// thin forms of, so a session can carry editions or the IDE type index without a bespoke
    /// constructor per combination (audit-3 finding 9).
    pub fn with_options(opts: CheckOptions) -> SessionChecker {
        let mut checker = Checker::new(Config::of(opts, true));
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
        // This entry's fresh constructors first — the forwarding pre-pass below consumes them (a
        // fresh constructor in a checked position is a forwarding site). Accumulated, like the
        // forwarding table.
        self.checker
            .symbols
            .fresh_constructors
            .extend(compute_fresh_constructors(entry));
        // A session entry's forwarding table extends the accumulated one (an entry may declare a
        // forwarding fn a later entry calls; cross-entry transitive forwarding is out of scope,
        // like any cross-entry forward reference).
        let fwd = compute_forwarding(
            entry,
            &self.checker.imports.extern_types,
            &self.checker.symbols.fresh_constructors,
        );
        self.checker.symbols.forwarding.extend(fwd.map);
        self.checker
            .symbols
            .forwarding_poisoned
            .extend(fwd.poisoned);
        self.checker
            .symbols
            .reflective_generic_types
            .extend(fwd.reflective);
        // Re-run the reachability fixpoint over the ACCUMULATED registries (this entry's
        // `destruct` class can make an earlier entry's type reachable), and record this entry's
        // parameter relevance.
        self.checker.compute_relevance(entry);
        // Tier declarations first — see the note in `check_program`.
        self.checker.check_tier_decls(entry);
        self.checker.check_semantic_roles(entry);
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
                attrs: Vec::new(),
                name: name.clone(),
                name_span: span,
                ty: None,
                default: None,
                span,
                positional: false,
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
        let mut sites = self
            .checker
            .sites
            .clone()
            .into_sites(self.checker.relevance.clone());
        // Slice E2: a checked session compile can also produce a `List<packed>` from scratch, so its
        // snapshot carries every `@packed` struct's layout for `make_packed`'s by-name resolution.
        sites.packed_type_layouts = sorted_packed_layouts(&self.checker.packed_layouts_public());
        sites
    }

    /// Reset the per-entry lexical scratch to its neutral state. The whole-program phases leave
    /// these neutral on a *clean* pass, but an entry that errored mid-body may not — and a session
    /// must isolate entries regardless.
    fn reset_scratch(&mut self) {
        self.checker.coloring.current_type = None;
        self.checker.coloring.dev_tier_at = None;
        self.checker.coloring.type_params = TypeScope::new();
        self.checker.coloring.current_ret = Type::Unknown;
        self.checker.coloring.collected_returns = None;
        self.checker.coloring.current_yield = None;
        self.checker.coloring.current_async = false;
        self.checker.coloring.concurrent_depth = 0;
        self.checker.coloring.loop_depth = 0;
        self.checker.coloring.current_forwarding.clear();
        self.checker.coloring.index_on_list.clear();
        // Span-keyed and entry-local: each entry is parsed with its own 0-based offsets, so a
        // stale entry's exhaustive `match` could otherwise be read as *this* entry's `match` at
        // the same offsets and wrongly satisfy E0048.
        self.checker.exhaustive_matches.clear();
    }
}

/// Type-check a program and return every diagnostic found, in source order. An empty result
/// means the program is well-typed (modulo the deliberate interior-hole tolerance documented
/// in the module docs). A thin projection of [`check_all`] for callers that need only the
/// diagnostics; the hot paths use [`check_all`] to avoid a second checker run.
pub fn check(program: &Program) -> Vec<Diagnostic> {
    check_all(program).diagnostics
}

/// Check `program` and report every body the checker **never entered** — the body-coverage ledger's
/// result as a value, for tests and tooling that want to assert the property rather than rely on the
/// `debug_assert` in [`Checker::verify_body_coverage`] firing.
///
/// Empty means every construct that owns a body was type-checked. A non-empty result is a defect in
/// the *checker*, not in `program`.
pub fn unchecked_bodies_for(program: &Program) -> Vec<noeta_ast::bodies::BodySite> {
    let mut checker = Checker::new(Config::of(CheckOptions::for_program(), false));
    checker.register_prelude();
    checker.collect_imports(program);
    checker.collect(program);
    let mut env: Env = vec![HashMap::new()];
    checker.check_program_in(program, &mut env);
    checker.unchecked_bodies(program)
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

pub(crate) fn type_to_repr_top(
    ty: &Type,
    kinds: &HashMap<String, noeta_types::TypeKind>,
) -> Option<noeta_ast::reflect::TypeRepr> {
    match ty {
        // An abstract kind-type (`Enum`/`Struct`/`Class`) has no precise static head — the runtime
        // value is a concrete enum/struct/class — so it defers to the runtime `type_of` path.
        //
        // A **trait object** defers for the same reason, and this arm is why. `type_to_repr` maps
        // `DynTrait` to `TypeRepr::Dyn`, which is right in a *nested/declared* position (`params_of`
        // has its own decoder and does report `Type.DynTrait(Trait)` there) but is a wrong answer at
        // a `type_of` site: the value behind a `dyn Trait` binding carries its own concrete tag, so
        // baking `Type.Dyn` erased a fact the runtime path recovers. `type_of(v)` on a `dyn Sp`
        // parameter reported `Type.Dyn` while the same value laundered through a bare `dyn` hop
        // reported `Type.Struct(D, [])` — one value, two answers, from one query.
        Type::Dyn | Type::DynTrait(_) | Type::Unknown | Type::Union(_) | Type::Kind(_) => None,
        concrete => Some(type_to_repr(concrete, kinds, true)),
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
    top: bool,
) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    // Every nested position is an element, where a fixed width is reified (packed-widths arc).
    let rec = |t: &Type| type_to_repr(t, kinds, false);
    match ty {
        // A trait object reflects as the dynamic top — the value carries its own concrete type.
        Type::DynTrait(_) => TypeRepr::Dyn,
        Type::Int => TypeRepr::Int,
        Type::Float => TypeRepr::Float,
        Type::F32 => TypeRepr::F32,
        // `f64` and the fixed-width integers keep their width in a *type-derived* reflection when they
        // sit in **element position** — a `List<f64>`/`List<i32>` construction tag — so those lists
        // are distinguishable from `List<float>`/`List<int>` (packed-widths arc). At the **top** they
        // erase to `Float`/`Int`, matching a scalar *value*, whose storage carries no width tag; so
        // `type_of` of a scalar reports `Float`/`Int`, and equal elements of the two list types stay
        // `==` (the width is storage/identity, never equality).
        Type::F64 if top => TypeRepr::Float,
        Type::IntN { .. } if top => TypeRepr::Int,
        Type::F64 => TypeRepr::F64,
        Type::IntN { signed, bits } => TypeRepr::IntN {
            signed: *signed,
            bits: *bits,
        },
        Type::Bool => TypeRepr::Bool,
        Type::String => TypeRepr::Str,
        Type::Bytes => TypeRepr::Bytes,
        Type::Unit => TypeRepr::Unit,
        Type::Never => TypeRepr::Never,
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
        // A still-un-instantiated type parameter projects as a bare nominal name — byte-identical
        // to what it produced when a parameter WAS a `Named`, so no reflection output moves. It
        // reaches here only from a template that the call site could not resolve; a resolved one
        // has already been substituted, and projects as whatever instantiated it.
        Type::Param(p) => TypeRepr::Named(p.name.clone(), Vec::new()),
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
    /// A callable's declared parameter. Lets an attribute declare itself parameter-only
    /// (`@attribute(Param)`), which is what per-argument metadata wants: `#[Arg(help: "…")]` is
    /// meaningless on a type, and saying so at the attribute's declaration is better than every
    /// consumer defending against it.
    Param,
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
            "Param" => TargetKind::Param,
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
            TargetKind::Param => "Param",
        }
    }
}

/// Map a native [`noeta_ext_abi::AttrTarget`] to the checker's [`TargetKind`] — the two are the same
/// closed placement vocabulary either side of the ABI (the ABI cannot name a syntax-crate type), so a
/// native `@attribute`'s placement restriction seeds the E0030 gate exactly as a `.noe` one's does.
fn attr_target_kind(t: noeta_ext_abi::AttrTarget) -> TargetKind {
    use noeta_ext_abi::AttrTarget;
    match t {
        AttrTarget::Struct => TargetKind::Struct,
        AttrTarget::Class => TargetKind::Class,
        AttrTarget::Enum => TargetKind::Enum,
        AttrTarget::Function => TargetKind::Function,
        AttrTarget::Method => TargetKind::Method,
        AttrTarget::Field => TargetKind::Field,
        AttrTarget::Variant => TargetKind::Variant,
        AttrTarget::Param => TargetKind::Param,
    }
}

/// The tier attachment site of a declaration `TargetKind` — the registry's site vocabulary. A
/// `struct`/`class`/`enum` all map to `Type`; a field or variant is never a tier site (`None`).
fn target_sites(target: TargetKind) -> noeta_ast::Sites {
    use noeta_ast::Sites;
    match target {
        TargetKind::Function => Sites::FN,
        TargetKind::Method => Sites::METHOD,
        TargetKind::Struct => Sites::STRUCT,
        TargetKind::Class => Sites::CLASS,
        TargetKind::Enum => Sites::ENUM,
        TargetKind::Field => Sites::FIELD,
        TargetKind::Variant => Sites::VARIANT,
        TargetKind::Param => Sites::PARAM,
    }
}

/// One kernel-trait binding: which native kernel **trait** a type acquired via
/// `impl <module>.<Trait> for T {}`, and through which module identity runtime dispatch routes.
///
/// **Surface adapter, NOT a second mechanism** (ExtBundle→ExtTrait fold-in, slice 4). The kernel
/// traits were `ExtBundle`s until the fold-in; `bundle_impls` (the `HashMap` of these) survives purely
/// as the **typing index** the checker's `bundle_method_call` reads to type a method on the bound `T`
/// (element receiver) and on `List<T>` (bulk receiver — the accepted `List<Self>` asymmetry a general
/// trait has no notion of). Everything else — the trait's identity, `self_constraint`, native-derived
/// `assoc_types`, and default-body `dispatch` — is the one `ExtTrait`, checked and dispatched through
/// the ordinary trait machinery; a binding is *also* recorded in `user_trait_impls` so bounds,
/// coherence, and associated-type resolution unify. Merging this index into `user_trait_impls` would
/// force the `List<Self>` receiver logic into the general trait path, i.e. a genuine second mechanism.
#[derive(Clone)]
struct BoundBundle {
    /// The owning module's root-qualified identity (`"std.vec"`) — the trait's `namespace`.
    module: String,
    bundle: &'static noeta_ext_abi::ExtTrait,
    /// The binding's trait span — conflict reporting orders by it (the textually-later binding
    /// carries the diagnostic).
    span: Span,
}

/// Two implemented traits contending for one method slot on one type (see
/// [`Symbols::trait_default_conflicts`]): both declare `method` **with a default body**, and the
/// type provides no method of that name to override them with. Found in pass 1 — where "provided
/// vs inherited" is decidable — and reported in pass 2, where the binding sites are known.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TraitDefaultConflict {
    /// The implementing type.
    type_name: String,
    /// The contested method name.
    method: String,
    /// The two traits that each supply a default for it. Which one is "first" is decided at report
    /// time by the *source* position of the two bindings, never by the order they were found in.
    traits: (String, String),
}

/// Where a private member was declared — `Some(span)` for a `.noe` declaration, `None` for one a
/// native extension registered, which has no source to point at. Distinct from the outer `Option`
/// the lookups return: that one answers *whether* the member is private at all.
pub(crate) type DeclSite = Option<Span>;

/// The checker's **symbol tables** — everything `collect` (pass 0/1) registers about the
/// program's declarations, read by every later pass. One of [`Checker`]'s four field groups
/// (audit-3 Finding 2): grouping makes each module's borrow surface explicit.
#[derive(Clone, Default)]
struct Symbols {
    /// **Per-source** static names a binding in that source may not shadow (E0059), each with the
    /// word the diagnostic uses for it: this file's own top-level functions and types, and the names
    /// its own `use` statements bind.
    ///
    /// Keyed by [`SourceId`] because the merged program flattens every module of every package into
    /// one table, and one flat table cannot answer this question. A package author cannot know a
    /// consumer's `fn page`, so `document(title, page)` in para/html must not be reported against it
    /// — and equally, a name declared in *this* file is one the author does know, so shadowing it
    /// must be reported. The two are told apart by the file the declaration came from, which is
    /// exactly what a span carries.
    ///
    /// The predecessor of this index inferred "merged from elsewhere" from a *dotted* declaration
    /// name, on the reasoning that the entry's own declarations keep bare ones. Deriving module
    /// paths from the filesystem made every declaration in every package dotted, which silently
    /// turned the statics half of E0059 off for all packaged code: a top-level `fn total` and a
    /// later `total = 5` then coexisted, with the binding swallowed and no diagnostic anywhere.
    source_statics: HashMap<noeta_span::SourceId, HashMap<String, &'static str>>,
    /// User-declared enums: name → variants (each with its **accurate** payload types, like a
    /// struct's fields in [`Checker::records`]).
    enums: HashMap<String, Vec<VariantInfo>>,
    /// Top-level functions: name → signature.
    functions: HashMap<String, FnSig>,
    /// Records/classes: name → declared fields (name, type).
    records: HashMap<String, Vec<(String, Type)>>,
    /// Declared type → its generic parameters **with their bounds**.
    ///
    /// [`Self::generic_types`] keeps the parameters as bare [`ParamRef`]s, which is all erasure and
    /// substitution need. Type-
    /// checking a standalone `impl`'s method bodies needs the bounds too — a body may call a method
    /// the bound provides (`<T: Comparable>` → `t.compare(u)`) — so that a body written outside its
    /// type's own declaration sees exactly what one written inside it sees.
    type_params: HashMap<String, Vec<TypeParam>>,
    /// Class name → the set of its fields declared `mut`. Drives the `x.f = v` field-assignment
    /// check (Phase 5.2): only a `mut` field may be assigned in place (else E0033). Records never
    /// have `mut` fields, so they never appear here.
    mut_fields: HashMap<String, HashSet<String>>,
    /// Type name → the set of its **private** fields (object-model slice 2d). A value `struct`'s
    /// fields are always public (it never appears here); a reference `class`'s fields default
    /// private, so this holds every field *not* declared `pub`. A private field is visible only
    /// inside the declaring type's own methods ([`Checker::current_type`]); read/write/construction
    /// elsewhere is E0035.
    /// The value is each private field's **declaration site**, which the diagnostic points a second
    /// label at and the white-box rule reads the declaring package from — see
    /// [`Checker::white_box_over`].
    private_fields: HashMap<String, HashMap<String, DeclSite>>,
    /// Type name → the set of its **private** methods (method-visibility arc). A method is private
    /// unless declared `pub`, in EVERY type kind — `struct`, `class` and `enum` alike, which is the
    /// one place this table differs from [`Self::private_fields`].
    ///
    /// Fields and methods answer different questions, so they do not have to share a default. A
    /// `struct`'s fields are public because a value *is* its contents: structural `==` already
    /// compares them field-wise and copy-on-write leaves no shared invariant to protect, so hiding
    /// them would be incoherent. API surface is orthogonal — a `Point` whose `x`/`y` are visible
    /// still benefits from an internal helper. Per-kind method visibility was considered and
    /// rejected: it would mean a struct could never have a private helper, which is exactly the gap
    /// this arc closes, merely narrowed to one kind.
    ///
    /// A method supplied by a `trait` — an `impl` block's own method, a hoisted default, a derive's
    /// bridge — never appears here: a trait is an outward contract, so implementing one puts the
    /// method on the type's public surface by construction. The `impl` must still *write* `pub`
    /// (E0015); this table records the access rule, not the declaration rule.
    /// The value is each private method's **declaration site**, on the same reasoning as
    /// [`Self::private_fields`]: a visibility error is about two places, and the one the programmer
    /// has to edit is the declaration.
    private_methods: HashMap<String, HashMap<String, DeclSite>>,
    /// Every top-level value binding's name, collected in the pre-pass (F1). Top-level globals are
    /// **hoisted** — a function body may reference one declared textually later — so the
    /// unknown-name gate treats them all as known regardless of order. (A top-level *direct*
    /// reference to a not-yet-bound global still fails at runtime; this gate does not try to catch
    /// that ordering case, only genuine typos.)
    global_binding_names: HashSet<String>,
    /// Every **nested** `fn` declaration's name, hoisted program-wide (collect pass 1). A nested
    /// fn's name is an ITEM of its enclosing body — recursion and sibling calls must resolve even
    /// inside a SEALED body (where value bindings need `use (…)` capture, but declarations do
    /// not). Coarse like [`Self::global_binding_names`]: an out-of-scope reference defers to the
    /// runtime error rather than the unknown-name gate.
    nested_fn_names: HashSet<String>,
    /// Declared type → its kind (`Enum`/`Struct`/`Class`). Drives the abstract kind-type
    /// membership rule (`Named(n) <: Enum` iff `n` is an enum) — the registry-dependent piece the
    /// pure lattice cannot decide, consulted by [`Checker::assignable`].
    type_kinds: HashMap<String, noeta_types::TypeKind>,
    /// User-defined methods: (type name, method name) → signature. Populated from class methods and
    /// `impl`-block methods so a method call on a user object resolves to a real type, with the
    /// owning class's generic parameters erased to `dyn` (they accept any argument).
    methods: HashMap<(String, String), FnSig>,
    /// How each `(type, method)` may be reached — instance-only, associated-only, or either
    /// ([`Receiver`]). DERIVED at collection time from the body *and* from whether a trait's
    /// interface supplies the method (prelude-redesign EX.2; well-defined because member access is
    /// explicit, EX.1). Drives the wrong-way-call check (E0047) and the shape of a `Type.method`
    /// handle. Read it through [`Checker::receiver_of`], never directly: an unclassified method has
    /// a *meaning* (`Either`), and re-deciding that meaning at each use site is exactly how the
    /// associated-call, instance-call, and turbofish paths drifted apart.
    method_receiver: HashMap<(String, String), Receiver>,
    /// The same classification keyed by the method's **name span** — one entry per *declaration*,
    /// where [`Symbols::method_receiver`] keeps one entry per callable `(type, method)` name and so
    /// keeps only the winner when an `impl` block shadows an inherent method of the same name.
    /// Written at the same two sites, from the same value, by [`Collector::record_receiver`].
    ///
    /// Exists for the IDE's receiver inlay hint, which anchors at a declaration and therefore asks
    /// by position, not by name: the name key would force the IDE to re-derive a method's owning
    /// type name (already a qualified identity by link time) and would answer nothing at all for the
    /// shadowed declaration it is drawing on screen.
    method_receiver_spans: HashMap<Span, Receiver>,
    /// Every method a **trait's interface** supplied, in registration order, with the trait that
    /// supplied it — the worklist [`Collector::narrow_declared_static`] replays once the trait table
    /// is complete, to downgrade a **declared-`static`** one from [`Receiver::Either`] to
    /// [`Receiver::Static`].
    ///
    /// It is a recorded worklist rather than a lookup at the classification site because the two
    /// facts become available at different times: a method is classified inside the SAME walk that
    /// registers `Stmt::Trait`, so a type written above its trait would read a half-populated table
    /// and classify differently from one written below it. Order-dependent classification is not a
    /// thing a language may have; deferring the *decision* (not the *observation*) to a pass over a
    /// complete table is what removes it.
    trait_supplied_methods: Vec<TraitSuppliedMethod>,
    /// Which built-in traits each user type satisfies: type name → set of trait names it `@derive`s
    /// or `impl`s. The basis (with the built-in-type table in [`Checker::satisfies`]) for enforcing a
    /// generic call's trait bounds (S4.2).
    trait_impls: HashMap<String, HashSet<BuiltinTrait>>,
    /// User-defined traits (L1 user traits), by name → its declaration. Populated in pass 1
    /// (`collect`) so forward references resolve. The basis for `impl` conformance (UT2),
    /// `<T: UserTrait>` generic bounds (UT3), and `dyn UserTrait` trait-object dispatch (UT4). A name
    /// here is a legal trait in an `impl`/bound alongside the closed [`BuiltinTrait`] set.
    user_traits: HashMap<String, noeta_ast::TraitDecl>,
    /// Which user traits each type implements: type name → user-trait name → the instantiation's
    /// type arguments (`impl Keyed<int> for Door` → `{"Door": {"Keyed": [int]}}`; empty args for
    /// a non-generic trait). From in-body/standalone `impl`s and `@derive`s. The user-trait
    /// analogue of [`Self::trait_impls`]; the basis for UT3 generic-bound satisfaction (including
    /// an instantiated bound `T: Keyed<int>`) and UT4 `dyn Trait` coercion. Populated in pass 1;
    /// coherence (one impl per trait per type) keeps a single entry per pair honest.
    user_trait_impls: HashMap<String, HashMap<String, Vec<Type>>>,
    /// Trait default-body call routes (ExtBundle→ExtTrait convergence, slice 2): `(type, method)` →
    /// `(trait-qualified, trait-short)` for every method that a **native** trait carrying a
    /// default-body dispatch ([`noeta_ext_abi::ExtTrait::dispatch`]) answers on `type` — i.e. a
    /// defaulted method the type neither declares (native inherent) nor overrides (impl body). Computed
    /// at collect time from the AST impl bodies (where "provided vs omitted" is decidable), so the
    /// call site is a pure lookup: a hit records a `trait_call_sites` route (source 2) and types the
    /// call from the trait's synthesized decl. Excludes overrides and inherent methods by construction
    /// (source 1 wins), and never holds a `.noe` trait (its default hoists — source 3).
    native_trait_default_sites: HashMap<(String, String), (String, String)>,
    /// Where each `(type, trait)` implementation was **written** — the trait path's span in the
    /// `@derive(T)` argument, the in-body `impl T { }` header, or the standalone `impl T for Type`
    /// header. Populated in pass 1 alongside [`Self::user_trait_impls`] (the same walk, the same
    /// three spellings), so a whole-program diagnostic about *two* implementations can point at
    /// both of them without re-walking the AST — the two-label discipline `check_coherence`
    /// established. A binding with no source site (a native type advertising a trait through its
    /// registry declaration) records nothing and is therefore never blamed.
    trait_impl_sites: HashMap<(String, String), Span>,
    /// **Ambiguous inherited defaults** found in pass 1: `(type, method)` that two implemented
    /// traits each supply a *default body* for, while the type itself provides no method of that
    /// name. Recorded rather than resolved — the winner used to be whichever trait a `HashMap`
    /// iteration reached first, which made the same source file check green in one process and red
    /// in the next (the method typed from a different trait's signature each run) while the
    /// backends' hoist always took the textually-first one. Reported as E0027 by
    /// [`Checker::report_trait_default_conflicts`], on the same ground the bundle path already
    /// reports it ("binding `X` would make the name ambiguous", `check_bundle_binding`) and the
    /// same ground coherence rejects a duplicate impl: a method table has one slot per name, and a
    /// contest for it is answered by the programmer, not by the compiler.
    trait_default_conflicts: Vec<TraitDefaultConflict>,
    /// Associated-type bindings per implementor (ExtBundle→ExtTrait convergence, slice 1a):
    /// `(type, trait)` → `assoc-name → concrete Type`. Populated at collect from each impl's
    /// `type Name = Concrete;` bindings merged over the trait's defaulted associated types. The basis
    /// for resolving a `Self::Name` projection ([`TypeRef::AssocProjection`]) in a trait/impl method
    /// signature to the implementor's concrete type — the trait analogue of a bundle's element
    /// resolution. An associated type never enters the [`Type`] lattice; it is projected at each
    /// typing site (or degrades to `Type::Unknown` under `dyn`, where the impl is unknown).
    trait_assoc: HashMap<(String, String), HashMap<String, Type>>,
    /// Native traits' **structural `Self`-constraints** (ExtBundle→ExtTrait convergence, slice 3):
    /// local trait name → the [`noeta_ext_abi::PackedConstraint`] its [`noeta_ext_abi::ExtTrait::self_constraint`]
    /// declares. Populated by [`Checker::seed_ext_traits`] **only when the native trait actually won
    /// the `user_traits` slot** (a same-named user `trait` collected first shadows it — and carries no
    /// such constraint), so the lookup respects shadow ordering. Read at the user `impl` site
    /// ([`Checker::check_user_trait_impl`]): when a native trait carries one, the implementing type
    /// must be a `@packed` struct matching it (the shared [`Checker::check_packed_self_constraint`],
    /// E0015) — the same shape check `check_bundle_binding` runs for a bundle. Empty for a `.noe` trait
    /// or a native trait with no constraint.
    native_trait_self_constraints: HashMap<String, noeta_ext_abi::PackedConstraint>,
    /// A native trait's **native-derived** associated types (slice 1b) — its local name → the
    /// `(assoc name, derivation)` pairs. Populated by [`Checker::seed_ext_traits`] only when the native
    /// trait won the `user_traits` slot. Read at the user `impl` site
    /// ([`Checker::check_user_trait_impl`]): these are **auto-supplied** — a `impl <trait> for T {}`
    /// must not be required to bind them (they are computed from `T`'s element, not written per-impl),
    /// so the coherence "must bind non-default assoc" check skips them and the impl site folds each
    /// derivation over `T`'s `@packed` element into `trait_assoc[(T, trait)]` so `Self::Name` resolves.
    /// Empty for a `.noe` trait or a native trait with no associated types (ExtBundle→ExtTrait fold-in).
    native_derived_assoc: HashMap<String, Vec<(String, noeta_ext_abi::AssocDerivation)>>,
    /// Declared `From` conversions (error-ergonomics): target type name → one entry per in-body
    /// `impl From<Source>` block, resolved at collection so a `?` site can consult them regardless
    /// of statement order.
    ///
    /// A type may declare **one conversion per distinct source**; coherence keys `From` on the
    /// source it names rather than on the trait name, so a repeated source is E0027 and the sources
    /// recorded here are distinct in a well-formed program. A `(source → target)` lookup is
    /// therefore unambiguous by construction, and it yields the method-table key
    /// ([`noeta_ast::conversion::from_conversion_keys`]) the call site must dispatch through.
    from_impls: HashMap<String, Vec<FromConversion>>,
    /// The **method-table key** each `impl From<Source>` block's `from` occupies, keyed by that
    /// method's name span ([`noeta_ast::conversion::from_conversion_keys`]). Written by
    /// [`Checker::record_from_impls`] before the owning type's methods are walked, and read by the
    /// one funnel every method registration passes through
    /// ([`Checker::collect_method_sig_classified`]) — so a conversion is registered under the key
    /// its call sites dispatch through, whether the walk reaches it through the type's flattened
    /// `methods` or through the `impl` block itself.
    ///
    /// Span-keyed rather than `(type, method)`-keyed because that pair is exactly what is ambiguous
    /// here: two conversions on one type share the name the parser flattened them under, and the
    /// declaration span is what tells them apart. Empty for every type declaring fewer than two
    /// conversions — those keep the plain `from`.
    from_method_keys: HashMap<Span, String>,
    /// The subset of [`Checker::trait_impls`] that came from `@derive(...)` (not a hand-written
    /// `impl`). A **generic** type's derive is conditional on its instantiated fields
    /// (derive-soundness S4); a hand-written impl is unconditional. Keyed like `trait_impls`.
    derived_traits: HashMap<String, HashSet<BuiltinTrait>>,
    /// Each type's `via:`-delegated derives: type name → `(trait name, via field)` pairs, for
    /// built-in and user traits alike. A via-derive's conditional constraint is the **via
    /// field's** alone — delegation exists precisely so sibling fields don't constrain the trait
    /// — so the instantiation-site checks (`satisfies`/`satisfies_user_trait`) consult this to
    /// judge the substituted via field instead of every field (S4's `via:` twin).
    via_derives: HashMap<String, Vec<(String, String)>>,
    /// Each generic user type's type parameters, in declaration order — so a field/method access
    /// can map an instance's type arguments (`Box<int>`) back onto the declaration's parameters
    /// (`T`) and read a field/return as `int` rather than the bare parameter or `dyn` (S4.5).
    ///
    /// Carries [`ParamRef`]s, not names: mapping an argument onto a parameter is exactly a
    /// substitution, and a substitution keys on identity.
    generic_types: HashMap<String, Vec<ParamRef>>,
    /// Every name a type annotation may legally resolve to: declared records/classes/enums plus
    /// names brought in by a `use` (whether merged in by the linker or left as an opaque stub).
    /// Built-in names and in-scope generic parameters are *not* stored here — they are checked
    /// separately (a built-in via [`Type::is_builtin_name`], a parameter via [`Checker::type_params`]).
    types: HashSet<String>,
    /// Where each declared type was **declared**: type name → its name span. The span's `SourceId`
    /// is what the package orphan rule resolves a type's package from — the symbol tables otherwise
    /// keep only a type's shape, and a merged program's statement list says nothing about which file
    /// (and so which package) a declaration arrived from. Only `.noe` declarations appear: a native
    /// `ExtType`/`ExtEnum` is registered from the registry and has no source, so its package is
    /// (correctly) unknown.
    type_decl_spans: HashMap<String, Span>,
    /// Traits that came from the **extension registry** rather than a `.noe` `trait` declaration
    /// ([`Checker::seed_ext_traits`]). Their synthesized [`noeta_ast::TraitDecl`]s carry a
    /// placeholder `Span::new(0, 0)`, which points at the *entry* source and would make a native
    /// trait look like a root-package declaration. Membership here means "declared by no package",
    /// so the orphan rule requires such an impl to live with its **type** — the same judgement a
    /// built-in trait gets.
    native_traits: HashSet<String>,
    /// Standalone `impl Trait for T {}` declarations, grouped by target type name, as
    /// `(coherence key, trait_span)` occurrences. Collected in pass 1 so each type's coherence check
    /// (`check_coherence`) counts standalone impls alongside its `@derive`s and in-body `impl`s.
    ///
    /// The key is the trait's name for every trait but `From`, which is keyed on the source it
    /// converts (`crate::traits::coherence_key`) — so the two spellings of one implementation
    /// collide here on the same terms an in-body block does.
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
    /// Every struct/class marked `@validated` (validation arc): the types whose literal construction
    /// (`T { ... }`, incl. a record-update spread) is barred OUTSIDE their own `impl`/methods
    /// (`E0060`). The construction check consults this set; it runs after `collect` so a
    /// `@validated` type declared later in the file is still recognized.
    validated_types: HashSet<String>,
    /// The tier name-space (tier-providers T2): built-ins ∪ this program's `@tier` declarations.
    /// Built by [`Checker::check_tier_decls`] (which also validates each declaration, E0051); the
    /// in-place `TierBlock` arm resolves names and config attributes against it.
    tier_registry: tiers::TierRegistry,
    /// Every struct marked `@packed` (P-PACK) — the value structs laid out unboxed and contiguous.
    /// Collected in pass 1 so a packed struct's field-type validation (a field may be another packed
    /// struct declared later) sees the full set, and so `List<Packed>` specialization can consult it.
    packed_structs: HashSet<String>,
    /// Every `@packed(Layout.Column)` struct (P-SIMD C2) — a subset of [`Checker::packed_structs`]
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
    /// Per struct/class, each field whose declared default means something to a **JSON decode**
    /// (json-defaults): type name → field name → [`noeta_ext_abi::FieldDefault`]. Read by
    /// [`Checker::type_to_recipe`] when it bakes a `TypeRecipe::Fielded`, so an omitted field with a
    /// literal default decodes to that default instead of erroring — the same optionality
    /// `attribute_optional_fields` above gives an attribute construction, and `construct(name,
    /// fields)` gives a dynamic construction. Only non-[`noeta_ext_abi::FieldDefault::Required`]
    /// fields are stored; an absent entry *is* `Required`.
    field_defaults: HashMap<String, HashMap<String, noeta_ext_abi::FieldDefault>>,
    /// Per struct/class, the fields marked `#[std.json.Transient]` — type name → field names, absent
    /// for the overwhelmingly common type that has none.
    ///
    /// Written by [`Checker::record_optional_fields`] from [`Checker::transient_names`], so a
    /// *user's* own `Transient` is never mistaken for std's and std's is recognized under whatever
    /// local name a `use` bound it to. Read by [`Checker::type_to_recipe`] (which marks the field
    /// [`noeta_ext_abi::FieldRecipe::skipped`]) and by the derive gate that requires a transient
    /// field to be fillable without the wire.
    transient_fields: HashMap<String, HashSet<String>>,
    /// Class names that declare a `destruct { ... }` block — the seeds of destruct-reachability.
    destructor_classes: HashSet<String>,
    /// The **type-param forwarding table** (poly-values F2b + composite slots D2a): top-level
    /// generic fn name → its ordered forwarding SLOTS (each a type template — bare `T` or a
    /// composite like `List<T>` — flowing into a call-site-typed position:
    /// `json.try_parse::<T>`, `attributes_of::<T>`, or transitively another forwarding generic).
    /// Computed by the syntactic pre-pass ([`compute_forwarding`]) before bodies are checked, so
    /// body-side sites and call sites agree on the hidden-argument layout.
    forwarding: ForwardingMap,
    /// Functions whose forwarding slot set failed to converge — polymorphic recursion through a
    /// composite forward (`f<T>` demanding `List<T>`, then `List<List<T>>`, …). Reported as a
    /// clear E0058 at the declaration instead of an unbounded table.
    forwarding_poisoned: HashSet<String>,
    /// The **fresh-constructor set** (generic constructor reflection): `(type, method)` pairs for
    /// every associated function of a generic struct/class whose every `return` hands back a
    /// freshly-built literal of that type ([`compute_fresh_constructors`]). A call of one records
    /// its resolved instantiation as a construction site, so the caller stamps the object with the
    /// type argument the constructor body could not know.
    fresh_constructors: crate::constructors::FreshConstructors,
    /// Generic types that **read one of their own type parameters reflectively** — a member spelling
    /// `type_name::<T>()` or another bare-parameter query ([`reflective_generic_types`]). Such a type
    /// needs its instances tagged at construction; one that is absent here never asks, so an untagged
    /// instance of it is harmless. Read by [`Checker::report_unrecordable`] as the guard on turning a
    /// provably-untaggable construction into a check-time error rather than noise.
    reflective_generic_types: HashSet<String>,
    /// Type names whose value, when dropped, could run *some* `destruct` block — transitively,
    /// through the type's own block, its fields, or its collection elements (the fixpoint
    /// [`compute_destruct_reachable`] computes). The input to per-binding destructor-relevance.
    destruct_reachable: HashSet<String>,
}

/// The checker's **import bindings** — the four `use`-import channels `collect_imports`
/// resolves (native modules, namespace groups, selective functions, extern types). One of
/// [`Checker`]'s four field groups.
#[derive(Clone, Default)]
struct Imports {
    /// Names bound to a native module by a `use std.{…}` import (`json`, `fs`, …) or a nested
    /// import (`use std.http.client` → `client`), each mapped to the module's **root-qualified
    /// identity** (`"std.json"`, `"std.http.client"`). A call `m.f(args)` on the bound name resolves
    /// through [`stdlib::module_return`] against that identity.
    modules: HashMap<String, String>,
    /// Names bound to a **namespace group** by `use std.http` — each mapped to the group's
    /// **root-qualified prefix** (`http` → `"std.http"`). A member access `http.client` resolves one
    /// hop through [`noeta_ext_abi::registry::Registry::resolve_namespace_child`] against this prefix;
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
    /// user-declared type of the same short name shadows it (user names in [`Checker::types`] take
    /// precedence). This is what lets a file pull in two same-short-named types from different
    /// namespaces, and a native `Counter` coexist with a user's own.
    extern_types: HashMap<String, String>,
}

/// The checker's **effect/scope coloring state** — the save/restore context that tracks where
/// checking currently is (enclosing function's return/yield/async, loop and concurrent depth,
/// current type, in-scope type params). One of [`Checker`]'s four field groups.
#[derive(Clone, Default)]
struct Coloring {
    /// While checking a type's own methods/destructor, the name of that type — so a private-field
    /// access on `self` *or* any same-type value is permitted (the type-scoped privacy rule). `None`
    /// at top level and inside free functions.
    current_type: Option<String>,
    /// While checking a **sealed** named-function/method body: top-level value bindings are not
    /// in scope there (only `use (…)` captures, `self`, and params are), so the hoisted-globals
    /// fallback in the unknown-name gate must not resolve them — the miss is the point, reported
    /// with an "add `use (name)`" hint. `false` at top level and inside top-level closures, which
    /// capture their surroundings.
    in_sealed_body: bool,
    /// While checking the body of a fn lifted from a **dev-tier block** (`@test`/…, slice 6d), the
    /// type-scoped privacy gates are relaxed to white-box access: co-located developer tooling may
    /// read/write/construct its module's private fields and call its private methods (the Rust
    /// `#[cfg(test)]` model). `None` for ordinary fns and methods; set from [`FnDecl::is_dev_tier`]
    /// in [`Checker::check_fn`].
    ///
    /// It holds the dev-tier fn's own **span**, not a bare bit, because "its module's" is half the
    /// rule. `#[cfg(test)]` opens the crate it sits in, not every crate it links; a `@test` block
    /// that could reach into a *dependency's* privates would report nothing where a consumer will
    /// fail, which is exactly how a release's worth of under-published methods got past this
    /// checker. The span answers which package is being trusted — see [`Checker::white_box_over`].
    dev_tier_at: Option<Span>,
    /// While checking a **trait's own default body** — the one declaration site where
    /// [`noeta_ast::FnDecl::is_static`] is a legal, meaningful modifier (static-trait-methods arc).
    ///
    /// Every body in the program funnels through [`Checker::check_fn`], which is therefore the one
    /// place the "`static` is a trait-contract term, never an implementation's" rule can be spelled
    /// *once*. A trait default is the single exception, and this flag states that exception where it
    /// lives rather than repeating the rule at each of the four other call sites. `false` everywhere
    /// else — a top-level `fn`, an inherent method, an `impl` block's method.
    in_trait_contract: bool,
    /// The generic type parameters in scope while checking the current declaration, each mapped to
    /// its declared trait **bounds** (`<T: Comparable>` → `{"T": [Comparable]}`), including an
    /// instantiated bound's type arguments (`<T: Keyed<int>>`). Empty at top level; saved and
    /// restored around each generic declaration. The bounds drive body-side enforcement (S4.3c —
    /// an operation on `T` is only allowed if a bound licenses it) and body-side TYPING: a method
    /// call on a `T`-typed receiver resolves through a bound's trait at the bound's instantiation
    /// (`x.key(): int` under `T: Keyed<int>` — [`Checker::type_param_trait_method`]).
    /// Keyed by the SPELLING that resolves to each parameter — a resolver, not a membership set.
    type_params: TypeScope,
    /// While checking a **generic type's instance method** body: the enclosing type's own type
    /// parameters, in DECLARATION order, with any name the method's own `<…>` shadows replaced by
    /// the empty string (which no identifier can equal). Empty everywhere else — at top level, in a
    /// free function, and in an ASSOCIATED function, which has no receiver to carry the
    /// instantiation. This is the channel that makes `type_name::<T>()` resolvable inside
    /// `class Repo<T>`: the index found here is the argument position to read off `self`'s
    /// reflected type tag. Saved and restored around each function.
    /// A slot holds `None` where the method's own `<…>` shadows the class's parameter — that
    /// parameter is a different one with no receiver channel. (This was a blank STRING, on the
    /// reasoning that no identifier equals `""`; the identity now says it outright.)
    self_type_params: Vec<Option<ParamRef>>,
    /// The `(literal span, expected type)` of the **named object literal currently being checked in
    /// a position that has an expectation** — the channel that lets a construction absorb type
    /// arguments its field values do not pin (`r: Repo<Todo> = Repo { tbl: "todos" }`, where no
    /// field mentions `T`). Read by `synth_object_named`, which fills only the parameters inference
    /// left open, so an argument the fields DO determine is never overridden by the annotation.
    /// `None` outside such a position.
    expected_object: Option<(noeta_span::Span, Type)>,
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
    /// The number of enclosing **closure bodies** around the expression being checked — the one
    /// place where a reference to a top-level binding declared *later* is legitimate.
    ///
    /// Top-level statements run in textual order, so a name they mention must already be bound;
    /// the environment says so and nothing may overrule it. A closure body is different: it is not
    /// evaluated where it is written, it resolves globals as a live view of the top-level scope,
    /// and `f = fn() => g` / `g = 1` / `echo f()` genuinely runs. That gap — *deferred* versus
    /// *now* — is exactly what the hoisted-globals fallback in [`Checker::is_known_name`] keys on.
    ///
    /// A depth rather than a flag because closures nest, and it is reset to `0`… nowhere: a closure
    /// inside a sealed named-fn body is already excluded by [`Coloring::in_sealed_body`], where top
    /// level is out of scope for a different reason. A closure *parameter default* is deliberately
    /// outside the count — it is evaluated in the enclosing scope, which is to say now.
    closure_depth: u32,
    /// The number of enclosing `for`/`while` loops around the statement being checked. A `break`
    /// or `continue` is only valid when this is non-zero; otherwise it is `E0024`.
    loop_depth: usize,
    /// While checking a top-level **forwarding** generic fn's body (poly-values F2b), its ordered
    /// forwarding slot TEMPLATES (bare `T` or a composite like `List<T>`, D2a) — the
    /// hidden-argument layout the body's dynamic sites (`json.try_parse::<List<T>>`) and
    /// onward-forwarding calls index into. A slot whose template mentions a name shadowed by a
    /// nested `fn`'s own type parameter is masked to `Unknown` inside that nested body (D2b) so
    /// it can never match the shadowing parameter. Empty everywhere else.
    current_forwarding: Vec<Type>,
    /// The type parameters a `type_name::<T>()` in the body being checked **would** forward — the
    /// enclosing top-level generic fn's own parameters, minus any a nested `fn` shadows (D2b).
    /// Empty in a method, in a nested fn's own shadowing parameters, and at top level.
    ///
    /// Distinct from [`Self::current_forwarding`], which holds only the slots some site actually
    /// consumes: this is the *capability*, not the realized layout, so a question about a parameter
    /// can be answered whether or not a site consuming it appears in the body — which is what
    /// [`Checker::reject_erased_type_param`] needs to tell "reachable, just not from here" apart
    /// from "not reachable at all", and what the packed check reads to decide whether a slot could
    /// carry a layout.
    forwardable_params: Vec<ParamRef>,
    /// How many `fn` bodies enclose the statement being checked: `0` at top level, `1` inside a
    /// top-level fn/method body, `2+` inside a nested `fn`. Distinguishes a TOP-LEVEL fn (whose
    /// name keys the forwarding/symbol tables) from a nested one that may share its name (D2b).
    fn_depth: usize,
    /// `Expr::Index` spans whose receiver typed as a built-in `List` — recorded as each index is
    /// synthesized so that [`Checker::synth_member`] can recognize a `list[i].field` read without
    /// re-synthesizing (and re-diagnosing) the inner receiver. Internal scratch, not exported (so it
    /// stays a plain `Checker` field, not part of [`SiteMaps`]).
    index_on_list: HashSet<Span>,
}

/// The checker's **run configuration** — what varies a whole-program check (mirrors
/// [`CheckOptions`]) plus session mode. One of [`Checker`]'s four field groups.
///
/// Built only by [`Config::of`]. The three entry points that used to assemble one by hand — the
/// batch check, the session check, and [`SessionChecker::with_options`] — each spelled the fields
/// out and ended in `..Config::default()`, and one of them stopped after `editions`, which is how
/// the session path silently lost the package map and the `@name` tables. One constructor,
/// destructuring [`CheckOptions`] with no rest-pattern, means a field added to either struct is a
/// compile error here rather than a default nobody chose.
#[derive(Clone)]
struct Config {
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
    /// The **extension registry** this checker resolves native modules, functions, extern types,
    /// tiers, and attributes against (instance-registry F2). `None` — the default — routes every
    /// lookup through the process-global default registry (via [`Checker::reg`]), so an ordinary
    /// whole-program check is unchanged. An embedding host that assembled a *per-session* extension
    /// set threads its own [`Registry`] here, and this checker then sees exactly those extensions —
    /// the same set its paired VM runs against. `&'static` because a [`Registry`]'s lookups already
    /// return `&'static` (its units are static); the handle is `Copy`, so `Clone` (the transactional
    /// session snapshot) stays cheap.
    registry: Option<&'static noeta_ext_abi::registry::Registry>,
    /// Where the merged program's sources came from — per-source editions ([`Checker::edition_at`]),
    /// per-source package ([`Checker::package_at`], feeding the orphan rule), and each package's
    /// `@name` bindings. One field rather than three, so the three ways into this struct cannot
    /// carry different subsets of it; see [`Provenance`].
    provenance: Provenance,
}

impl Checker {
    /// A fresh checker against `config` — every other field at its empty start. The only way to make
    /// one, so a checker cannot exist without someone having decided what it is checking under.
    fn new(config: Config) -> Checker {
        Checker {
            config,
            ..Checker::default()
        }
    }
}

/// Exists only so [`Checker`] can derive `Default` for its "every other field empty" spread. It is
/// **not** a way to configure a check: [`Config::of`] is the only path from a caller's
/// [`CheckOptions`] to a [`Config`], and [`Checker::new`] the only path from a `Config` to a checker.
impl Default for Config {
    fn default() -> Config {
        Config {
            record_expr_types: false,
            session_mode: false,
            registry: None,
            provenance: Provenance::unattributed(),
        }
    }
}

impl Config {
    /// The one way to build a [`Config`]. Destructures [`CheckOptions`] with **no** rest-pattern, so
    /// a new option is a compile error at this single site instead of a silent default at three.
    fn of(opts: CheckOptions, session_mode: bool) -> Config {
        let CheckOptions {
            record_expr_types,
            registry,
            provenance,
        } = opts;
        Config {
            record_expr_types,
            session_mode,
            registry,
            provenance,
        }
    }
}

// `Clone` so a [`SessionChecker`] entry is transactional (clone-before, restore-on-error) —
// prompt-scale state, so the per-entry clone is cheap insurance, never a hot path.
#[derive(Clone, Default)]
struct Checker {
    /// The symbol tables `collect` builds (see [`Symbols`]).
    symbols: Symbols,
    /// The `use`-import bindings (see [`Imports`]).
    imports: Imports,
    /// Every spelling this program may write the transient-field marker as
    /// ([`noeta_ast::reflect::JSON_ATTR_TRANSIENT`]), resolved once by
    /// [`noeta_ast::attribute_local_names`] at import time. Read where a field's attributes are
    /// walked — deliberately the *shared* projection rather than [`Checker::attr_key`], because both
    /// backends resolve the marker with it too and the three must agree on which fields are out of
    /// the wire form.
    transient_names: std::collections::HashSet<String>,
    /// The effect/scope coloring state (see [`Coloring`]).
    coloring: Coloring,
    /// The run configuration (see [`Config`]).
    config: Config,
    /// The span-keyed **codegen site maps** the checker produces for the backends and lowering — its
    /// codegen-hint output, grouped apart from the checker's own type-environment/coloring state. See
    /// [`SiteMaps`].
    sites: SiteMaps,
    /// The destructor-relevance of each binding (memory-management migration, Phase 3.2b): the
    /// drop-insertion pass reads it to mark a `DropVar`'s `relevant` bit, which Phase 4 uses to skip
    /// the destructor check for a value whose type can run no destructor.
    relevance: DestructorRelevance,
    /// The pending RETURN-position expectation for the METHOD call currently being synthesized
    /// (generic methods, D3): `(the call's span, the expected type)`, armed by check-mode's
    /// default arm just before it synthesizes a `Call`-with-`Member`-callee and consumed by
    /// [`Checker::call_user_method`] on an exact span match — so `u: User = box.pick(text)` seeds
    /// the method's own type parameters from the annotation, the method twin of the free-fn F2c
    /// arm. Cleared unconditionally after the synthesis returns; sub-expression calls have
    /// different spans, so it can never mis-seed a nested call.
    pending_member_ret: Option<(Span, Type)>,
    /// The span of every declaration body this run actually **entered** — the checker's half of the
    /// body ledger ([`noeta_ast::bodies`]). Compared against [`noeta_ast::bodies::body_sites`] once
    /// the program is checked: a site that was enumerated but never visited is a hole in the
    /// checker, and is reported as one rather than passing in silence.
    ///
    /// This exists because a standalone `impl Trait for T`'s method bodies were never checked at
    /// all — not checked wrongly, never *visited* — and nothing could have told us. Coverage is now
    /// a thing the checker proves about itself on every run.
    visited_bodies: HashSet<Span>,
    /// The span of every `match` this run proved **exhaustive** (`MatchCoverage::Total`): its
    /// unguarded arms cover the scrutinee's whole domain, so control is guaranteed to enter one of
    /// them. Written by [`Checker::check_exhaustive`] while the `match` is typed, read afterwards
    /// by the return-flow analysis — a statement-`match` diverges when it is exhaustive *and*
    /// every arm diverges (`subst::expr_diverges`).
    ///
    /// A side table rather than a re-derivation because exhaustiveness needs the scrutinee's
    /// **type**, which only the typing pass has: `E0048` runs as a walk over the bare AST once the
    /// body is checked, and a second approximation there could disagree with `E0011` — the exact
    /// way an unsound "this match always returns" would slip in. Reset per session entry
    /// (`SessionChecker::reset_scratch`), so a previous entry's spans can never be consulted.
    exhaustive_matches: HashSet<Span>,
    diags: Vec<Diagnostic>,
}

/// Which scope frames count as "already bound" for [`Checker::check_shadow`] (the no-shadowing
/// rule, E0059) — chosen per binder family by where the binder physically lands. See
/// `check_shadow`'s doc for the mapping.
#[derive(Clone, Copy)]
enum ShadowScopes {
    /// Any frame — the binder just pushed a fresh frame, so every hit is a shadow (including a
    /// duplicate in the same parameter list).
    All,
    /// Strictly-enclosing frames only — the binder lands in the *current* frame, where a
    /// same-frame hit is re-declaration/reassignment with its own existing rules.
    Enclosing,
    /// No frames — `lookup` already established the name is unbound; only static names apply.
    StaticsOnly,
}

impl Checker {
    /// The extension [`Registry`] this checker resolves native names against (instance-registry F2):
    /// the per-session registry when one was threaded in ([`Checker::registry`]), otherwise the
    /// process-global default. `&'static` because a registry's lookups already yield `&'static`
    /// data. Every stdlib/extern/tier lookup in the checker goes through here, so pointing a session
    /// at a different extension set is a single field — no lookup site knows which registry it holds.
    fn reg(&self) -> &'static noeta_ext_abi::registry::Registry {
        self.config
            .registry
            .unwrap_or_else(noeta_ext_abi::registry::single_registry_process)
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
        self.config.provenance.editions.at(span)
    }

    /// The **package** that declared whatever `span` points at — resolved from the per-source
    /// [`PackageMap`] the loader threaded in, via the span's `SourceId`.
    ///
    /// `None` means *unknown*, never "the root package": a single-file check, a REPL fragment, a
    /// synthetic span, and compile-time generated code all land here, and the package orphan rule
    /// treats every one of them as unjudgeable rather than guessing. This is the whole reason
    /// provenance is a side-table and not a namespace-prefix heuristic — a namespace is declared per
    /// file and says nothing about which package shipped it.
    fn package_at(&self, span: Span) -> Option<&PackageOrigin> {
        self.config.provenance.packages.at(span)
    }

    /// Whether the body being checked may see the private member declared at `decl` — the white-box
    /// half of [`Checker::field_visible`] and [`Checker::method_visible`], spelled once so the two
    /// gates cannot answer it differently.
    ///
    /// True only inside a dev-tier body (`@test`/…) that belongs to the **same package** as the
    /// declaration. `#[cfg(test)]` is the model and the boundary is the same one: a test may open
    /// the module it ships with, never a dependency's. Without this, a package's own `@test` blocks
    /// silently type-check calls no consumer can make, and every under-published method looks fine
    /// right up until somebody imports it.
    ///
    /// Unknown provenance on either side is permissive, matching [`Self::package_at`]'s contract
    /// that `None` means *unjudgeable* rather than "the root": a single-file check, a REPL fragment,
    /// and generated code have no package to compare, and inventing one would refuse programs on a
    /// guess.
    fn white_box_over(&self, decl: DeclSite) -> bool {
        let Some(at) = self.coloring.dev_tier_at else {
            return false;
        };
        match (self.package_at(at), decl.and_then(|d| self.package_at(d))) {
            (Some(here), Some(there)) => here == there,
            _ => true,
        }
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

    /// Record a **warning** diagnostic (well-formed program, advisory), returning `&mut` to it so a
    /// help line can be chained in place. The warning counterpart of [`error`](Self::error).
    fn warn(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.diags.push(Diagnostic::warning(code, span, message));
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
    fn imported_extern(&self, name: &str) -> Option<&'static noeta_ext_abi::registry::ExtType> {
        self.imports
            .extern_types
            .get(name)
            .and_then(|q| self.reg().find_type_qualified(q))
    }

    /// Reject declaring a type whose name a `use std.<ns>.<Type> [as Alias]` in this file already
    /// bound (E0020): the local name would be ambiguous between the imported native type and the
    /// local declaration. Mirrors the linker's user-import collision rule — the reason a user type
    /// and a same-named native type can safely coexist is that they can never both be in scope.
    fn check_extern_import_collision(&mut self, name: &str, span: Span) {
        if let Some(qualified) = self.imports.extern_types.get(name) {
            self.error(
                DiagnosticCode::NameCollision,
                span,
                format!("`{name}` is already imported from `{qualified}`"),
            )
            .help("rename the local type, or import the native type under an alias (`as …`)");
        }
    }

    fn check_reserved_name(&mut self, name: &str, span: Span) {
        if is_reserved_prelude(name) {
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

    /// The **no-shadowing** rule (E0059): one name, one meaning, per scope stack. A binder — a
    /// parameter, `for` variable, match-pattern binding, or fresh local declaration — may not
    /// reuse a name that already means something: another binding in scope, a top-level function
    /// or type, or an imported name. Assignment never re-declares (it reassigns, under
    /// E0006/E0007), and `is`-narrowing refines the *same* binding through `env::bind` directly —
    /// both bypass this gate by construction, so neither is ever flagged.
    ///
    /// Each binder family passes the [`ShadowScopes`] matching where it binds:
    /// - [`ShadowScopes::All`] — params, `for` vars, pattern binders: they land in a freshly
    ///   pushed frame, so *any* env hit — an enclosing scope or a duplicate in the same list
    ///   (`fn(x, x)`) — is a shadow.
    /// - [`ShadowScopes::Enclosing`] — binders that land in the *current* frame (destructure
    ///   targets, annotated/`mut` declarations, nested `fn` names): a same-frame hit is
    ///   re-declaration/reassignment with its own existing rules (and the REPL's persistent
    ///   global frame re-enters bindings legally), so only strictly-enclosing frames count.
    /// - [`ShadowScopes::StaticsOnly`] — a fresh bare `x = …` declaration: `lookup` already
    ///   found nothing (else it would be a reassignment), so only the static names apply. Also
    ///   skips the hoisted-globals set, which contains this very declaration's own name.
    fn check_shadow(&mut self, name: &str, span: Span, env: &Env, scopes: ShadowScopes) {
        if name == "_" || name == "self" {
            return;
        }
        let scope_hit = match scopes {
            ShadowScopes::All => env::lookup(env, name).is_some(),
            ShadowScopes::Enclosing => env[..env.len().saturating_sub(1)]
                .iter()
                .any(|frame| frame.contains_key(name)),
            ShadowScopes::StaticsOnly => false,
        };
        // No hoisted-globals half: a named function's body is SEALED — top-level value bindings
        // are simply not in scope there, so a param named like a global shadows nothing. Where
        // globals genuinely are in scope (top level itself, top-level closures), the env walk
        // above already sees them. The statics half is asked **of this binding's own source**
        // (see `Symbols::source_statics`): a name declared or imported in the same file is one its
        // author knows about, and a merged-in package's is not.
        let shadowed = if scope_hit {
            Some("a binding already in scope")
        } else {
            self.symbols
                .source_statics
                .get(&span.source)
                .and_then(|names| names.get(name))
                .copied()
        };
        if let Some(what) = shadowed {
            self.error(
                DiagnosticCode::ShadowedBinding,
                span,
                format!("binding `{name}` shadows {what}"),
            )
            .help(
                "every name means one thing per scope — rename this binding (or, for an \
                 import, bring it in under an alias: `use … as …`)",
            );
        }
    }

    /// Consume the checker into the public [`Checked`] result — the whole-program finisher
    /// (`check_all` moves; the session flavor clones and keeps the checker alive instead).
    fn into_checked(self) -> Checked {
        let bundle_bindings = self.bundle_bindings_public();
        let packed_layouts = self.packed_layouts_public();
        let packed_type_layouts = sorted_packed_layouts(&packed_layouts);
        let relevance = self.relevance;
        let method_receivers = self.symbols.method_receiver_spans;
        let mut sites = self.sites;
        let expr_types = std::mem::take(&mut sites.expr_types);
        let diverging_stmts = std::mem::take(&mut sites.diverging_stmts);
        let mut sites = sites.into_sites(relevance);
        sites.packed_type_layouts = packed_type_layouts;
        Checked {
            diagnostics: self.diags,
            expr_types,
            diverging_stmts,
            sites,
            bundle_bindings,
            packed_layouts,
            method_receivers,
        }
    }

    /// Every `@packed` struct's flat layout, by type name — the IDE storage-fact index
    /// ([`Checked::packed_layouts`]). A malformed packed struct (a field E0038 already diagnosed)
    /// yields no layout and is simply absent.
    fn packed_layouts_public(&self) -> HashMap<String, noeta_ast::reflect::PackedLayout> {
        self.symbols
            .packed_structs
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
        self.symbols
            .bundle_impls
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
    fn check_program(&mut self, program: &Program, cancel: &dyn Fn()) {
        let mut env: Env = vec![HashMap::new()];
        self.check_program_in_cancellable(program, &mut env, cancel);
    }

    /// [`Checker::check_program`] against a **caller-owned** environment — the seam the
    /// [`SessionChecker`] rides (session-checker C0): a REPL/console session passes its persistent
    /// global scope, so an entry sees the bindings earlier entries committed and the session keeps
    /// whatever this entry binds. The whole-program path passes a fresh one-frame env
    /// (behavior-identical). Never cancels — the poll is a no-op (session/REPL entries are already
    /// prompt-sized; only the whole-file batch path threads a real cancellation poll).
    fn check_program_in(&mut self, program: &Program, env: &mut Env) {
        self.check_program_in_cancellable(program, env, &|| {});
    }

    /// [`Checker::check_program_in`], polling `cancel` once per **top-level declaration** before it
    /// is checked (audit F9 residual b — intra-check cancellation granularity). The whole-file batch
    /// entry ([`check_all_cancellable`]) threads salsa's revision-cancellation check here, so a
    /// pending input write aborts a long check of a large module promptly — mid-module — instead of
    /// only at query boundaries (a whole-program check is a *single* salsa query, so without this
    /// poll cancellation could not take effect until the entire module was checked). `cancel`
    /// signals by unwinding (`salsa::Cancelled`), which this loop lets propagate untouched; the
    /// checker holds no partial state salsa cares about, so the unwind leaves the session consistent.
    fn check_program_in_cancellable(
        &mut self,
        program: &Program,
        env: &mut Env,
        cancel: &dyn Fn(),
    ) {
        // Implicit async top level (Track A): if the module body contains a top-level `.await` (one
        // not inside a nested `fn`/closure), the top level is itself an async context, so its awaits
        // are legal (executable since A.1 — a top-level `.await` runs its future to completion).
        self.coloring.current_async = block_has_await(&program.stmts);
        for stmt in &program.stmts {
            // Poll for cancellation between declarations: a big module is many top-level items, so
            // this bounds the work a superseded check does after the write to a single declaration.
            cancel();
            self.check_stmt(stmt, env);
        }
        self.coloring.current_async = false;
        self.check_unrefined_muts(&program.stmts);
        // Whole-program, like coherence itself: the two traits contending for one method slot can
        // be bound in two different modules, so the judgement belongs to the linked program rather
        // than to either declaration's own check.
        self.report_trait_default_conflicts();
        self.verify_body_coverage(program);
    }

    /// **The body-coverage gate.** Assert that every body-owning site in the program was actually
    /// entered ([`noeta_ast::bodies`]).
    ///
    /// A checker bug that makes it *wrong* about a body shows up as a bad diagnostic, and a test
    /// catches it. A checker bug that makes it never *look* at a body shows up as nothing at all —
    /// which is how standalone `impl` method bodies went unchecked indefinitely. This closes that
    /// class: coverage is a property the checker proves about itself on every single run, so a
    /// construct added later that nobody wires up fails loudly instead of silently type-checking to
    /// `ok`.
    ///
    /// It is a `debug_assert`: every test, corpus run, and dev build pays for it and gets the
    /// guarantee, while a release compiler does no work. A discrepancy is a *compiler* defect, not
    /// a user error, so it panics rather than emitting a diagnostic — there is no action a user
    /// could take.
    fn verify_body_coverage(&self, program: &Program) {
        debug_assert!(
            self.unchecked_bodies(program).is_empty(),
            "the checker never visited these bodies, so their contents were never type-checked \
             — every construct that owns a body must be wired into the checker:\n{}",
            self.unchecked_bodies(program)
                .iter()
                .map(|s| format!("  {} ({:?}) at {:?}", s.describe(), s.kind, s.span))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// The body sites this run enumerated but never entered. Empty on a correct run; see
    /// [`Self::verify_body_coverage`]. Exposed for the coverage gate that sweeps the corpus.
    fn unchecked_bodies(&self, program: &Program) -> Vec<noeta_ast::bodies::BodySite> {
        noeta_ast::bodies::body_sites(program)
            .into_iter()
            .filter(|site| !self.visited_bodies.contains(&site.span))
            .collect()
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

    /// An `if`/`while` condition must be a `bool`.
    ///
    /// Both backends enforce this at run time (`Op::RequireCondBool`), and that check is exactly
    /// what a *gradual* condition needs — whether a `dyn` value is a bool is genuinely a runtime
    /// question, and the backends agreeing on it is worth having. A **concretely** typed condition
    /// is a different matter: `if 1 { … }` is ill-typed, the checker knows it, and handing it to
    /// the runtime to reject is the check-vs-run divergence in miniature.
    ///
    /// So the runtime check stays as the backstop for the gradual case, and this makes the
    /// concrete case static. Nothing that ran before stops running: a program reaching the runtime
    /// check with a concrete non-bool condition always aborted there.
    fn check_condition(&mut self, cond: &Expr, env: &mut Env, keyword: &str) {
        let ty = self.synth(cond, env);
        if !ty.defers_to_runtime() && ty != Type::Bool {
            self.error(
                DiagnosticCode::TypeMismatch,
                cond.span(),
                format!("`{keyword}` condition must be a `bool`, found `{ty}`"),
            );
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
                let params = decl.params.iter().map(|p| self.annot_param(p)).collect();
                let ret = decl
                    .ret
                    .as_ref()
                    .map(|t| self.annot(t))
                    .unwrap_or(Type::Unknown);
                bind(
                    env,
                    decl.name.as_str(),
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
        let saved_yield = self.coloring.current_yield.take();
        let saved_async = std::mem::replace(&mut self.coloring.current_async, false);
        // A `concurrent` scope likewise does not cross into a closure — a `spawn` inside a closure
        // passed to a builtin is an orphan (E0041), the same coloring rule.
        let saved_concurrent = std::mem::replace(&mut self.coloring.concurrent_depth, 0);
        // Inside the body — and only the body — evaluation is deferred, so a top-level binding
        // declared later is a legitimate reference. The parameter defaults validated above are
        // outside this on purpose: they are evaluated in the enclosing scope.
        self.coloring.closure_depth += 1;
        let result = self.closure_body_type_inner(body, expected, env);
        self.coloring.closure_depth -= 1;
        self.coloring.concurrent_depth = saved_concurrent;
        self.coloring.current_async = saved_async;
        self.coloring.current_yield = saved_yield;
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
                let saved_loop = std::mem::replace(&mut self.coloring.loop_depth, 0);
                let ret = match expected {
                    Some(exp) => {
                        // Check each `return` against `exp`; the closure's return type is `exp`.
                        let saved_ret =
                            std::mem::replace(&mut self.coloring.current_ret, exp.clone());
                        let saved_col = self.coloring.collected_returns.take();
                        self.check_block(stmts, env);
                        self.coloring.collected_returns = saved_col;
                        self.coloring.current_ret = saved_ret;
                        exp.clone()
                    }
                    None => {
                        // Infer: collect the `return` types and join them.
                        let saved_ret =
                            std::mem::replace(&mut self.coloring.current_ret, Type::Unknown);
                        let saved_col = self.coloring.collected_returns.replace(Vec::new());
                        self.check_block(stmts, env);
                        let collected =
                            std::mem::replace(&mut self.coloring.collected_returns, saved_col)
                                .unwrap_or_default();
                        self.coloring.current_ret = saved_ret;
                        join_closure_returns(stmts, collected)
                    }
                };
                self.coloring.loop_depth = saved_loop;
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
                let ty = self.check(value, &Type::Unknown, env);
                self.note_render_hint(&ty, value.span());
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
                        let expected = self.annot(ty);
                        self.check(value, &expected, env);
                        // Record destructor-relevance of this binding for the drop-insertion pass.
                        if self.type_relevant(&expected) {
                            self.relevance.locals.insert(*name_span);
                        }
                        // Annotated = a fresh declaration; carry its `mut`-ness for the field-set rule.
                        // Fresh means it may not shadow an enclosing binding or a static name (E0059).
                        self.check_shadow(name, *name_span, env, ShadowScopes::Enclosing);
                        if *mut_decl {
                            bind_mut(env, name, expected);
                        } else {
                            bind(env, name, expected);
                        }
                    }
                    None => {
                        // **Resolve the name before checking the value.** A bare `x = …` whose name
                        // already resolves is a *reassignment*, and then the binding's own type —
                        // not `Unknown` — is the expectation the value is checked against. Looking
                        // the name up only afterwards is what made
                        // `mut acc: List<int> = [1, 2]` / `acc = []` an `E0023` whose help
                        // suggested the annotation the code already carried: the value was checked
                        // against `Unknown` and the un-inferable-literal gate below fired before
                        // anything had discovered the obvious expectation sitting one line above.
                        //
                        // `mut x = …` is a fresh declaration, never a reassignment; the two
                        // compound-assignment desugars (`x.f = v`, `x ??= y`) run their own checks
                        // and deliberately change the binding's type, so both keep the open
                        // expectation. Those are exactly the cases the branches below exempt.
                        let target = (!*mut_decl
                            && !matches!(value, Expr::FieldSet { .. } | Expr::Coalesce { .. }))
                        .then(|| lookup(env, name))
                        .flatten()
                        .cloned();
                        // The expectation flows only into a literal the target type is *shaped*
                        // for, and only once that type is resolved. An unresolved target is the
                        // accumulator pattern (`mut acc = []` then `acc = [1]`): its hole is
                        // filled by what this write synthesizes, so checking against the hole
                        // would freeze `List<?>` instead of refining it to `List<int>`.
                        let expectation = target.as_ref().filter(|ty| {
                            !ty.contains_unknown() && self.absorbs_expectation(value, ty)
                        });
                        let vty = match expectation {
                            Some(expected) => self.check(value, expected, env),
                            None => self.check(value, &Type::Unknown, env),
                        };
                        if self.type_relevant(&vty) {
                            self.relevance.locals.insert(*name_span);
                        }
                        // An *immutable* binding to a context-free polymorphic literal (`x = []`,
                        // `m = {}`, `x = none`) can never be reassigned (that would be `E0006`), so
                        // its element/payload type is fixed yet undeterminable — `E0023`, fixable
                        // with an annotation. A `mut` binding is exempt: it is an accumulator whose
                        // later writes supply the type (L3).
                        //
                        // `E0023` is a *binding-site* diagnostic — nothing in reach determines the
                        // type — so a reassignment of an already-**resolved** binding is never one:
                        // that binding's type is the answer. A reassignment of an unresolved one
                        // (`mut acc = []` then `acc = []`) still is, since the write refines
                        // nothing.
                        let undeterminable = target.as_ref().is_none_or(Type::contains_unknown);
                        if !*mut_decl && undeterminable && is_uninferable_literal(value) {
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
                        // `mut x = …` is a fresh declaration in the innermost frame — which may
                        // not shadow an enclosing binding or a static name (E0059).
                        if *mut_decl {
                            self.check_shadow(name, *name_span, env, ShadowScopes::Enclosing);
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
                            // tree-walker deferred both of these to the runtime. `target` is the
                            // resolution done above, before the value was checked.
                            match &target {
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
                                    } else if !self.assignable(&vty, existing) {
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
                                // Unbound in every frame, so only static names can be shadowed.
                                None => {
                                    self.check_shadow(
                                        name,
                                        *name_span,
                                        env,
                                        ShadowScopes::StaticsOnly,
                                    );
                                    bind(env, name, vty);
                                }
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
                    // A destructure target lands in the current frame (a same-frame name is
                    // reassignment under its own rules) — enclosing frames and statics only.
                    self.check_shadow(name, *name_span, env, ShadowScopes::Enclosing);
                    if self.type_relevant(&t) {
                        self.relevance.locals.insert(*name_span);
                    }
                    bind(env, name, t);
                }
            }
            Stmt::Expr { expr, span } => {
                // A `match` that is the whole of an expression statement has its value discarded, so
                // block-bodied arms (aether F1) are legitimate here (side effects). Route it through
                // `synth_match` with `value_used` false so it is not flagged E0055; any other
                // expression is checked normally. `synth_match` also means **no expectation** reaches
                // the arms, which is exactly right here: a discarded value has no expected type.
                let ty = if let Expr::Match {
                    scrutinee,
                    arms,
                    span,
                } = expr
                {
                    self.synth_match(scrutinee, arms, *span, env, false)
                } else {
                    self.check(expr, &Type::Unknown, env)
                };
                // `never` here means the statement DOES NOT RETURN — `os.exit(1)`,
                // `server.serve(…)`. Recorded so the tier runners can ask the question instead of
                // guessing it from syntax; see [`Checked::diverging_stmts`]. This reads the type the
                // check above already produced — it is not a second walk over the expression.
                if ty == Type::Never {
                    self.sites.diverging_stmts.insert(*span);
                }
            }
            Stmt::Return { value, span } => {
                // In a generator, only bare `return;` is allowed (it ends iteration); a value has no
                // place under pure-pull `next() -> ?T` (no completion type) → E0039.
                if self.coloring.current_yield.is_some() {
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
                        let expected = self.coloring.current_ret.clone();
                        self.check(value, &expected, env)
                    }
                    None => {
                        // A bare `return` yields unit. When the function has a *known* declared
                        // return (`current_ret` is `Unknown` only while inferring a closure, where
                        // the collected returns are joined instead), unit must be assignable to it —
                        // otherwise `return;` silently escapes a non-`void` function without a value.
                        if !matches!(self.coloring.current_ret, Type::Unknown) {
                            let expected = self.coloring.current_ret.clone();
                            self.subsume(&Type::Unit, &expected, *span);
                        }
                        Type::Unit
                    }
                };
                if let Some(returns) = &mut self.coloring.collected_returns {
                    returns.push(ty);
                }
            }
            Stmt::Yield { value, span } => {
                // `yield e` is valid only inside a generator (a function containing `yield`), where it
                // is checked against the element type `T` of the declared `Iterator<T>` return.
                match self.coloring.current_yield.clone() {
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
                self.check_condition(cond, env, "if");
                // Flow-narrowing: `if ident is T { … }` sees `ident` as `T` in the then-body —
                // but only when the body never reassigns it (a write could invalidate the
                // narrowing). The else-body keeps the declared type (negative narrowing is not
                // done). Mirrors the per-arm narrowing in `synth_match`.
                if let Expr::TypeTest { expr, ty, .. } = cond
                    && let Expr::Ident { name, .. } = expr.as_ref()
                    && !reassigns(then_body, name.as_str())
                    // A test that can never hold (E0065 — `x is P` on an `Option<P>`) narrows
                    // nothing: the branch is unreachable, and narrowing it to the payload is what
                    // used to let the dead code type-check.
                    && lookup(env, name.as_str())
                        .is_none_or(|t| self.impossible_type_test(t, ty).is_none())
                {
                    env.push(HashMap::new());
                    bind(env, name.as_str(), self.annot(ty));
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
                self.check_iterable(&iter_ty, iterable.span());
                env.push(HashMap::new());
                self.bind_for_pattern(pattern, &iter_ty, env);
                self.coloring.loop_depth += 1;
                for stmt in body {
                    self.check_stmt(stmt, env);
                }
                self.coloring.loop_depth -= 1;
                env.pop();
            }
            Stmt::While { cond, body, .. } => {
                self.check_condition(cond, env, "while");
                self.coloring.loop_depth += 1;
                self.check_block(body, env);
                self.coloring.loop_depth -= 1;
            }
            Stmt::Concurrent { body, span } => {
                // `concurrent { }` is a structured-concurrency scope (Track A.3b). It is async-only —
                // joining spawned tasks needs suspend machinery — so it is illegal in a sync context
                // (the coloring rule, E0040), exactly like `.await`.
                if !self.coloring.current_async {
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
                self.coloring.concurrent_depth += 1;
                self.bind_nested_fns(body, env);
                for stmt in body {
                    self.check_stmt(stmt, env);
                }
                self.coloring.concurrent_depth -= 1;
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                // A loop-control statement is only meaningful inside a `for`/`while` body.
                if self.coloring.loop_depth == 0 {
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
                self.check_reserved_name(decl.name.as_str(), decl.name_span);
                // A NESTED fn's name is a value binding in the enclosing body (pre-bound into the
                // current frame by `bind_nested_fns`, hence `Enclosing` — it must not flag
                // itself). A top-level fn (depth 1) is in `symbols.functions`, where duplicate
                // declaration has its own rules; the statics half would self-flag it, so skip.
                if env.len() > 1 {
                    self.check_shadow(
                        decl.name.as_str(),
                        decl.name_span,
                        env,
                        ShadowScopes::Enclosing,
                    );
                }
                self.check_fn(decl, env, &[], TargetKind::Function);
            }
            Stmt::Struct(r) => {
                self.check_reserved_name(r.name.as_str(), r.name_span);
                self.check_reserved_type_name(r.name.as_str(), r.name_span);
                self.check_struct(r, env);
            }
            Stmt::Class(c) => {
                self.check_reserved_name(c.name.as_str(), c.name_span);
                self.check_reserved_type_name(c.name.as_str(), c.name_span);
                self.check_class(c, env);
            }
            Stmt::Enum(e) => {
                self.check_reserved_name(e.name.as_str(), e.name_span);
                self.check_reserved_type_name(e.name.as_str(), e.name_span);
                self.check_enum(e, env);
            }
            Stmt::Impl(decl) => {
                self.check_standalone_impl(decl, env);
            }
            Stmt::Trait(decl) => self.check_trait_decl(decl, env),
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
                items,
                attached,
                ..
            } => {
                let diags_before = self.diags.len();
                // A top-level declaration's leading `@name` is parsed as a one-item `TierBlock`
                // (the adjacency form), so an extension's own `@`-directive arrives here too. It
                // is not a tier: resolve it as a directive and site-check it against what it wraps,
                // rather than reporting "unknown dev-tier" about a name the registry knows.
                if let Some(d) = self.resolve_ext_directive_at(*tier_span, tier) {
                    self.check_ext_directive_block(d, items, *tier_span);
                } else {
                    // Resolve the `@name` in the package that wrote it (per-package naming arc), then
                    // extract owned facts so the diagnostic pushes below don't borrow the registry — the
                    // same resolution activation runs, so the checker accepts exactly what activation
                    // keeps (a renamed tier is not "unknown" here). `sites` is `&'static`.
                    let facts = self
                        .resolve_tier_at(*tier_span, tier)
                        .map(|r| (r.is_expr(), r.config().map(str::to_string), r.sites()));
                    match &facts {
                        None => self.diags.push(tiers::unknown_tier_diagnostic(
                            self.reg(),
                            tier,
                            *tier_span,
                        )),
                        Some((is_expr, config, _)) => {
                            if *is_expr {
                                // An expression tier's block in *statement* position (expr-tiers arc):
                                // its value would be silently discarded — and it never activates/
                                // strips. Shared E0052 with activation.
                                self.diags
                                    .push(tiers::expr_tier_statement_diagnostic(tier, *tier_span));
                            } else if config.is_none() {
                                // Args on a knob-less tier (`@test(x)`) — E0037. Resolved knob-less, so
                                // the free check (args presence) not the name-keyed method.
                                if let Some(d) = tiers::knobless_args_diagnostic_for(tier, args) {
                                    self.diags.push(d);
                                }
                            } else if !args.is_empty()
                                && let Some(attr_name) = config.clone()
                            {
                                // Args on a knob-carrying tier construct its config attribute
                                // (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`) — validate that
                                // construction through the ordinary attribute gate, so this default
                                // path rejects exactly what the activated path's stamped attributes do.
                                let synth =
                                    tiers::synthesized_config_attr(&attr_name, args, *tier_span);
                                self.check_attrs(
                                    std::slice::from_ref(&synth),
                                    TargetKind::Function,
                                );
                            }
                        }
                    }
                    // The **annotation** form: `@test fn foo()` is desugared by the parser into a
                    // one-item `TierBlock`, so a tier attaching to a top-level declaration arrives
                    // here rather than at `check_directives`. Only when the branches above found
                    // nothing better to say — an unknown tier is already E0036, an expression tier
                    // E0052; a site error would be a second, vaguer complaint about the same line.
                    let already_reported = self.diags.len() != diags_before;
                    if *attached
                        && !already_reported
                        && let Some((_, _, sites)) = facts
                    {
                        for item in items {
                            self.check_declared_sites(
                                tier,
                                sites,
                                item.attachment_site(),
                                None,
                                *tier_span,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Validate the `@<tier>` directives leading a function or method (directive attachment-site
    /// model). Each directive must (1) name a known tier — else the same E0036 the block/annotation
    /// forms raise — and (2) attach at a site the tier's registration permits, else **E0054**. A
    /// tier with no declared sites is unrestricted (the gate never fires). `@test`/`@bench` on a
    /// method carry the extra rule that the method must be an **associated function** (never reads
    /// `self`), so the runner can call it with no receiver.
    /// Site-check an extension `@`-directive written in the **adjacency** form — `@name(…)`
    /// immediately before a declaration, which the parser wraps in a one-item `TierBlock`.
    ///
    /// The statement kind maps to a site through `Stmt::attachment_site`, the same vocabulary the
    /// decorator and annotation positions use, and the gate itself is shared.
    fn check_ext_directive_block(
        &mut self,
        directive: &'static noeta_ext_abi::registry::ExtDirective,
        items: &[Stmt],
        span: Span,
    ) {
        for item in items {
            self.check_declared_sites(
                directive.name,
                directive.sites,
                item.attachment_site(),
                None,
                span,
            );
        }
    }

    fn check_directives(
        &mut self,
        directives: &[MethodDirective],
        target: TargetKind,
        decl: &FnDecl,
    ) {
        for dir in directives {
            // An extension's own `@`-directive may be declared for `Function`/`Method` sites, and
            // then it is legal here — it is not a tier, so "unknown dev-tier" would be the wrong
            // complaint about a name the registry knows perfectly well. Its sites gate exactly as a
            // tier's do below.
            let ext_directive = self.resolve_ext_directive_at(dir.name_span, &dir.name);
            // Not an ext directive → resolve it as a tier for the package that wrote it (per-package
            // naming arc). Owned facts (sites `&'static`, canonical id) so the diagnostic pushes below
            // don't borrow the registry.
            let tier_facts = if ext_directive.is_none() {
                self.resolve_tier_at(dir.name_span, &dir.name)
                    .map(|r| (r.sites(), r.id()))
            } else {
                None
            };
            if ext_directive.is_none() && tier_facts.is_none() {
                let d = tiers::unknown_tier_diagnostic(self.reg(), &dir.name, dir.name_span);
                self.diags.push(d);
                continue;
            }
            let sites = match ext_directive {
                Some(d) => d.sites,
                None => tier_facts.as_ref().map_or(&[][..], |(s, _)| *s),
            };
            let before = self.diags.len();
            self.check_declared_sites(&dir.name, sites, target_sites(target), None, dir.name_span);
            if self.diags.len() != before {
                continue;
            }
            // A `@test`/`@bench` method is invoked with no receiver, so it must not read `self` —
            // i.e. it must be reachable without one, which is every classification but `Instance`.
            // Judged by the std test/bench *identity* (whatever local name a rename gave it), not the
            // spelling.
            let is_std_test_or_bench = tier_facts.as_ref().is_some_and(|(_, id)| {
                *id == tiers::TierId::std("test") || *id == tiers::TierId::std("bench")
            });
            if target == TargetKind::Method
                && is_std_test_or_bench
                && let Some(ty) = self.coloring.current_type.clone()
                && !self
                    .receiver_of(&ty, decl.name.as_str())
                    .allows_static_call()
            {
                self.error(
                    DiagnosticCode::InvalidDirectiveSite,
                    dir.name_span,
                    format!("a `@{}` method must be a static function", dir.name),
                )
                .help(
                    "a test/bench method is called with no receiver, so its body must not use \
                     `self` — drop the `self` references, or make it a top-level function",
                );
            }
        }
    }

    /// A declaration's own `<T>` that reuses a name the enclosing declaration's `<…>` already
    /// bound — **E0075, a warning**.
    ///
    /// The rule it reports is [`extend_param_scope`]'s: layering is by replacement, so the inner
    /// `<T>` overwrites the outer entry and every reference in the body resolves to the inner
    /// parameter. That overwrite *is* the shadowing, which is why the condition is read here, off
    /// the scope as it stands before the layering — after it, the outer parameter is gone and
    /// nothing is left to compare against. (It is read here rather than inside `extend_param_scope`
    /// itself for two reasons: that function is a pure scope builder with no diagnostic sink, and
    /// it runs four times per declaration — collection, forwarding, and body checking — so a
    /// warning raised there would be reported up to four times. `check_fn` is the one place every
    /// body funnels through exactly once.)
    ///
    /// A **warning** and not an error: the two are different parameters, the inner one shadows, and
    /// the program means exactly what it says. What it costs is legibility — a reader of
    /// `Repo::<Todo>.label::<User>()` cannot tell which `T` the body means — so this reports a
    /// readability judgement, not a correctness one.
    ///
    /// Scoped to *type parameters against type parameters*. A parameter that merely shares a name
    /// with a declared type (`struct T { }` beside a `class Repo<T>`) is silent: the scope this
    /// consults holds parameters only, so a nominal name is not in it to be hidden.
    fn warn_shadowed_type_params(&mut self, params: &[TypeParam], target: TargetKind) {
        for p in params {
            let Some(outer) = self
                .coloring
                .type_params
                .param(&p.name)
                .map(|s| s.param.clone())
            else {
                continue;
            };
            // The enclosing declaration, when it is nameable — a method's is the type it is
            // declared in. A hit with no name to give still reports; the label says where.
            let owner = match (target, &self.coloring.current_type) {
                (TargetKind::Method, Some(ty)) => format!(" of `{ty}`"),
                _ => String::new(),
            };
            let name = &p.name;
            let d = self.warn(
                DiagnosticCode::ShadowedTypeParameter,
                p.span,
                format!("type parameter `{name}` shadows the enclosing `{name}`{owner}"),
            );
            d.help(format!(
                "they are two different parameters, so `{name}` here never means the enclosing one \
                 — rename one of them"
            ));
            d.label(p.span, format!("this `{name}` hides the one below"));
            if let Some(outer_span) = outer.id.decl_span() {
                d.label(
                    outer_span,
                    format!("the enclosing `{name}` is declared here"),
                );
            }
        }
    }

    /// Check a function (or method) body. `extra` seeds the body scope with additional bindings
    /// (a class's fields, when checking a method).
    pub(crate) fn check_fn(
        &mut self,
        decl: &FnDecl,
        env: &mut Env,
        extra: &[(String, Type)],
        target: TargetKind,
    ) {
        // The checker's half of the body ledger: this run has entered this body. Recorded at the
        // one place every function/method body funnels through, so it cannot drift from what is
        // actually checked.
        self.visited_bodies.insert(decl.name_span);
        // `static` declares receiver-ness, and receiver-ness is a **contract** term — so it belongs
        // to a `trait`'s declaration and nowhere else (static-trait-methods arc). Everywhere else
        // the body already answers the question: a method that mentions `self` needs one and a
        // self-less one does not, derived, exactly, from source the reader is looking at. A
        // modifier there would be a second source of truth that can disagree with the first, which
        // is the very failure this arc exists to remove — so it is refused rather than trusted or
        // silently ignored. E0015, the code every other "this implementation does not match what
        // was declared" mismatch already carries.
        if decl.is_static && !self.coloring.in_trait_contract {
            self.error(
                DiagnosticCode::InvalidImpl,
                decl.name_span,
                format!(
                    "`{}` cannot be declared `static`: only a trait's method contract may declare \
                     a receiver",
                    decl.name
                ),
            )
            .help(
                "drop `static` — outside a `trait` declaration the body decides: a method that \
                 mentions `self` takes a receiver, a self-less one is called on the type",
            );
        }
        self.require_signature(decl);
        // A function/method's `#[...]` attributes are validated like a type's: each names an
        // `Attribute` capability (E0029) and constructs it from its literal args (E0009/E0007/E0005).
        // `target` distinguishes a top-level `Function` from a `Method` for placement checks (P2.5).
        self.check_attrs(&decl.attrs, target);
        // Each parameter's own `#[...]` attributes, validated by exactly the same rules — an
        // attribute struct (E0029), constructed from its literal arguments, and permitted at
        // `Param` (E0030). The parameter is a target kind like any other, so a `@attribute(Field)`
        // written on a parameter is rejected the same way one written on a method is.
        self.check_param_attrs(&decl.params);
        // A method's leading `@<tier>` directives (`@test`/`@doc`/…): each names a known tier and
        // attaches at a site the tier permits (E0054, the directive attachment-site model).
        self.check_directives(&decl.directives, target, decl);
        // Bring the function's own generic parameters into scope for its body (a free function may
        // be generic; a method is generic over its class's parameters, already in scope, and
        // carries none of its own). Union with the current set so a method does not lose the
        // class's parameters; restored after the body. Bounds are validated AFTER the parameters
        // enter scope — a bound argument may name a sibling parameter (`<K, T: Keyed<K>>`).
        // While checking a forwarding generic's body (poly-values F2b), expose its type-argument
        // slot layout so the body's dynamic sites and onward-forwarding calls can index
        // it. A METHOD gets its own layout too (Axis A), keyed `Type.method`; a nested context
        // retains the enclosing one (D2b) and a closure/destructor gets none.
        // A slot set that failed the pre-pass fixpoint (polymorphic recursion through a
        // composite forward, D2a) is a clear error at the declaration — the static table cannot
        // enumerate its instantiations.
        // The key this declaration's own forwarding slots are recorded under — its bare name for a
        // top-level `fn`, `Type.method` for a method. `None` for anything that carries no slots of
        // its own: a nested `fn` (D2b — it reads the ENCLOSING fn's, through closure capture), a
        // closure, a destructor.
        let fwd_key = if self.coloring.fn_depth == 0 {
            match target {
                TargetKind::Function => Some(decl.name.to_string()),
                TargetKind::Method => self
                    .coloring
                    .current_type
                    .as_ref()
                    .map(|ty| format!("{ty}.{}", decl.name)),
                _ => None,
            }
        } else {
            None
        };
        if let Some(key) = &fwd_key
            && self.symbols.forwarding_poisoned.contains(key.as_str())
        {
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                decl.name_span,
                format!(
                    "type-parameter forwarding in `{}` does not converge: recursion keeps \
                     building deeper composite instantiations",
                    decl.name
                ),
            )
            .help(
                "erased generics deliver each forwarded instantiation through a static table, \
                 which polymorphic recursion (e.g. `f::<List<T>>` inside `f<T>`) cannot \
                 enumerate; restructure so the composite is built by the caller",
            );
        }
        let next_forwarding = if let Some(key) = &fwd_key {
            self.symbols
                .forwarding
                .get(key.as_str())
                .map(|f| f.iter().map(|s| s.template.clone()).collect())
                .unwrap_or_default()
        } else if target == TargetKind::Function {
            // A NESTED `fn` (D2b) consumes the ENCLOSING top-level fn's slots — its body
            // reads the enclosing hidden locals through closure capture — so the layout is
            // retained, with any slot whose template mentions a name this declaration's own
            // type parameters shadow masked out (`Unknown` never matches a lookup).
            let shadowed = param_ids(&decl.type_params);
            self.coloring
                .current_forwarding
                .iter()
                .map(|t| {
                    if mentions_param(t, &shadowed) {
                        Type::Unknown
                    } else {
                        t.clone()
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        // The forwarding *capability* alongside the realized layout: which parameters a
        // `type_name::<T>()` here would resolve through a hidden slot. Same top-level-fn scope and
        // same D2b shadow-masking as the layout above, but derived from the declaration rather
        // than from the sites, so a diagnostic can say whether a parameter is reachable at all
        // whether or not a site consuming it already appears in the body.
        // A **self-less member of a generic type** forwards its CLASS's parameters too: it has no
        // receiver to read the instantiation's tag off, so the hidden slot is its only channel (see
        // `forwarding::forwardable_class_params`, which decides the same thing for the layout).
        let next_forwardable = if fwd_key.is_some() {
            let mut ps: Vec<ParamRef> = decl.type_params.iter().map(param_ref).collect();
            if target == TargetKind::Method
                && let Some(ct) = self.coloring.current_type.clone()
                && !self
                    .receiver_of(&ct, decl.name.as_str())
                    .allows_instance_call()
                && let Some(class_params) = self.symbols.generic_types.get(&ct)
            {
                let extra: Vec<ParamRef> = class_params
                    .iter()
                    .filter(|p| !ps.contains(p))
                    .cloned()
                    .collect();
                ps.extend(extra);
            }
            ps
        } else if target == TargetKind::Function {
            // A nested declaration's own `<…>` shadow the enclosing ones — and a shadowing `T` is a
            // DIFFERENT parameter, so it is dropped by identity rather than by name.
            let shadowed = param_ids(&decl.type_params);
            self.coloring
                .forwardable_params
                .iter()
                .filter(|p| !shadowed.contains(&p.id))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let saved_forwardable =
            std::mem::replace(&mut self.coloring.forwardable_params, next_forwardable);
        let saved_forwarding =
            std::mem::replace(&mut self.coloring.current_forwarding, next_forwarding);
        self.coloring.fn_depth += 1;
        // Record the hidden-slot count for lowering — for EVERY forwarding fn or method, called
        // or not, so the body's dynamic sites always have their slots. Keyed exactly as the slot
        // layout is (`fwd_key`), which is why a nested fn is absent: it retains the enclosing
        // layout for its body's SITES (D2b) but carries no slots of its own (it captures the
        // enclosing `$ty` locals instead).
        if let Some(key) = fwd_key
            && !self.coloring.current_forwarding.is_empty()
        {
            self.sites
                .forwarding_fns
                .insert(key, self.coloring.current_forwarding.len() as u32);
        }
        // The enclosing generic type's parameters, for `type_name::<T>()` inside an INSTANCE method
        // (generic constructor reflection, Gap B): the instantiation is not in the body — one
        // compiled body serves every `Repo<…>` — but the receiver carries it as a reflected type
        // tag, so a parameter's declaration index here is the argument position to read off `self`.
        // Only an instance method has that receiver; an associated function keeps the E0058.
        let saved_self_params = std::mem::take(&mut self.coloring.self_type_params);
        if target == TargetKind::Method
            && let Some(ct) = self.coloring.current_type.clone()
            && self
                .receiver_of(&ct, decl.name.as_str())
                .allows_instance_call()
            && let Some(params) = self.symbols.generic_types.get(&ct)
        {
            // A method's own `<U>` shadows a same-named type parameter, and it has no receiver
            // channel of its own — blank the slot so no lookup can match it.
            // A method's own `<U>` shadows a same-named class parameter, and has no receiver
            // channel of its own — blank the slot so no lookup can match it. Shadowing is decided
            // on the SPELLING here, because that is what shadowing is: the method's `<T>` makes the
            // name `T` mean its own parameter, so the class's is unreachable from this body.
            let own: HashSet<&str> = decl.type_params.iter().map(|p| p.name.as_str()).collect();
            self.coloring.self_type_params = params
                .iter()
                .map(|p| (!own.contains(p.name.as_str())).then(|| p.clone()))
                .collect();
        }
        let saved_type_params = self.coloring.type_params.clone();
        // Read the shadowing off the scope BEFORE it is layered over (E0075, warning): the entry a
        // parameter is about to replace is the one it hides.
        self.warn_shadowed_type_params(&decl.type_params, target);
        // Layered over the enclosing declaration's, so a method's `<T>` inside a class `<T>` makes
        // `T` mean the method's — the shadowing rule, stated once.
        self.coloring.type_params = extend_param_scope(
            &self.coloring.type_params,
            &decl.type_params,
            &self.imports.extern_types,
        );
        self.check_type_param_bounds(&decl.type_params);
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
            .map(|t| self.annot(t))
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
        let saved_yield = std::mem::replace(&mut self.coloring.current_yield, yield_elem);
        // An `async fn` body is an async context: its `.await`s are legal (Track A). `current_ret`
        // stays the *inner* declared type `T` (the body writes `return t`); a call site sees the
        // wrapped `Future<T>` via the signature. Reset for a non-async function so an enclosing async
        // context does not leak into a nested ordinary function.
        let saved_async = std::mem::replace(&mut self.coloring.current_async, decl.is_async);
        let saved_ret = std::mem::replace(&mut self.coloring.current_ret, ret);
        // A function body is a fresh control-flow context: `break`/`continue` inside it cannot
        // target a loop the *enclosing* code is in, so reset the depth (restored after).
        let saved_loop_depth = std::mem::replace(&mut self.coloring.loop_depth, 0);
        // White-box field privacy inside a dev-tier fn (slice 6d). Sticky: a nested fn declared in a
        // dev-tier body stays white-box too (co-located tooling). Restored after the body.
        let saved_dev_tier = self.coloring.dev_tier_at;
        if decl.is_dev_tier {
            self.coloring.dev_tier_at = Some(decl.name_span);
        }
        // SEALED body env: a named function's body sees its `use (…)` captures, `self`/`extra`,
        // and its parameters — never the surrounding value scope implicitly (anonymous closures
        // are the auto-capturing form). Each capture resolves against the DECLARATION site's env
        // as a live view of that binding, keeping its mutability; a name that only exists as a
        // hoisted-but-later top-level binding is accepted at `Unknown` (immutable view — its type
        // completes at runtime); anything else is an unknown name.
        let mut sealed: Env = vec![HashMap::new()];
        for (name, span) in &decl.captures {
            self.check_reserved_name(name, *span);
            // A duplicate in the capture list, or a capture named like a static, is a shadow.
            self.check_shadow(name, *span, &sealed, ShadowScopes::All);
            if let Some(ty) = lookup(env, name) {
                let ty = ty.clone();
                if lookup_mutable(env, name) {
                    bind_mut(&mut sealed, name, ty);
                } else {
                    bind(&mut sealed, name, ty);
                }
            } else if self.symbols.global_binding_names.contains(name) {
                bind(&mut sealed, name, Type::Unknown);
            } else {
                self.error(
                    DiagnosticCode::UnknownName,
                    *span,
                    format!(
                        "cannot capture `{name}`: no binding of that name at the declaration site"
                    ),
                )
                .help("`use (…)` names a value binding visible where the function is declared");
            }
        }
        // From here on the body checks against the sealed env only.
        let env = &mut sealed;
        let saved_sealed = std::mem::replace(&mut self.coloring.in_sealed_body, true);
        // Validate parameter defaults: trailing-only (`E0026`) and each default's type against its
        // parameter (`E0007`). Checked against the SEALED env before the parameter frame is pushed
        // — a default sees statics and this fn's `use (…)` captures, exactly like the body, and
        // never other parameters. (Runtime agrees: both backends evaluate an omitted argument's
        // default thunk in the definition scope.)
        self.validate_param_defaults(&decl.params, env);
        env.push(HashMap::new());
        for (name, ty) in extra {
            bind(env, name, ty.clone());
        }
        for p in &decl.params {
            self.check_reserved_name(&p.name, p.name_span);
            // Params land in the just-pushed frame: any env hit — a capture or a duplicate in
            // this very list (`fn(x, x)`) — is a shadow (E0059).
            self.check_shadow(&p.name, p.name_span, env, ShadowScopes::All);
            bind(env, &p.name, self.annot_param(p));
        }
        self.bind_nested_fns(&decl.body, env);
        for stmt in &decl.body {
            self.check_stmt(stmt, env);
        }
        self.coloring.in_sealed_body = saved_sealed;
        // E0048: a non-`void` function must return a value on every path. If control can reach the end
        // of the body — it falls off the end, or an `if` without an `else` leaves a path open — the
        // function would implicitly return `unit` where its signature promised another type, and a
        // caller would silently bind that type to `unit`. (`return`s inside are already checked
        // against the declared type above; this is the complementary "did every path return" check.)
        // Runs *after* the body is typed, so `exhaustive_matches` already holds this body's
        // `match` verdicts — an exhaustive `match` whose arms all return counts as returning.
        // Reads `never_exprs` alongside `exhaustive_matches`, and for the same reason: both are
        // typing facts this body just established, so a call to a `never` function counts as a
        // path that returned.
        if must_return_value
            && !block_diverges(
                &decl.body,
                &self.exhaustive_matches,
                &self.sites.never_exprs,
            )
        {
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
        self.coloring.dev_tier_at = saved_dev_tier;
        self.coloring.current_ret = saved_ret;
        self.coloring.current_async = saved_async;
        self.coloring.current_yield = saved_yield;
        self.coloring.loop_depth = saved_loop_depth;
        self.coloring.type_params = saved_type_params;
        self.coloring.self_type_params = saved_self_params;
        self.coloring.fn_depth -= 1;
        self.coloring.current_forwarding = saved_forwarding;
        self.coloring.forwardable_params = saved_forwardable;
    }
}

#[cfg(test)]
mod tests;
