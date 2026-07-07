//! The shared map-key representation (extern-types X4): a map key is a `string` or a
//! `key_capable` extern-type value. Defined ONCE here so the two backends' ordering, equality,
//! hashing, and display of keys agree **by construction** — the VM's `HashMap<MapKey, _>` sorts
//! by [`Ord`] for every order-observing accessor, and the tree-walker's `BTreeMap<MapKey, _>`
//! iterates in the same [`Ord`] — one contract, one place.
//!
//! An extern key owns its value inline ([`crate::ExternBox`]), NOT a backend heap reference: a
//! key is a snapshot, which is sound because `key_capable` forbids mutating methods — so map
//! keys stay out of the GC entirely (no retain/release, no destructor-order concern), exactly
//! like string keys.
//!
//! The registry-coupled helpers (`extern_key_capable`, which reads the [`crate::registry`]
//! key-capability flag, and `map_key_error`) live in `noeta-stdlib` next to the concrete
//! registration — they cannot sit here without pulling the std registration back into the ABI.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::ExternBox;

/// A map key: a string (the overwhelmingly common case, in the P-SSO compact representation) or
/// a key-capable extern value.
#[derive(Debug, Clone)]
pub enum MapKey {
    Str(compact_str::CompactString),
    Extern(ExternBox),
}

impl MapKey {
    /// The key's display form inside a rendered map: a string key keeps its quoted `{k:?}` form;
    /// an extern key renders its canonical display UNQUOTED (it is not a string).
    pub fn render(&self) -> String {
        match self {
            MapKey::Str(s) => format!("{s:?}"),
            MapKey::Extern(e) => e.display_string(),
        }
    }

    /// The key as a native/JSON object key: a string key verbatim, an extern key its display
    /// form (JSON object keys are strings by definition).
    pub fn as_native_str(&self) -> String {
        match self {
            MapKey::Str(s) => s.as_str().to_owned(),
            MapKey::Extern(e) => e.display_string(),
        }
    }

    /// The string content, if this is a string key.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MapKey::Str(s) => Some(s.as_str()),
            MapKey::Extern(_) => None,
        }
    }
}

/// Content-only hashing: a string key hashes exactly as the bare `str` does (so the VM's
/// heterogeneous `&str` probe finds it with no key allocation); an extern key hashes its stable
/// [`crate::ExternValue::hash_value`]. Cross-kind collisions merely fall through to `eq`.
impl Hash for MapKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            MapKey::Str(s) => s.as_str().hash(h),
            MapKey::Extern(e) => h.write_u64(e.hash_value()),
        }
    }
}

impl PartialEq for MapKey {
    fn eq(&self, other: &MapKey) -> bool {
        match (self, other) {
            (MapKey::Str(a), MapKey::Str(b)) => a == b,
            (MapKey::Extern(a), MapKey::Extern(b)) => a.eq_value(&**b),
            _ => false,
        }
    }
}

impl Eq for MapKey {}

/// The total key order every order-observing accessor uses (display, `keys()`, iteration,
/// destructor walks): strings by content, extern keys by `(type_name, cmp_value)` — `cmp_value`
/// is total within a key-capable kind by contract, and the type name breaks ties across
/// different extern kinds — and cross-kind `Str < Extern` (arbitrary but fixed; a typed map
/// never mixes kinds, so this only steadies `dyn` paths).
impl Ord for MapKey {
    fn cmp(&self, other: &MapKey) -> Ordering {
        match (self, other) {
            (MapKey::Str(a), MapKey::Str(b)) => a.cmp(b),
            (MapKey::Extern(a), MapKey::Extern(b)) => a
                .type_name()
                .cmp(b.type_name())
                .then_with(|| a.cmp_value(&**b).unwrap_or(Ordering::Equal)),
            (MapKey::Str(_), MapKey::Extern(_)) => Ordering::Less,
            (MapKey::Extern(_), MapKey::Str(_)) => Ordering::Greater,
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
