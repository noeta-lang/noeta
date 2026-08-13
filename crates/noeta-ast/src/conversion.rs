//! **Declared conversions** — the `impl From<Source>` blocks a type carries, and the method-table
//! key each of them occupies.
//!
//! A type may declare one conversion per distinct source, and a method table has one slot per name.
//! Those two facts meet here: this module is the single rule for naming a conversion's body, asked
//! by everything that builds or resolves a method table — the checker's signature registration, IR
//! lowering, and the bytecode compiler's prototype reservation. They agree because they ask the same
//! function about the same declaration, not because three walks were written to match.
//!
//! It lives in the AST crate rather than beside [`BuiltinTrait`](noeta_types::BuiltinTrait) because
//! the compiler is one of those askers and depends on no type-system crate; the trait's own
//! spellings ([`FROM_TRAIT`], [`FROM_METHOD`]) are declared here and read *by* the built-in trait
//! table, so there is still exactly one place either word is written.

use std::collections::HashMap;

use noeta_span::Span;

use crate::{ImplBlock, shape};

/// The name a `From` implementation is written with (`impl From<Source> { … }`).
pub const FROM_TRAIT: &str = "From";

/// The method a `From` implementation provides (`fn from(value: Source): Target`).
pub const FROM_METHOD: &str = "from";

/// The method-table key the conversion **from `source`** occupies on its target, when the target
/// declares more than one.
pub fn from_method_key(source: &str) -> String {
    format!("{FROM_METHOD}<{source}>")
}

/// **The method-table key each of a type's `impl From<Source>` conversions occupies**, keyed by the
/// span of that block's `from` — the join between a type's `impls` and the flattened `methods` the
/// parser copies them into. Empty when the type declares fewer than two conversions: those keep the
/// plain `from`, and there is nothing to rename.
///
/// A type declaring **several** conversions names each after the source it converts
/// (`from<JsonError>`); a type declaring a **single** conversion leaves it under the plain `from`
/// every other declaration form uses.
///
/// That is one rule, not two. A conversion resolves when the source in hand names exactly one of
/// them: at a `?` the source is the propagated `Err` type, at `Target.from(x)` it is `x`'s type, and
/// at a **by-name** lookup carrying no source at all — `invoke("T.from", …)`, a `dyn` receiver, a
/// `<T: Trait>` static call — every declared conversion is a candidate, so the lookup resolves
/// exactly when the type declares one. Leaving the sole conversion under `from` is what keeps those
/// name-keyed paths answering; a type with several has no single `from`, and they correctly find
/// none.
///
/// The source's spelling is the type reference **verbatim** ([`shape::type_source`]), which is the
/// linker-qualified identity by the time any caller runs — so two impls naming one source spell it
/// identically (and collide as the coherence conflict they are), and two impls naming different
/// sources cannot collide.
pub fn from_conversion_keys(impls: &[ImplBlock]) -> HashMap<Span, String> {
    let blocks: Vec<&ImplBlock> = impls
        .iter()
        .filter(|b| b.trait_name.as_str() == FROM_TRAIT && b.trait_args.len() == 1)
        .collect();
    if blocks.len() < 2 {
        return HashMap::new();
    }
    blocks
        .into_iter()
        .flat_map(|b| {
            let key = from_method_key(&shape::type_source(&b.trait_args[0]));
            b.methods
                .iter()
                .filter(|m| m.name.as_str() == FROM_METHOD)
                .map(move |m| (m.name_span, key.clone()))
        })
        .collect()
}
