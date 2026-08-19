//! **Declared conversions** — the `impl From<Source>` blocks a type carries, and the method-table
//! key each of them occupies.
//!
//! A conversion is named after the source it converts — `impl From<JsonError>` fills the
//! `from<JsonError>` slot — because its identity is the pair `(target, source)` and a method table
//! has one slot per name. This module is the single rule for that name, asked by everything that
//! builds or resolves a method table: the checker's signature registration, IR lowering, the
//! bytecode compiler's prototype reservation, and the reflection manifest. They agree because they
//! ask the same function about the same declaration, not because four walks were written to match.
//!
//! It lives in the AST crate rather than beside `BuiltinTrait` because the compiler is one of those
//! askers and depends on no type-system crate; the trait's own spellings ([`FROM_TRAIT`],
//! [`FROM_METHOD`]) are declared here and read *by* the built-in trait table, so there is still
//! exactly one place either word is written.

use std::collections::HashMap;

use noeta_span::Span;

use crate::{ImplBlock, shape};

/// The name a `From` implementation is written with (`impl From<Source> { … }`).
pub const FROM_TRAIT: &str = "From";

/// The method a `From` implementation provides (`fn from(value: Source): Target`).
pub const FROM_METHOD: &str = "from";

/// The method-table key the conversion **from `source`** occupies on its target.
pub fn from_method_key(source: &str) -> String {
    format!("{FROM_METHOD}<{source}>")
}

/// **The method-table key each of a type's `impl From<Source>` conversions occupies**, keyed by the
/// span of that block's `from` — the join between a type's `impls` and the flattened `methods` the
/// parser copies them into.
///
/// **A conversion is named after the source it converts**, always: `impl From<JsonError>` fills the
/// `from<JsonError>` slot whether the type declares one conversion or five. A conversion's identity
/// is the pair `(target, source)`, so its name says both; the plain `from` names a *set* and is
/// therefore not a name a conversion can hold.
///
/// Two things follow, and both are the point rather than a consequence to be tolerated.
///
/// **The plain `from` slot is left free** — deliberately, because it is contended. A backed enum
/// reserves `Plan.from("free")` for its backing-value conversion, and that is not the same operation
/// as an `impl From<Raw>` on the same enum. With conversions named after their sources the two
/// coexist: the built-in keeps `from`, the declared one is `from<Raw>`, and neither has to be
/// resolved in the other's favor.
///
/// **A by-name lookup carrying no source finds no conversion.** `invoke("T.from", …)`, a `dyn`
/// receiver, a `<T: Trait>` static call — none of them says *which* conversion, and the answer is a
/// miss naming the conversions the type does declare rather than a guess. Uniform is what makes that
/// honest: were the sole conversion left under `from`, adding an unrelated `impl From<B>` would
/// rename the *first* one out from under a caller that never mentioned `B`.
///
/// The source's spelling is the type reference **verbatim** ([`shape::type_source`]), which is the
/// linker-qualified identity by the time any caller runs — so two impls naming one source spell it
/// identically (and collide as the coherence conflict they are), and two impls naming different
/// sources cannot collide.
pub fn from_conversion_keys(impls: &[ImplBlock]) -> HashMap<Span, String> {
    let mut out = HashMap::new();
    collect_conversion_keys(
        impls
            .iter()
            .map(|b| (b.trait_name.as_str(), b.trait_args.as_slice(), &b.methods)),
        &mut out,
    );
    out
}

/// **The method-table key every conversion in the whole program occupies**, keyed by the span of
/// each `from` — [`from_conversion_keys`] asked of both spellings at once.
///
/// A conversion is written either **in the target's body** (`impl From<Cents> { … }`) or **beside
/// it** (`impl From<Cents> for Money { … }`), and the two are the same declaration: the backends'
/// hoist grafts the standalone form's methods onto the target before lowering, so by the time a
/// method table is built there is one flattened `from` per conversion and nothing left to say which
/// spelling it came from. Its key must therefore be decided from the whole program rather than from
/// one type's `impls` — otherwise the standalone form's `from` keeps the bare name, two of them
/// collide in the table, and `Money.from(cents)` silently dispatches to whichever conversion the
/// walk registered last.
///
/// Asked by everything that builds or resolves a method table — the checker's signature
/// registration, IR lowering, the bytecode compiler's prototype reservation, and the reflection
/// manifest — so the key is one answer to one question rather than four walks written to agree.
pub fn from_conversion_keys_program(stmts: &[crate::Stmt]) -> HashMap<Span, String> {
    use crate::Stmt;
    let mut out = HashMap::new();
    for stmt in stmts {
        match stmt {
            Stmt::Struct(d) => collect_conversion_keys(blocks(&d.impls), &mut out),
            Stmt::Class(d) => collect_conversion_keys(blocks(&d.impls), &mut out),
            Stmt::Enum(d) => collect_conversion_keys(blocks(&d.impls), &mut out),
            Stmt::Impl(d) => collect_conversion_keys(
                std::iter::once((d.trait_name.as_str(), d.trait_args.as_slice(), &d.methods)),
                &mut out,
            ),
            _ => {}
        }
    }
    out
}

