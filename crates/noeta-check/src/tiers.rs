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
use noeta_ast::{AttrArg, Attribute, Program, Sites, Stmt};
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
                        config: t.config.as_ref().map(|(n, _)| n.clone()),
                        text: t.text.as_ref().map(|(lang, _)| lang.clone()),
                        expr: t.expr.as_ref().map(|(ty, _)| ty.clone()),
                        runner: f.name.clone(),
                        root: runner_root(&f.name),
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
        self.reg()
            .find_ext_tier(tier)
            .map(|t| t.sites)
            .unwrap_or(&[])
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
fn knobless_args_diagnostic_for(tier: &str, args: &[AttrArg]) -> Option<Diagnostic> {
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
        name: attr_name.to_string(),
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
    ext::ext_attributes()
        .map(|attr| noeta_ast::reflect::TypeInfo {
            name: attr.name.to_string(),
            kind: noeta_ast::reflect::TypeKind::Struct,
            fields: attr.fields.iter().map(|f| f.name.to_string()).collect(),
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
        })
        .collect()
}

/// Embed the installed extensions' attribute shapes into a freshly built reflection artifact —
/// idempotent (a name the program itself declares, or one already embedded by an earlier REPL
/// entry, is left alone: the program's own declaration wins, matching prelude shadowing).
pub fn extend_reflection(info: &mut noeta_ast::reflect::ReflectionInfo) {
    for ty in extension_attribute_types() {
        if info.type_named(&ty.name).is_none() {
            info.types.push(ty);
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
    // Sources that already produced a non-attached text block — the first is the module doc, the
    // rest are sections. Adjacency state is per-tier, so a module doc and a module spec coexist.
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
        let decl_target = program.stmts.get(i + 1).and_then(|next| {
            if next.span().source != span.source {
                return None;
            }
            let (name, name_span) = match next {
                Stmt::Fn(d) => (&d.name, d.name_span),
                Stmt::Struct(d) => (&d.name, d.name_span),
                Stmt::Class(d) => (&d.name, d.name_span),
                Stmt::Enum(d) => (&d.name, d.name_span),
                _ => return None,
            };
            Some(DocTarget::Decl {
                name: name.clone(),
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
    for stmt in &program.stmts {
        let methods = match stmt {
            Stmt::Struct(d) => &d.methods,
            Stmt::Class(d) => &d.methods,
            Stmt::Enum(d) => &d.methods,
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
                        name: method.name.clone(),
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

/// Resolve `program`'s `@<tier> { … }` blocks against `active` (the set of live tier names),
/// **everywhere they appear** — top-level (a `@test` block of declarations) and nested in statement
/// position (a `@debug { … }` block inside a fn/method body or a control-flow branch). Active blocks
/// are inlined in place; inactive blocks are dropped; every block's name is validated. The `@test`
/// fns among the activated *top-level* blocks are collected as roots the runner invokes.
pub fn activate_tiers(program: &Program, active: &[&str]) -> Activated {
    activate_tiers_with(program, active, &std::collections::BTreeMap::new())
}

/// [`activate_tiers`] under a target's tier → **provider** map (tier-providers provider
/// dispatch): the provider selection decides which declaration's config attribute a
/// `@<tier>(args)` block stamps — `bench = "criterion"` stamps criterion's config, not std's
/// `Bench`. An empty map is default resolution (extension first, then first declaration).
pub fn activate_tiers_with(
    program: &Program,
    active: &[&str],
    providers: &std::collections::BTreeMap<String, String>,
) -> Activated {
    let mut roots = Roots::default();
    let mut diagnostics = Vec::new();
    // The tier name-space: built-ins ∪ the program's own `@tier` declarations (imported packages'
    // included — the linked program carries their decls). Unknown-name validation, config-attr
    // stamping, and root collection all resolve against it.
    let registry = TierRegistry::collect(program);
    // With the `doc` tier live, a declaration-attached `@doc` block (adjacency-resolved on the
    // *input* program, before its blocks are gone) stamps `#[Doc("…")]` onto its declaration —
    // the text tier's counterpart of `@bench`'s knob stamping, giving runtime docstrings via
    // `attributes_of`. Keyed by the declaration's name-span, which survives inlining.
    let doc_stamps: std::collections::HashMap<Span, String> = if active.contains(&"doc") {
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
    // The text roots of active *declared* text tiers (text-tiers arc), resolved on the input
    // program like the doc stamps — the blocks themselves strip below, exactly like `@doc`.
    let mut texts: std::collections::BTreeMap<String, Vec<TextBlock>> =
        std::collections::BTreeMap::new();
    for block in resolve_texts(program) {
        if block.tier != "doc"
            && active.contains(&block.tier.as_str())
            && registry.declared(&block.tier).is_some()
        {
            texts.entry(block.tier.clone()).or_default().push(block);
        }
    }
    // The top-level statement list collects roots (a `@test`/`@bench` block's fns are runnable roots
    // only here — a tier block nested in a fn body holds inline code, not roots).
    let mut stmts = resolve_block(
        &program.stmts,
        active,
        &registry,
        providers,
        &mut diagnostics,
        &mut roots,
        true,
    );
    if !doc_stamps.is_empty() {
        for stmt in &mut stmts {
            let (name_span, attrs) = match stmt {
                Stmt::Fn(d) => (d.name_span, &mut d.attrs),
                Stmt::Struct(d) => (d.name_span, &mut d.decorators.attrs),
                Stmt::Class(d) => (d.name_span, &mut d.decorators.attrs),
                Stmt::Enum(d) => (d.name_span, &mut d.decorators.attrs),
                _ => continue,
            };
            if let Some(text) = doc_stamps.get(&name_span)
                && !attrs.iter().any(|a| a.name == TIER_ATTR_DOC)
            {
                attrs.push(Attribute {
                    name: TIER_ATTR_DOC.to_string(),
                    name_span,
                    args: vec![AttrArg {
                        name: Some("text".to_string()),
                        value: noeta_ast::AttrValue::Str(text.clone()),
                        span: name_span,
                    }],
                    span: name_span,
                });
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
                match dir.name.as_str() {
                    "test" if active.contains(&"test") => make_test = true,
                    "bench" if active.contains(&"bench") => make_bench = true,
                    _ => {}
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
                });
            }
            if make_bench {
                roots.benches.push(TierFn {
                    name,
                    span: method.name_span,
                    attrs: method.attrs.clone(),
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
    active: &[&str],
    registry: &TierRegistry,
    providers: &std::collections::BTreeMap<String, String>,
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
                active,
                registry,
                providers,
                diagnostics,
            ));
            continue;
        };

        let config = registry.config_attribute_for(tier, providers);
        if !registry.is_known(tier) {
            diagnostics.push(unknown_tier_diagnostic(registry.reg(), tier, *tier_span));
        } else if registry.is_expr_tier(tier) {
            // An expression tier's block in statement position (E0052) — never activates.
            diagnostics.push(expr_tier_statement_diagnostic(tier, *tier_span));
        } else if config.is_none()
            && let Some(d) = knobless_args_diagnostic_for(tier, args)
        {
            // Directive args on a knob-less tier (`@test(x)`) — E0037. A knob-carrying tier's args
            // are validated as the stamped attribute's construction, by the checker, below.
            diagnostics.push(d);
        }
        if !active.contains(&tier.as_str()) {
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
        let resolved = resolve_block(
            items,
            active,
            registry,
            providers,
            diagnostics,
            roots,
            collect_roots,
        );
        for mut item in resolved {
            if let Stmt::Fn(decl) = &mut item {
                decl.is_dev_tier = true;
                if let Some(attr_name) = config_attr.as_deref()
                    && !decl.attrs.iter().any(|a| a.name == attr_name)
                {
                    decl.attrs
                        .push(synthesized_config_attr(attr_name, args, *tier_span));
                }
                if collect_roots {
                    let sink = match tier.as_str() {
                        "test" => Some(&mut roots.tests),
                        "bench" => Some(&mut roots.benches),
                        // A declared tier's fns are roots for its runner's dispatch.
                        name if registry.declared(name).is_some() => {
                            Some(roots.custom.entry(name.to_string()).or_default())
                        }
                        _ => None,
                    };
                    if let Some(sink) = sink {
                        sink.push(TierFn {
                            name: decl.name.clone(),
                            span: decl.name_span,
                            attrs: decl.attrs.clone(),
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
    active: &[&str],
    registry: &TierRegistry,
    providers: &std::collections::BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Stmt {
    let mut stmt = stmt.clone();
    let block = |stmts: &[Stmt], diags: &mut Vec<Diagnostic>| -> Vec<Stmt> {
        // Nested statement lists never produce runnable roots (`collect_roots = false`); the sink is
        // a throwaway.
        let mut sink = Roots::default();
        resolve_block(stmts, active, registry, providers, diags, &mut sink, false)
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
                && !self.symbols.attributes.contains(config)
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
            match stmt {
                // Placement — which directive may sit on which declaration — is one check for
                // every kind, driven off the metadata table (`directives.rs`). What stays here is
                // the per-directive work that placement does not cover: a `@role`'s tags must name
                // variants of a `@semantic` enum, and a `@packed` struct's fields must all be
                // packable.
                Stmt::Struct(r) => {
                    self.check_directive_placement(
                        &r.decorators,
                        &crate::directives::Placement {
                            site: Sites::STRUCT,
                            article_noun: "a struct",
                            name: &r.name,
                            name_span: r.name_span,
                        },
                    );
                    self.check_role_tags(
                        r.name_span,
                        r.decorators.role.as_deref(),
                        r.decorators.attribute.is_some(),
                    );
                    self.check_packed_struct(r);
                }
                Stmt::Class(c) => {
                    self.check_directive_placement(
                        &c.decorators,
                        &crate::directives::Placement {
                            site: Sites::CLASS,
                            article_noun: "a class",
                            name: &c.name,
                            name_span: c.name_span,
                        },
                    );
                }
                Stmt::Enum(e) => {
                    self.check_directive_placement(
                        &e.decorators,
                        &crate::directives::Placement {
                            site: Sites::ENUM,
                            article_noun: "an enum",
                            name: &e.name,
                            name_span: e.name_span,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// Whether `ty` can be a field of a `@packed` struct (P-PACK): a primitive (`int`/`float`/`bool`)
    /// or another packed struct (a non-generic `Named` in `packed_structs`). Everything else — a
    /// string/list/map/class/enum/`dyn`/generic — is heap-shaped and cannot lay out flat.
    pub(crate) fn is_packable_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Int | Type::Float | Type::F32 | Type::Bool => true,
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
    /// Resolve a checker [`Type`] into a [`noeta_ext_abi::TypeRecipe`] for call-site-typed
    /// deserialization (`json.parse::<T>`), or `None` if `T` has no JSON decoding: an enum or class
    /// (a reference/identity type, or a sum with no canonical JSON form), a tuple/set/result/`dyn`,
    /// a non-string-keyed map, a generic instantiation, or a struct with any such field. A struct
    /// records its fields in **declared order** (so the decoder emits them in the order the backend's
    /// registered type expects).
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
            // Only a non-generic value struct decodes (a class is reference/identity; an enum has no
            // canonical JSON shape). The field set is the declared record fields, in order.
            Type::Named(name, args)
                if args.is_empty()
                    && self.symbols.type_kinds.get(name)
                        == Some(&noeta_types::TypeKind::Struct) =>
            {
                let fields = self
                    .symbols
                    .records
                    .get(name)?
                    .iter()
                    .map(|(fname, fty)| Some((fname.clone(), self.type_to_recipe(fty)?)))
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
            _ => return None,
        })
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
            if !self.symbols.semantic_enums.contains(&tag.enum_name) {
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
            Some("Bench")
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
        let declared: Vec<&str> = noeta_stdlib::registry::ext_attributes()
            .map(|a| a.name)
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
                declared.contains(&name),
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

    /// A `@bench(iterations: N)` block's directive args are stamped onto each contained fn as the
    /// `Bench` config attribute (the desugar), and a fn that already carries its own `#[Bench(…)]`
    /// keeps it — the per-fn attribute wins over the block's.
    #[test]
    fn bench_block_args_stamp_the_config_attribute() {
        let program = parse_program(
            "@bench(iterations: 1000) {\n\
                 fn plain(): void { return; }\n\
                 #[Bench(iterations: 5)]\n\
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
