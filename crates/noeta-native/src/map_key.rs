//! The shared map-key representation (extern-types X4, packed keys P-PKEY): a map key is a
//! `string`, a `key_capable` extern-type value, or a key-capable `@packed` struct. Defined ONCE
//! here so the two backends' ordering, equality, hashing, and display of keys agree **by
//! construction** — the VM's `HashMap<MapKey, _>` sorts by [`Ord`] for every order-observing
//! accessor, and the tree-walker's `BTreeMap<MapKey, _>` iterates in the same [`Ord`] — one
//! contract, one place.
//!
//! An extern key owns its value inline ([`crate::ExternBox`]), NOT a backend heap reference: a
//! key is a snapshot, which is sound because `key_capable` forbids mutating methods — so map
//! keys stay out of the GC entirely (no retain/release, no destructor-order concern), exactly
//! like string keys. A packed key is likewise a snapshot: its identity is `(type name, field
//! values)` — `@packed` means value semantics, so the snapshot can never drift from any
//! aliased original.
//!
//! The registry-coupled helpers (`extern_key_capable`, which reads the [`crate::registry`]
//! key-capability flag, and `map_key_error`) live in `noeta-stdlib` next to the concrete
//! registration — they cannot sit here without pulling the std registration back into the ABI.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::ExternBox;

/// A map key: a string (the overwhelmingly common case, in the P-SSO compact representation), a
/// key-capable extern value, or a key-capable `@packed` struct (P-PKEY).
#[derive(Debug, Clone)]
pub enum MapKey {
    Str(compact_str::CompactString),
    Extern(ExternBox),
    /// A key-capable `@packed` struct's snapshot: the qualified type name, the field values in
    /// declaration order ([`PackedKeyField`] — plain data, so the key holds no backend heap
    /// reference and reconstructs a value for `keys()`), and the value's display form (carried
    /// for [`MapKey::render`]/JSON — deliberately **excluded** from identity, which is
    /// `(type_name, fields)` alone).
    Packed {
        type_name: compact_str::CompactString,
        fields: Box<[PackedKeyField]>,
        display: compact_str::CompactString,
    },
}

/// One field of a packed key (P-PKEY), in declaration order: the erased integer word (`int` and
/// every fixed-width `{i,u}N`), a bool, or a nested key-capable packed struct. Plain data —
/// backend-neutral, thread-safe, GC-free. The derived [`Ord`] gives field-wise semantic order
/// (a well-typed key never compares across variants at one position: field kinds are fixed per
/// struct type, and the type name is compared first).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PackedKeyField {
    Int(i64),
    Bool(bool),
    Struct(compact_str::CompactString, Box<[PackedKeyField]>),
}

impl MapKey {
    /// A packed-struct key (P-PKEY) from its canonical parts: field values in declaration order
    /// plus the value's display form (render/JSON only, not identity).
    pub fn packed(type_name: &str, fields: Vec<PackedKeyField>, display: String) -> MapKey {
        MapKey::Packed {
            type_name: compact_str::CompactString::from(type_name),
            fields: fields.into_boxed_slice(),
            display: compact_str::CompactString::from(display),
        }
    }

    /// The key's display form inside a rendered map: a string key keeps its quoted `{k:?}` form;
    /// an extern or packed key renders its canonical display UNQUOTED (it is not a string).
    pub fn render(&self) -> String {
        match self {
            MapKey::Str(s) => format!("{s:?}"),
            MapKey::Extern(e) => e.display_string(),
            MapKey::Packed { display, .. } => display.as_str().to_owned(),
        }
    }

    /// The key as a native/JSON object key: a string key verbatim, an extern or packed key its
    /// display form (JSON object keys are strings by definition).
    pub fn as_native_str(&self) -> String {
        match self {
            MapKey::Str(s) => s.as_str().to_owned(),
            MapKey::Extern(e) => e.display_string(),
            MapKey::Packed { display, .. } => display.as_str().to_owned(),
        }
    }

    /// The string content, if this is a string key.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MapKey::Str(s) => Some(s.as_str()),
            MapKey::Extern(_) | MapKey::Packed { .. } => None,
        }
    }
}

/// Content-only hashing: a string key hashes exactly as the bare `str` does (so the VM's
/// heterogeneous `&str` probe finds it with no key allocation); an extern key hashes its stable
/// [`crate::ExternValue::hash_value`]; a packed key hashes its identity — `(type_name, fields)`,
/// never the display. Cross-kind collisions merely fall through to `eq`.
impl Hash for MapKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            MapKey::Str(s) => s.as_str().hash(h),
            MapKey::Extern(e) => h.write_u64(e.hash_value()),
            MapKey::Packed {
                type_name, fields, ..
            } => {
                type_name.as_str().hash(h);
                fields.hash(h);
            }
        }
    }
}

impl PartialEq for MapKey {
    fn eq(&self, other: &MapKey) -> bool {
        match (self, other) {
            (MapKey::Str(a), MapKey::Str(b)) => a == b,
            (MapKey::Extern(a), MapKey::Extern(b)) => a.eq_value(&**b),
            (
                MapKey::Packed {
                    type_name: an,
                    fields: af,
                    ..
                },
                MapKey::Packed {
                    type_name: bn,
                    fields: bf,
                    ..
                },
            ) => an == bn && af == bf,
            _ => false,
        }
    }
}