/// The in-body impl blocks of one type as the shared `(trait, args, methods)` triples.
fn blocks(
    impls: &[ImplBlock],
) -> impl Iterator<Item = (&str, &[crate::TypeRef], &Vec<crate::FnDecl>)> {
    impls
        .iter()
        .map(|b| (b.trait_name.as_str(), b.trait_args.as_slice(), &b.methods))
}

/// The one rule, over whichever impl form the caller has: a `From` block carrying exactly one type
/// argument names its `from` after that source.
fn collect_conversion_keys<'a>(
    impls: impl Iterator<Item = (&'a str, &'a [crate::TypeRef], &'a Vec<crate::FnDecl>)>,
    out: &mut HashMap<Span, String>,
) {
    for (trait_name, args, methods) in impls {
        if trait_name != FROM_TRAIT || args.len() != 1 {
            continue;
        }
        let key = from_method_key(&shape::type_source(&args[0]));
        for m in methods.iter().filter(|m| m.name.as_str() == FROM_METHOD) {
            out.insert(m.name_span, key.clone());
        }
    }
}

/// The source a conversion key names, or `None` if the name is not one — the inverse of
/// [`from_method_key`], and the only reader of that format.
///
/// A method table holds names, not declarations, so this is how a **runtime** lookup recovers what a
/// type converts: the keys are the record.
pub fn from_key_source(name: &str) -> Option<&str> {
    name.strip_prefix(FROM_METHOD)?
        .strip_prefix('<')?
        .strip_suffix('>')
}

/// **The message a by-name lookup of the bare `from` gets on a type that declares conversions**, or
/// `None` when the type declares none (leaving the caller's own "no such method" wording).
///
/// A conversion is named after its source, so `from` names no single one and a lookup for it misses.
/// Saying only that it missed contradicts the source in front of the reader — the `impl From<…>`
/// blocks are right there — so this names them and the spelling that reaches each.
///
/// Sourced from the type's **method names**, which is what a runtime dispatch site has, and sorted,
/// because a method table is a hash map and an unsorted message would differ between two runs of one
/// program and between the two backends. One function so the two report identically: it is called
/// from the by-name miss in each, and the differential compares their output byte for byte.
pub fn missing_from_message<'a>(
    type_name: &str,
    looked_up: &str,
    method_names: impl Iterator<Item = &'a str>,
) -> Option<String> {
    if looked_up != FROM_METHOD {
        return None;
    }
    let mut sources: Vec<&str> = method_names.filter_map(from_key_source).collect();
    if sources.is_empty() {
        return None;
    }
    sources.sort_unstable();
    sources.dedup();
    // "a and b" for what the type declares, "a or b" for the spellings that reach them — a list of
    // alternatives reads as a choice.
    let list = |items: &[String], conjunction: &str| match items {
        [only] => only.clone(),
        [rest @ .., last] => format!("{} {conjunction} {last}", rest.join(", ")),
        [] => String::new(),
    };
    let names: Vec<String> = sources.iter().map(|s| format!("`{s}`")).collect();
    let calls: Vec<String> = sources
        .iter()
        .map(|s| format!("`{}`", from_method_key(s)))
        .collect();
    let declares = if sources.len() == 1 {
        "declares a conversion from"
    } else {
        "declares conversions from"
    };
    Some(format!(
        "type `{type_name}` {declares} {}; call {}",
        list(&names, "and"),
        list(&calls, "or")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversion_key_round_trips_through_its_source() {
        assert_eq!(from_method_key("JsonError"), "from<JsonError>");
        assert_eq!(from_key_source("from<JsonError>"), Some("JsonError"));
        assert_eq!(
            from_key_source(&from_method_key("std.json.JsonError")),
            Some("std.json.JsonError")
        );
        // Every other method name — including ones that merely start with the word.
        assert_eq!(from_key_source("from"), None);
        assert_eq!(from_key_source("fromage"), None);
        assert_eq!(from_key_source("to_string"), None);
    }

    /// The wording of the by-name miss, at each arity — the two-and-more forms are what the
    /// conformance corpus pins end to end; the three-source form has only this.
    #[test]
    fn the_missing_from_message_names_every_conversion_and_how_to_reach_it() {
        let message =
            |names: &[&str]| missing_from_message("W", FROM_METHOD, names.iter().copied());

        assert_eq!(
            message(&["from<A>", "area"]).as_deref(),
            Some("type `W` declares a conversion from `A`; call `from<A>`")
        );
        assert_eq!(
            message(&["from<A>", "from<B>"]).as_deref(),
            Some("type `W` declares conversions from `A` and `B`; call `from<A>` or `from<B>`")
        );
        assert_eq!(
            message(&["from<C>", "from<A>", "from<B>"]).as_deref(),
            Some(
                "type `W` declares conversions from `A`, `B` and `C`; call `from<A>`, `from<B>` or \
                 `from<C>`"
            ),
            "sorted, because a method table is unordered and the two backends must agree"
        );

        // Nothing to say: the type declares no conversion, or the lookup was for another name. The
        // caller keeps its own wording in both cases.
        assert_eq!(message(&["area", "new"]), None);
        assert_eq!(
            missing_from_message("W", "area", ["from<A>"].into_iter()),
            None
        );
    }
}
