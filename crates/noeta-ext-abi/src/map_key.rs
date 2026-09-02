//! The shared map-key representation: a map key is a
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

/// A map key: a string (the overwhelmingly common case, in the compact small-string
/// representation), an integer (`int` and the erased fixed-width family; an immediate, so the fastest
/// key kind: zero-allocation build, one-word hash), a key-capable extern value, or a key-capable
/// `@packed` struct.
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
    /// Boxed, deliberately: the payload is two pointers-plus wide, and an inline variant grew
    /// `MapKey` 24 → 40 bytes — a +67% footprint on EVERY string/int map entry, measured as
    /// ~+18% on the 100k-string-key xlang `assoc` bench (2026-07-17). Packed keys are the rare
    /// kind; they pay one cold Box per key build so the common kinds stay one cache-line-lean
    /// word each. Same lesson as `Op`; the `map_key_size` test is the ratchet.
    Packed(Box<PackedKey>),
}

/// The boxed payload of [`MapKey::Packed`]: the qualified type name and the field values in
/// declaration order. Identity is `(type_name, fields)` — display derives on demand through
/// [`packed_names`], so building a key formats nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedKey {
    pub type_name: compact_str::CompactString,
    pub fields: Box<[PackedKeyField]>,
}

/// The **field-name registry** for packed keys: `type name → field names`, registered
/// once per key-capable type by each backend as it loads/declares the type, and read only when a
/// packed key must *render* (display, JSON) — key construction, hashing, and comparison never
/// touch it. An unregistered type (defensive) renders positionally.
///
/// **Deliberately process-global** — like the shape interner, and *outside* the per-session
/// `Registry` story: it caches display-only
/// derived data keyed by type name, both backends register from the same declarations so renders
/// agree, and first-registration-wins is idempotent for a given program. The known limit is
/// cosmetic and accepted: two *sessions* in one process whose `@packed` types share a short name
/// (or a hot-swap that renames fields) get the first registration's field names in *rendered*
/// output only — never in key identity, hashing, or comparison, which carry the names nowhere.
/// Move it onto the session/VM when a real per-session need materializes.
pub mod packed_names {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use super::PackedKeyField;
    use crate::RenderHint;

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
        display_hinted(type_name, fields, None)
    }

    /// [`display`], with the slots the caller's [`RenderHint`] marks unsigned written as the `u64`
    /// they stand for. A packed key holds only the erased i64 words, so nothing in it distinguishes
    /// a `u64` field from an `i64` one — the signedness lives in the static type at the door, and
    /// this is where a rendered map's keys read it. `None` is [`display`] exactly.
    ///
    /// The hint is the packed struct's own [`RenderHint::Slots`], numbered by declared field, and a
    /// nested packed struct takes the nested hint at its slot — the display twin of
    /// [`crate::map_key_order`]'s packed arm, and the same numbering, so a map's rendered keys and
    /// its key order cannot disagree about which field is unsigned.
    pub fn display_hinted(
        type_name: &str,
        fields: &[PackedKeyField],
        hint: Option<&RenderHint>,
    ) -> String {
        let names = registry()
            .lock()
            .expect("packed-name registry poisoned")
            .get(type_name)
            .cloned();
        let mut out = String::with_capacity(16 + 12 * fields.len());
        write_struct(&mut out, type_name, fields, names.as_deref(), hint);
        out
    }

    fn write_struct(
        out: &mut String,
        type_name: &str,
        fields: &[PackedKeyField],
        names: Option<&[compact_str::CompactString]>,
        hint: Option<&RenderHint>,
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
            let slot = hint.and_then(|h| h.slot(i as u32));
            match f {
                PackedKeyField::Int(v) => {
                    // The one reinterpretation, from the one place that spells it.
                    if matches!(slot, Some(RenderHint::Unsigned)) {
                        out.push_str(&crate::unsigned_digits(*v));
                    } else {
                        let _ = write!(out, "{v}");
                    }
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
                    write_struct(out, nested_name, nested, nested_names.as_deref(), slot);
                }
            }
        }
        out.push('}');
    }
}

