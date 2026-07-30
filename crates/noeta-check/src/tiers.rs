//! Dev-tier **activation** (object-model slice 6): resolve a program's `@<tier> { … }` blocks
//! against an *active-tier set* before the checker and the backends see it.
//!
//! A tier block is co-located developer-tooling content (`test`/`bench`/`doc`/`debug`). Whether a
//! block is compiled in is the *build target*'s call (the `noeta.toml` manifest); this module is
//! the front-end mechanism a target drives. Given the resolved active set, [`activate_tiers`]:
//!
//! - **inlines** an active code-tier block's items into the top-level statement stream, so they are
//!   checked and lowered as ordinary declarations (the block is pure grouping sugar), **stamping**
//!   the block's directive args onto each lifted fn as the tier's config attribute
//!   (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`; a per-fn attribute wins) — so the
//!   ordinary attribute construction gate validates them and the runner reads one place;
//! - **drops** an inactive block (it never reaches the checker or the IR — the strip is by
//!   construction, no DCE pass);
//! - **validates** every block's tier name against the extension-declared ∪ program-declared
//!   tier set (an unknown tier is an `E0036`, active or not — a typo must surface, not silently
//!   vanish); and
//! - **discovers** the `@test` fns it activated, so the runner finds them without a second walk.
//!
//! The *default* program path (`lang run`, the conformance differential) runs with an **empty**
//! active set and does **not** call this — those paths keep stripping inactive blocks at lowering
//! (`noeta_ir::lower`), and the checker keeps validating tier names in place. Only the test runner
//! activates a tier, so the differential is untouched by construction. The two E0036 sources (this
//! module and the checker's in-place arm) share [`unknown_tier_diagnostic`], so they never drift.

use noeta_ast::reflect::TIER_ATTR_DOC;
use noeta_ast::{AttrArg, Attribute, Name, Program, Stmt};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

use super::*;

/// A tier brought into existence by a `@tier(name[, config: T]) fn runner(…)` declaration
/// (tier-providers T2) — the program-declared counterpart of an extension's [`ExtTier`] entry
/// (`noeta_ext_abi::registry::ExtTier`).
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredTier {
    /// The tier name consumers write as `@<name> { … }`.
    pub name: String,
    /// The knob-attribute type from `config: T`, if the tier has knobs.
    pub config: Option<String>,
    /// The body language ID from `text: "<lang>"`, when the tier is a **text tier** (text-tiers
    /// arc): its blocks hold verbatim text (lexer-captured, un-parsed) tagged with this language
    /// for tooling. `None` for a code tier. Mutually exclusive with `config` (E0051).
    pub text: Option<String>,
    /// The block-value type from `expr: T`, when the tier is an **expression tier** (expr-tiers
    /// arc): its `@<name> { … }` blocks are expressions, desugared during activation to a call of
    /// the handler (`runner` doubles as the handler name). Mutually exclusive with `config`
    /// (E0051); no runner semantics (`noeta <tier>` rejects it, blocks never activate/strip).
    pub expr: Option<String>,
    /// The runner fn's (possibly link-qualified) name — what dispatch invokes with the roots.
    /// For an expression tier this is the **handler** the desugar calls instead.
    pub runner: String,
    /// The declaring **package root** — the runner's first qualified segment (`fuzzkit` for
    /// `fuzzkit.tiers.run_fuzz`), or `""` for an entry-local declaration. This is the provider
    /// identity a target's `tiers` map selects (`bench = "fuzzkit"`).
    pub root: String,
    /// The `@tier` directive's span, for diagnostics.
    pub span: Span,
}

/// The tier name-space a program sees: the built-in four plus every `@tier` declaration in the
/// (linked) program — imported packages' declarations included, since linking merges their
/// modules. The one lookup activation, the checker's in-place `TierBlock` arm, and the CLI's
/// runner dispatch all resolve against.
#[derive(Debug, Clone, Default)]
pub struct TierRegistry {
    /// Declarations keyed by tier name. Several packages may declare the same tier name (each a
    /// distinct **provider**, told apart by [`DeclaredTier::root`]); a target's `tiers` map picks
    /// which one is live. In declaration order per name.
    declared: std::collections::HashMap<String, Vec<DeclaredTier>>,
    /// The extension registry the **extension-tier** half of the name-space resolves against
    /// (instance-registry IR4): `None` (the default) uses the process-global default registry — the
    /// single-registry CLI/IDE/MCP path — while an embed session whose own extension declares a
    /// `@tier` threads its assembled set via [`TierRegistry::collect_with_registry`]. Read through
    /// [`TierRegistry::reg`]; `Option` so `#[derive(Default)]` (a `&'static` has no default) holds.
    registry: Option<&'static noeta_ext_abi::registry::Registry>,
}

impl PartialEq for TierRegistry {
    /// Equality is over the program-declared tiers only — the `registry` is ambient resolution
    /// context (a `&'static` selector), not part of a name-space's value identity, and `Registry`
    /// is not `PartialEq`. Two registries with the same declarations compare equal regardless of
    /// which extension set backs their extension-tier half.
    fn eq(&self, other: &TierRegistry) -> bool {
        self.declared == other.declared
    }
}

/// Every declaration a tier can attach to — what a program-declared `@tier` (which has no site
/// syntax) permits.
pub(crate) const ANY_DECLARATION: &[noeta_ext_abi::registry::TierSite] = &[
    noeta_ext_abi::registry::TierSite::Function,
    noeta_ext_abi::registry::TierSite::Method,
    noeta_ext_abi::registry::TierSite::Type,
    noeta_ext_abi::registry::TierSite::Trait,
];

/// Who provides a tier under a given provider selection — the extension declaration (native
/// runner for the built-ins) or a program/package `@tier` declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedProvider<'a> {
    Extension,
    Declared(&'a DeclaredTier),
}

/// The declaring package root of a link-qualified fn name (`fuzzkit` for
/// `fuzzkit.tiers.run_fuzz`; `""` for an unqualified entry-local name).
fn runner_root(qualified: &str) -> String {
    match qualified.rsplit_once('.') {
        Some((path, _)) => path.split('.').next().unwrap_or("").to_string(),
        None => String::new(),
    }
}

/// The **canonical identity** of a tier: the provider's namespace root and the name that provider
/// exported (a std tier is `("std", "test")`; a dependency `fuzzkit`'s is `("fuzzkit", "fuzz")`). Two
/// `@name` occurrences — possibly under *different local names in different packages* — denote the
/// same tier iff their identities are equal. This is what activation membership and every built-in
/// tier's special behavior key on, so a renamed tier (`@crit` bound to `criterion:bench`) is judged
/// by what it *is*, not the local spelling — the de-hardcoding of `active.contains("test")`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TierId {
    pub root: String,
    pub exported: String,
}

impl TierId {
    /// A std tier's identity — `("std", name)`. The provider the built-in four resolve to.
    pub fn std(name: &str) -> TierId {
        TierId {
            root: BUILTIN_PROVIDER.to_string(),
            exported: name.to_string(),
        }
    }
}

/// The built-in tier provider root (`"std"`), mirroring `noeta_pm`'s `BUILTIN_PROVIDER` — the syntax
/// crates sit beneath the package manager, so the string is pinned here and a test keeps the two equal.
pub(crate) const BUILTIN_PROVIDER: &str = "std";

/// A `@name` resolved to a concrete declaration — an extension tier (carrying its provider root) or a
/// program `@tier`. The property accessors ([`Self::config`], [`Self::sites`], …) read from whichever
/// half, so every consumer resolves once and then asks the resolved tier, rather than re-resolving a
/// bare name against the global name-space at each query.
#[derive(Debug, Clone)]
pub enum ResolvedTier<'a> {
    /// An extension-declared tier and the namespace root of the unit that declares it.
    Ext(&'a noeta_ext_abi::registry::ExtTier, String),
    /// A program `@tier(…)` declaration.
    Declared(&'a DeclaredTier),
}

impl ResolvedTier<'_> {
    /// The canonical identity — `(provider root, exported name)`.
    pub fn id(&self) -> TierId {
        match self {
            ResolvedTier::Ext(t, root) => TierId {
                root: root.clone(),
                exported: t.name.to_string(),
            },
            ResolvedTier::Declared(d) => TierId {
                root: d.root.clone(),
                exported: d.name.clone(),
            },
        }
    }

    /// The config knob-attribute name, if the tier carries one.
    pub fn config(&self) -> Option<&str> {
        match self {
            ResolvedTier::Ext(t, _) => t.config,
            ResolvedTier::Declared(d) => d.config.as_deref(),
        }
    }

    /// The declaration sites the tier permits (an extension tier's registered `sites`; a program
    /// `@tier` attaches to any declaration).
    pub fn sites(&self) -> &'static [noeta_ext_abi::registry::TierSite] {
        match self {
            ResolvedTier::Ext(t, _) => t.sites,
            ResolvedTier::Declared(_) => ANY_DECLARATION,
        }
    }

    /// Whether the tier is an **expression tier** (its `@name { … }` block is a value).
    pub fn is_expr(&self) -> bool {
        match self {
            ResolvedTier::Ext(t, _) => t.expr.is_some(),
            ResolvedTier::Declared(d) => d.expr.is_some(),
        }
    }
}

impl TierRegistry {
    /// Collect every `@tier` declaration from `program`'s top-level fns, in source order per tier
    /// name (the checker reports same-provider duplicates as E0051; collection keeps everything so
    /// provider selection stays total).
    pub fn collect(program: &Program) -> TierRegistry {
        TierRegistry::collect_with_registry_opt(program, None)
    }

    /// As [`TierRegistry::collect`], but resolving the **extension-tier** half of the name-space
    /// against an explicit `registry` (instance-registry IR4) — so an embed session whose own
    /// extension declares a `@tier` validates its `@<tier>` blocks against *its* registry, not the
    /// process-global default. The checker builds its `tier_registry` this way from its own registry.
    pub fn collect_with_registry(
        program: &Program,
        registry: &'static noeta_ext_abi::registry::Registry,
    ) -> TierRegistry {
        TierRegistry::collect_with_registry_opt(program, Some(registry))
    }

    fn collect_with_registry_opt(
        program: &Program,
        registry: Option<&'static noeta_ext_abi::registry::Registry>,
    ) -> TierRegistry {
        let mut declared: std::collections::HashMap<String, Vec<DeclaredTier>> =
            std::collections::HashMap::new();
        for stmt in &program.stmts {
            if let Stmt::Fn(f) = stmt
                && let Some(t) = &f.tier
            {
                declared
                    .entry(t.name.clone())
                    .or_default()
                    .push(DeclaredTier {
                        name: t.name.clone(),
                        config: t.config.as_ref().map(|(n, _)| n.to_string()),
                        text: t.text.as_ref().map(|(lang, _)| lang.clone()),
                        expr: t.expr.as_ref().map(|(ty, _)| ty.to_string()),
                        runner: f.name.to_string(),
                        root: runner_root(f.name.as_str()),
                        span: t.span,
                    });
            }
        }
        TierRegistry { declared, registry }
    }

