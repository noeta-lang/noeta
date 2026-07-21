//! The one placement check for the built-in `@`-directives.
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

use noeta_ast::{BuiltinDirective, Decorators, Sites};
use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;

use crate::Checker;

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
    }
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
