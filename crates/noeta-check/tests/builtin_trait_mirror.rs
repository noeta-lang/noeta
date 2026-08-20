//! The **built-in trait vocabulary** exists twice, and this pins the copy to the original.
//!
//! `noeta_types::BUILTIN_TRAITS` is the authority: trait identity is a `BuiltinTrait` variant, and
//! `BuiltinTrait::from_name` is the one boundary a name crosses to become one. `noeta-ext-abi`
//! carries a `&[&str]` mirror of the same list, because it sits *below* the type system
//! (`noeta-ast` depends on it, `noeta-types` depends on `noeta-ast`) and so cannot name the enum —
//! yet it is where a native declaration's `traits` list and a `SigType::BoundedVar` bound are
//! resolved, at assembly, before anything can silently drop them.
//!
//! A mirror nothing compares is exactly the defect the mirror was added to prevent, one level up: a
//! trait added to the enum and not to the ABI list would make every native declaration of it refuse
//! to assemble, and a name removed from the enum but left in the list would sail through assembly
//! and then be dropped by `from_name` — the silent absence, back again. `noeta-check` is the lowest
//! crate that can see both sides *and* the one that performs the resolution, so the comparison lives
//! here.

#[test]
fn builtin_trait_names_mirror_the_type_system() {
    let authority: Vec<&str> = noeta_types::BUILTIN_TRAITS
        .iter()
        .map(|t| t.name())
        .collect();
    assert_eq!(
        authority,
        noeta_ext_abi::registry::BUILTIN_TRAIT_NAMES.to_vec(),
        "`noeta_ext_abi::registry::BUILTIN_TRAIT_NAMES` must list exactly \
         `noeta_types::BUILTIN_TRAITS`, in order — the registry resolves a native declaration's \
         `traits` entry against it at assembly, and the checker resolves the same string with \
         `BuiltinTrait::from_name` at the lookup"
    );
}

#[test]
fn every_mirrored_name_parses_as_a_builtin_trait() {
    // The other direction, stated as the property that actually matters: every name assembly
    // accepts must be one `from_name` answers, or a declaration passes validation and is dropped.
    for name in noeta_ext_abi::registry::BUILTIN_TRAIT_NAMES {
        assert!(
            noeta_types::BuiltinTrait::from_name(name).is_some(),
            "the ABI accepts `{name}` as a built-in trait, but `BuiltinTrait::from_name` does not \
             resolve it — a declaration naming it would assemble and then hold no trait"
        );
    }
}