    /// The extension registry this name-space's extension tiers resolve against — the threaded one,
    /// or the process-global default (instance-registry IR4).
    /// The extension registry this name-space resolves against — public so the directive registry
    /// composed over this one can reach the extension-declared `@`-directives.
    pub fn registry(&self) -> &'static noeta_ext_abi::registry::Registry {
        self.reg()
    }

    fn reg(&self) -> &'static noeta_ext_abi::registry::Registry {
        self.registry
            .unwrap_or_else(noeta_ext_abi::registry::single_registry_process)
    }

    /// Whether `tier` names a known tier — extension-declared or program-declared.
    pub fn is_known(&self, tier: &str) -> bool {
        self.reg().find_ext_tier(tier).is_some() || self.declared.contains_key(tier)
    }

    /// The declaration sites `tier` permits (directive attachment-site model). An extension-declared
    /// tier carries its registered `sites`; a program-declared `@tier` (no site syntax yet) and an
    /// unknown name yield the empty slice — **unrestricted**, so the checker's site gate never fires
    /// on them.
    pub fn sites(&self, tier: &str) -> &'static [noeta_ext_abi::registry::TierSite] {
        if let Some(t) = self.reg().find_ext_tier(tier) {
            return t.sites;
        }
        if self.declared.contains_key(tier) {
            // A program `@tier` declaration has no site syntax yet, so it attaches to any
            // declaration. Stated explicitly: an empty slice now means "attaches to nothing", so
            // falling through to one would reject `@fuzz fn f()` for every program-declared tier.
            return ANY_DECLARATION;
        }
        // An unknown tier — E0036 elsewhere. No attachment claim to make.
        &[]
    }

    /// The extension-declared half of the name-space: every installed extension's [`ExtTier`]
    /// (`test`/`bench`/`doc`/`debug` plus any native package's tiers), resolved against this
    /// registry's extension set. With [`TierRegistry::declared_tiers`] this enumerates exactly the
    /// names [`TierRegistry::is_known`] accepts — what IDE completion offers after `@`.
    pub fn extension_tiers(
        &self,
    ) -> impl Iterator<Item = &'static noeta_ext_abi::registry::ExtTier> + '_ {
        self.reg().ext_tiers()
    }

    /// The program-declared half of the name-space: every `@tier` declaration collected from the
    /// (linked) program, one entry per declaration (a name several packages provide appears once
    /// per provider). Counterpart of [`TierRegistry::extension_tiers`].
    pub fn declared_tiers(&self) -> impl Iterator<Item = &DeclaredTier> {
        self.declared.values().flat_map(|v| v.iter())
    }

    /// The **default-provider** declaration for `tier`: the first program/package declaration —
    /// what resolution falls back to when no target selects a provider and no extension declares
    /// the name. `None` for a purely-extension tier.
    pub fn declared(&self, tier: &str) -> Option<&DeclaredTier> {
        self.declared.get(tier).and_then(|v| v.first())
    }

    /// The declaration of `tier` from the package rooted at `root`, if any — the lookup a
    /// target's explicit provider selection uses.
    pub fn declared_by(&self, tier: &str, root: &str) -> Option<&DeclaredTier> {
        self.declared.get(tier)?.iter().find(|d| d.root == root)
    }

    /// Resolve a `@local` written by the package at `origin` to a concrete [`ResolvedTier`] — the heart
    /// of per-package tier resolution (per-package naming arc). **A `[tiers]` binding wins**: the
    /// package's own local `@name` → the provider it named + the tier that provider exported (so a
    /// rename or a third-party provider resolves to exactly what the manifest declared). **Otherwise the
    /// name resolves ambiently** — a std extension tier or a program `@tier` of that bare name — which
    /// is what a bare script (no manifest) and any unbound built-in use rely on. A name bound to a
    /// provider that declares no such tier is unresolved (`None`; the caller raises the diagnostic).
    ///
    /// `origin` is the span's package (`packages.at(span)`); `None` (unknown provenance, e.g. a
    /// single-file check with no manifest) skips the binding step and resolves ambiently.
    pub fn resolve_at<'a>(
        &'a self,
        local: &str,
        origin: Option<&noeta_span::PackageOrigin>,
        uses: &noeta_span::PackageUses,
    ) -> Option<ResolvedTier<'a>> {
        if let Some(o) = origin
            && let Some(u) = uses.get(o, local)
        {
            // Bound: the provider_root(s) the local name maps to. Find the one that actually declares
            // the exported tier (a scope dependency's key covers several roots; the common case is one).
            for root in &u.provider_roots {
                let one = std::slice::from_ref(root);
                if let Some(t) = self.reg().find_ext_tier_scoped(one, &u.exported) {
                    return Some(ResolvedTier::Ext(t, root.clone()));
                }
                if let Some(d) = self.declared_by(&u.exported, root) {
                    return Some(ResolvedTier::Declared(d));
                }
            }
            return None;
        }
        // Ambient (no `[tiers]` binding for this name). With **no binding regime in effect at all**
        // — an empty table: a bare script, an embed session, a single-file check — resolve GLOBALLY
        // (any provider root), the tier counterpart of the directive ambient fallback
        // (`resolve_ext_directive_at`). Without it a session extension's own tier (root ≠ `"std"`,
        // with no manifest to bind it in) reads as unknown. When a binding regime IS in effect, a
        // third-party extension tier stays reachable **only** through a binding, so ambient is
        // std-scoped there (the per-package opt-in). A program `@tier` of the bare name resolves either way.
        if uses.is_empty() {
            if let Some(t) = self.reg().find_ext_tier(local) {
                let root = self
                    .reg()
                    .ext_tier_root(local)
                    .unwrap_or(BUILTIN_PROVIDER)
                    .to_string();
                return Some(ResolvedTier::Ext(t, root));
            }
        } else {
            let std_root = [BUILTIN_PROVIDER.to_string()];
            if let Some(t) = self.reg().find_ext_tier_scoped(&std_root, local) {
                return Some(ResolvedTier::Ext(t, BUILTIN_PROVIDER.to_string()));
            }
        }
        self.declared(local).map(ResolvedTier::Declared)
    }

    /// The canonical [`TierId`] a `@local` at `origin` resolves to (see [`Self::resolve_at`]).
    pub fn resolve_id(
        &self,
        local: &str,
        origin: Option<&noeta_span::PackageOrigin>,
        uses: &noeta_span::PackageUses,
    ) -> Option<TierId> {
        self.resolve_at(local, origin, uses).map(|r| r.id())
    }

    /// Resolve who provides `tier` under `providers` (a target's tier → provider map; empty ⇒ no
    /// target). Explicit `"std"` selects the extension declaration; an explicit dependency key
    /// selects that package's `@tier` declaration; no entry falls back to the extension
    /// declaration if one exists, else the first program declaration. `Err` is the human-readable
    /// mismatch (a provider that declares no such tier) the caller reports.
    pub fn resolve_provider<'a>(
        &'a self,
        tier: &str,
        providers: &std::collections::BTreeMap<String, String>,
    ) -> Result<ResolvedProvider<'a>, String> {
        match providers.get(tier).map(String::as_str) {
            Some("std") => {
                if self.reg().find_ext_tier(tier).is_some() {
                    Ok(ResolvedProvider::Extension)
                } else {
                    Err(format!(
                        "tier `{tier}` is mapped to provider `std`, but no installed extension \
                         declares it"
                    ))
                }
            }
            Some(root) => self
                .declared_by(tier, root)
                .map(ResolvedProvider::Declared)
                .ok_or_else(|| {
                    format!(
                        "tier `{tier}` is mapped to provider `{root}`, but `{root}` declares no \
                     `@tier({tier})` — is the dependency imported (`use {root}.…`)?"
                    )
                }),
            None => {
                if self.reg().find_ext_tier(tier).is_some() {
                    Ok(ResolvedProvider::Extension)
                } else if let Some(d) = self.declared(tier) {
                    Ok(ResolvedProvider::Declared(d))
                } else {
                    Err(format!("unknown dev-tier `{tier}`"))
                }
            }
        }
    }

    /// The tier's config attribute under `providers` — the extension declaration's or the
    /// selected `@tier` directive's `config:`. Falls back to default resolution on a provider
    /// mismatch (the mismatch itself is reported where the provider map is validated).
    pub fn config_attribute_for(
        &self,
        tier: &str,
        providers: &std::collections::BTreeMap<String, String>,
    ) -> Option<String> {
        match self.resolve_provider(tier, providers) {
            Ok(ResolvedProvider::Extension) => self
                .reg()
                .find_ext_tier(tier)
                .and_then(|t| t.config)
                .map(str::to_string),
            Ok(ResolvedProvider::Declared(d)) => d.config.clone(),
            Err(_) => self.config_attribute(tier).map(str::to_string),
        }
    }

    /// The body language of `tier` when it declares one — from the extension declaration (`doc` →
    /// `"markdown"`, a native `@json` → `"json"`) or a program `@tier(…, text: "<lang>")`, `None`
    /// for a code tier. This is the language editor injection colors the body as and the LSP
    /// reports on hover; decoupled from the tier name (text-tiers design). Extension declarations
    /// win (a built-in name is not shadowable). Reads the first program declaration (text and
    /// expression tiers are single-provider today).
    pub fn text_lang(&self, tier: &str) -> Option<&str> {
        if let Some(lang) = self.reg().find_ext_tier(tier).and_then(|t| t.text) {
            return Some(lang);
        }
        self.declared(tier).and_then(|d| d.text.as_deref())
    }

    /// The value type of `tier` when it is an **expression tier** (expr-tiers arc) — the `expr: T`
    /// its `@<name> { … }` blocks evaluate to — from an extension declaration or a program
    /// `@tier(…, expr: T)`, else `None`. Surfaced by the LSP alongside [`Self::text_lang`] so
    /// hovering an embedded block reports both its language and its type.
    pub fn expr_type(&self, tier: &str) -> Option<&str> {
        if let Some(ty) = self.reg().find_ext_tier(tier).and_then(|t| t.expr) {
            return Some(ty);
        }
        self.declared(tier).and_then(|d| d.expr.as_deref())
    }

    /// Whether `tier` is an **expression tier** (expr-tiers arc) — extension-declared or
    /// program-declared. The single predicate the checker's E0052 statement-position guard and
    /// the CLI's `noeta <tier>` dispatch use, so both cover native and program tiers alike.
    pub fn is_expr_tier(&self, tier: &str) -> bool {
        self.reg()
            .find_ext_tier(tier)
            .is_some_and(|t| t.expr.is_some())
            || self.declared(tier).is_some_and(|d| d.expr.is_some())
    }

    /// The handler an **expression tier**'s `@<name> { … }` block desugars to — a program tier's
    /// `@tier` fn (`DeclaredTier::runner`, a named Noeta function) or an extension tier's native
    /// module function (`ExtTier::handler`). `None` if `tier` is not an expression tier (or an
    /// extension expr tier omitted its handler, a misconfiguration). The single resolution point
    /// the checker's typing and IR lowering both consult, so native and program expr tiers desugar
    /// through the identical `Call` path.
    pub fn expr_tier_handler(&self, tier: &str) -> Option<noeta_ast::desugar::ExprTierHandler> {
        use noeta_ast::desugar::ExprTierHandler;
        if let Some(t) = self.reg().find_ext_tier(tier).filter(|t| t.expr.is_some()) {
            return t.handler.map(ExprTierHandler::from_native_path);
        }
        self.declared(tier)
            .filter(|d| d.expr.is_some())
            .map(|d| ExprTierHandler::Program(d.runner.clone()))
    }

    /// Every declared **verbatim-body** tier's name — text tiers *and* expression tiers, both of
    /// whose `@<name> { … }` bodies the lexer captures un-parsed (`noeta_lexer::TextTiers::with`).
    /// (The live pipeline drives capture off the lexer's own `text_tier_decls` scan, which keys on
    /// `text:`/`expr:` identically; this registry-side twin must match, so it includes both.)
    pub fn text_tier_names(&self) -> impl Iterator<Item = &str> {
        self.declared
            .values()
            .flatten()
            .filter(|d| d.text.is_some() || d.expr.is_some())
            .map(|d| d.name.as_str())
    }

    /// The tier's config attribute under **default** resolution (no target) — the extension
    /// declaration's for an extension tier, the first `@tier` directive's `config:` for a
    /// program-declared one.
    pub fn config_attribute<'a>(&'a self, tier: &str) -> Option<&'a str> {
        self.reg()
            .find_ext_tier(tier)
            .and_then(|t| t.config)
            .or_else(|| self.declared(tier).and_then(|d| d.config.as_deref()))
    }

    /// The `E0037` for directive arguments on a tier that has no knob attribute (`@test(x)` —
    /// `test` takes no arguments) under **default** provider resolution, or `None` when the args
    /// are acceptable at this level. Args on a knob-carrying tier are *not* validated here — they
    /// construct the tier's config attribute, checked by the ordinary attribute construction gate
    /// (in place by the checker's `TierBlock` arm on the default path; on the stamped fns when
    /// activated, where the resolution is provider-aware).
    pub fn knobless_args_diagnostic(&self, tier: &str, args: &[AttrArg]) -> Option<Diagnostic> {
        if self.config_attribute(tier).is_some() {
            return None;
        }
        knobless_args_diagnostic_for(tier, args)
    }
}

/// The `E0037` for directive args on a tier resolved to no config attribute — the shared message
/// both the default-resolution wrapper and the provider-aware activation path emit.
pub(crate) fn knobless_args_diagnostic_for(tier: &str, args: &[AttrArg]) -> Option<Diagnostic> {
    let span = args.first().map(|a| a.span)?;
    Some(
        Diagnostic::error(
            DiagnosticCode::InvalidDirectiveArgument,
            span,
            format!("tier `@{tier}` takes no arguments"),
        )
        .with_help("a tier's knobs come from its config attribute (`@bench`'s is `Bench { iterations: int }`; a declared tier's is its `@tier(…, config: T)`)"),
    )
}

/// The attribute a tier block's directive args construct, stamped at the block's spans — the
/// desugar `@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`. Shared by the stamping in
/// [`activate_tiers`] and the checker's in-place validation of an inactive block, so the two paths
/// check the identical construction.
pub fn synthesized_config_attr(attr_name: &str, args: &[AttrArg], tier_span: Span) -> Attribute {
    Attribute {
        name: Name::canonical(attr_name),
        name_span: tier_span,
        args: args.to_vec(),
        span: tier_span,
    }
}

/// Every installed extension's declared attributes as reflection [`TypeInfo`]s (tier-extensions
/// port) — the materialization shapes `attributes_of` needs for an attribute that has no AST
/// declaration. [`extend_reflection`] embeds these into the reflection artifact at compile time,
/// so the backends materialize an extension attribute exactly as a program-declared one; the
/// registry declaration is the single source (the old hardcoded `builtin_attribute_shape`
/// fallback is gone).
pub fn extension_attribute_types() -> Vec<noeta_ast::reflect::TypeInfo> {
    use noeta_ext_abi::registry as ext;
    let data_only = ext::ext_attributes().map(|attr| noeta_ast::reflect::TypeInfo {
        // The **qualified** identity (`std.test.Skip`) — the manifest shape `attributes_of`
        // matches must key on the same FQN the loader rewrites applications to (D2b).
        name: attr.qualified(),
        kind: noeta_ast::reflect::TypeKind::Struct,
        fields: attr.fields.iter().map(|f| f.name.to_string()).collect(),
        // The field's declared type as a reflection `TypeRepr` (struct-reflection arc), so
        // `field_specs_of` reports a data-only native attribute's field types precisely.
        field_types: attr
            .fields
            .iter()
            .map(|f| match f.ty {
                ext::AttrFieldType::Int => noeta_ast::reflect::TypeRepr::Int,
                ext::AttrFieldType::Str => noeta_ast::reflect::TypeRepr::Str,
                ext::AttrFieldType::Dyn => noeta_ast::reflect::TypeRepr::Dyn,
            })
            .collect(),
        // Optional iff the field carries a literal default (`Skip.reason = ""`).
        field_optional: attr.fields.iter().map(|f| f.default.is_some()).collect(),
        field_defaults: attr
            .fields
            .iter()
            .map(|f| {
                f.default.map(|d| match d {
                    ext::AttrFieldDefault::Str(s) => noeta_ast::AttrValue::Str(s.to_string()),
                    ext::AttrFieldDefault::Int(n) => noeta_ast::AttrValue::Int(n),
                })
            })
            .collect(),
        variants: Vec::new(),
    });
    // A native **fielded** `@attribute` struct (D2) — a real `ExtFielded` carrying
    // `ExtTypeDirective::Attribute` — is an attribute to every consumer, so its shape must reach the
    // same reflection manifest the data-only `ExtAttribute`s do. Without this, `attribute_shape`
    // finds no `TypeInfo` for the fielded attribute and `attributes_of::<Route>()` materializes an
    // empty instance. Assembly guarantees a fielded `@attribute` is `Struct`-kind. `ExtField` carries
    // no literal default, so every field is mandatory (`None`) — the E0009 construction check ensures
    // each is supplied at the application site, exactly as for a data-only attribute's mandatory field.
    let fielded = ext::ext_fielded_attributes().map(fielded_type_info);
    data_only.chain(fielded).collect()
}

/// Every installed extension's declared **fielded types** — native classes and native value structs
/// — as reflection [`TypeInfo`]s. The fielded twin of [`extension_enum_types`], and what makes
/// `field_specs_of("std.http.Frame")` report a native type's four fields instead of the empty list
/// that, paired with `variants_of`'s empty one, means "I know nothing about this name".
///
/// Only the fielded types that are `@attribute`s used to be seeded (by [`extension_attribute_types`],
/// which needed their shapes to materialize an attribute instance), so a native type that is simply a
/// *type* was skipped — though its `ExtField` list carries exactly the `SigType`s the attribute arm
/// already projected. Both arms now go through one [`fielded_type_info`], so an attribute and a plain
/// native type cannot come to report their fields differently.
///
/// **Seeding this does not make a native type falsely constructible, and that was measured rather
/// than assumed.** A seeded `TypeInfo` is also what makes `construct(name, …)` resolve, and the
/// hazard this was split off for was `construct` minting a native class with no native state behind
/// it. It cannot: an [`ExtField`](noeta_ext_abi::registry::ExtField) carries no literal default, so
/// every field of a native type is **mandatory**, and the shared `plan_construct` /
/// `plan_construct_named` refuse a construction that omits a mandatory field — `construct("fx.Handle",
/// {})` is `Err("missing required field `guard` of `fx.Handle`")`, not an empty `Handle`. Supplying
/// the field means supplying a real extern handle obtained from native code, which is exactly the
/// requirement a source-written `Handle { guard: g }` literal has (both backends already register
/// every native fielded type as constructible, so the source literal was always the same operation).
/// So reflection reporting the schema and `construct` accepting a value are two views of one
/// declaration here, which is the invariant the type-level queries exist to keep — not two
/// permissions that had to be granted separately.
pub fn extension_fielded_types() -> Vec<noeta_ast::reflect::TypeInfo> {
    use noeta_ext_abi::registry as ext;
    let Some(reg) = ext::default_registry() else {
        return Vec::new();
    };
    reg.fielded().map(fielded_type_info).collect()
}

