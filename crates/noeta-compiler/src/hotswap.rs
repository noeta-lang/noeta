//! Hot-swap diffing: given two versions of a program, decide what a live session
//! can swap in place and what forces a restart.
//!
//! The differ compares **source-text slices**, not AST nodes: the two programs come from separate
//! parses, so every node's spans differ even when the code is identical — derived `PartialEq` over
//! the AST would report everything changed. A definition whose text is byte-identical between
//! versions is unchanged by construction; where the text differs, a structural fingerprint of the
//! *signature* (built from the sub-node spans, sliced against each version's own source) separates
//! a body-only edit (swappable) from a signature/layout change (restart).
//!
//! What is swappable rides on two shipped runtime invariants (see `plans/server-hmr/README.md`):
//!
//! - **Function bodies** — top-level `fn`s dispatch through their global slot at call time
//!   (`Op::CallGlobal`), so re-evaluating a changed `fn` declaration as a session entry stores a
//!   new closure into the *existing* slot and every live call site picks it up.
//! - **Method bodies** — dispatch is by `(type, method)` name, and shapes are content-interned, so
//!   re-evaluating a type declaration whose *layout is unchanged* re-registers its methods against
//!   new protos while existing instances keep the same `&'static Shape` and flow into the new
//!   bodies untouched.
//!
//! A changed **top-level statement** makes the swap re-running: the new top level
//! re-executes with the unchanged reactive anchors (`signal`/`computed`/`cell.new`/`synced_signal`
//! bindings) withheld, so their live nodes — the state — survive; the previous epoch's effects are
//! disposed first and re-created by the re-run. The language rule this implements: *reactive state
//! survives edits; plain state re-initializes.*
//!
//! Everything else — a changed signature (arity/defaults compiled at call sites we cannot
//! enumerate), a changed field/variant layout (a different interned shape; positional
//! `Op::ExtractField` indices would misread old instances), a changed `namespace` (re-links the
//! world) — is a [`SwapBlocker`]: the driver falls back to a full restart.
//!
//! Known, deliberate semantic edge: a **function value captured before the swap** (`mut h = f`)
//! keeps the old body — closure values hold their proto directly; only slot-routed calls rebind.
//! Pinned in the session tests as intended behavior.

use std::collections::{HashMap, HashSet};

use noeta_ast::{ClassDecl, EnumDecl, FnDecl, Program, Stmt, StructDecl};
use noeta_span::Span;

