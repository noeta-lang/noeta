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

/// A map key: a string (the overwhelmingly common case, in the P-SSO compact representation), an
/// integer (P-PKEY S4 — `int` and the erased fixed-width family; an immediate, so the fastest
/// key kind: zero-allocation build, one-word hash), a key-capable extern value, or a key-capable
/// `@packed` struct (P-PKEY).
#[derive(Debug, Clone)]
pub enum MapKey {
    Str(compact_str::CompactString),
    Int(i64),
    Extern(ExternBox),
    /// A key-capable `@packed` struct's snapshot: the qualified type name and the field values
    /// in declaration order ([`PackedKeyField`] — plain data, so the key holds no backend heap
    /// reference and reconstructs a value for `keys()`). Identity is `(type_name, fields)`;
    /// display derives on demand through [`packed_names`] (field names register once per type at
    /// load/declare), so building a key — the hot map/set path — formats nothing.
    Packed {
        type_name: compact_str::CompactString,
        fields: Box<[PackedKeyField]>,
    },
}

/// The **field-name registry** for packed keys (P-PKEY): `type name → field names`, registered
/// once per key-capable type by each backend as it loads/declares the type, and read only when a
/// packed key must *render* (display, JSON) — key construction, hashing, and comparison never
/// touch it. An unregistered type (defensive) renders positionally.
///
/// **Deliberately process-global** — like the shape interner, and *outside* the per-session
/// `Registry` story (audit-2 Finding 12, disposition recorded here): it caches display-only
/// derived data keyed by type name, both backends register from the same declarations so renders
/// agree, and first-registration-wins is idempotent for a given program. The known limit is
/// cosmetic and accepted: two *sessions* in one process whose `@packed` types share a short name
/// (or a hot-swap that renames fields) get the first registration's field names in *rendered*
/// output only — never in key identity, hashing, or comparison, which carry the names nowhere.
/// Move it onto the session/VM when a real per-session need materializes (tracked in
/// `plans/deferred.md`, "Instance-registry residue").
pub mod packed_names {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use super::PackedKeyField;

    type Names = std::sync::Arc<[compact_str::CompactString]>;

    fn registry() -> &'static Mutex<HashMap<compact_str::CompactString, Names>> {
        static REGISTRY: OnceLock<Mutex<HashMap<compact_str::CompactString, Names>>> =
            OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Register `type_name`'s field names (declaration order). Idempotent; called once per
    /// key-capable type at load (VM) / declare (eval).
    pub fn register(type_name: &str, fields: impl Iterator<Item = impl AsRef<str>>) {
        let mut map = registry().lock().expect("packed-name registry poisoned");
        map.entry(compact_str::CompactString::from(type_name))
            .or_insert_with(|| {
                fields
                    .map(|f| compact_str::CompactString::from(f.as_ref()))
                    .collect()
            });
    }

    /// The display form of a packed key — the struct's own display (`Cell {x: 3, y: 4}`), derived
    /// from the registered names; positional (`Cell {3, 4}`) only for an unregistered type.
    pub fn display(type_name: &str, fields: &[PackedKeyField]) -> String {
        let names = registry()
            .lock()
            .expect("packed-name registry poisoned")
            .get(type_name)
            .cloned();
        let mut out = String::with_capacity(16 + 12 * fields.len());
        write_struct(&mut out, type_name, fields, names.as_deref());
        out
    }