/// One native fielded type as a reflection [`TypeInfo`] — the single projection both
/// [`extension_attribute_types`]'s fielded arm and [`extension_fielded_types`] read.
///
/// The kind comes from the declaration's own [`FieldedKind`](noeta_ext_abi::FieldedKind), so a native
/// class reflects as `Class` and a native value struct as `Struct` — the same discriminant the
/// compiler's constructible-type record and the checker's `type_kinds` take, so a consumer branching
/// on kind sees what the rest of the language sees. (A fielded `@attribute` is `Struct`-kind by
/// assembly, so the attribute arm's shape is unchanged by sharing this.)
///
/// Field types come from the same `SigType` signature vocabulary the checker seeds into
/// `symbols.records`, projected through [`sig_type_to_repr`]. An `ExtField` carries no literal
/// default, so every field is mandatory (`optional: false`, `default: None`) — for an attribute that
/// is what makes the E0009 construction check require each one at the application site, and for a
/// plain native type it is what makes a `construct` that omits one a refusal.
fn fielded_type_info(f: &noeta_ext_abi::registry::ExtFielded) -> noeta_ast::reflect::TypeInfo {
    use noeta_ext_abi::NominalType;
    noeta_ast::reflect::TypeInfo {
        name: f.qualified(),
        kind: match f.kind {
            noeta_ext_abi::FieldedKind::Class => noeta_ast::reflect::TypeKind::Class,
            noeta_ext_abi::FieldedKind::Struct => noeta_ast::reflect::TypeKind::Struct,
        },
        fields: f
            .fields
            .iter()
            .map(|field| field.name.to_string())
            .collect(),
        field_types: f
            .fields
            .iter()
            .map(|field| sig_type_to_repr(&field.ty))
            .collect(),
        field_optional: f.fields.iter().map(|_| false).collect(),
        field_defaults: f.fields.iter().map(|_| None).collect(),
        variants: Vec::new(),
    }
}

/// Project a registry [`SigType`](noeta_ext_abi::registry::SigType) onto its reflection
/// [`TypeRepr`](noeta_ast::reflect::TypeRepr) — the type-level counterpart of the checker's
/// [`sig_to_typeref`](crate::stdlib::sig_to_typeref)/[`sig_to_type`](crate::stdlib::sig_to_type),
/// so a native fielded type's field types reflect through `field_specs_of` the same way a `.noe`
/// struct's do. A polymorphic/variable position has no declaration-site type and becomes `Dyn`; a
/// trailing-optional wrapper is an arity marker and unwraps to its inner type.
///
/// A **nominal** resolves through the registry to the identity and kind a *value* of that type
/// carries ([`nominal_to_repr`]), which is a correction this projection needed the moment
/// `extension_param_records` started reporting native signatures: a registry signature spells a
/// nominal by its **short** name (`Named("Uuid")`), while `type_of` on one of its values reports the
/// qualified identity (`Type.Named(std.id.Uuid, [])`). So `returns_of("std.id.uuid")` said
/// `Type.Named(Uuid, [])` about a value that says `Type.Named(std.id.Uuid, [])` — one type, two
/// names, from the two queries the docs promise share a decoder, and a framework matching a declared
/// return against a runtime tag would have missed on every native type.
fn sig_type_to_repr(sig: &noeta_ext_abi::registry::SigType) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    use noeta_ext_abi::registry::SigType;
    let boxed = |s: &SigType| Box::new(sig_type_to_repr(s));
    match sig {
        SigType::Int => TypeRepr::Int,
        SigType::Float => TypeRepr::Float,
        SigType::F32 => TypeRepr::F32,
        SigType::Bool => TypeRepr::Bool,
        SigType::String => TypeRepr::Str,
        SigType::Bytes => TypeRepr::Bytes,
        SigType::Unit => TypeRepr::Unit,
        SigType::Dyn => TypeRepr::Dyn,
        SigType::Never => TypeRepr::Never,
        SigType::List(t) => TypeRepr::List(boxed(t)),
        SigType::Option(t) => TypeRepr::Option(boxed(t)),
        SigType::Map(k, v) => TypeRepr::Map(boxed(k), boxed(v)),
        SigType::Result(ok, err) => TypeRepr::Result(boxed(ok), boxed(err)),
        SigType::Future(t) => TypeRepr::Named("Future".to_string(), vec![sig_type_to_repr(t)]),
        SigType::Named(n) => nominal_to_repr(n, Vec::new()),
        SigType::Generic(n, args) => {
            nominal_to_repr(n, args.iter().map(sig_type_to_repr).collect())
        }
        SigType::Union(members) => TypeRepr::Union(members.iter().map(sig_type_to_repr).collect()),
        SigType::Fn(params, ret) => {
            TypeRepr::Fn(params.iter().map(sig_type_to_repr).collect(), boxed(ret))
        }
        // A trailing-optional parameter is an arity marker, not a value type (as `sig_to_typeref`
        // treats it) — reflect the inner type.
        SigType::Optional(inner) => sig_type_to_repr(inner),
        // A signature-level type variable has no declaration-site type — a permissive hole.
        SigType::Var(_) | SigType::BoundedVar(_, _) => TypeRepr::Dyn,
        // A trait associated-type projection (`Self::Wide`, slice 1b) is resolved per-implementor by
        // the checker, not at the declaration site — a permissive hole in a reflected signature.
        SigType::Assoc(_) => TypeRepr::Dyn,
        // `Self` is likewise receiver-relative: a reflected signature has no receiver to resolve it
        // against, so it reflects as the same permissive hole rather than a fabricated nominal type.
        SigType::SelfTy => TypeRepr::Dyn,
        // "Any number" has no single reflected type — enumerating twelve members here would say
        // less than the hole does, since reflection consumers read a shape, not a constraint.
        SigType::Numeric => TypeRepr::Dyn,
    }
}

/// One nominal name out of a registry signature as a reflection [`TypeRepr`] under the **identity**
/// the rest of the language knows that type by — its qualified `namespace.name`, resolved through the
/// installed registry.
///
/// A registry signature spells a nominal by the short name its own extension knows it under
/// (`Named("Uuid")`, `Named("Framing")`), but identity is the qualified name: it is what `type_of`
/// stamps on a value, what `field_specs_of` / `variants_of` are keyed on, and what a `.noe`
/// annotation of the same type reflects as once the loader has qualified it. Without this resolution
/// `returns_of("std.id.uuid")` said `Type.Named(Uuid, [])` about a value that says
/// `Type.Named(std.id.Uuid, [])` — one type under two names, from the two queries the reflection docs
/// promise share a decoder, so a framework matching a declared return against a runtime tag missed on
/// every native type.
///
/// Kind-**agnostic** [`TypeRepr::Named`], deliberately, and for the same reason: that is exactly what
/// a `.noe` declaration of this type reflects as (`fn f(): Framing` → `Type.Named(std.http.Framing,
/// [])`, the documented spelling of a declared nominal annotation), so one type in a declared position
/// reads the same however it was declared. Classifying the native side into `Enum`/`Struct`/`Class`
/// would report the *value* channel's spelling in the *declaration* channel and make a consumer that
/// branches on `Type.Named(n, _)` miss precisely the native declarations.
///
/// A name the registry does not resolve keeps its bare spelling (the synthesized `Future` wrapper, a
/// third-party name registered elsewhere): inventing a namespace for it would fabricate an identity,
/// which is the failure this resolution exists to prevent.
fn nominal_to_repr(
    name: &str,
    args: Vec<noeta_ast::reflect::TypeRepr>,
) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    use noeta_ext_abi::NominalType;
    use noeta_ext_abi::registry as ext;
    let qualified = ext::default_registry().and_then(|reg| {
        reg.resolve_enum(name)
            .map(|t| t.qualified())
            .or_else(|| reg.resolve_fielded(name).map(|t| t.qualified()))
            .or_else(|| reg.resolve_type(name).map(|t| t.qualified()))
            .or_else(|| reg.resolve_trait(name).map(|t| t.qualified()))
    });
    TypeRepr::Named(qualified.unwrap_or_else(|| name.to_string()), args)
}

/// Every installed extension's declared **enums** as reflection [`TypeInfo`]s — the native twin of
/// [`extension_attribute_types`], and of `noeta_ast::reflect::prelude_type_infos`.
///
/// A native enum reaches a program under its qualified identity (`std.http.Framing`), which is what
/// `type_of` stamps on one of its values and therefore the key a consumer probes with; so that is
/// what this keys on, exactly as the attribute projection keys on the attribute's FQN. Payload slot
/// names are synthesized positionally (`_0`, `_1`, …) — the same convention the compiler's
/// `ext_enum_type_info` and a prelude variant use, and for the same reason: a native payload is
/// positional, so only the slot *count* and the declared types are load-bearing. A **backed**
/// native enum's per-variant constant rides through as the variant's `backing`, so `variants_of`
/// reports the wire values a schema derived from it must emit.
pub fn extension_enum_types() -> Vec<noeta_ast::reflect::TypeInfo> {
    use noeta_ext_abi::registry as ext;
    let Some(reg) = ext::default_registry() else {
        return Vec::new();
    };
    reg.enums()
        .map(|en| noeta_ast::reflect::TypeInfo {
            name: en.qualified(),
            kind: noeta_ast::reflect::TypeKind::Enum,
            fields: Vec::new(),
            field_types: Vec::new(),
            field_optional: Vec::new(),
            field_defaults: Vec::new(),
            variants: en
                .variants
                .iter()
                .map(|v| noeta_ast::reflect::VariantInfo {
                    name: v.name.to_string(),
                    fields: (0..v.fields.len()).map(|i| format!("_{i}")).collect(),
                    field_types: v.fields.iter().map(sig_type_to_repr).collect(),
                    backing: match v.value {
                        ext::VariantValue::None => None,
                        ext::VariantValue::Str(s) => Some(noeta_ast::AttrValue::Str(s.to_string())),
                        ext::VariantValue::Int(n) => Some(noeta_ast::AttrValue::Int(n)),
                    },
                })
                .collect(),
        })
        .collect()
}

/// Every installed extension's declared **callables** as reflection
/// [`ParamRecord`](noeta_ast::reflect::ParamRecord)s — the signature twin of
/// [`extension_enum_types`], and what makes `params_of` / `returns_of` answer for a native function
/// or method instead of reporting a shipped stdlib callable as a typo.
///
/// `returns_of`'s `none` is documented to mean *this target names no known callable* — chosen
/// deliberately over folding into a `void` so a mistyped target stays distinguishable from a real
/// one. Nothing seeded `ReflectionInfo::params` from the registry, so `returns_of("std.math.sqrt")`
/// was that `none`: reflection called a function the checker types, the linker resolves and both
/// backends dispatch a nonexistent name.
///
/// **What is keyed, and under what string.** Every callable an extension declares, under exactly the
/// spelling the rest of reflection uses for that kind of declaration:
///
/// * a **module function** (`ExtModule::functions`, plus the higher-order `ctx_functions` and the
///   call-site-typed `typed_functions` — a name lives in exactly one table) under its root-qualified
///   path, `std.math.sqrt`;
/// * a **native type's method** — an extern handle's ([`ExtType`](noeta_ext_abi::registry::ExtType)),
///   a native fielded type's, a native enum's — under `Type.method` on the type's **qualified**
///   identity, `std.id.Uuid.to_string`, the same identity `type_of` stamps on its values and
///   `extension_enum_types` keys a type on;
/// * a **native trait's method** under `Trait.method` on the trait's qualified identity, matching the
///   convention `reflect::build` already keys a `.noe` trait's method signatures under.
///
/// A native method's `params` exclude the receiver, exactly as a `.noe` method's `ParamRecord` does,
/// so the reported arity is the one a call site writes.
///
/// **Parameter names are real, and that was measured rather than assumed.** The row that filed this
/// hole warned that shipped `ExtFn`s "mostly leave `param_names` empty", which would have made a DI
/// framework inject by a blank name — a silently-wrong answer of its own, and a reason to fill the
/// tables before seeding. Measured across the installed std registry: of 214 module functions and 180
/// native methods, **every one that takes a parameter names it**; the empty `param_names` are all on
/// zero-arity signatures, where empty is the only correct value. A name is synthesized positionally
/// (`_0`, `_1`, …) only where a declaration genuinely has none — the convention a native enum's
/// positional payload already reflects under — so a blank name never travels.
///
/// The return type projects through the same [`sig_type_to_repr`] the type side uses, with the three
/// polymorphic [`RetTy`](noeta_ext_abi::registry::RetTy) forms resolved as precisely as the
/// declaration allows rather than flattened to `dyn`: `SameAsArg(n)` reports parameter `n`'s declared
/// type (`vec.add(v, w): typeof v`), `NumericPreserving` reports the union it means (`int | float`),
/// and a call-site-typed `TypeArg` reports its declared *wrapper* around a `dyn` hole — `T` itself is
/// named at the call site, so a signature reflection has nothing to resolve it against, the same
/// permissive hole `SigType::Var` takes.
pub fn extension_param_records() -> Vec<noeta_ast::reflect::ParamRecord> {
    use noeta_ext_abi::registry as ext;
    let Some(reg) = ext::default_registry() else {
        return Vec::new();
    };
    let mut out: Vec<noeta_ast::reflect::ParamRecord> = Vec::new();
    // A module function's target is its root-qualified path (`std.math.sqrt`) — the module identity
    // `Registry::find_module` resolves, extended by the function's own name.
    for unit in reg.extensions() {
        for module in unit.modules() {
            for table in [
                module.functions,
                module.ctx_functions,
                module.typed_functions,
            ] {
                for f in table {
                    out.push(ext_fn_record(
                        format!("{}.{}.{}", unit.root(), module.name, f.name),
                        f,
                    ));
                }
            }
        }
        // A native trait's methods, keyed `Trait.method` on the trait's qualified identity — the
        // convention `reflect::build` uses for a `.noe` trait's own method signatures.
        for tr in unit.traits() {
            for m in tr.methods {
                out.push(ext_fn_record(
                    format!("{}.{}", tr.qualified(), m.sig.name),
                    &m.sig,
                ));
            }
        }
    }
    // Every native type's methods, keyed `Type.method` on the type's qualified identity: an extern
    // handle's, a native fielded type's, and a native enum's.
    for t in reg.extensions().iter().flat_map(|e| e.types()) {
        for table in [t.methods, t.ctx_methods] {
            for f in table {
                out.push(ext_fn_record(format!("{}.{}", t.qualified(), f.name), f));
            }
        }
    }
    for t in reg.fielded() {
        for f in t.methods {
            out.push(ext_fn_record(format!("{}.{}", t.qualified(), f.name), f));
        }
    }
    for t in reg.enums() {
        for f in t.methods {
            out.push(ext_fn_record(format!("{}.{}", t.qualified(), f.name), f));
        }
    }
    out
}