/// The swappable subset of a version-to-version diff: a fragment of the NEW program to re-evaluate
/// against the live session (added/changed `use` imports, changed/added `fn` declarations, and
/// type declarations whose only changes are method bodies — or the *whole* declaration region, in
/// the new order, when that order moved), plus the bookkeeping a driver reports.
///
/// **Every reported name list is sorted** ([`SwapPlan::changed`], [`added`], [`removed`],
/// [`preserved`], and the [`SwapBlocker`]s of a [`SwapDiff::NeedsRestart`]). They are filled by
/// walking hash-keyed indexes of the two versions, so their *discovery* order varies per process;
/// what a developer reads (`[hot] swapped: …`) and what a driver logs must not. The fragment itself
/// never depended on this — the include set is membership-only and the fragment is assembled in the
/// new program's source order — so the sort is about a reproducible report, not a correct swap.
///
/// [`added`]: SwapPlan::added
/// [`removed`]: SwapPlan::removed
/// [`preserved`]: SwapPlan::preserved
#[derive(Debug, Clone)]
pub struct SwapPlan {
    /// The statements to re-evaluate, cloned from the new program in its source order. Running
    /// this as one session entry *is* the swap: `use`s (re)bind import globals, `fn` declarations
    /// store fresh closures into their existing slots, type declarations re-register their methods.
    pub fragment: Program,
    /// Definitions whose behavior changed (`f`, or `Type.method` for a method-level change).
    pub changed: Vec<String>,
    /// Definitions new in this version.
    pub added: Vec<String>,
    /// Definitions the new version no longer contains. Their old bindings stay live (a stale
    /// global/method entry is unreachable from freshly-checked code but keeps in-flight callers
    /// sound); reported so a driver can surface them.
    pub removed: Vec<String>,
    /// Whether the top level changed, making this a **re-running** swap: the
    /// fragment carries the new version's top-level statements (minus `preserved`), and the
    /// session first disposes the previous epoch's effects plus the reactive nodes the re-run
    /// re-binds. `false` = a body-only swap: no top-level statement re-runs, all state
    /// (reactive or plain) is trivially preserved.
    pub rerun_top_level: bool,
    /// Whether the **declaration region moved** — a declaration reordered, or one inserted before
    /// another the previous version also had (see [`declaration_region_moved`]). Then, and only
    /// then, the fragment carries the whole region in the new version's order rather than only what
    /// changed, because the fragment's sequence is what re-orders the source-ordered reflection
    /// artifact.
    ///
    /// A bit of bookkeeping like [`rerun_top_level`](Self::rerun_top_level), not a second
    /// description of the program: it says which *kind* of change this is, so a consumer reading
    /// [`changed`](Self::changed) — no declaration's behaviour changed under a reorder, so that list
    /// is empty — can tell "nothing to do" from "everything was re-registered".
    pub declarations_reordered: bool,
    /// Binding names whose statements were **withheld from the re-run** because they are
    /// unchanged reactive anchors (`mut s = signal(…)` / `computed(…)` / `cell.new(…)` /
    /// `synced_signal(…)`): the live node survives the swap and re-run code keeps referring to
    /// it through the untouched global. This is the language's HMR state rule — *reactive state
    /// survives edits; plain state re-initializes.*
    pub preserved: Vec<String>,
}

impl SwapPlan {
    /// Every declaration the fragment re-evaluates, by name, in the new program's source order —
    /// including a code tier's `fn`s, which the differ attributes like top-level ones.
    ///
    /// Usually this is [`changed`](Self::changed) + [`added`](Self::added) read off the fragment
    /// instead of off the report. It is *more* than that exactly when the declaration region moved
    /// (see [`declaration_region_moved`]): re-registering a declaration re-registers its reflection
    /// records, so a consumer that asks "what may behave differently now" has to count a
    /// re-registered declaration even though nothing about its own text changed. One reader of the
    /// fragment rather than a `match` per consumer, because the fragment's statement kinds are the
    /// differ's business.
    pub fn fragment_declarations(&self) -> Vec<&str> {
        fn name_of(stmt: &Stmt) -> Option<&str> {
            match stmt {
                Stmt::Fn(decl) => Some(decl.name.as_str()),
                Stmt::Struct(decl) => Some(decl.name.as_str()),
                Stmt::Class(decl) => Some(decl.name.as_str()),
                Stmt::Enum(decl) => Some(decl.name.as_str()),
                _ => None,
            }
        }
        self.fragment
            .stmts
            .iter()
            .flat_map(|stmt| match stmt {
                Stmt::TierBlock { items, .. } => items.iter().filter_map(name_of).collect(),
                other => name_of(other).into_iter().collect::<Vec<_>>(),
            })
            .collect()
    }
}

/// A change the live session cannot absorb — the driver must restart instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapBlocker {
    /// A function or method's signature (params/defaults/return/attributes/genericity) changed;
    /// call sites compiled against the old signature cannot be enumerated for recompilation.
    SignatureChanged { name: String },
    /// A type's layout changed (fields, variants, derives, packing, traits, destructor, …): old
    /// instances would keep the old interned shape while new code assumes the new one.
    LayoutChanged { type_name: String },
    /// A type declaration was removed. New code cannot construct it, but stale-vs-fresh shape
    /// questions make this a restart until a concrete need says otherwise.
    TypeRemoved { type_name: String },
    /// A standalone `impl Trait for Type` was added/removed/edited — trait coherence is
    /// whole-program.
    ImplChanged { trait_name: String, target: String },
    /// The `namespace` declaration changed — qualified identity is baked into everything the
    /// linker resolved; only a restart re-links.
    NamespaceChanged { detail: String },
}

