//! The `@`-directive name-space: what `@name` resolves to, and where it may be written.
//!
//! Two things that used to be scattered, kept together because they answer the same question from
//! opposite ends. [`DirectiveRegistry`] is the one lookup over the whole name-space — the closed
//! built-in set plus the open tier set — replacing the "`from_name`, else go build a `TierRegistry`
//! and probe both halves" that each IDE surface had open-coded. [`check_directive_placement`] is
//! the one placement check.
//!
//! ## Placement
//!
//! Which declarations a directive may sit on used to be decided in four files and several shapes: a
//! hand-written `else if` chain for traits (`traits.rs`), `check_misplaced_semantic` and
//! `check_misplaced_packed` for two specific directives, an inline arm for `@role` on a class, and
//! nothing at all for the rest. The rule was therefore only enforced where somebody had remembered
//! to enforce it, and three cases had been forgotten:
//!
//!   - `@validated` on an enum or a trait — no check anywhere, and (before the AST carried it) no
//!     field to check either, so it was discarded by the parser and the author got silence.
//!   - `@attribute` on an enum — the same.
//!   - `@role` on an enum — checked for a class, forgotten for an enum.
//!
//! [`check_directive_placement`] replaces all of it with one loop over
//! [`BuiltinDirective::ALL`](noeta_ast::BuiltinDirective::ALL), consulting the `sites` field of the
//! shared metadata table. A new directive cannot be added without declaring where it belongs, and
//! cannot be added to a declaration kind nobody thought about — the check is total by construction
//! rather than by diligence.
//!
//! ## Diagnostic codes
//!
//! Existing codes are preserved exactly, because they are the language's public error surface and a
//! conformance case names each one: a misplaced `@packed` stays `E0038`, `@semantic`/`@role` stay
//! `E0031`, and anything on a `trait` stays `E0053` regardless of directive. The cases that had no
//! code because they had no check report `E0054` (`InvalidDirectiveSite`) — which already means
//! precisely "this directive cannot attach here", and was until now used only by the tier
//! attachment gate.
//!
//! That leaves the codes inconsistent: three different codes for one class of fault, depending on
//! which directive it is. Unifying them is a visible change to every affected program's error
//! output, so it is a separate decision rather than a side effect of this refactor.

use noeta_ast::{BuiltinDirective, Decorators, Program, Sites};
use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;

use crate::Checker;
use crate::tiers::{DeclaredTier, TierRegistry};