    fn write_struct(
        out: &mut String,
        type_name: &str,
        fields: &[PackedKeyField],
        names: Option<&[compact_str::CompactString]>,
    ) {
        use std::fmt::Write as _;
        out.push_str(type_name);
        out.push_str(" {");
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if let Some(name) = names.and_then(|n| n.get(i)) {
                let _ = write!(out, "{name}: ");
            }
            match f {
                PackedKeyField::Int(v) => {
                    let _ = write!(out, "{v}");
                }
                PackedKeyField::Bool(b) => {
                    let _ = write!(out, "{b}");
                }
                PackedKeyField::Struct(nested_name, nested) => {
                    let nested_names = registry()
                        .lock()
                        .expect("packed-name registry poisoned")
                        .get(nested_name.as_str())
                        .cloned();
                    write_struct(out, nested_name, nested, nested_names.as_deref());
                }
            }
        }
        out.push('}');
    }
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
    /// A packed-struct key (P-PKEY) from its canonical parts: field values in declaration order.
    pub fn packed(type_name: &str, fields: Vec<PackedKeyField>) -> MapKey {
        MapKey::Packed {
            type_name: compact_str::CompactString::from(type_name),
            fields: fields.into_boxed_slice(),
        }
    }

    /// The key's display form inside a rendered map: a string key keeps its quoted `{k:?}` form;
    /// an extern or packed key renders its canonical display UNQUOTED (it is not a string).
    pub fn render(&self) -> String {
        match self {
            MapKey::Str(s) => format!("{s:?}"),
            MapKey::Int(i) => i.to_string(),
            MapKey::Extern(e) => e.display_string(),
            MapKey::Packed { type_name, fields } => packed_names::display(type_name, fields),
        }
    }

    /// The key as a native/JSON object key: a string key verbatim, an extern or packed key its
    /// display form (JSON object keys are strings by definition).
    pub fn as_native_str(&self) -> String {
        match self {
            MapKey::Str(s) => s.as_str().to_owned(),
            MapKey::Int(i) => i.to_string(),
            MapKey::Extern(e) => e.display_string(),
            MapKey::Packed { type_name, fields } => packed_names::display(type_name, fields),
        }
    }

    /// The string content, if this is a string key.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MapKey::Str(s) => Some(s.as_str()),
            MapKey::Int(_) | MapKey::Extern(_) | MapKey::Packed { .. } => None,
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
            MapKey::Int(i) => i.hash(h),
            MapKey::Extern(e) => h.write_u64(e.hash_value()),
            MapKey::Packed { type_name, fields } => {
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
            (MapKey::Int(a), MapKey::Int(b)) => a == b,
            (MapKey::Extern(a), MapKey::Extern(b)) => a.eq_value(&**b),
            (
                MapKey::Packed {
                    type_name: an,
                    fields: af,
                },
                MapKey::Packed {
                    type_name: bn,
                    fields: bf,
                },
            ) => an == bn && af == bf,
            _ => false,
        }
    }
}

impl Eq for MapKey {}

/// The total key order every order-observing accessor uses (display, `keys()`, iteration,
/// destructor walks): ints numerically; strings by content; extern keys by
/// `(type_name, cmp_value)`; packed keys by `(type_name, fields)` — field-wise semantic order in
/// declaration order. Cross-kind `Int < Str < Extern < Packed` (arbitrary but fixed; a typed map
/// never mixes kinds, so this only steadies `dyn` paths).
impl Ord for MapKey {
    fn cmp(&self, other: &MapKey) -> Ordering {
        // Cross-kind rank: Int(0) < Str(1) < Extern(2) < Packed(3).
        fn rank(k: &MapKey) -> u8 {
            match k {
                MapKey::Int(_) => 0,
                MapKey::Str(_) => 1,
                MapKey::Extern(_) => 2,
                MapKey::Packed { .. } => 3,
            }
        }
        match (self, other) {
            (MapKey::Int(a), MapKey::Int(b)) => a.cmp(b),
            (MapKey::Str(a), MapKey::Str(b)) => a.cmp(b),
            (MapKey::Extern(a), MapKey::Extern(b)) => a
                .type_name()
                .cmp(b.type_name())
                .then_with(|| a.cmp_value(&**b).unwrap_or(Ordering::Equal)),
            (
                MapKey::Packed {
                    type_name: an,
                    fields: af,
                },
                MapKey::Packed {
                    type_name: bn,
                    fields: bf,
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

    fn mk(ty: &str, ints: &[i64], _display: &str) -> MapKey {
        MapKey::packed(ty, ints.iter().map(|&v| PackedKeyField::Int(v)).collect())
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
        // Display derives through the name registry; unregistered types render positionally.
        assert_eq!(a.render(), "Cell {3, 4}");
        packed_names::register("Cell", ["x", "y"].iter());
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
        );
        assert_ne!(nested_a, nested_b);
        assert!(nested_a < nested_b, "false < true");
    }
}