impl std::fmt::Display for SwapBlocker {
    /// The human-readable reason the dev loop reports next to "restarting".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapBlocker::SignatureChanged { name } => {
                write!(f, "the signature of `{name}` changed")
            }
            SwapBlocker::LayoutChanged { type_name } => {
                write!(f, "the layout of type `{type_name}` changed")
            }
            SwapBlocker::TypeRemoved { type_name } => {
                write!(f, "type `{type_name}` was removed")
            }
            SwapBlocker::ImplChanged { trait_name, target } => {
                write!(f, "`impl {trait_name} for {target}` changed")
            }
            SwapBlocker::NamespaceChanged { detail } => {
                write!(f, "the namespace changed: {detail}")
            }
        }
    }
}

/// The differ's verdict for one old→new program pair.
#[derive(Debug, Clone)]
pub enum SwapDiff {
    /// No behavioral difference (byte-identical or formatting-only).
    Unchanged,
    /// Every change is body-level: apply `SwapPlan` to the live session.
    Swap(SwapPlan),
    /// At least one change the session cannot absorb; restart and report why.
    NeedsRestart(Vec<SwapBlocker>),
}

/// Diff two parsed versions of a program (each with the source text it was parsed from) into a
/// swap verdict. Pure; the caller owns checking the NEW version (a plan must only be applied after
/// its program checked green — the transactional gate) and applying the plan to a session.
pub fn diff_programs(old: &Program, old_src: &str, new: &Program, new_src: &str) -> SwapDiff {
    let old_items = classify(&old.stmts, old_src);
    let new_items = classify(&new.stmts, new_src);
    let mut blockers = Vec::new();
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    // Names of fn/type declarations to include in the fragment (uses are selected by text below).
    let mut include: HashSet<&str> = HashSet::new();

    // A changed `namespace` declaration re-links the world — restart.
    if old_items.namespaces != new_items.namespaces {
        let detail = first_divergence(&old_items.namespaces, &new_items.namespaces);
        blockers.push(SwapBlocker::NamespaceChanged { detail });
    }

    // Top-level non-declaration statements compared as an ordered text sequence. Any difference
    // makes this a RE-RUNNING swap: the new top level re-executes — with the
    // unchanged reactive anchors withheld (preserved) so their live nodes survive — instead of
    // blocking. Identical sequences keep the body-only swap, which re-runs nothing.
    let rerun_top_level = old_items.others != new_items.others;

    // Free functions: keyed by name; body-only edits swap, signature edits block.
    for (&name, new_fn) in &new_items.fns {
        match old_items.fns.get(name) {
            None => {
                added.push(name.to_string());
                include.insert(name);
            }
            Some(old_fn) => match compare_fn(old_fn, old_src, new_fn, new_src) {
                FnChange::Unchanged => {}
                FnChange::BodyChanged => {
                    changed.push(name.to_string());
                    include.insert(name);
                }
                FnChange::SignatureChanged => {
                    blockers.push(SwapBlocker::SignatureChanged {
                        name: name.to_string(),
                    });
                }
            },
        }
    }
    for &name in old_items.fns.keys() {
        if !new_items.fns.contains_key(name) {
            removed.push(name.to_string());
        }
    }

    // Type declarations: layout must be identical; method-level edits swap the whole declaration.
    for (&name, new_ty) in &new_items.types {
        match old_items.types.get(name) {
            None => {
                added.push(name.to_string());
                include.insert(name);
            }
            Some(old_ty) => diff_type(
                name,
                old_ty,
                old_src,
                new_ty,
                new_src,
                &mut blockers,
                &mut changed,
                &mut added,
                &mut removed,
                &mut include,
            ),
        }
    }
    for &name in old_items.types.keys() {
        if !new_items.types.contains_key(name) {
            blockers.push(SwapBlocker::TypeRemoved {
                type_name: name.to_string(),
            });
        }
    }

    // Standalone `impl Trait for Type`: any difference is a coherence change — restart.
    if old_items.impls != new_items.impls {
        for key in new_items.impls.symmetric_difference(&old_items.impls) {
            let (trait_name, target) = key.clone();
            blockers.push(SwapBlocker::ImplChanged { trait_name, target });
        }
    }

    if !blockers.is_empty() {
        blockers.sort_by_key(|b| format!("{b:?}"));
        blockers.dedup();
        return SwapDiff::NeedsRestart(blockers);
    }

    // Declaration ORDER is part of the comparison, and it is the one part a name-keyed diff cannot
    // see. Every table above is keyed by name, so two versions holding the same declarations in a
    // different order compared as *nothing changed* — while the reflection artifact is
    // source-ordered, so a cold start of the new version answers `attributes_of` / `roles_of` /
    // `params_of` in the new order and the untouched live session answers in the old one
    // (`plans/parallel-path-audit.md` §14: eleven corpus programs, all reflection).
    //
    // The repair is to hand the region to the fragment when it moved, in the NEW program's order:
    // re-evaluating a declaration re-registers its reflection records, and
    // `ReflectionInfo::accumulate` fills the slots they held in the fragment's order. So the
    // fragment's own sequence carries the new order — nothing beside it has to describe it, and
    // nothing can drift from it.
    //
    // Only when it actually moved. An edit that appends declarations at the end of the file — the
    // ordinary "add a function" save — leaves every existing declaration where it was and the
    // merge's append rule already lands the newcomer where a cold start has it, so that edit keeps
    // its one-declaration fragment.
    let declarations_reordered = declaration_region_moved(&old_items.decls, &new_items.decls);
    if declarations_reordered {
        include.extend(new_items.decls.iter().copied());
    }

    // Assemble the fragment in the NEW program's source order: added `use`s (an import re-run is
    // an idempotent global (re)bind; removed ones just leave a stale binding), the included
    // declarations, and — on a re-running swap — every top-level statement except the preserved
    // reactive anchors. Cloned as-is — the fragment carries the new version's spans.
    let old_other_texts: HashSet<&str> = old_items.others.iter().copied().collect();
    let mut preserved = Vec::new();
    let fragment_stmts: Vec<Stmt> = new
        .stmts
        .iter()
        .filter(|stmt| match stmt {
            Stmt::Use { .. } => !old_items.uses.contains(text(new_src, stmt.span())),
            Stmt::Fn(decl) => include.contains(decl.name.as_str()),
            Stmt::Struct(decl) => include.contains(decl.name.as_str()),
            Stmt::Class(decl) => include.contains(decl.name.as_str()),
            Stmt::Enum(decl) => include.contains(decl.name.as_str()),
            // A code tier block rides along when one of its fns changed (stripped at lowering —
            // a live server re-evaluates it as a no-op; the bookkeeping is what matters).
            Stmt::TierBlock {
                items: tier_items,
                doc_text: None,
                ..
            } => tier_items
                .iter()
                .any(|i| matches!(i, Stmt::Fn(d) if include.contains(d.name.as_str()))),
            // A standalone `impl Trait for T` rides along **only** when the region moved, and then
            // it must: it declares `T.method`'s manifest rows, so leaving it out re-registers the
            // type without them and those rows keep the slot they held in the old order. Safe to
            // re-evaluate because getting here means it is byte-identical to the running one —
            // any edit to an impl is a `SwapBlocker::ImplChanged` restart.
            Stmt::Impl(_) => declarations_reordered,
            Stmt::Namespace { .. } => false,
            other => {
                if !rerun_top_level {
                    return false;
                }
                // An unchanged reactive-anchor binding is withheld: its live node IS the state
                // the swap preserves. A *changed* anchor re-runs — the developer redefined the
                // signal itself, so it resets (its replaced node is disposed pre-run).
                if reactive_anchor_name(other).is_some()
                    && old_other_texts.contains(text(new_src, other.span()))
                {
                    preserved.push(
                        reactive_anchor_name(other)
                            .expect("just matched Some")
                            .to_string(),
                    );
                    return false;
                }
                true
            }
        })
        .cloned()
        .collect();

    if !rerun_top_level && fragment_stmts.is_empty() && removed.is_empty() {
        return SwapDiff::Unchanged;
    }
    // Sorted because they were discovered through hash-keyed walks of the two versions: unsorted,
    // the `[hot] swapped: …` line a developer reads (and every driver that logs a plan) would
    // reorder itself from run to run for one unchanged edit. See the type's docs.
    changed.sort();
    added.sort();
    removed.sort();
    preserved.sort();
    SwapDiff::Swap(SwapPlan {
        fragment: Program {
            stmts: fragment_stmts,
            span: new.span,
        },
        changed,
        added,
        removed,
        rerun_top_level,
        declarations_reordered,
        preserved,
    })
}