/// What an `@name` resolves to across the whole directive name-space.
///
/// The name-space is deliberately half-closed and half-open: the built-in decorator directives are
/// a fixed enum the grammar knows, while tiers are an open set contributed by extensions and by any
/// package's own `@tier` declaration. Keeping both in one lookup — rather than "try `from_name`,
/// else go ask the tier registry" open-coded at each call site — is what lets a consumer handle the
/// name-space totally instead of remembering both halves.
#[derive(Debug, Clone, PartialEq)]
pub enum DirectiveKind<'a> {
    /// A built-in decorator directive. The closed set the parser's grammar recognises.
    Builtin(BuiltinDirective),
    /// A tier declared by an installed extension (`test`/`bench`/`doc`/`debug`, plus any native
    /// package's).
    ExtTier(&'static noeta_ext_abi::registry::ExtTier),
    /// A tier declared in the (linked) program by `@tier(name) fn runner(…)`.
    DeclaredTier(&'a DeclaredTier),
    /// A directive an installed extension declares. Resolved last, so an extension can shadow
    /// neither a built-in directive nor a tier.
    ExtDirective(&'static noeta_ext_abi::registry::ExtDirective),
}

/// One offerable `@name`, as completion and hover want it.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectiveEntry {
    pub name: String,
    /// The one-line usage shown beside the name.
    pub detail: String,
}

/// The whole `@name` name-space a program sees: the built-in directives ∪ the tier name-space.
///
/// Three IDE surfaces — completion after `@`, signature help inside `@name(…)`, and hover — each
/// used to union these halves themselves, with their own copy of how a tier renders and their own
/// `from_name`-else-tier dispatch. They resolve through here instead, so a name that completes is a
/// name that hovers.
#[derive(Debug, Clone)]
pub struct DirectiveRegistry {
    tiers: TierRegistry,
}

impl DirectiveRegistry {
    /// Collect the name-space visible to `program`, resolving tiers against the process-global
    /// extension registry (the single-registry CLI/IDE path).
    pub fn collect(program: &Program) -> DirectiveRegistry {
        DirectiveRegistry {
            tiers: TierRegistry::collect(program),
        }
    }

    /// As [`collect`](Self::collect), but resolving the extension half against an explicit registry
    /// (instance-registry IR4 — an embed session's own extension set).
    pub fn collect_with_registry(
        program: &Program,
        registry: &'static noeta_ext_abi::registry::Registry,
    ) -> DirectiveRegistry {
        DirectiveRegistry {
            tiers: TierRegistry::collect_with_registry(program, registry),
        }
    }

    /// Wrap a [`TierRegistry`] the caller already collected — the checker holds one for the whole
    /// check, and re-collecting per declaration would walk the program once per decorated type.
    pub fn from_tiers(tiers: TierRegistry) -> DirectiveRegistry {
        DirectiveRegistry { tiers }
    }

    /// The tier half on its own, for the consumers that genuinely want tiers rather than directives
    /// (activation, runner dispatch).
    pub fn tiers(&self) -> &TierRegistry {
        &self.tiers
    }

    /// Resolve `@name`. A built-in directive wins over a tier of the same name — the grammar
    /// already commits to reading the built-in spelling, so any other answer would let completion
    /// describe something the parser will not produce.
    pub fn lookup(&self, name: &str) -> Option<DirectiveKind<'_>> {
        if let Some(d) = BuiltinDirective::from_name(name) {
            return Some(DirectiveKind::Builtin(d));
        }
        if let Some(t) = self.tiers.extension_tiers().find(|t| t.name == name) {
            return Some(DirectiveKind::ExtTier(t));
        }
        if let Some(t) = self.tiers.declared(name) {
            return Some(DirectiveKind::DeclaredTier(t));
        }
        self.tiers
            .registry()
            .find_ext_directive(name)
            .map(DirectiveKind::ExtDirective)
    }

    /// Every offerable `@name`: the built-ins in declaration order, then the extension tiers, then
    /// the program-declared ones. De-duplicated by name, first occurrence winning — a program that
    /// re-declares an extension tier is a second *provider* of one name, not a second name.
    pub fn all(&self) -> Vec<DirectiveEntry> {
        let mut out: Vec<DirectiveEntry> = Vec::new();
        let mut push = |name: String, detail: String| {
            if !out.iter().any(|e| e.name == name) {
                out.push(DirectiveEntry { name, detail });
            }
        };
        for d in BuiltinDirective::ALL {
            push(d.as_str().to_string(), d.info().detail.to_string());
        }
        for t in self.tiers.extension_tiers() {
            push(t.name.to_string(), tier_detail(t.expr, t.text, ""));
        }
        for d in self.tiers.registry().ext_directives() {
            push(d.name.to_string(), d.detail.to_string());
        }
        for t in self.tiers.declared_tiers() {
            let provider = if t.root.is_empty() {
                String::new()
            } else {
                format!(" [{}]", t.root)
            };
            push(
                t.name.clone(),
                tier_detail(t.expr.as_deref(), t.text.as_deref(), &provider),
            );
        }
        out
    }
}

/// How a tier describes itself in completion — one renderer for both halves of the tier
/// name-space, which previously had two near-identical copies inline in `noeta-ide`.
fn tier_detail(expr: Option<&str>, text: Option<&str>, suffix: &str) -> String {
    match (expr, text) {
        (Some(ty), _) => format!("expression tier — a block is a value of {ty}{suffix}"),
        (None, Some(lang)) => format!("text tier ({lang}){suffix}"),
        (None, None) => format!("dev-tier{suffix}"),
    }
}