/// One native signature as a reflection [`ParamRecord`](noeta_ast::reflect::ParamRecord) under
/// `target` — the single projection every callable kind in [`extension_param_records`] goes through,
/// so a module function and a method cannot come to describe their parameters differently.
///
/// A parameter is **optional** exactly when the declaration makes it so: a trailing
/// [`SigType::Optional`](noeta_ext_abi::registry::SigType::Optional) is the registry's arity marker
/// (a call may leave it unsupplied), which is precisely what `ParamSig::optional` reports for a
/// `.noe` parameter carrying a default. Its *type* is the wrapped inner type, not an `Option<…>` —
/// `sig_type_to_repr` already unwraps it, and reporting the marker as the value type would describe
/// a parameter the callee never sees.
fn ext_fn_record(
    target: String,
    f: &noeta_ext_abi::registry::ExtFn,
) -> noeta_ast::reflect::ParamRecord {
    use noeta_ext_abi::registry as ext;
    let params = f
        .params
        .iter()
        .enumerate()
        .map(|(i, ty)| noeta_ast::reflect::ParamSig {
            // A declared name where there is one (measured: every std signature that takes a
            // parameter names it); the positional slot name a native payload already reflects under
            // where there is not, so a blank name never reaches a consumer keying on it.
            name: f
                .param_names
                .get(i)
                .map(|n| (*n).to_string())
                .unwrap_or_else(|| format!("_{i}")),
            ty: sig_type_to_repr(ty),
            optional: matches!(ty, ext::SigType::Optional(_)),
        })
        .collect();
    noeta_ast::reflect::ParamRecord {
        target,
        params,
        ret: ret_ty_to_repr(&f.ret, f.params),
    }
}

/// Project a registry [`RetTy`](noeta_ext_abi::registry::RetTy) onto its reflection
/// [`TypeRepr`](noeta_ast::reflect::TypeRepr) — the return-type half of [`sig_type_to_repr`], and
/// deliberately more precise than the checker's `ret_to_typeref`, which flattens every polymorphic
/// form to `dyn` because it only needs a *declaration-site* annotation for the user-trait checkers.
/// `returns_of` reports what the signature says, so each form resolves as far as the declaration
/// does:
///
/// * `SameAsArg(n)` **is** parameter `n`'s declared type (`vec.add(v, w): typeof v`), so it reports
///   that type — `dyn` only when the parameter itself is a hole, or when the index names no
///   parameter (a declaration bug the registry's own conformance test catches).
/// * `NumericPreserving` means `int` when every argument is concretely `int` and `float` otherwise —
///   a union of exactly those two, which is what the surface renderer already prints for it.
/// * `TypeArg(wrap)` is named by the call site's turbofish, which a signature reflection has nothing
///   to resolve against — so the declared *wrapper* is reported around a `dyn` hole, the same
///   permissive hole a `SigType::Var` takes. The `Result` wrap's error type is declared, not
///   call-site, so it stays precise.
fn ret_ty_to_repr(
    ret: &noeta_ext_abi::registry::RetTy,
    params: &[noeta_ext_abi::registry::SigType],
) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    use noeta_ext_abi::registry as ext;
    match ret {
        ext::RetTy::Concrete(s) => sig_type_to_repr(s),
        ext::RetTy::SameAsArg(n) => params
            .get(*n)
            .map(sig_type_to_repr)
            .unwrap_or(TypeRepr::Dyn),
        ext::RetTy::NumericPreserving => TypeRepr::Union(vec![TypeRepr::Int, TypeRepr::Float]),
        ext::RetTy::TypeArg(ext::TypeArgWrap::Plain) => TypeRepr::Dyn,
        ext::RetTy::TypeArg(ext::TypeArgWrap::Option) => TypeRepr::Option(Box::new(TypeRepr::Dyn)),
        ext::RetTy::TypeArg(ext::TypeArgWrap::Result(e)) => {
            TypeRepr::Result(Box::new(TypeRepr::Dyn), Box::new(sig_type_to_repr(e)))
        }
    }
}

/// Embed every type the **language and its installed extensions** declare into a freshly built
/// reflection artifact: the prelude enums, the extensions' attribute shapes, and the extensions'
/// enums. Idempotent (a name the program itself declares, or one already embedded by an earlier
/// REPL entry, is left alone: the program's own declaration wins, matching prelude shadowing).
///
/// `noeta_ast::reflect::build` walks a *program*, so a type the program does not declare is absent
/// from the artifact however real it is to the rest of the language — and both type-level queries
/// answer the empty list for an absent name, which by their pair rule means "I know nothing about
/// this name". That is why the prelude enums and the native enums are seeded here and not left to
/// the AST walk: `Ordering` and `std.http.Framing` are as constructible, matchable and namable as
/// any declared enum, and reflection was the one consumer still reporting them as unknown.
///
/// The **signature** index is seeded the same way and for the same reason
/// ([`extension_param_records`]): a native callable has no AST declaration either, so `params_of`
/// answered the empty list and `returns_of` the `none` that means "no such callable" for every
/// function the stdlib ships. Guarded identically — a program's own `fn` or method of that target
/// wins — and keyed on `returns_for`, because the two queries read one record and a callable present
/// in one index is present in both.
pub fn extend_reflection(info: &mut noeta_ast::reflect::ReflectionInfo) {
    let seeded = noeta_ast::reflect::prelude_type_infos()
        .into_iter()
        .chain(extension_attribute_types())
        .chain(extension_enum_types())
        .chain(extension_fielded_types());
    for ty in seeded {
        if info.type_named(&ty.name).is_none() {
            info.types.push(ty);
        }
    }
    for record in extension_param_records() {
        if info.returns_for(&record.target).is_none() {
            info.params.push(record);
        }
    }
}

/// Dedent a verbatim `@doc` body for presentation: drop leading/trailing blank lines, then strip
/// the common leading whitespace shared by all non-blank lines (so text written indented inside
/// `@doc { … }` renders flush-left). Blank lines do not count toward the common indent and are
/// emitted empty. The lexer captured the body exactly; this is purely presentation formatting —
/// shared by `lang doc` and the LSP's hover so both render identically. The AST's bytes are
/// untouched.
pub fn dedent_doc(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    // Trim leading and trailing blank lines.
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(start);
    let body = &lines[start..=end];

    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    body.iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The E0052 an **expression tier's** block raises in *statement* position (expr-tiers arc): the
/// block is a value and never activates/strips, so a bare statement would silently discard it.
/// Shared by the checker's in-place arm and activation, so the two never drift.
pub fn expr_tier_statement_diagnostic(tier: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::InvalidTierExpression,
        span,
        format!("`@{tier}` is an expression tier — its block is a value; assign or return it"),
    )
    .with_help(format!(
        "write `x = @{tier} {{ … }}` (or use it in any expression position); a bare statement \
         block would discard the value"
    ))
}