/// Whether the two versions' **declaration sequences** describe a different region — the question a
/// name-keyed comparison cannot answer, and the one the source-ordered reflection artifact depends
/// on.
///
/// Two shapes count, and they are the two the reflection merge cannot repair on its own:
///
/// - **A reorder** — the declarations both versions have, read in each version's own order, differ.
/// - **An insertion** — a declaration new in this version sits before one the old version also had.
///   The merge has no slot for a name the session has never seen, so it appends; a cold start has it
///   in the middle. Only a newcomer in the file's *trailing* run of new declarations is safely
///   appended, which is what an ordinary "add a function at the end" edit produces.
///
/// Everything else — a body edit, a pure append, a removal — leaves the surviving declarations in
/// their order, and answering `false` keeps that edit's fragment down to what it changed.
fn declaration_region_moved(old: &[&str], new: &[&str]) -> bool {
    let old_names: HashSet<&str> = old.iter().copied().collect();
    let new_names: HashSet<&str> = new.iter().copied().collect();
    if !old
        .iter()
        .filter(|n| new_names.contains(*n))
        .eq(new.iter().filter(|n| old_names.contains(*n)))
    {
        return true;
    }
    match new.iter().rposition(|n| old_names.contains(n)) {
        Some(last) => new[..last].iter().any(|n| !old_names.contains(n)),
        None => false,
    }
}