/// The declaration a directive was written on, for the diagnostic's wording.
pub(crate) struct Placement<'a> {
    /// The site as a set bit — `Sites::ENUM` for an enum, and so on.
    pub site: Sites,
    /// The word used in the message ("an enum", "a class").
    pub article_noun: &'a str,
    pub name: &'a str,
    pub name_span: Span,
}

impl Checker {
    /// Report every directive on this declaration that does not belong there.
    ///
    /// Total over [`BuiltinDirective::ALL`] and over declaration kinds: the loop asks the metadata
    /// table where each directive is legal rather than encoding the answer per call site.
    pub(crate) fn check_directive_placement(
        &mut self,
        decorators: &Decorators,
        at: &Placement<'_>,
    ) {
        for directive in BuiltinDirective::ALL {
            if !is_present(decorators, directive) {
                continue;
            }
            if directive.info().sites.contains(at.site) {
                continue;
            }
            // The checks this replaces used TWO conventions: `@packed`/`@semantic` pointed at the
            // directive keyword, while the trait chain and the class `@role` arm pointed at the
            // declaration's name. There was no single existing behavior to preserve.
            //
            // Point at the directive where the AST knows where it is — `@semantic`, `@packed` and
            // `@validated` store their own keyword span — and fall back to the declaration name
            // otherwise. `@derive`/`@role`/`@attribute` record only their *arguments'* spans, and
            // underlining `Clone` in `@derive(Clone)` for a *placement* fault would be worse than
            // underlining the declaration. Carrying every directive's keyword span on `Decorators`
            // would let this be uniform; that is a change to the AST, not to this check.
            let span = keyword_span(decorators, directive).unwrap_or(at.name_span);
            let code = misplacement_code(directive, at.site);
            let message = if at.site == Sites::TRAIT {
                format!("`@{directive}` does not apply to a trait `{}`", at.name)
            } else {
                format!(
                    "`@{directive}` does not apply to {} `{}`",
                    at.article_noun, at.name
                )
            };
            let legal = directive.info().sites.label();
            self.error(code, span, message)
                .help(format!("`@{directive}` applies to {legal}"));
        }
        self.check_foreign_directives(decorators, at);
    }

