//! What an extension's `@`-directive is allowed to be: where it may attach, and what arguments it
//! may take.
//!
//! These decisions used to live in the checker, which was the only consumer. Compile-time
//! *expansion* ([`noeta_ext_abi::registry::ExtDirective::expand`]) added a second: the loader must
//! not hand a hook an invocation the checker is about to reject, because a hook's contract is that
//! it only ever sees a directive that sat somewhere legal with arguments it declared.
//!
//! Two consumers is exactly the condition under which a rule gets written twice and the copies
//! drift, so the rule moved here rather than being duplicated — this crate is the one both the
//! checker and the loader already depend on. The split of labour is deliberate: these functions
//! decide, and the checker *words* the diagnostic. Wording is the checker's job; the answer is not
//! two jobs.

use noeta_ast::{AttrArg, Sites};
use noeta_ext_abi::registry::{ExtDirective, TierSite};

/// Widen the extension ABI's four-variant [`TierSite`] into the AST's finer site model. An
/// extension says "a type"; the language knows that means a struct, a class or an enum — and never
/// a trait, which is a contract rather than a data type.
pub fn sites_of(sites: &[TierSite]) -> Sites {
    sites.iter().fold(Sites::NONE, |acc, s| {
        acc.union(match s {
            TierSite::Function => Sites::FN,
            TierSite::Method => Sites::METHOD,
            TierSite::Type => Sites::TYPE,
            TierSite::Trait => Sites::TRAIT,
        })
    })
}

/// Whether something declaring `sites` may attach to a declaration at `at` — the one predicate
/// behind every attachment question in the language.
///
/// An empty `sites` attaches to **nothing** (a pure block tier: `@debug { … }`, `@json { … }`),
/// and `Sites::NONE` is not a declaration, so both answer `false`. That the empty case used to
/// mean "unrestricted" is why this could never be enforced: the tiers that attach to nothing were
/// spelled identically to the tiers that attach to everything.
pub fn attaches_to(sites: &[TierSite], at: Sites) -> bool {
    !at.is_empty() && sites_of(sites).contains(at)
}

/// Narrow a declaration site back into the ABI's vocabulary, for handing to an extension that
/// speaks only [`TierSite`].
///
/// Total in the direction that matters: `at` is always a *single* site bit (it comes from
/// [`noeta_ast::Stmt::decorated`], which names one declaration), so the several AST sites that
/// share one `TierSite` collapse without ambiguity. A `Sites::NONE` or a multi-bit set is not a
/// declaration and yields `None`.
pub fn tier_site_of(at: Sites) -> Option<TierSite> {
    // An if-chain rather than a `match` on the constants: `Sites` is a bit-set newtype, so
    // `Sites::TYPE` overlaps `Sites::STRUCT` and pattern arms would read as though they were
    // disjoint when they are not.
    if at == Sites::FN {
        Some(TierSite::Function)
    } else if at == Sites::METHOD {
        Some(TierSite::Method)
    } else if at == Sites::STRUCT || at == Sites::CLASS || at == Sites::ENUM {
        Some(TierSite::Type)
    } else if at == Sites::TRAIT {
        Some(TierSite::Trait)
    } else {
        None
    }
}

/// One way an invocation fails its directive's declared argument contract.
///
/// Carries the *facts*, not a message: the checker turns these into diagnostics with the spans it
/// has, and the loader only asks whether the list is empty. A shared enum keeps those two from
/// disagreeing about what "valid" means while still letting the checker say it well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgFault {
    /// More positional arguments than [`ExtDirective::max_args`] allows.
    TooManyPositional { max: usize, given: usize },
    /// A `key:` the directive does not list in [`ExtDirective::named_keys`]. `index` is the
    /// argument's position in the invocation, so the caller can blame the right span.
    UnknownKey { index: usize, key: String },
}

/// Check an invocation against what the directive declared: how many positional arguments it
/// takes, and which `name:` keys it understands.
///
/// Returns **every** fault, not the first — an author who wrote two unknown keys should learn
/// about both in one compile rather than one per attempt.
pub fn arg_faults(directive: &ExtDirective, args: &[AttrArg]) -> Vec<ArgFault> {
    let mut faults = Vec::new();
    let positional = args.iter().filter(|a| a.name.is_none()).count();
    if let Some(max) = directive.max_args
        && positional > max
    {
        faults.push(ArgFault::TooManyPositional {
            max,
            given: positional,
        });
    }
    for (index, arg) in args.iter().enumerate() {
        let Some(key) = &arg.name else { continue };
        if !directive.named_keys.contains(&key.as_str()) {
            faults.push(ArgFault::UnknownKey {
                index,
                key: key.clone(),
            });
        }
    }
    faults
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: ExtDirective = ExtDirective {
        name: "openapi",
        sites: &[TierSite::Type],
        max_args: Some(1),
        named_keys: &["version"],
        detail: "",
        doc: "",
        params: &[],
        expand: None,
    };

    fn positional(value: &str) -> AttrArg {
        AttrArg {
            name: None,
            value: noeta_ast::AttrValue::Str(value.to_string()),
            span: noeta_span::Span::new(0, 0),
        }
    }

    fn named(key: &str) -> AttrArg {
        AttrArg {
            name: Some(key.to_string()),
            ..positional("v")
        }
    }

    #[test]
    fn a_conforming_invocation_has_no_faults() {
        assert!(arg_faults(&D, &[positional("spec.yaml"), named("version")]).is_empty());
    }

    #[test]
    fn every_unknown_key_is_reported_not_just_the_first() {
        let faults = arg_faults(&D, &[named("v1"), named("v2")]);
        assert_eq!(
            faults,
            vec![
                ArgFault::UnknownKey {
                    index: 0,
                    key: "v1".to_string()
                },
                ArgFault::UnknownKey {
                    index: 1,
                    key: "v2".to_string()
                },
            ]
        );
    }

    #[test]
    fn named_arguments_do_not_count_against_the_positional_maximum() {
        // `max_args: Some(1)` bounds the positional arguments alone; a `version:` beside the one
        // positional argument is not a second positional.
        assert!(arg_faults(&D, &[positional("a"), named("version")]).is_empty());
        assert_eq!(
            arg_faults(&D, &[positional("a"), positional("b")]),
            vec![ArgFault::TooManyPositional { max: 1, given: 2 }]
        );
    }

    #[test]
    fn empty_sites_attach_to_nothing_and_none_is_not_a_declaration() {
        assert!(!attaches_to(&[], Sites::STRUCT));
        assert!(!attaches_to(&[TierSite::Type], Sites::NONE));
        assert!(attaches_to(&[TierSite::Type], Sites::STRUCT));
        assert!(!attaches_to(&[TierSite::Type], Sites::TRAIT));
    }

    #[test]
    fn narrowing_and_widening_agree_on_every_declaration_site() {
        // The round trip is what keeps the loader's view of "may this attach" identical to the
        // checker's: every site the AST can report must narrow into a `TierSite` that widens back
        // to a set containing it.
        for at in [
            Sites::FN,
            Sites::METHOD,
            Sites::STRUCT,
            Sites::CLASS,
            Sites::ENUM,
            Sites::TRAIT,
        ] {
            let narrowed = tier_site_of(at).expect("a declaration site narrows");
            assert!(
                sites_of(&[narrowed]).contains(at),
                "{at:?} did not survive the round trip"
            );
        }
        assert_eq!(tier_site_of(Sites::NONE), None);
    }
}
