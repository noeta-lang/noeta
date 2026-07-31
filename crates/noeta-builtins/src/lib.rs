//! The M0 prelude support: the small set of built-in capabilities programs rely on
//! without importing anything — today just the prelude name metadata.
//! (`IdGen`, the M0 id source, left with the id-entropy arc: `id.next_id()` is a registry
//! function over the Host's `Ids` capability now, one counter shared by both backends.)

/// How a prelude name reaches a program — the distinction the two consumers of [`PRELUDE`] differ
/// on, recorded here rather than as a second filtered list beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreludeForm {
    /// A **keyword**: consumed by the parser as its own statement form, so it never reaches
    /// identifier resolution and is not a shadowable binding. `echo` is the only one.
    Keyword,
    /// An ordinary **identifier** binding the prelude supplies (`Ok`, `some`, `panic`, …) — the
    /// names the checker's unknown-name gate must always resolve.
    Value,
}

/// The names reserved by the M0 prelude, each with the form it takes. Used by name resolution
/// (Slice 8) and by the IDE to mark prelude identifiers. Grows as value-returning builtins land.
///
/// The `Keyword`/`Value` split is here, on the entry, because the checker wants only the value
/// half (a keyword never reaches its unknown-name gate) while the compiler wants all of it — and
/// before this the checker kept its own copy of the value half, so "did a prelude name land?" had
/// two answers. `noeta-check` deliberately does not link the lexer, so it cannot ask which names
/// are keywords; the form travels with the name instead, and
/// [`tests::the_keyword_form_is_the_lexers_own_answer`] checks it against the lexer.
pub const PRELUDE: &[(&str, PreludeForm)] = &[
    ("echo", PreludeForm::Keyword),
    // `len`/`map`/`filter`/`sum` left the prelude (prelude-redesign P1.2): they are collection
    // METHODS now (`xs.len()`, `xs.map(f)`), passable as values via method handles (`list.len`).
    ("Ok", PreludeForm::Value),
    ("Err", PreludeForm::Value),
    ("some", PreludeForm::Value),
    ("none", PreludeForm::Value),
    ("panic", PreludeForm::Value),
    ("assert", PreludeForm::Value),
    // `signal`/`computed`/`effect` left the prelude (P2a) for `use std.reactive`, and
    // `sleep`/`all`/`race`/`map_bounded` (P2b) for `use std.task` — both ordinary registry
    // modules since higher-order-abi (the interim virtual-module mechanism is gone).
];

/// Whether `name` is reserved by the prelude in any form.
pub fn is_prelude_name(name: &str) -> bool {
    PRELUDE.iter().any(|(n, _)| *n == name)
}

/// Every prelude name of one form, in declaration order.
pub fn prelude_names(form: PreludeForm) -> impl Iterator<Item = &'static str> {
    PRELUDE
        .iter()
        .filter(move |(_, f)| *f == form)
        .map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The recorded form is what the lexer actually does with the name.**
    ///
    /// [`PreludeForm::Keyword`] is a claim about the token table one crate over, and this crate
    /// cannot make that claim structurally — it does not link the lexer, deliberately, and neither
    /// does `noeta-check`. So the claim is checked here instead, where a dev-dependency costs the
    /// build graph nothing: a prelude name filed as a `Value` that the lexer has since made a
    /// keyword would silently become a binding nothing can bind, and the reverse would drop a real
    /// binding out of the checker's always-resolvable set.
    #[test]
    fn the_keyword_form_is_the_lexers_own_answer() {
        for (name, form) in PRELUDE {
            let reserved = noeta_lexer::ReservedWord::from_spelling(name).is_some();
            let expected = if reserved {
                PreludeForm::Keyword
            } else {
                PreludeForm::Value
            };
            assert_eq!(
                *form, expected,
                "`{name}` is filed as {form:?} but the lexer says otherwise"
            );
        }
        // Not vacuous in either direction: the split is a real split.
        assert!(prelude_names(PreludeForm::Keyword).next().is_some());
        assert!(prelude_names(PreludeForm::Value).next().is_some());
    }
}