    /// Resolve every directive the decorator grammar does not own against the full name-space.
    ///
    /// The parser used to answer this, which meant it could only ever answer for the *closed*
    /// set — an extension's directive was indistinguishable from a typo, so `@openapi(…)` was a
    /// syntax error no extension could make legal. Deciding here is what opens the name-space,
    /// and it also folds the old parser-level errors into the one placement check: `@tier` on a
    /// type is now a misplacement like any other, rather than a separate `UnexpectedToken`.
    fn check_foreign_directives(&mut self, decorators: &Decorators, at: &Placement<'_>) {
        if decorators.foreign.is_empty() {
            return;
        }
        // Over the registry the check already holds — collected once for the whole program.
        let registry = DirectiveRegistry::from_tiers(self.symbols.tier_registry.clone());
        for f in &decorators.foreign {
            match registry.lookup(&f.name) {
                // An extension directive: legal here unless it restricts its sites.
                Some(DirectiveKind::ExtDirective(d)) => {
                    let allowed = sites_of(d.sites);
                    if !allowed.is_empty() && !allowed.contains(at.site) {
                        self.error(
                            DiagnosticCode::InvalidDirectiveSite,
                            f.name_span,
                            format!(
                                "`@{}` does not apply to {} `{}`",
                                f.name, at.article_noun, at.name
                            ),
                        )
                        .help(format!(
                            "`@{}` applies to {}",
                            f.name,
                            allowed.label()
                        ));
                    }
                }
                // `@tier` decorates the `fn` that runs a tier, never a type.
                Some(DirectiveKind::Builtin(BuiltinDirective::Tier)) => {
                    self.error(
                        DiagnosticCode::InvalidDirectiveSite,
                        f.name_span,
                        format!(
                            "`@tier` does not apply to {} `{}`",
                            at.article_noun, at.name
                        ),
                    )
                    .help(
                        "a tier is declared by decorating its runner: `@tier(name) fn run(…)`"
                            .to_string(),
                    );
                }
                // A tier name used as a decorator. A tier annotates a `fn` or introduces a block;
                // it is not a declaration decorator.
                Some(DirectiveKind::ExtTier(_)) | Some(DirectiveKind::DeclaredTier(_)) => {
                    self.error(
                        DiagnosticCode::InvalidDirectiveSite,
                        f.name_span,
                        format!(
                            "`@{}` is a tier, and does not decorate {} `{}`",
                            f.name, at.article_noun, at.name
                        ),
                    )
                    .help(format!(
                        "write a tier as a block — `@{} {{ … }}` — or annotate a `fn` with it",
                        f.name
                    ));
                }
                // Any other built-in reaching here would mean the parser failed to consume a name
                // its own grammar owns.
                Some(DirectiveKind::Builtin(d)) => {
                    self.error(
                        DiagnosticCode::InvalidDirectiveSite,
                        f.name_span,
                        format!("`@{d}` does not apply to {} `{}`", at.article_noun, at.name),
                    );
                }
                None => {
                    let known: Vec<String> = registry.all().into_iter().map(|e| e.name).collect();
                    let d = self.error(
                        DiagnosticCode::UnknownDirective,
                        f.name_span,
                        format!("unknown directive `@{}`", f.name),
                    );
                    match noeta_diagnostics::closest(&f.name, known.iter().map(String::as_str)) {
                        Some(s) => d.help(format!("did you mean `@{s}`?")),
                        None => d.help(format!(
                            "the directives that decorate a declaration are {}",
                            BuiltinDirective::decorator_list()
                        )),
                    };
                }
            }
        }
    }
}

/// Widen the extension ABI's three-variant [`TierSite`](noeta_ext_abi::registry::TierSite) into the
/// checker's finer site model. An extension says "a type"; the checker knows that means a struct, a
/// class or an enum — and never a trait.
fn sites_of(sites: &[noeta_ext_abi::registry::TierSite]) -> Sites {
    use noeta_ext_abi::registry::TierSite;
    sites.iter().fold(Sites::NONE, |acc, s| {
        acc.union(match s {
            TierSite::Function => Sites::FN,
            TierSite::Method => Sites::METHOD,
            TierSite::Type => Sites::TYPE,
        })
    })
}

/// The span of the directive keyword itself, for the directives whose AST slot records one.
///
/// `None` where the AST keeps only the arguments' spans (`@derive`, `@role`, `@attribute`) — the
/// caller falls back to the declaration's name rather than blaming an argument.
fn keyword_span(decorators: &Decorators, directive: BuiltinDirective) -> Option<Span> {
    match directive {
        BuiltinDirective::Semantic => decorators.semantic,
        BuiltinDirective::Packed => decorators.packed.map(|p| p.span),
        BuiltinDirective::Validated => decorators.validated,
        BuiltinDirective::Derive
        | BuiltinDirective::Role
        | BuiltinDirective::Attribute
        | BuiltinDirective::Tier => None,
    }
}

/// Whether `directive` was written on this declaration.
///
/// Exhaustive on the directive: a new one must say how its presence is detected, which is what
/// keeps this check total.
fn is_present(decorators: &Decorators, directive: BuiltinDirective) -> bool {
    match directive {
        BuiltinDirective::Derive => !decorators.derives.is_empty(),
        BuiltinDirective::Attribute => decorators.attribute.is_some(),
        BuiltinDirective::Role => decorators.role.is_some(),
        BuiltinDirective::Semantic => decorators.semantic.is_some(),
        BuiltinDirective::Packed => decorators.packed.is_some(),
        BuiltinDirective::Validated => decorators.validated.is_some(),
        // `@tier` decorates a `fn` and rides on `FnDecl::tier`, never on a type's decorators, so it
        // can never be present here. Named rather than folded into a `_` arm so a new directive
        // must state how to detect it.
        BuiltinDirective::Tier => false,
    }
}