/// If `stmt` is a top-level binding whose initializer is a **reactive constructor** call —
/// `signal(…)`, `computed(…)`, `synced_signal(…)`, or `cell.new(…)` — return the bound name.
/// These are the state anchors the HMR rule preserves when their text is unchanged. Detection is
/// syntactic and conservative: an aliased constructor (`use std.reactive.signal as s`) is missed,
/// which degrades to a reset (never to unsoundness).
fn reactive_anchor_name(stmt: &Stmt) -> Option<&str> {
    let Stmt::Binding { name, value, .. } = stmt else {
        return None;
    };
    let noeta_ast::Expr::Call { callee, .. } = value else {
        return None;
    };
    let is_anchor = match callee.as_ref() {
        noeta_ast::Expr::Ident { name, .. } => {
            matches!(name.as_str(), "signal" | "computed" | "synced_signal")
        }
        noeta_ast::Expr::Member { receiver, name, .. } => {
            name == "new"
                && matches!(receiver.as_ref(),
                    noeta_ast::Expr::Ident { name, .. } if name == "cell")
        }
        _ => false,
    };
    is_anchor.then_some(name.as_str())
}

/// The top-level statements of one version, bucketed for comparison.
struct Items<'a> {
    fns: HashMap<&'a str, &'a FnDecl>,
    types: HashMap<&'a str, TypeItem<'a>>,
    /// Standalone `impl Trait for Type` declarations: `(trait, target-plus-text)` pairs, so an
    /// added, removed, retargeted, *or edited* impl all register through one set comparison.
    impls: HashSet<(String, String)>,
    /// Text of every `use` statement (idempotent to re-run; set-compared).
    uses: HashSet<&'a str>,
    /// Text of every `namespace` declaration (must be identical across versions).
    namespaces: Vec<&'a str>,
    /// Text of every other top-level statement, in source order.
    others: Vec<&'a str>,
    /// The name of every declaration [`Items::fns`] and [`Items::types`] hold, **in source order** —
    /// the sequence the reflection artifact is built in, and the one thing about a program that the
    /// name-keyed buckets above throw away. Read by [`declaration_region_moved`].
    decls: Vec<&'a str>,
}

