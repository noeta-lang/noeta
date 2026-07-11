//! std's **dev-tier declarations** (tier-extensions port): the built-in four tiers and the
//! prelude attributes they own, declared through the extension ABI (`ExtTier`/`ExtAttribute`)
//! instead of hardcoded checker registration — the same surface a third-party tier package uses,
//! dogfooded. The checker registers these at prelude time (attribute construction gate,
//! reflection materialization, shadowable by a user declaration) and resolves tier names against
//! them; the **runners stay native** (`noeta test`'s parallel executor, `noeta bench`'s two-point
//! measurement, `noeta doc`'s extractor, `--tier debug`'s inline activation) — only the
//! declarations live here.
//!
//! Names are literals rather than `noeta_ast::reflect` constants because the ABI sits beneath the
//! syntax crates (neither `noeta-native` nor this crate sees `noeta-ast`); a checker test pins
//! the two spellings together so they cannot drift.

use noeta_native::registry::{
    AttrFieldDefault, AttrFieldType, ExtAttrField, ExtAttribute, ExtTier,
};

/// The built-in dev-tiers. `bench` carries its knob attribute; the rest are knob-less (`test`'s
/// metadata attributes attach per-fn, not through directive args, so they are not a `config`).
pub const TIERS: &[ExtTier] = &[
    ExtTier {
        name: "test",
        config: None,
    },
    ExtTier {
        name: "bench",
        config: Some("Bench"),
    },
    ExtTier {
        name: "doc",
        config: None,
    },
    ExtTier {
        name: "debug",
        config: None,
    },
];

/// The prelude attributes the built-in tiers own: the test runner's metadata quartet
/// (`Skip`/`Name`/`Group`/`Data`), `bench`'s knob (`Bench { iterations }`), and the doc tier's
/// stamped text carrier (`Doc { text }` — written by activation from an adjacency-attached
/// `@doc { … }` block, never by hand).
pub const ATTRIBUTES: &[ExtAttribute] = &[
    ExtAttribute {
        name: "Skip",
        fields: &[ExtAttrField {
            name: "reason",
            ty: AttrFieldType::Str,
            // Optional: both `#[Skip]` and `#[Skip("flaky")]` construct it.
            default: Some(AttrFieldDefault::Str("")),
        }],
    },
    ExtAttribute {
        name: "Name",
        fields: &[ExtAttrField {
            name: "value",
            ty: AttrFieldType::Str,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Group",
        fields: &[ExtAttrField {
            name: "value",
            ty: AttrFieldType::Str,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Data",
        fields: &[ExtAttrField {
            name: "rows",
            ty: AttrFieldType::Dyn,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Bench",
        fields: &[ExtAttrField {
            name: "iterations",
            ty: AttrFieldType::Int,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Doc",
        fields: &[ExtAttrField {
            name: "text",
            ty: AttrFieldType::Str,
            default: None,
        }],
    },
];