/// The code for "this directive does not belong here".
///
/// Preserves the codes that already existed — they are the public error surface, and conformance
/// cases name them. `E0054` covers the placements that previously had no check at all.
fn misplacement_code(directive: BuiltinDirective, at: Sites) -> DiagnosticCode {
    // A trait rejects every type directive under one code, whichever directive it is.
    if at == Sites::TRAIT {
        return DiagnosticCode::InvalidTraitDeclaration;
    }
    match directive {
        BuiltinDirective::Packed => DiagnosticCode::InvalidPackedType,
        BuiltinDirective::Semantic | BuiltinDirective::Role => DiagnosticCode::InvalidRole,
        BuiltinDirective::Attribute => DiagnosticCode::NotAnAttribute,
        // `@validated` and `@derive` had no placement check outside a trait, so there is no code to
        // preserve; `E0054` already means "this directive cannot attach here".
        BuiltinDirective::Derive | BuiltinDirective::Validated | BuiltinDirective::Tier => {
            DiagnosticCode::InvalidDirectiveSite
        }
    }
}

#[cfg(test)]
mod registry_tests {
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
            "fixture must parse cleanly: {:?}",
            parsed.diagnostics
        );
        parsed.program
    }

    /// One lookup answers for both halves of the name-space — the closed built-in set and the open
    /// tier set — and says `None` exactly once for a name in neither. Consumers used to ask
    /// `from_name` and, on `None`, go and build a `TierRegistry` themselves.
    #[test]
    fn one_lookup_spans_the_whole_directive_name_space() {
        let program = parse_program("@tier(audit) fn run_audit(roots: List<string>) {}\n");
        let registry = DirectiveRegistry::collect(&program);

        assert_eq!(
            registry.lookup("derive"),
            Some(DirectiveKind::Builtin(BuiltinDirective::Derive)),
        );
        assert!(
            matches!(registry.lookup("test"), Some(DirectiveKind::ExtTier(_))),
            "a std tier resolves through the same lookup",
        );
        assert!(
            matches!(
                registry.lookup("audit"),
                Some(DirectiveKind::DeclaredTier(d)) if d.runner == "run_audit",
            ),
            "the program's own `@tier` declaration resolves too",
        );
        assert_eq!(registry.lookup("openapi"), None);
    }

    /// Enumeration covers everything the lookup accepts — the property completion relies on, and
    /// the one that the hand-rolled unions could break by forgetting a half.
    #[test]
    fn everything_enumerated_resolves_and_everything_resolvable_is_enumerated() {
        let program = parse_program("@tier(audit) fn run_audit(roots: List<string>) {}\n");
        let registry = DirectiveRegistry::collect(&program);
        let all = registry.all();

        for entry in &all {
            assert!(
                registry.lookup(&entry.name).is_some(),
                "offered `@{}` does not resolve",
                entry.name
            );
            assert!(!entry.detail.is_empty(), "`@{}` has no detail", entry.name);
        }
        for d in BuiltinDirective::ALL {
            assert!(all.iter().any(|e| e.name == d.as_str()), "missing `@{d}`");
        }
        assert!(all.iter().any(|e| e.name == "audit"));

        let mut names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a name is offered once: {names:?}");
    }

    /// A program that re-declares an extension tier is a second *provider* of one name, not a
    /// second name — and the built-in spelling wins the lookup, because the grammar commits to it.
    #[test]
    fn a_redeclared_tier_is_one_name_and_builtins_win() {
        let program = parse_program("@tier(test) fn run_test(roots: List<string>) {}\n");
        let registry = DirectiveRegistry::collect(&program);
        assert_eq!(
            registry.all().iter().filter(|e| e.name == "test").count(),
            1,
        );
        assert!(matches!(
            registry.lookup("test"),
            Some(DirectiveKind::ExtTier(_))
        ));
    }
}