/// A struct/class/enum declaration, viewed uniformly for diffing.
enum TypeItem<'a> {
    Struct(&'a StructDecl),
    Class(&'a ClassDecl),
    Enum(&'a EnumDecl),
}

impl<'a> TypeItem<'a> {
    fn span(&self) -> Span {
        match self {
            TypeItem::Struct(d) => d.span,
            TypeItem::Class(d) => d.span,
            TypeItem::Enum(d) => d.span,
        }
    }

    fn methods(&self) -> &'a [FnDecl] {
        match self {
            TypeItem::Struct(d) => &d.methods,
            TypeItem::Class(d) => &d.methods,
            TypeItem::Enum(d) => &d.methods,
        }
    }
}

fn classify<'a>(stmts: &'a [Stmt], src: &'a str) -> Items<'a> {
    let mut items = Items {
        fns: HashMap::new(),
        types: HashMap::new(),
        impls: HashSet::new(),
        uses: HashSet::new(),
        namespaces: Vec::new(),
        others: Vec::new(),
        decls: Vec::new(),
    };
    for stmt in stmts {
        match stmt {
            Stmt::Fn(decl) => {
                items.fns.insert(decl.name.as_str(), decl);
                items.decls.push(decl.name.as_str());
            }
            Stmt::Struct(decl) => {
                items
                    .types
                    .insert(decl.name.as_str(), TypeItem::Struct(decl));
                items.decls.push(decl.name.as_str());
            }
            Stmt::Class(decl) => {
                items
                    .types
                    .insert(decl.name.as_str(), TypeItem::Class(decl));
                items.decls.push(decl.name.as_str());
            }
            Stmt::Enum(decl) => {
                items.types.insert(decl.name.as_str(), TypeItem::Enum(decl));
                items.decls.push(decl.name.as_str());
            }
            Stmt::Impl(decl) => {
                // Key + body text: an edited impl body must register as a difference too, so the
                // text rides in the target component (compared as a set).
                items.impls.insert((
                    decl.trait_name.to_string(),
                    format!("{} :: {}", decl.target, text(src, decl.span)),
                ));
                // …and its TARGET joins the declaration sequence, because the impl's methods
                // contribute manifest rows from where the impl sits, not from where the type was
                // declared. Two standalone impls for one type are indistinguishable in this
                // sequence — swapping those two, and nothing else, is the one move it cannot see.
                items.decls.push(decl.target.as_str());
            }
            Stmt::Use { .. } => {
                items.uses.insert(text(src, stmt.span()));
            }
            Stmt::Namespace { .. } => items.namespaces.push(text(src, stmt.span())),
            // A code tier's fns (`@test fn adds…`) diff like top-level fns: a
            // test-body edit is a body-level change attributed to `adds` — the impact engine
            // narrows reruns to its callers, and a hot server doesn't re-run its top level over
            // a test tweak (the swapped block is stripped at lowering anyway). Non-fn tier items
            // and a `@doc` text body compare as ordinary top-level statements.
            Stmt::TierBlock {
                items: tier_items,
                doc_text,
                ..
            } if doc_text.is_none() => {
                for item in tier_items {
                    match item {
                        Stmt::Fn(decl) => {
                            items.fns.insert(decl.name.as_str(), decl);
                            items.decls.push(decl.name.as_str());
                        }
                        other => items.others.push(text(src, other.span())),
                    }
                }
            }
            _ => items.others.push(text(src, stmt.span())),
        }
    }
    items
}

enum FnChange {
    Unchanged,
    BodyChanged,
    SignatureChanged,
}

/// Compare one function (or method) across versions. Byte-identical text is unchanged; otherwise
/// the signature fingerprint decides body-swap vs blocker. Equal fingerprint + equal body text
/// with differing whole text is formatting-only — unchanged.
fn compare_fn(old: &FnDecl, old_src: &str, new: &FnDecl, new_src: &str) -> FnChange {
    if text(old_src, old.span) == text(new_src, new.span) {
        return FnChange::Unchanged;
    }
    if fn_signature(old, old_src) != fn_signature(new, new_src) {
        return FnChange::SignatureChanged;
    }
    if body_text(&old.body, old_src) != body_text(&new.body, new_src) {
        return FnChange::BodyChanged;
    }
    FnChange::Unchanged
}

/// The signature fingerprint: everything about a `fn` that call sites or the reflection manifest
/// may have compiled against. Attributes are included — a changed `#[Route(...)]` is a manifest
/// change, not a body edit.
fn fn_signature(decl: &FnDecl, src: &str) -> Vec<String> {
    let mut sig = vec![
        decl.name.to_string(),
        decl.is_public.to_string(),
        decl.is_async.to_string(),
    ];
    sig.extend(
        decl.type_params
            .iter()
            .map(|tp| text(src, tp.span).to_string()),
    );
    sig.extend(decl.params.iter().map(|p| text(src, p.span).to_string()));
    sig.push(
        decl.ret
            .as_ref()
            .map_or(String::new(), |r| text(src, r.span()).to_string()),
    );
    sig.extend(decl.attrs.iter().map(|a| text(src, a.span).to_string()));
    sig
}

/// The text of a statement list: the source slice from the first statement's start to the last's
/// end (comments between statements ride along identically in both versions' slices).
fn body_text<'a>(body: &[Stmt], src: &'a str) -> &'a str {
    match (body.first(), body.last()) {
        (Some(first), Some(last)) => {
            let span = Span {
                start: first.span().start,
                end: last.span().end,
                source: first.span().source,
            };
            text(src, span)
        }
        _ => "",
    }
}