/// One field of a packed key, in declaration order: the erased integer word (`int` and
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
    /// A packed-struct key from its canonical parts: field values in declaration order.
    pub fn packed(type_name: &str, fields: Vec<PackedKeyField>) -> MapKey {
        MapKey::Packed(Box::new(PackedKey {
            type_name: compact_str::CompactString::from(type_name),
            fields: fields.into_boxed_slice(),
        }))
    }

    /// The key's display form inside a rendered map: a string key keeps its quoted `{k:?}` form;
    /// an extern or packed key renders its canonical display UNQUOTED (it is not a string).
    pub fn render(&self) -> String {
        match self {
            MapKey::Str(s) => format!("{s:?}"),
            MapKey::Int(i) => i.to_string(),
            MapKey::Extern(e) => e.display_string(),
            MapKey::Packed(p) => packed_names::display(&p.type_name, &p.fields),
        }
    }

    /// The key as a native/JSON object key: a string key verbatim, an extern or packed key its
    /// display form (JSON object keys are strings by definition).
    pub fn as_native_str(&self) -> String {
        match self {
            MapKey::Str(s) => s.as_str().to_owned(),
            MapKey::Int(i) => i.to_string(),
            MapKey::Extern(e) => e.display_string(),
            MapKey::Packed(p) => packed_names::display(&p.type_name, &p.fields),
        }
    }

    /// The string content, if this is a string key.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MapKey::Str(s) => Some(s.as_str()),
            MapKey::Int(_) | MapKey::Extern(_) | MapKey::Packed(_) => None,
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
            MapKey::Packed(p) => {
                p.type_name.as_str().hash(h);
                p.fields.hash(h);
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
            (MapKey::Packed(a), MapKey::Packed(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for MapKey {}

/// The total key order every order-observing accessor uses (display, `keys()`, iteration,
/// destructor walks): ints numerically; strings by content; extern keys by
/// `(type_identity, cmp_value)` — the qualified identity, so two key types sharing a short name
/// stay grouped by their own type; packed keys by `(type_name, fields)` — field-wise semantic
/// order in declaration order. Cross-kind `Int < Str < Extern < Packed` (arbitrary but fixed; a
/// typed map never mixes kinds, so this only steadies `dyn` paths).
impl Ord for MapKey {
    // Inlined by request: the deterministic destructor-order sort compares millions of keys on
    // big-map teardown, and the 4-variant match stopped inlining into the sort on its own —
    // measured as ~+13% instructions on the 300k-entry assoc bench.
    #[inline]
    fn cmp(&self, other: &MapKey) -> Ordering {
        // Cross-kind rank: Int(0) < Str(1) < Extern(2) < Packed(3).
        fn rank(k: &MapKey) -> u8 {
            match k {
                MapKey::Int(_) => 0,
                MapKey::Str(_) => 1,
                MapKey::Extern(_) => 2,
                MapKey::Packed(_) => 3,
            }
        }
        match (self, other) {
            (MapKey::Int(a), MapKey::Int(b)) => a.cmp(b),
            (MapKey::Str(a), MapKey::Str(b)) => a.cmp(b),
            (MapKey::Extern(a), MapKey::Extern(b)) => a
                .type_identity()
                .cmp(b.type_identity())
                .then_with(|| a.cmp_value(&**b).unwrap_or(Ordering::Equal)),
            (MapKey::Packed(a), MapKey::Packed(b)) => a
                .type_name
                .cmp(&b.type_name)
                .then_with(|| a.fields.cmp(&b.fields)),
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
/// equals the stored key's, so hashbrown's `get(key_str)` works with zero allocation, leaving the
/// small-string hot path untouched.
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

#[cfg(test)]
mod size_tests {
    use super::*;

    /// The footprint ratchet the boxed `Packed` variant exists for: every map/set entry carries
    /// a `MapKey` inline, so its size is a per-entry memory tax on the two overwhelmingly common
    /// key kinds (string, int). 24 bytes = `CompactString`'s own size (its niche absorbs the
    /// discriminant). Growing this is a measured regression on 100k-entry maps, not a style
    /// choice — box new wide variants like `Packed`.
    #[test]
    fn map_key_stays_at_compact_string_size() {
        assert_eq!(
            std::mem::size_of::<MapKey>(),
            std::mem::size_of::<compact_str::CompactString>(),
            "MapKey grew past CompactString — box the new variant's payload"
        );
    }
}