impl Eq for MapKey {}

/// The total key order every order-observing accessor uses (display, `keys()`, iteration,
/// destructor walks): strings by content; extern keys by `(type_name, cmp_value)`; packed keys
/// by `(type_name, fields)` — field-wise semantic order in declaration order. Cross-kind
/// `Str < Extern < Packed` (arbitrary but fixed; a typed map never mixes kinds, so this only
/// steadies `dyn` paths).
impl Ord for MapKey {
    fn cmp(&self, other: &MapKey) -> Ordering {
        // Cross-kind rank: Str(0) < Extern(1) < Packed(2).
        fn rank(k: &MapKey) -> u8 {
            match k {
                MapKey::Str(_) => 0,
                MapKey::Extern(_) => 1,
                MapKey::Packed { .. } => 2,
            }
        }
        match (self, other) {
            (MapKey::Str(a), MapKey::Str(b)) => a.cmp(b),
            (MapKey::Extern(a), MapKey::Extern(b)) => a
                .type_name()
                .cmp(b.type_name())
                .then_with(|| a.cmp_value(&**b).unwrap_or(Ordering::Equal)),
            (
                MapKey::Packed {
                    type_name: an,
                    fields: af,
                    ..
                },
                MapKey::Packed {
                    type_name: bn,
                    fields: bf,
                    ..
                },
            ) => an.cmp(bn).then_with(|| af.cmp(bf)),
            (a, b) => rank(a).cmp(&rank(b)),
        }
    }
}

impl PartialOrd for MapKey {
    fn partial_cmp(&self, other: &MapKey) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<&str> for MapKey {
    fn from(s: &str) -> MapKey {
        MapKey::Str(compact_str::CompactString::from(s))
    }
}

impl From<String> for MapKey {
    fn from(s: String) -> MapKey {
        MapKey::Str(compact_str::CompactString::from(s))
    }
}

/// Heterogeneous `&str` lookup into a `HashMap<MapKey, _>`/`BTreeMap` probe — matches a string
/// key by content, never an extern key. With the content-only [`Hash`] above, the probe's hash
/// equals the stored key's, so hashbrown's `get(key_str)` works with zero allocation (the
/// P-SSO hot path unchanged).
impl equivalent::Equivalent<MapKey> for str {
    fn equivalent(&self, key: &MapKey) -> bool {
        matches!(key, MapKey::Str(s) if s.as_str() == self)
    }
}

/// Heterogeneous extern-value lookup (`m[uuid]`): hash and compare through the extern contract
/// without boxing a fresh key.
#[derive(Debug, Clone, Copy)]
pub struct ExternKeyRef<'a>(pub &'a dyn crate::ExternValue);

impl Hash for ExternKeyRef<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        h.write_u64(self.0.hash_value());
    }
}

impl equivalent::Equivalent<MapKey> for ExternKeyRef<'_> {
    fn equivalent(&self, key: &MapKey) -> bool {
        matches!(key, MapKey::Extern(e) if self.0.eq_value(&**e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(ty: &str, ints: &[i64], display: &str) -> MapKey {
        MapKey::packed(
            ty,
            ints.iter().map(|&v| PackedKeyField::Int(v)).collect(),
            display.to_string(),
        )
    }

    /// Packed-key identity is `(type_name, fields)` — display is carried but excluded; distinct
    /// types with identical fields stay distinct; order is field-wise semantic (negatives before
    /// positives); the cross-kind order is fixed.
    #[test]
    fn packed_key_identity_and_order() {
        let a = mk("Cell", &[3, 4], "Cell {x: 3, y: 4}");
        let same = mk("Cell", &[3, 4], "whatever — display is not identity");
        let other_ty = mk("Pos", &[3, 4], "Pos {x: 3, y: 4}");
        let smaller = mk("Cell", &[3, -1], "Cell {x: 3, y: -1}");
        assert_eq!(a, same);
        assert_ne!(a, other_ty);
        assert!(smaller < a, "field-wise semantic order: -1 < 4");
        assert!(
            mk("Cell", &[-9, 0], "n") < mk("Cell", &[2, 0], "p"),
            "sign order"
        );
        assert!(MapKey::from("zzz") < a, "Str < Packed, fixed");
        assert_eq!(a.render(), "Cell {x: 3, y: 4}");
        // Nested structs and bools take part in identity and order.
        let nested_a = MapKey::packed(
            "Outer",
            vec![
                PackedKeyField::Struct(
                    "Cell".into(),
                    vec![PackedKeyField::Int(1)].into_boxed_slice(),
                ),
                PackedKeyField::Bool(false),
            ],
            "Outer {…}".to_string(),
        );
        let nested_b = MapKey::packed(
            "Outer",
            vec![
                PackedKeyField::Struct(
                    "Cell".into(),
                    vec![PackedKeyField::Int(1)].into_boxed_slice(),
                ),
                PackedKeyField::Bool(true),
            ],
            "Outer {…}".to_string(),
        );
        assert_ne!(nested_a, nested_b);
        assert!(nested_a < nested_b, "false < true");
    }
}