/// Diff one type declaration across versions. The **residual** — the declaration's text with every
/// method's span spliced out — covers fields, variants, derives, directives, impl-block headers,
/// and the destructor: if it differs at all, the layout (or something equally whole-program) moved
/// and the swap blocks. With an identical residual, methods diff exactly like free functions.
#[allow(clippy::too_many_arguments)]
fn diff_type<'a>(
    name: &str,
    old: &TypeItem<'_>,
    old_src: &str,
    new: &TypeItem<'a>,
    new_src: &str,
    blockers: &mut Vec<SwapBlocker>,
    changed: &mut Vec<String>,
    added: &mut Vec<String>,
    removed: &mut Vec<String>,
    include: &mut HashSet<&'a str>,
) {
    if text(old_src, old.span()) == text(new_src, new.span()) {
        return;
    }
    if residual_fingerprint(old, old_src) != residual_fingerprint(new, new_src) {
        blockers.push(SwapBlocker::LayoutChanged {
            type_name: name.to_string(),
        });
        return;
    }

    let old_methods: HashMap<&str, &FnDecl> =
        old.methods().iter().map(|m| (m.name.as_str(), m)).collect();
    let mut any_change = false;
    for method in new.methods() {
        let qualified = format!("{name}.{}", method.name);
        match old_methods.get(method.name.as_str()) {
            None => {
                added.push(qualified);
                any_change = true;
            }
            Some(old_method) => match compare_fn(old_method, old_src, method, new_src) {
                FnChange::Unchanged => {}
                FnChange::BodyChanged => {
                    changed.push(qualified);
                    any_change = true;
                }
                FnChange::SignatureChanged => {
                    blockers.push(SwapBlocker::SignatureChanged { name: qualified });
                }
            },
        }
    }
    let new_names: HashSet<&str> = new.methods().iter().map(|m| m.name.as_str()).collect();
    for old_name in old_methods.keys() {
        if !new_names.contains(old_name) {
            // The old method entry stays dispatchable (append-only tables); fresh code can no
            // longer be checked against it. Recorded, not blocked — symmetric with removed fns.
            removed.push(format!("{name}.{old_name}"));
            any_change = true;
        }
    }
    if any_change {
        // Re-evaluating the whole declaration re-registers every method (unchanged ones get fresh
        // protos with identical behavior) against the SAME content-interned shape.
        match new {
            TypeItem::Struct(d) => include.insert(d.name.as_str()),
            TypeItem::Class(d) => include.insert(d.name.as_str()),
            TypeItem::Enum(d) => include.insert(d.name.as_str()),
        };
    }
}