/// The `E0036` diagnostic for an `@name` that resolves to nothing. Shared by [`activate_tiers`]
/// and the checker's in-place `TierBlock` arm so the two never diverge.
///
/// It used to say "unknown dev-tier", which was only ever right in the block position — an
/// extension's `@`-directive written before a `fn` arrives here too, and calling it a dev-tier
/// named the wrong thing entirely. The offer spans the whole name-space of `reg`
/// (instance-registry IR4 — the session's registry, or the process-global default), with a
/// did-you-mean when the name is close to a real one.
pub fn unknown_tier_diagnostic(
    reg: &noeta_ext_abi::registry::Registry,
    tier: &str,
    span: Span,
) -> Diagnostic {
    let known: Vec<String> = reg
        .ext_tiers()
        .map(|t| t.name.to_string())
        .chain(reg.ext_directives().map(|d| d.name.to_string()))
        .collect();
    let d = Diagnostic::error(
        DiagnosticCode::UnknownDirective,
        span,
        format!("unknown directive `@{tier}`"),
    );
    match noeta_diagnostics::closest(tier, known.iter().map(String::as_str)) {
        Some(s) => d.with_help(format!("did you mean `@{s}`?")),
        None => d.with_help(format!(
            "the available ones are {} — or declare a tier with `@tier`",
            known
                .iter()
                .map(|n| format!("`@{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A code-tier `fn` surfaced by activation — a root the matching runner invokes by name (a `@test`
/// fn for `lang test`, a `@bench` fn for `lang bench`).
#[derive(Debug, Clone, PartialEq)]
pub struct TierFn {
    /// The fn's name, used to invoke it.
    pub name: String,
    /// Where it is declared (for the runner's report).
    pub span: Span,
    /// The `#[...]` data attributes on the fn — test metadata (`#[Skip]`, `#[Name("…")]`,
    /// `#[Group("…")]`, `#[Data([…])]`) and tier knobs (`#[Bench(iterations: N)]`, whether written
    /// per-fn or stamped from the block's `@bench(…)` directive args). The one place a runner reads
    /// configuration from. Empty for an unannotated fn in a bare block.
    pub attrs: Vec<Attribute>,
    /// Whether the fn is `async fn`. A runner invokes a root by **synthesizing a call** to it, and a
    /// call to an `async fn` evaluates to a `Future` — so without this the future is constructed,
    /// dropped, and the body never runs at all. (For `@test` that is silent and total: every
    /// assertion in an async test passes, because none of them executes.) A runner that synthesizes
    /// a call must `.await` it when this is set.
    pub is_async: bool,
}

/// A text-tier block's verbatim body (`@doc { … }`, or any declared `text:` tier — slice 6f,
/// generalized by the text-tiers arc). The text is the source between the braces, captured
/// un-parsed by the lexer, with the `\{`/`\}`/`\\` escapes undone.
#[derive(Debug, Clone, PartialEq)]
pub struct TextBlock {
    /// The tier the block belongs to (`"doc"`, `"spec"`, …).
    pub tier: String,
    /// The verbatim body text.
    pub text: String,
    /// The whole `@<tier> { … }` span, for the extractor's source-location header.
    pub span: Span,
    /// What the block annotates — resolved by adjacency (see [`resolve_texts`]).
    pub target: DocTarget,
}

/// What a `@doc { … }` block documents, resolved purely by **position** (no new syntax):
#[derive(Debug, Clone, PartialEq)]
pub enum DocTarget {
    /// A block immediately followed by a declaration (`fn`/`struct`/`class`/`enum`) in the same
    /// source: it documents that declaration. Tooling keys off `name_span` (stable through
    /// activation's inlining), display uses `name`.
    Decl { name: String, name_span: Span },
    /// The first *non-attached* doc block of its source file — the module's doc. In practice a
    /// module doc sits above the file's `use` header (or stands alone), so adjacency
    /// disambiguates naturally: prose above a declaration belongs to the declaration, prose above
    /// anything else opens the module.
    Module,
    /// Any later non-attached block — free-floating section prose between declarations.
    Section,
}

/// Resolve every top-level `@doc { … }` block, in source order, with its adjacency-resolved
/// [`DocTarget`] — [`resolve_texts`] filtered to the built-in `doc` tier. Consumers: `lang doc`
/// (extraction with symbol association), the LSP's hover, and [`activate_tiers`]'s `#[Doc]`
/// stamping when the `doc` tier is live.
pub fn resolve_docs(program: &Program) -> Vec<TextBlock> {
    let mut texts = resolve_texts(program);
    texts.retain(|t| t.tier == "doc");
    texts
}

/// Resolve every top-level text-tier block (`@doc` or any declared `text:` tier), in source
/// order, with its adjacency-resolved [`DocTarget`]. Text tiers are *declaration-position*, so a
/// top-level walk is the whole story; on a normal run the bodies never reach the checker or
/// lowering (stripped like any inactive tier). Works from a bare parse — no type-checking — so
/// text extracts from work-in-progress code. Matches on the lexer's verbatim capture
/// (`doc_text`), which is exactly the set of blocks whose tier was a known text tier at lex time.
pub fn resolve_texts(program: &Program) -> Vec<TextBlock> {
    resolve_texts_with_registry(program, noeta_ext_abi::registry::single_registry_process())
}

/// As [`resolve_texts`], against an explicit extension registry (instance-registry IR4), so an
/// embed session's own text tier resolves its attachment against *its* set.
pub fn resolve_texts_with_registry(
    program: &Program,
    reg: &'static noeta_ext_abi::registry::Registry,
) -> Vec<TextBlock> {
    // Sources that already produced a non-attached text block — the first is the module doc, the
    // rest are sections. Adjacency state is per-tier, so a module doc and a module spec coexist.
    let registry = TierRegistry::collect_with_registry(program, reg);
    let mut module_doc_seen = std::collections::HashSet::new();
    let mut docs = Vec::new();
    for (i, stmt) in program.stmts.iter().enumerate() {
        let Stmt::TierBlock {
            tier,
            doc_text: Some(text),
            span,
            ..
        } = stmt
        else {
            continue;
        };
        // What this block documents, if the next statement is a declaration it may attach to.
        //
        // "May attach to" is the tier's own `sites` — the same question, and the same answer, that
        // gates the annotation form. This used to be a fourth, hardcoded list of declaration kinds
        // that agreed with nobody: it accepted `Fn`/`Struct`/`Class`/`Enum` and had no `Trait` arm,
        // so a `@doc { … }` above a trait silently became the *module* doc rather than the trait's.
        let sites = registry.sites(tier);
        let decl_target = program.stmts.get(i + 1).and_then(|next| {
            if next.span().source != span.source {
                return None;
            }
            if !noeta_parser::directives::attaches_to(sites, next.attachment_site()) {
                return None;
            }
            let (name, name_span) = match next {
                Stmt::Fn(d) => (&d.name, d.name_span),
                Stmt::Struct(d) => (&d.name, d.name_span),
                Stmt::Class(d) => (&d.name, d.name_span),
                Stmt::Enum(d) => (&d.name, d.name_span),
                Stmt::Trait(d) => (&d.name, d.name_span),
                _ => return None,
            };
            Some(DocTarget::Decl {
                name: name.to_string(),
                name_span,
            })
        });
        let target = decl_target.unwrap_or_else(|| {
            if module_doc_seen.insert((span.source, tier.clone())) {
                DocTarget::Module
            } else {
                DocTarget::Section
            }
        });
        docs.push(TextBlock {
            tier: tier.clone(),
            text: text.clone(),
            span: *span,
            target,
        });
    }
    // Method-level text-tier directives (`@doc { … }` leading a method): a method has no top-level
    // statement to document by adjacency, so its directive rides on `FnDecl.directives`. Emit a
    // `Decl` target keyed by the method's own `name_span` — every consumer (hover, the docs browser,
    // the `#[Doc]` stamp) already resolves prose by `name_span` and already visits member name-spans,
    // so a method's `@doc` lights up the same paths as a top-level one with no consumer change.
    //
    // A type's `methods` already contains the flattened copy of every in-body `impl Trait { … }`
    // method (the parser clones them there so dispatch resolves them), so walking `impls` as well
    // would emit each of those twice. A **standalone** `impl Trait for T { … }` is the one method
    // carrier that is not flattened anywhere, so it is walked on its own.
    for stmt in &program.stmts {
        let methods = match stmt {
            Stmt::Struct(d) => &d.methods,
            Stmt::Class(d) => &d.methods,
            Stmt::Enum(d) => &d.methods,
            Stmt::Impl(d) => &d.methods,
            _ => continue,
        };
        for method in methods {
            for dir in &method.directives {
                let Some(text) = &dir.doc_text else { continue };
                docs.push(TextBlock {
                    tier: dir.name.clone(),
                    text: text.clone(),
                    span: dir.span,
                    target: DocTarget::Decl {
                        name: method.name.to_string(),
                        name_span: method.name_span,
                    },
                });
            }
        }
    }
    docs
}

/// The result of resolving a program's tier blocks against an active set.
#[derive(Debug, Clone, PartialEq)]
pub struct Activated {
    /// The program with active tier blocks inlined and inactive ones removed — ready to check and
    /// lower as if the tier blocks had never been a distinct form.
    pub program: Program,
    /// The `@test` fns activated by this resolution, in source order (roots for `lang test`).
    pub tests: Vec<TierFn>,
    /// The `@bench` fns activated by this resolution, in source order (roots for `lang bench`).
    pub benches: Vec<TierFn>,
    /// The roots of every activated **declared** tier (tier-providers T2), keyed by tier name — a
    /// `@fuzz { fn f() }` block's fns under `"fuzz"`, for the dispatching runner.
    pub custom: std::collections::BTreeMap<String, Vec<TierFn>>,
    /// The text blocks of every activated declared **text** tier (text-tiers arc), keyed by tier
    /// name — a `@spec { <case/> }` body under `"spec"`, for the dispatching runner (which
    /// receives them as `List<TierText>`). Built-in `doc` is not collected here: its activation
    /// surface is the `#[Doc]` stamp.
    pub texts: std::collections::BTreeMap<String, Vec<TextBlock>>,
    /// The tier name-space this resolution ran against — built-ins plus the program's `@tier`
    /// declarations. Surfaced so the caller (the CLI's dispatch) resolves the runner from the same
    /// registry activation validated with.
    pub registry: TierRegistry,
    /// `E0036` for any block naming an unknown tier (active or not).
    pub diagnostics: Vec<Diagnostic>,
}

/// The per-package resolution context activation needs to judge each `@name` occurrence *by identity*
/// rather than by its local spelling (per-package naming arc): the whole program's `[tiers]`/
/// `[directives]` bindings and the span→package map that says which package wrote each block. Both are
/// empty for a single-program preview (the IDE/MCP `activate_tiers` path), which then resolves every
/// `@name` ambiently — the behaviour that predates per-package naming.
#[derive(Clone, Copy, Debug)]
pub struct TierContext<'a> {
    pub uses: &'a noeta_span::PackageUses,
    pub packages: &'a noeta_span::PackageMap,
}

/// Resolve `program`'s `@<tier> { … }` blocks against `active` (the set of live tier *local names* from
/// the root's target), **everywhere they appear** — top-level (a `@test` block of declarations) and
/// nested in statement position (a `@debug { … }` block inside a fn/method body or a control-flow
/// branch). Active blocks are inlined in place; inactive blocks are dropped; every block's name is
/// validated. The `@test` fns among the activated *top-level* blocks are collected as roots the runner
/// invokes. This is the single-program form — every `@name` resolves ambiently (no per-package bindings).
pub fn activate_tiers(program: &Program, active: &[&str]) -> Activated {
    let uses = noeta_span::PackageUses::new();
    let packages = noeta_span::PackageMap::default();
    activate_tiers_with(
        program,
        active,
        &TierContext {
            uses: &uses,
            packages: &packages,
        },
    )
}

/// [`activate_tiers`] resolving each `@name` **per the package that wrote it** ([`TierContext`]): the
/// active set and every block are judged by their canonical [`TierId`], so a renamed tier (`@crit`
/// bound to `criterion:bench`) activates and stamps exactly the provider it names — the identity-based
/// replacement for the old `active.contains("test")` / provider-map dispatch.
pub fn activate_tiers_with(program: &Program, active: &[&str], ctx: &TierContext) -> Activated {
    let mut roots = Roots::default();
    let mut diagnostics = Vec::new();
    // The tier name-space: built-ins ∪ the program's own `@tier` declarations (imported packages'
    // included — the linked program carries their decls). Unknown-name validation, config-attr
    // stamping, and root collection all resolve against it.
    let registry = TierRegistry::collect(program);
    // The active set as canonical identities: each live *local* name from the root's target resolved
    // in the root's own context. A block anywhere in the program is live iff its identity is in here —
    // so a dependency's `@test` and the root's `@test` (both `(std, test)`) share one activation switch,
    // and a rename is judged by what it is. Names that resolve to nothing (an activated tier no package
    // provides) simply contribute no identity.
    let active_ids: std::collections::HashSet<TierId> = active
        .iter()
        .filter_map(|n| registry.resolve_id(n, Some(&noeta_span::PackageOrigin::Root), ctx.uses))
        .collect();
    // With the `doc` tier live, a declaration-attached `@doc` block (adjacency-resolved on the
    // *input* program, before its blocks are gone) stamps `#[Doc("…")]` onto its declaration —
    // the text tier's counterpart of `@bench`'s knob stamping, giving runtime docstrings via
    // `attributes_of`. Keyed by the declaration's name-span, which survives inlining.
    let doc_stamps: std::collections::HashMap<Span, String> =
        if active_ids.contains(&TierId::std("doc")) {
            resolve_docs(program)
                .into_iter()
                .filter_map(|d| match d.target {
                    DocTarget::Decl { name_span, .. } => Some((name_span, d.text)),
                    _ => None,
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        };
    // The text roots of active *declared* text tiers (text-tiers arc), resolved on the input program
    // like the doc stamps — the blocks themselves strip below, exactly like `@doc`. Keyed by the
    // block's local tier name (its `texts` bucket), but *gated* on its resolved identity: an active
    // program-declared text tier that is not the std `doc` tier (which the doc stamps own).
    let mut texts: std::collections::BTreeMap<String, Vec<TextBlock>> =
        std::collections::BTreeMap::new();
    for block in resolve_texts(program) {
        let Some(resolved) =
            registry.resolve_at(&block.tier, ctx.packages.at(block.span), ctx.uses)
        else {
            continue;
        };
        if matches!(resolved, ResolvedTier::Declared(_))
            && resolved.id() != TierId::std("doc")
            && active_ids.contains(&resolved.id())
        {
            texts.entry(block.tier.clone()).or_default().push(block);
        }
    }
    // The top-level statement list collects roots (a `@test`/`@bench` block's fns are runnable roots
    // only here — a tier block nested in a fn body holds inline code, not roots).
    let mut stmts = resolve_block(
        &program.stmts,
        &active_ids,
        &registry,
        ctx,
        &mut diagnostics,
        &mut roots,
        true,
    );
    if !doc_stamps.is_empty() {
        for stmt in &mut stmts {
            // The declaration's own prose (`@doc` above a `fn`/`struct`/`class`/`enum`/`trait` —
            // exactly the four `TierSite`s the `doc` tier declares, no more).
            match stmt {
                Stmt::Fn(d) => stamp_doc(&doc_stamps, d.name_span, &mut d.attrs),
                Stmt::Struct(d) => stamp_doc(&doc_stamps, d.name_span, &mut d.decorators.attrs),
                Stmt::Class(d) => stamp_doc(&doc_stamps, d.name_span, &mut d.decorators.attrs),
                Stmt::Enum(d) => stamp_doc(&doc_stamps, d.name_span, &mut d.decorators.attrs),
                // A trait is a documentable site (`TierSite::Trait`) and `resolve_texts` has
                // resolved prose above one to the trait since traits joined the site model — but
                // this stamping match had no arm for it, so the resolved text was dropped on the
                // floor. `reflect::build` already keys a trait's attributes by its bare name.
                Stmt::Trait(d) => stamp_doc(&doc_stamps, d.name_span, &mut d.decorators.attrs),
                _ => {}
            }
            // …and its members' prose. A method's `@doc` rides on `FnDecl::directives` rather than
            // a statement wrapper, so it never reached the walk above; the stamp lands on the
            // method's own `attrs`, which `reflect::build` keys `Type.method` — the same target
            // convention `params_of`/`returns_of` use, so the rows join on one key.
            //
            // Every carrier a method can sit in is covered: the type's own `methods` (which the
            // parser has already flattened each in-body `impl Trait { … }` method into), the
            // retained `impls` blocks (so the two copies of a flattened method do not disagree
            // about their attributes), and a standalone `impl Trait for T { … }`.
            match stmt {
                Stmt::Struct(d) => stamp_doc_methods(&doc_stamps, &mut d.methods, &mut d.impls),
                Stmt::Class(d) => stamp_doc_methods(&doc_stamps, &mut d.methods, &mut d.impls),
                Stmt::Enum(d) => stamp_doc_methods(&doc_stamps, &mut d.methods, &mut d.impls),
                Stmt::Impl(d) => stamp_doc_methods(&doc_stamps, &mut d.methods, &mut []),
                _ => {}
            }
        }
    }
    // Method-level `@test`/`@bench` directives (directive attachment sites): a method carrying one
    // becomes a runnable root named `Type.method` — an associated function the runner invokes with
    // no receiver (E0054 guarantees a `@test`/`@bench` method reads no `self`). It is collected only
    // when the tier is active, mirroring the top-level block, and marked `is_dev_tier` for the same
    // white-box field access a lifted top-level tier fn gets.
    for stmt in &mut stmts {
        let (type_name, methods) = match stmt {
            Stmt::Struct(d) => (d.name.clone(), &mut d.methods),
            Stmt::Class(d) => (d.name.clone(), &mut d.methods),
            Stmt::Enum(d) => (d.name.clone(), &mut d.methods),
            _ => continue,
        };
        for method in methods.iter_mut() {
            let mut make_test = false;
            let mut make_bench = false;
            for dir in &method.directives {
                // Resolve the method's `@name` in the package that wrote it, then judge by identity:
                // the std `test`/`bench` tiers become runnable roots when live, whatever local name a
                // rename gave them. Any other tier at a method site collects no root (as before).
                let Some(id) =
                    registry.resolve_id(&dir.name, ctx.packages.at(dir.name_span), ctx.uses)
                else {
                    continue;
                };
                if !active_ids.contains(&id) {
                    continue;
                }
                if id == TierId::std("test") {
                    make_test = true;
                } else if id == TierId::std("bench") {
                    make_bench = true;
                }
            }
            if !make_test && !make_bench {
                continue;
            }
            method.is_dev_tier = true;
            let name = format!("{type_name}.{}", method.name);
            if make_test {
                roots.tests.push(TierFn {
                    name: name.clone(),
                    span: method.name_span,
                    attrs: method.attrs.clone(),
                    is_async: method.is_async,
                });
            }
            if make_bench {
                roots.benches.push(TierFn {
                    name,
                    span: method.name_span,
                    attrs: method.attrs.clone(),
                    is_async: method.is_async,
                });
            }
        }
    }
    Activated {
        program: Program {
            stmts,
            span: program.span,
        },
        tests: roots.tests,
        benches: roots.benches,
        custom: roots.custom,
        texts,
        registry,
        diagnostics,
    }
}

/// Stamp `#[std.doc.Doc(text: "…")]` onto one declaration's attribute list, when the `doc` tier
/// resolved prose for the declaration named at `name_span`.
///
/// The name-span is the join key throughout: it is what [`resolve_docs`] reports as the target of a
/// `@doc` block and it survives activation's inlining, so every declaration kind — a top-level
/// `fn`, a type, a trait, a method — stamps through this one function rather than repeating the
/// construction per site (which is how a site came to be missing in the first place). A
/// hand-written `#[std.doc.Doc]` already on the declaration wins.
fn stamp_doc(
    stamps: &std::collections::HashMap<Span, String>,
    name_span: Span,
    attrs: &mut Vec<Attribute>,
) {
    let Some(text) = stamps.get(&name_span) else {
        return;
    };
    if attrs.iter().any(|a| a.name == TIER_ATTR_DOC) {
        return;
    }
    attrs.push(Attribute {
        name: Name::canonical(TIER_ATTR_DOC),
        name_span,
        args: vec![AttrArg {
            name: Some("text".to_string()),
            value: noeta_ast::AttrValue::Str(text.clone()),
            span: name_span,
        }],
        span: name_span,
    });
}

/// [`stamp_doc`] over a type's methods and the `impl Trait { … }` blocks retained beside them.
///
/// The parser flattens an in-body impl block's methods into the type's own `methods` (so dispatch
/// resolves them) *and* keeps the block, so the same method exists twice in the AST. Both copies are
/// stamped: `reflect::build` reads the flattened one, but leaving the block's copy unstamped would
/// leave two records of one method disagreeing about its attributes.
fn stamp_doc_methods(
    stamps: &std::collections::HashMap<Span, String>,
    methods: &mut [FnDecl],
    impls: &mut [noeta_ast::ImplBlock],
) {
    for method in methods.iter_mut() {
        stamp_doc(stamps, method.name_span, &mut method.attrs);
    }
    for block in impls.iter_mut() {
        for method in &mut block.methods {
            stamp_doc(stamps, method.name_span, &mut method.attrs);
        }
    }
}

/// The runnable roots a top-level activation collects, partitioned by tier — `@test` fns for
/// `lang test`, `@bench` fns for `lang bench`.
#[derive(Default)]
struct Roots {
    tests: Vec<TierFn>,
    benches: Vec<TierFn>,
    /// Declared-tier roots, keyed by tier name.
    custom: std::collections::BTreeMap<String, Vec<TierFn>>,
}

/// Resolve the tier blocks in one statement list. A `@<tier> { … }` is validated, then inlined (its
/// items spliced in place, each recursively resolved) when its tier is active or dropped when not;
/// every other statement is left in place with its *own* nested statement lists resolved
/// ([`resolve_children`]). `collect_tests` is true only for the program's top-level list, so only a
/// top-level `@test` block's fns become runnable roots.
fn resolve_block(
    stmts: &[Stmt],
    active_ids: &std::collections::HashSet<TierId>,
    registry: &TierRegistry,
    ctx: &TierContext,
    diagnostics: &mut Vec<Diagnostic>,
    roots: &mut Roots,
    collect_roots: bool,
) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let Stmt::TierBlock {
            tier,
            tier_span,
            args,
            items,
            ..
        } = stmt
        else {
            out.push(resolve_children(
                stmt,
                active_ids,
                registry,
                ctx,
                diagnostics,
            ));
            continue;
        };

        // Resolve the block's `@name` in the package that wrote it (its `tier_span`'s origin), then
        // judge everything below by the resolved tier's identity — not the local spelling.
        let resolved = registry.resolve_at(tier, ctx.packages.at(*tier_span), ctx.uses);
        let config: Option<String> = resolved
            .as_ref()
            .and_then(|r| r.config().map(str::to_string));
        match &resolved {
            None => diagnostics.push(unknown_tier_diagnostic(registry.reg(), tier, *tier_span)),
            Some(r) if r.is_expr() => {
                // An expression tier's block in statement position (E0052) — never activates.
                diagnostics.push(expr_tier_statement_diagnostic(tier, *tier_span));
            }
            Some(_) if config.is_none() => {
                if let Some(d) = knobless_args_diagnostic_for(tier, args) {
                    // Directive args on a knob-less tier (`@test(x)`) — E0037. A knob-carrying tier's
                    // args are validated as the stamped attribute's construction, by the checker, below.
                    diagnostics.push(d);
                }
            }
            _ => {}
        }
        let is_active = resolved
            .as_ref()
            .is_some_and(|r| active_ids.contains(&r.id()));
        if !is_active {
            // Inactive (including an unknown tier): stripped, never reaches the checker or the IR.
            continue;
        }

        // Active tier: resolve the items (so a tier block nested among them, and each item's own
        // body, are handled), then splice them in place. The items are spliced at *this* level, so
        // `collect_roots` carries through unchanged. Each lifted `fn` is marked `is_dev_tier` so the
        // checker grants it white-box access to the module's private fields (slice 6d), and the
        // block's directive args are stamped onto it as the tier's config attribute
        // (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`) unless the fn already carries one —
        // a per-fn attribute wins. The checker then validates the stamp through the ordinary
        // attribute construction gate, and the runner reads it off the fn's `attrs`; a top-level
        // `@test`/`@bench` block's fns are also recorded as roots.
        let config_attr = config.filter(|_| !args.is_empty());
        let resolved_items = resolve_block(
            items,
            active_ids,
            registry,
            ctx,
            diagnostics,
            roots,
            collect_roots,
        );
        // `is_active` implies `resolved` is `Some` (a `None` never contributes an active identity).
        let id = resolved.as_ref().map(ResolvedTier::id);
        for mut item in resolved_items {
            if let Stmt::Fn(decl) = &mut item {
                decl.is_dev_tier = true;
                if let Some(attr_name) = config_attr.as_deref()
                    && !decl.attrs.iter().any(|a| a.name == attr_name)
                {
                    decl.attrs
                        .push(synthesized_config_attr(attr_name, args, *tier_span));
                }
                if collect_roots {
                    // Roots keyed by identity: the std `test`/`bench` tiers feed the native runners
                    // whatever local name a rename gave them; any other **program-declared** tier's fns
                    // are roots for its own `@tier` runner, keyed by the tier's exported name.
                    let sink = if id.as_ref() == Some(&TierId::std("test")) {
                        Some(&mut roots.tests)
                    } else if id.as_ref() == Some(&TierId::std("bench")) {
                        Some(&mut roots.benches)
                    } else if let Some(ResolvedTier::Declared(d)) = &resolved {
                        Some(roots.custom.entry(d.name.clone()).or_default())
                    } else {
                        None
                    };
                    if let Some(sink) = sink {
                        sink.push(TierFn {
                            name: decl.name.to_string(),
                            span: decl.name_span,
                            attrs: decl.attrs.clone(),
                            is_async: decl.is_async,
                        });
                    }
                }
            }
            out.push(item);
        }
    }
    out
}

/// Rewrite a non-tier statement's own nested statement lists (control-flow branches, loop and
/// fn/method bodies, a class destructor), resolving any tier blocks within. Nested lists never
/// collect tests (`collect_tests = false`). Statements with no nested statements are returned
/// unchanged. Tier blocks live only in statement position, so there is no need to descend into
/// expressions (closures and `match`/`if` *expressions* are expression-bodied).
fn resolve_children(
    stmt: &Stmt,
    active_ids: &std::collections::HashSet<TierId>,
    registry: &TierRegistry,
    ctx: &TierContext,
    diagnostics: &mut Vec<Diagnostic>,
) -> Stmt {
    let mut stmt = stmt.clone();
    let block = |stmts: &[Stmt], diags: &mut Vec<Diagnostic>| -> Vec<Stmt> {
        // Nested statement lists never produce runnable roots (`collect_roots = false`); the sink is
        // a throwaway.
        let mut sink = Roots::default();
        resolve_block(stmts, active_ids, registry, ctx, diags, &mut sink, false)
    };
    match &mut stmt {
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            *then_body = block(then_body, diagnostics);
            if let Some(eb) = else_body {
                *eb = block(eb, diagnostics);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => {
            *body = block(body, diagnostics);
        }
        Stmt::Fn(decl) => decl.body = block(&decl.body, diagnostics),
        Stmt::Class(c) => {
            for m in &mut c.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut c.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
            if let Some(d) = &mut c.destructor {
                *d = block(d, diagnostics);
            }
        }
        Stmt::Struct(s) => {
            for m in &mut s.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut s.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
        }
        Stmt::Enum(en) => {
            for m in &mut en.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut en.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
        }
        Stmt::Impl(im) => {
            for m in &mut im.methods {
                m.body = block(&m.body, diagnostics);
            }
        }
        _ => {}
    }
    stmt
}

/// Render a folded literal ([`noeta_ast::AttrValue`]) as **JSON text**, or `None` when it has no JSON
/// spelling. The write half of the json-defaults boundary: what this renders is what a decode can
/// bake into a [`noeta_ext_abi::FieldDefault::Literal`] and fill for an omitted field.
///
/// Only the forms a JSON document can carry are rendered — scalars and lists of them. A `Set`/`Map`/
/// enum/struct/type-reference literal, and a non-finite float (no JSON spelling at all), return
/// `None`, so the field falls back to [`noeta_ext_abi::FieldDefault::Dynamic`] rather than baking a
/// value the decoder could not reproduce. The text is decoded through the field's own recipe, so
/// `1` for a `float` field widens exactly as a supplied `1` would.
fn attr_value_to_json(value: &noeta_ast::AttrValue) -> Option<String> {
    use noeta_ast::AttrValue;
    Some(match value {
        AttrValue::Str(s) => noeta_ext_abi::json_text::json_string(s),
        AttrValue::Int(n) => n.to_string(),
        AttrValue::Float(f) if f.is_finite() => {
            // Always spell a float with a fractional part so it round-trips as a JSON *number* the
            // same way the source literal reads (`1.0`, not `1`). Both decode to `float` anyway
            // (int widens), but the baked text should mirror the declaration.
            let text = f.to_string();
            if text.contains(['.', 'e', 'E']) {
                text
            } else {
                format!("{text}.0")
            }
        }
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) => {
            let rendered = items
                .iter()
                .map(attr_value_to_json)
                .collect::<Option<Vec<_>>>()?;
            format!("[{}]", rendered.join(", "))
        }
        // No JSON spelling: a set/map/enum/struct/type literal, or a non-finite float.
        _ => return None,
    })
}

// ----- the checker's tier/semantic-role validation passes (impl Checker, moved from lib.rs) -----

impl Checker {
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
    pub(crate) fn check_tier_decls(&mut self, program: &Program) {
        // Resolve the extension-tier half of the name-space against THIS checker's registry
        // (instance-registry IR4), so an embed session whose own extension declares a `@tier`
        // validates its `@<tier>` blocks correctly. Defaults to the process-global registry.
        self.symbols.tier_registry =
            tiers::TierRegistry::collect_with_registry(program, self.reg());
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
            let root = decl_runner_root(f.name.as_str());
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
                && !self.symbols.attributes.contains(config.as_str())
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

    pub(crate) fn check_semantic_roles(&mut self, program: &Program) {
        for stmt in &program.stmts {
            // Placement — which directive may sit on which declaration — is one check for every
            // kind, over every decorated declaration the AST reports. The walk used to name three
            // kinds and end in `_ => {}`, each hand-passing its own site and noun; `Stmt::decorated`
            // is exhaustive over the statement kinds instead, so a new decorated declaration cannot
            // be silently left unchecked, and the diagnostic's noun is derived from the site rather
            // than written out a second time.
            if let Some(at) = stmt.decorated() {
                self.check_directive_placement(&at);
            }
            // The per-directive work placement does not cover: a `@role`'s tags must name variants
            // of a `@semantic` enum, and a `@packed` struct's fields must all be packable.
            if let Stmt::Struct(r) = stmt {
                self.check_role_tags(
                    r.name_span,
                    r.decorators.role.as_deref(),
                    r.decorators.attribute.is_some(),
                );
                self.check_packed_struct(r);
            }
        }
    }

    /// Whether `ty` can be a field of a `@packed` struct (P-PACK): a primitive (`int`/`float`/`bool`)
    /// or another packed struct (a non-generic `Named` in `packed_structs`). Everything else — a
    /// string/list/map/class/enum/`dyn`/generic — is heap-shaped and cannot lay out flat.
    pub(crate) fn is_packable_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Int | Type::Float | Type::F32 | Type::F64 | Type::IntN { .. } | Type::Bool => {
                true
            }
            Type::Named(name, args) if args.is_empty() => {
                self.symbols.packed_structs.contains(name)
            }
            _ => false,
        }
    }

    /// The flat [`PackedLayout`] of `ty` if it is a `@packed` struct, else `None` (P-PACK Phase 2).
    /// Recurses through nested packed fields, flattening them inline. `check_packed_struct` has
    /// already guaranteed every field of a packed struct is packable, so the field walk never bails on
    /// a well-typed program; the `?`s defend against a malformed registry (and an unpacked element).
    /// Intern one **concrete instantiation** of a forwarding slot into the program-wide
    /// type-argument table, and answer the [`noeta_ext_abi::HiddenArg`] that selects it.
    ///
    /// **The one place a [`noeta_ext_abi::TypeArgInfo`] is built.** Every projection an erased
    /// instantiation can be asked for at run time is derived here, from the one resolved `sigma`:
    /// its name-keyed identity ([`noeta_types::Type::head_name`]) and its build recipe
    /// ([`Self::type_to_recipe`]). The two instantiating paths — a call
    /// ([`Self::check_generic_call_seeded`]) and a value-position instantiation
    /// (`resolve_value_hidden_slots`) — carried byte-identical copies of this, error text included.
    /// That is precisely the drift this table has already suffered once: `name` was interned as
    /// `sigma.to_string()`, which renders the SHORT name, while every name-keyed runtime registry is
    /// keyed on the linker's qualified one — so a forwarded `attributes_of::<T>()` under a
    /// `namespace` silently answered the empty list. One site, one derivation, so a projection added
    /// later cannot reach one path and miss the other.
    ///
    /// A slot whose consumers need a recipe (`needs_recipe`) but whose instantiation has none is
    /// reported here rather than at either caller, for the same reason: the message is part of the
    /// contract, and two copies of a contract are one copy too many.
    pub(crate) fn intern_type_arg(
        &mut self,
        sigma: &Type,
        slot: &crate::forwarding::ForwardSlot,
        callee: &str,
        span: Span,
    ) -> noeta_ext_abi::HiddenArg {
        let recipe = self.type_to_recipe(sigma);
        if slot.needs_recipe && recipe.is_none() {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{sigma}` cannot be built by the call-site-typed `::<{}>` position of \
                     `{callee}`",
                    slot.template
                ),
            );
        }
        let info = noeta_ext_abi::TypeArgInfo {
            name: sigma.head_name(),
            recipe,
        };
        // The third projection: the instantiation's reflected [`TypeRepr`], for a slot a
        // **construction site** reads (`Sites::dynamic_construction_sites` stamps it onto the object
        // an inner fresh constructor built). It lives in the parallel `type_arg_reprs` table rather
        // than on `TypeArgInfo`, which is `noeta-ext-abi`'s and may not depend on `noeta-ast`.
        //
        // It is part of the DEDUP KEY. `name` is head-keyed and a class carries no decode recipe, so
        // `Repository<Todo>` and `Repository<Order>` produce the identical `TypeArgInfo` — folding
        // them into one entry would make two differently-instantiated construction sites report each
        // other's argument. Every existing consumer reads only the fields it always read; the pair
        // merely stops entries that differ in a fact SOME consumer needs from collapsing.
        let repr = crate::type_to_repr_top(sigma, &self.symbols.type_kinds);
        let idx = match self
            .sites
            .type_arg_table
            .iter()
            .zip(&self.sites.type_arg_reprs)
            .position(|(e, r)| *e == info && *r == repr)
        {
            Some(i) => i,
            None => {
                self.sites.type_arg_table.push(info);
                self.sites.type_arg_reprs.push(repr);
                self.sites.type_arg_table.len() - 1
            }
        };
        noeta_ext_abi::HiddenArg::Table(idx as u32)
    }

    /// Resolve a checker [`Type`] into a [`noeta_ext_abi::TypeRecipe`] for call-site-typed
    /// deserialization (`json.parse::<T>`), or `None` if `T` has no JSON decoding: a class (a
    /// reference/identity type), a tuple/set/result/`dyn`, a non-string-keyed map, a generic
    /// instantiation, an enum with a payload-carrying variant (see [`Self::enum_to_recipe`]), or a
    /// struct with any such field. A struct records its fields in **declared order** (so the decoder
    /// emits them in the order the backend's registered type expects); an enum records its variants
    /// in declared order with the wire value each is selected by.
    pub(crate) fn type_to_recipe(&self, ty: &Type) -> Option<noeta_ext_abi::TypeRecipe> {
        use noeta_ext_abi::TypeRecipe;
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
            // A non-generic value struct (a class is reference/identity, so it never decodes; an
            // enum has its own arm below). The field set is the declared record fields, in order.
            Type::Named(name, args)
                if args.is_empty()
                    && self.symbols.type_kinds.get(name)
                        == Some(&noeta_types::TypeKind::Struct) =>
            {
                let defaults = self.symbols.field_defaults.get(name);
                let fields = self
                    .symbols
                    .records
                    .get(name)?
                    .iter()
                    .map(|(fname, fty)| {
                        Some(noeta_ext_abi::FieldRecipe {
                            name: fname.clone(),
                            recipe: self.type_to_recipe(fty)?,
                            // What an omitted field means (json-defaults): a declared LITERAL
                            // default is baked in and fills the field; any other default is
                            // `Dynamic` and stays required. Absent from the table ⇒ `Required`.
                            default: defaults
                                .and_then(|d| d.get(fname))
                                .cloned()
                                .unwrap_or_default(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                // Validation arc: a struct implementing `Validate` carries the flag so the recipe
                // door re-enters to run `validate()` on the freshly-built value (bottom-up).
                let has_validator = self.satisfies(
                    &Type::Named(name.clone(), Vec::new()),
                    noeta_types::BuiltinTrait::Validate,
                );
                TypeRecipe::Struct {
                    name: name.clone(),
                    fields,
                    has_validator,
                }
            }
            // A non-generic enum whose every variant is payload-free decodes from the wire values its
            // own JSON Schema advertises: a backed enum's backings, a plain enum's case names. See
            // [`Self::enum_to_recipe`] for why a payload-carrying variant declines the whole enum.
            Type::Named(name, args)
                if args.is_empty()
                    && self.symbols.type_kinds.get(name) == Some(&noeta_types::TypeKind::Enum) =>
            {
                self.enum_to_recipe(name)?
            }
            _ => return None,
        })
    }

    /// The [`noeta_ext_abi::TypeRecipe::Enum`] for the declared enum `name`, or `None` if it has no
    /// JSON decoding.
    ///
    /// **The tag comes from the declaration, so decode and schema cannot drift.** A variant with a
    /// folded backing tags on that backing; a variant of an unbacked enum tags on its case name
    /// ([`noeta_ext_abi::VariantTag::Name`]). That is precisely the vocabulary a `{"enum": […]}`
    /// schema derived from `variants_of` emits, which is the property that makes an enum-typed field
    /// decodable from the very document its schema describes.
    ///
    /// Two shapes decline, and both decline the **whole** enum rather than half of it:
    ///
    /// - **any payload-carrying variant.** A data-carrying sum has no canonical JSON spelling, so
    ///   decoding the payload-free cases alone would accept documents against a schema that cannot
    ///   describe the type. Such a variant is built by `construct("Enum.Variant", payload)` instead.
    /// - **a backed variant whose backing did not fold to a literal.** The recipe is pure data with
    ///   no way to run an expression, exactly as a non-literal field default is
    ///   [`noeta_ext_abi::FieldDefault::Dynamic`]; a partial tag set would make one case
    ///   unreachable from the wire with nothing saying why.
    fn enum_to_recipe(&self, name: &str) -> Option<noeta_ext_abi::TypeRecipe> {
        use noeta_ext_abi::{TypeRecipe, VariantRecipe, VariantTag};
        let variants = self.symbols.enums.get(name)?;
        let recipes = variants
            .iter()
            .enumerate()
            .map(|(index, variant)| {
                if !variant.fields.is_empty() {
                    return None;
                }
                let tag = match &variant.backing {
                    None => VariantTag::Name,
                    Some(noeta_ast::AttrValue::Str(s)) => VariantTag::Str(s.clone()),
                    Some(noeta_ast::AttrValue::Int(n)) => VariantTag::Int(*n),
                    Some(noeta_ast::AttrValue::Float(f)) => VariantTag::Float(*f),
                    Some(noeta_ast::AttrValue::Bool(b)) => VariantTag::Bool(*b),
                    // A backing that folded to a non-scalar (a list) has no wire spelling a tag can
                    // match; the enum declines rather than silently omitting the case.
                    Some(_) => return None,
                };
                Some(VariantRecipe {
                    name: variant.name.clone(),
                    index: u32::try_from(index).ok()?,
                    tag,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        // An enum with no variants at all decodes nothing, so it is not a decodable type — reporting
        // it as one would produce a recipe that rejects every document with an empty accepted list.
        if recipes.is_empty() {
            return None;
        }
        // Validation arc: an enum implementing `Validate` carries the flag so the decode door
        // re-enters to run `validate()` on the built case, exactly as a struct's does.
        let has_validator = self.satisfies(
            &Type::Named(name.to_string(), Vec::new()),
            noeta_types::BuiltinTrait::Validate,
        );
        Some(TypeRecipe::Enum {
            name: name.to_string(),
            variants: recipes,
            has_validator,
        })
    }

    /// Classify one field's declared default for a decode recipe (json-defaults): what
    /// [`noeta_ext_abi::FieldDefault`] a *missing* input field means for it.
    ///
    /// The fillable/required boundary lives here, and it is **literalness**. A decode is a pure data
    /// walk in `noeta-stdlib` with no access to the program's code, so it can only fill a default it
    /// carries as data — the literal subset [`noeta_ast::reflect::fold_const_expr`] folds, which is
    /// exactly the subset `TypeInfo::field_defaults` already reports. A default that folds (or whose
    /// folded value has no JSON spelling — an untyped `Set`/`Map`/enum literal, a non-finite float)
    /// is [`noeta_ext_abi::FieldDefault::Dynamic`]: still required in JSON, but named as such in the
    /// error, so the author is told *why* a field they gave a default is being demanded.
    pub(crate) fn field_default_recipe(
        field: &noeta_ast::FieldDecl,
    ) -> noeta_ext_abi::FieldDefault {
        use noeta_ext_abi::FieldDefault;
        let Some(expr) = &field.default else {
            return FieldDefault::Required;
        };
        match noeta_ast::reflect::fold_const_expr(expr)
            .as_ref()
            .and_then(attr_value_to_json)
        {
            Some(json) => FieldDefault::Literal(json),
            None => FieldDefault::Dynamic,
        }
    }

    /// Validate a struct's `@role(Enum.Variant)` tags. Each must name a **fieldless** variant of a
    /// `@semantic` enum, and may only tag a struct that is itself an attribute (`@attribute`) — the
    /// role rides on what the attribute attaches to. Multiple roles are allowed. Each violation is
    /// `E0031` at its span; `name_span` locates the declaration for the "not an attribute" case.
    pub(crate) fn check_role_tags(
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
            if !self.symbols.semantic_enums.contains(tag.enum_name.as_str()) {
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
                .symbols
                .enums
                .get(tag.enum_name.as_str())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn parse_program(text: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "test.noe", text.to_string());
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "fixture must parse cleanly"
        );
        parsed.program
    }

    /// A method carrying a `@test` directive is discovered as a root named `Type.method` (an
    /// associated function the runner calls with no receiver) and marked `is_dev_tier`.
    #[test]
    fn method_test_directive_becomes_a_qualified_root() {
        noeta_stdlib::registry::default_seeded();
        let program = parse_program(
            "struct Point {\n    \
             x: int = 0\n    \
             @test\n    \
             fn is_zero() { assert(Point {}.x == 0); }\n\
             }\n",
        );
        let out = activate_tiers(&program, &["test"]);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            out.tests
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["Point.is_zero"]
        );
        let Stmt::Struct(d) = &out.program.stmts[0] else {
            panic!("expected a struct");
        };
        assert!(
            d.methods[0].is_dev_tier,
            "the test method is marked is_dev_tier"
        );
    }

    /// An active `@test` block inlines its fns as top-level decls and surfaces them as tests; the
    /// program's own declarations are preserved and the `@test` *block* form is gone.
    #[test]
    fn active_test_block_inlines_and_discovers() {
        let program = parse_program(
            "fn add(a: int, b: int): int { return a + b; }\n\
             @test {\n\
                 fn adds() { assert(add(1, 2) == 3); }\n\
                 fn more() { assert(add(2, 2) == 4); }\n\
             }\n",
        );
        let out = activate_tiers(&program, &["test"]);
        assert!(out.diagnostics.is_empty());
        assert_eq!(
            out.tests
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["adds", "more"]
        );
        // `add` + the two inlined test fns — and no `TierBlock` survives.
        assert_eq!(out.program.stmts.len(), 3);
        assert!(
            !out.program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::TierBlock { .. }))
        );
    }

    /// With the tier inactive (a normal run / `lang test` over a non-`test` tier) the block is
    /// dropped entirely: no inlining, no tests, and nothing left in the stream.
    #[test]
    fn inactive_test_block_is_stripped() {
        let program = parse_program(
            "fn add(a: int, b: int): int { return a + b; }\n\
             @test { fn adds() { assert(add(1, 2) == 3); } }\n",
        );
        let out = activate_tiers(&program, &[]);
        assert!(out.diagnostics.is_empty());
        assert!(out.tests.is_empty());
        assert_eq!(out.program.stmts.len(), 1);
    }

    /// Provider resolution: no selection falls back extension-first; `"std"` selects the
    /// extension; a dependency key selects that package's declaration; a mismatch is a
    /// human-readable error. Two packages may declare one tier name (told apart by root).
    #[test]
    fn provider_resolution_selects_between_extension_and_declared() {
        let program = parse_program(
            "@tier(bench, config: Fuzz)\n\
             fn my_bench(roots: List<TierRoot>): void { return; }\n\
             @attribute(Function)\n\
             struct Fuzz { cases: int }\n",
        );
        let reg = TierRegistry::collect(&program);
        let none = std::collections::BTreeMap::new();
        // Default: the extension declaration wins for a built-in name.
        assert_eq!(
            reg.resolve_provider("bench", &none),
            Ok(ResolvedProvider::Extension)
        );
        assert_eq!(
            reg.config_attribute_for("bench", &none).as_deref(),
            Some("std.bench.Bench")
        );
        // Explicit std: same.
        let std_sel = std::collections::BTreeMap::from([("bench".into(), "std".into())]);
        assert_eq!(
            reg.resolve_provider("bench", &std_sel),
            Ok(ResolvedProvider::Extension)
        );
        // Explicit entry-local provider (root "" — an entry-declared runner).
        let local = std::collections::BTreeMap::from([("bench".into(), String::new())]);
        match reg.resolve_provider("bench", &local) {
            Ok(ResolvedProvider::Declared(d)) => {
                assert_eq!(d.runner, "my_bench");
                assert_eq!(d.config.as_deref(), Some("Fuzz"));
            }
            other => panic!("expected the local declaration, got {other:?}"),
        }
        assert_eq!(
            reg.config_attribute_for("bench", &local).as_deref(),
            Some("Fuzz")
        );
        // A provider that declares no such tier is a clear error.
        let missing = std::collections::BTreeMap::from([("bench".into(), "ghost".into())]);
        assert!(
            reg.resolve_provider("bench", &missing)
                .unwrap_err()
                .contains("ghost")
        );
    }

    /// The extension-declared names and the `noeta_ast::reflect` constants are two spellings of
    /// one contract (the ABI sits beneath the syntax crates, so they cannot share a symbol) — pin
    /// them together so neither drifts. Also pins the built-in four and `bench`'s knob mapping.
    #[test]
    fn extension_declarations_match_the_reflect_constants() {
        use noeta_ast::reflect::{
            TEST_ATTR_DATA, TEST_ATTR_GROUP, TEST_ATTR_NAME, TEST_ATTR_SKIP, TIER_ATTR_BENCH,
            TIER_ATTR_DOC,
        };
        // The contract is the **qualified** identity now (D2b): the reflect constants are FQNs, so
        // pin them against each declaration's `qualified()`, not its short `name`.
        let declared: Vec<String> = noeta_stdlib::registry::ext_attributes()
            .map(|a| a.qualified())
            .collect();
        for name in [
            TEST_ATTR_SKIP,
            TEST_ATTR_NAME,
            TEST_ATTR_GROUP,
            TEST_ATTR_DATA,
            TIER_ATTR_BENCH,
            TIER_ATTR_DOC,
        ] {
            assert!(
                declared.iter().any(|d| d == name),
                "`{name}` missing from std's declarations"
            );
        }
        use noeta_stdlib::registry::find_ext_tier;
        for tier in ["test", "bench", "doc", "debug"] {
            assert!(
                find_ext_tier(tier).is_some(),
                "`{tier}` missing from std's tiers"
            );
        }
        assert_eq!(
            find_ext_tier("bench").and_then(|t| t.config),
            Some(TIER_ATTR_BENCH)
        );
        assert_eq!(find_ext_tier("test").and_then(|t| t.config), None);
        // The materialization shapes flow from the same declarations.
        let types = extension_attribute_types();
        let skip = types
            .iter()
            .find(|t| t.name == TEST_ATTR_SKIP)
            .expect("Skip shape");
        assert_eq!(skip.fields, ["reason"]);
        assert_eq!(
            skip.field_defaults,
            [Some(noeta_ast::AttrValue::Str(String::new()))]
        );
    }

    /// A `@tier`-declared tier opens the name space: its blocks activate (no E0036), block knobs
    /// stamp its config attribute, and its fns collect as roots under the tier's name in
    /// `Activated.custom`.
    #[test]
    fn a_declared_tier_activates_and_collects_custom_roots() {
        let program = parse_program(
            "@attribute(Function)\n\
             struct Fuzz { cases: int }\n\
             @tier(fuzz, config: Fuzz)\n\
             fn run_fuzz(roots: List<TierRoot>): void { return; }\n\
             @fuzz(cases: 9) { fn probe(): void { return; } }\n",
        );
        let out = activate_tiers(&program, &["fuzz"]);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tier = out.registry.declared("fuzz").expect("declared");
        assert_eq!(tier.runner, "run_fuzz");
        assert_eq!(tier.config.as_deref(), Some("Fuzz"));
        let roots = &out.custom["fuzz"];
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "probe");
        // The block knob stamped the declared config attribute.
        assert!(roots[0].attrs.iter().any(|a| a.name == "Fuzz"));
        // The activated program (stamp included) type-checks through the ordinary gates.
        assert!(crate::check_all(&out.program).diagnostics.is_empty());
        // Inactive: the block strips like any tier.
        let stripped = activate_tiers(&program, &[]);
        assert!(stripped.custom.is_empty());
        assert!(stripped.diagnostics.is_empty());
    }

    /// A `@doc` above a **trait** documents the trait.
    ///
    /// It used to become the *module* doc: the adjacency resolver carried its own list of target
    /// kinds — `Fn`/`Struct`/`Class`/`Enum`, no `Trait` — so the block matched nothing, fell
    /// through to the module/section fallback, and the prose silently reattached to the file. The
    /// resolver asks the tier's own `sites` now, and `doc` says it attaches to a trait.
    #[test]
    fn a_doc_block_above_a_trait_documents_the_trait() {
        noeta_stdlib::registry::default_seeded();
        let program = parse_program(
            "@doc { Shapes have an area. }\n             trait Shape { fn area(): int }\n",
        );
        let docs = resolve_docs(&program);
        assert_eq!(docs.len(), 1);
        assert!(
            matches!(&docs[0].target, DocTarget::Decl { name, .. } if name == "Shape"),
            "expected the trait, got {:?}",
            docs[0].target
        );
    }

    /// A tier that attaches to nothing does not swallow the declaration after it. `@json`'s block
    /// is a value; the `struct` below it is not documented by it, and is not its target.
    #[test]
    fn a_block_only_tier_claims_no_adjacent_declaration() {
        noeta_stdlib::registry::default_seeded();
        let program = parse_program("@json { {\"a\": 1} }\n             struct P { x: int }\n");
        let texts = resolve_texts(&program);
        assert!(
            texts
                .iter()
                .all(|t| !matches!(t.target, DocTarget::Decl { .. })),
            "a block-only tier must claim no declaration, got {:?}",
            texts.iter().map(|t| &t.target).collect::<Vec<_>>()
        );
    }

    /// `resolve_docs` adjacency: a file-leading `@doc` is the module doc (Python-docstring rule),
    /// a block immediately followed by a declaration attaches to it, and anything else is a
    /// free-floating section.
    #[test]
    fn doc_blocks_resolve_module_decl_and_section_targets() {
        let program = parse_program(
            "@doc { The module. }\n\
             @doc { Adds. }\n\
             fn add(a: int, b: int): int { return a + b; }\n\
             @doc { A section. }\n\
             x = 1;\n",
        );
        let docs = resolve_docs(&program);
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].target, DocTarget::Module);
        assert!(
            matches!(&docs[1].target, DocTarget::Decl { name, .. } if name == "add"),
            "{:?}",
            docs[1].target
        );
        assert_eq!(docs[2].target, DocTarget::Section);
    }

    /// With the `doc` tier live, activation stamps a declaration-attached block as `#[Doc(text:
    /// "…")]` on that declaration (runtime docstrings); with it inactive nothing is stamped and the
    /// blocks strip as before, so production carries no doc text.
    #[test]
    fn active_doc_tier_stamps_the_doc_attribute() {
        let program = parse_program(
            "@doc { Adds two ints. }\n\
             fn add(a: int, b: int): int { return a + b; }\n",
        );
        let doc_attr_of = |activated: &Activated| {
            activated.program.stmts.iter().find_map(|s| match s {
                Stmt::Fn(d) => d.attrs.iter().find(|a| a.name == TIER_ATTR_DOC).cloned(),
                _ => None,
            })
        };
        let active = activate_tiers(&program, &["doc"]);
        let attr = doc_attr_of(&active).expect("Doc attr stamped");
        assert!(
            matches!(&attr.args[0].value, noeta_ast::AttrValue::Str(t) if t.contains("Adds two ints")),
            "{:?}",
            attr.args
        );
        // The stamped attribute passes the ordinary construction gate.
        assert!(crate::check_all(&active.program).diagnostics.is_empty());
        // Inactive: no stamp, block stripped.
        let inactive = activate_tiers(&program, &[]);
        assert!(doc_attr_of(&inactive).is_none());
        assert_eq!(inactive.program.stmts.len(), 1);
    }

    /// A program exercising every declaration kind a `@doc` block may legally attach to, so the
    /// stamping tests below all read from one source of truth about what "legal" is.
    ///
    /// The set is exactly the `doc` tier's four declared `TierSite`s: a function, a type
    /// (struct/class/enum), a trait, and a method — the last in each of its three carriers (a
    /// type's own body, an in-body `impl Trait { … }` block, and a standalone `impl Trait for T`).
    /// A field, an enum variant and a trait method *signature* are absent because the grammar has
    /// no directive position on them at all, so a `@doc` there never reaches the checker.
    const EVERY_DOC_SITE: &str = "@doc { A function. }\n\
         fn top(): int { return 1; }\n\
         @doc { A class. }\n\
         class K {\n\
             @doc { A class method. }\n\
             fn m(): int { return 2; }\n\
         }\n\
         @doc { A struct. }\n\
         struct S {\n\
             x: int\n\
             @doc { A struct method. }\n\
             fn sm(): int { return 3; }\n\
         }\n\
         @doc { An enum. }\n\
         enum E {\n\
             A;\n\
             @doc { An enum method. }\n\
             fn em(): int { return 4; }\n\
         }\n\
         @doc { A trait. }\n\
         trait T { fn area(): int }\n\
         class C {\n\
             impl T {\n\
                 @doc { An in-body impl method. }\n\
                 fn area(): int { return 5; }\n\
             }\n\
         }\n\
         struct P { y: int }\n\
         impl T for P {\n\
             @doc { A standalone impl method. }\n\
             fn area(): int { return self.y; }\n\
         }\n";

    /// Every `(target, prose)` pair the reflection manifest carries for the `Doc` attribute, which
    /// is what `attributes_of::<std.doc.Doc>()` surfaces at runtime.
    fn doc_manifest(program: &Program) -> Vec<(String, String)> {
        noeta_ast::reflect::build(program, &[], &Default::default())
            .manifest
            .iter()
            .filter(|r| r.name == TIER_ATTR_DOC)
            .map(|r| {
                let noeta_ast::AttrValue::Str(text) = &r.args[0].value else {
                    panic!("Doc text is a string literal");
                };
                (r.target.clone(), text.trim().to_string())
            })
            .collect()
    }

    /// The stamp reaches **every** declaration kind a `@doc` may attach to, keyed by the reflection
    /// manifest's own target convention (`Type.method` for a member, the bare name otherwise) — so a
    /// method's prose joins with `params_of`/`returns_of` on one key.
    ///
    /// A method was the reported gap: its `@doc` resolved (`noeta doc` extracted it) but the
    /// stamping walk only visited top-level statements, so a framework reading a handler's
    /// documentation — every handler is a method — got nothing. A trait was the same gap one step
    /// further back: the resolver had learned to attach prose to a trait, and this walk had no arm
    /// for it.
    #[test]
    fn the_doc_stamp_reaches_every_legal_declaration_kind() {
        let program = parse_program(EVERY_DOC_SITE);
        let active = activate_tiers(&program, &["doc"]);
        assert!(active.diagnostics.is_empty(), "{:?}", active.diagnostics);
        let mut docs = doc_manifest(&active.program);
        docs.sort();
        assert_eq!(
            docs,
            vec![
                ("C.area".to_string(), "An in-body impl method.".to_string()),
                ("E".to_string(), "An enum.".to_string()),
                ("E.em".to_string(), "An enum method.".to_string()),
                ("K".to_string(), "A class.".to_string()),
                ("K.m".to_string(), "A class method.".to_string()),
                (
                    "P.area".to_string(),
                    "A standalone impl method.".to_string()
                ),
                ("S".to_string(), "A struct.".to_string()),
                ("S.sm".to_string(), "A struct method.".to_string()),
                ("T".to_string(), "A trait.".to_string()),
                ("top".to_string(), "A function.".to_string()),
            ]
        );
        // Every stamped attribute passes the ordinary construction gate — the stamp is a normal
        // attribute application, not a privileged one.
        assert!(crate::check_all(&active.program).diagnostics.is_empty());
    }

    /// With the tier inactive nothing is stamped **anywhere** — production carries no doc text for
    /// a method any more than for a top-level fn.
    #[test]
    fn an_inactive_doc_tier_stamps_no_declaration_kind() {
        let program = parse_program(EVERY_DOC_SITE);
        let inactive = activate_tiers(&program, &[]);
        assert_eq!(doc_manifest(&inactive.program), Vec::new());
    }

    /// An in-body `impl Trait { … }` method exists twice in the AST — the parser flattens it into
    /// the type's own `methods` so dispatch resolves it, and keeps the block for the checker. Both
    /// copies are stamped, so the two records of one method cannot disagree about its attributes.
    #[test]
    fn both_copies_of_a_flattened_impl_method_are_stamped() {
        let program = parse_program(
            "trait T { fn area(): int }\n\
             class C {\n\
                 impl T {\n\
                     @doc { Its area. }\n\
                     fn area(): int { return 5; }\n\
                 }\n\
             }\n",
        );
        let active = activate_tiers(&program, &["doc"]);
        let class = active
            .program
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Class(d) => Some(d),
                _ => None,
            })
            .expect("the class survives activation");
        let stamped = |m: &FnDecl| m.attrs.iter().any(|a| a.name == TIER_ATTR_DOC);
        assert!(class.methods.iter().all(stamped), "flattened copy");
        assert!(
            class.impls.iter().flat_map(|b| &b.methods).all(stamped),
            "impl-block copy"
        );
    }

    /// A hand-written `#[std.doc.Doc(…)]` on a method wins over the block's prose, exactly as it
    /// does on a top-level fn — the stamp never overwrites what the author wrote.
    #[test]
    fn an_explicit_doc_attribute_on_a_method_wins_over_the_stamp() {
        let program = parse_program(
            "class K {\n\
                 @doc { From the block. }\n\
                 #[std.doc.Doc(text: \"From the attribute.\")]\n\
                 fn m(): int { return 2; }\n\
             }\n",
        );
        let active = activate_tiers(&program, &["doc"]);
        assert_eq!(
            doc_manifest(&active.program),
            vec![("K.m".to_string(), "From the attribute.".to_string())]
        );
    }

    /// A `@bench(iterations: N)` block's directive args are stamped onto each contained fn as the
    /// `Bench` config attribute (the desugar), and a fn that already carries its own `#[Bench(…)]`
    /// keeps it — the per-fn attribute wins over the block's. The per-fn override is written by its
    /// **qualified** identity here (`std.bench.Bench`), the form the loader rewrites `#[Bench]` to
    /// after `use std.bench.Bench` — `activate_tiers` runs post-loader, so it sees the FQN (D2b).
    #[test]
    fn bench_block_args_stamp_the_config_attribute() {
        let program = parse_program(
            "@bench(iterations: 1000) {\n\
                 fn plain(): void { return; }\n\
                 #[std.bench.Bench(iterations: 5)]\n\
                 fn tuned(): void { return; }\n\
             }\n",
        );
        let out = activate_tiers(&program, &["bench"]);
        assert!(out.diagnostics.is_empty());
        let knob = |name: &str| {
            let bench = out.benches.iter().find(|b| b.name == name).expect(name);
            let attr = bench
                .attrs
                .iter()
                .find(|a| a.name == noeta_ast::reflect::TIER_ATTR_BENCH)
                .expect("Bench attr");
            attr.args[0].value.clone()
        };
        assert_eq!(knob("plain"), noeta_ast::AttrValue::Int(1000));
        assert_eq!(knob("tuned"), noeta_ast::AttrValue::Int(5));
        // The stamped attribute type-checks through the ordinary construction gate.
        assert!(crate::check_all(&out.program).diagnostics.is_empty());
    }

    /// The stamped construction is validated like any attribute: a wrong-typed knob
    /// (`@bench(iterations: true)`) is rejected by the construction gate on the activated program,
    /// and the same block fails `check_all` in place on the default (non-activated) path — the two
    /// paths reject identically.
    #[test]
    fn bad_bench_arg_fails_both_paths() {
        let program = parse_program("@bench(iterations: true) { fn b(): void { return; } }\n");
        let activated = activate_tiers(&program, &["bench"]);
        assert!(activated.diagnostics.is_empty(), "stamping itself is clean");
        assert!(
            !crate::check_all(&activated.program).diagnostics.is_empty(),
            "activated path must reject the stamped construction"
        );
        assert!(
            !crate::check_all(&program).diagnostics.is_empty(),
            "default path must reject the in-place block args"
        );
    }

    /// Directive args on a knob-less tier are an E0037 (`@test` takes no arguments), from both the
    /// activation path and the checker's in-place arm.
    #[test]
    fn args_on_a_knobless_tier_are_e0037() {
        let program = parse_program("@test(1) { fn t(): void { return; } }\n");
        let out = activate_tiers(&program, &["test"]);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(
            out.diagnostics[0].code,
            DiagnosticCode::InvalidDirectiveArgument
        );
        let in_place = crate::check_all(&program).diagnostics;
        assert!(
            in_place
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidDirectiveArgument),
            "in-place arm must also reject: {in_place:?}"
        );
    }

    /// White-box (slice 6d): a private `class` field is visible inside an active dev-tier (`@test`)
    /// fn body — read, write, and construct — so a white-box test type-checks with no E0035. With
    /// the tier inactive the block is stripped, so the bare program checks clean too.
    #[test]
    fn dev_tier_fn_gets_white_box_field_access() {
        let program = parse_program(
            "class Account { mut balance: int  fn new(b: int): Account { return Account { balance: b }; } }\n\
             @test fn touches(): void { mut a = Account { balance: 0 }; a.balance = 5; assert(a.balance == 5); }\n",
        );
        let active = crate::check_all(&activate_tiers(&program, &["test"]).program);
        assert!(
            active.diagnostics.is_empty(),
            "white-box dev-tier fn must not raise E0035: {:?}",
            active.diagnostics
        );
        let inactive = crate::check_all(&activate_tiers(&program, &[]).program);
        assert!(inactive.diagnostics.is_empty());
    }

    /// The white-box relaxation is **scoped**: ordinary same-module code (not a dev-tier fn) still
    /// cannot read a private field — it is an E0035, exactly as before slice 6d.
    #[test]
    fn ordinary_fn_cannot_read_private_field() {
        let program = parse_program(
            "class Account { balance: int  fn new(b: int): Account { return Account { balance: b }; } }\n\
             fn reads(): int { a = Account.new(1); return a.balance; }\n",
        );
        let diags = crate::check_all(&program).diagnostics;
        assert!(
            diags.iter().any(|d| d.code == DiagnosticCode::PrivateField),
            "ordinary code reading a private field must still be E0035: {diags:?}"
        );
    }

    /// An unknown tier is an `E0036` whether or not it would be active, and its block is dropped.
    #[test]
    fn unknown_tier_reports_e0036() {
        let program = parse_program("@tset { fn x() { echo \"hi\"; } }\n");
        let out = activate_tiers(&program, &["test"]);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, DiagnosticCode::UnknownDirective);
        assert!(out.tests.is_empty());
        assert!(out.program.stmts.is_empty());
    }

    /// The number of `echo` statements anywhere inside a fn body (recursively) — a proxy for how
    /// much of a `@debug` block survived activation.
    fn echoes_in_fn(stmt: &Stmt) -> usize {
        fn count(stmts: &[Stmt]) -> usize {
            stmts
                .iter()
                .map(|s| match s {
                    Stmt::Echo { .. } => 1,
                    Stmt::For { body, .. } | Stmt::While { body, .. } => count(body),
                    _ => 0,
                })
                .sum()
        }
        match stmt {
            Stmt::Fn(decl) => count(&decl.body),
            _ => 0,
        }
    }

    /// A `@debug { … }` block *nested in a fn body* (statement position) is resolved recursively:
    /// inlined in place when `debug` is active, stripped when not. (The top-level `@test` resolution
    /// is not the only one — activation reaches inside bodies.)
    #[test]
    fn nested_debug_block_is_resolved_in_place() {
        noeta_stdlib::registry::default_seeded();
        let program = parse_program(
            "fn f(x: int): void {\n\
                 @debug { echo \"dbg ${x}\"; }\n\
                 echo \"always\";\n\
             }\n",
        );
        // Inactive: only the unconditional `echo` survives in the body.
        let stripped = activate_tiers(&program, &[]);
        assert_eq!(echoes_in_fn(&stripped.program.stmts[0]), 1);
        // Active: the `@debug` echo is inlined too — two echoes in the body.
        let active = activate_tiers(&program, &["debug"]);
        assert_eq!(echoes_in_fn(&active.program.stmts[0]), 2);
        // An active nested `@debug` block does not produce test roots.
        assert!(active.tests.is_empty());
    }

    /// Regression (w7 tier-overflow fix): **two nested** `@debug` tier blocks whose inlined echo
    /// carries a **nested interpolation hole** (`${ … "${x}" … }` — a `${…}` inside a `${…}`).
    /// Parsing each hole re-enters the whole grammar, and the inner hole re-enters it again, one level
    /// deeper — the exact re-entrant path that overflowed the parser before [`parse_hole`] grew its
    /// stack. A depth-capped "fix" that silently bailed on the deeper block would drop an echo, so
    /// this pins the full resolution: the inner block's echo is inlined when `debug` is active (two
    /// echoes in the body), stripped when not (one), and a nested block never yields test roots.
    #[test]
    fn nested_tier_blocks_with_nested_hole_are_resolved() {
        noeta_stdlib::registry::default_seeded();
        let program = parse_program(
            "fn f(x: int): void {\n\
                 @debug {\n\
                     @debug { echo \"dbg ${ [x][0] } and ${x}\"; }\n\
                 }\n\
                 echo \"always ${x}\";\n\
             }\n",
        );
        // Stripped: both `@debug` layers drop, leaving only the unconditional `echo "always"`.
        let stripped = activate_tiers(&program, &[]);
        assert_eq!(echoes_in_fn(&stripped.program.stmts[0]), 1);
        assert!(stripped.tests.is_empty());
        // Active: the doubly-nested `@debug` echo is inlined in place — two echoes in the body.
        let active = activate_tiers(&program, &["debug"]);
        assert_eq!(echoes_in_fn(&active.program.stmts[0]), 2);
        // A nested `@debug` block produces no test roots, however many layers deep it sits.
        assert!(active.tests.is_empty());
    }
}