/// The declaration's **token fingerprint** with each method's tokens removed — the
/// layout-and-everything-else check. Everything that is not a method body is
/// included by construction — fields, variants, defaults, derives, directives, type params,
/// impl-block headers, the destructor — so the check stays sound against syntax it has never
/// heard of; but it compares *tokens*, not raw text, so a comment or whitespace edit inside a
/// type declaration does not read as a layout change (a raw-text check would force a
/// state-losing restart on a doc tweak between two fields). Transitivity is free: a layout
/// change to an embedded type is a change to *that* type's own declaration, which blocks the
/// whole swap regardless of who contains it. Deliberate staleness: a **doc-comment** edit is
/// trivia to the compile path, so it swaps as "unchanged" and live reflection docstrings stay
/// stale until a code change or restart.
fn residual_fingerprint(item: &TypeItem, src: &str) -> Vec<String> {
    let decl_span = item.span();
    let decl_text = text(src, decl_span);
    let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "<residual>", decl_text);
    let lexed = noeta_lexer::lex(&source);
    // Method ranges, relative to the declaration slice the tokens were lexed against.
    let cuts: Vec<(u32, u32)> = item
        .methods()
        .iter()
        .map(|m| {
            (
                m.span.start.saturating_sub(decl_span.start),
                m.span.end.saturating_sub(decl_span.start),
            )
        })
        .filter(|(s, e)| s < e)
        .collect();
    lexed
        .tokens
        .iter()
        .filter(|t| {
            !cuts
                .iter()
                .any(|&(s, e)| t.span.start >= s && t.span.start < e)
        })
        .map(|t| decl_text[t.span.start as usize..t.span.end as usize].to_string())
        .collect()
}

fn text(src: &str, span: Span) -> &str {
    &src[span.start as usize..span.end as usize]
}

/// A one-line description of the first differing top-level statement, for the blocker report.
fn first_divergence(old: &[&str], new: &[&str]) -> String {
    for (a, b) in old.iter().zip(new.iter()) {
        if a != b {
            return format!("`{}` -> `{}`", snippet(a), snippet(b));
        }
    }
    match new.len().cmp(&old.len()) {
        std::cmp::Ordering::Greater => format!("added `{}`", snippet(new[old.len()])),
        std::cmp::Ordering::Less => format!("removed `{}`", snippet(old[new.len()])),
        std::cmp::Ordering::Equal => "top-level statements changed".to_string(),
    }
}

fn snippet(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() > 40 {
        let cut = (1..=40)
            .rev()
            .find(|&i| flat.is_char_boundary(i))
            .unwrap_or(0);
        format!("{}…", &flat[..cut])
    } else {
        flat
    }
}
