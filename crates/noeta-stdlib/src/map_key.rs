//! The registry-coupled map-key helpers. The `MapKey`/`ExternKeyRef` types and their
//! ordering/equality/hashing contract live in the ABI crate ([`noeta_native::map_key`], re-exported
//! here); this module adds the two helpers that need the concrete `std` registration —
//! `extern_key_capable` (which reads the registered [`crate::registry::ExtType::key_capable`] flag)
//! and the canonical `map_key_error`.

pub use noeta_native::map_key::{ExternKeyRef, MapKey, PackedKeyField, packed_names};

/// Whether an extern value may key a map / member a set — its registered
/// [`crate::registry::ExtType::key_capable`] flag (extern-types X4). The runtime gate both
/// backends consult; string keys never reach it (they short-circuit earlier).
pub fn extern_key_capable(value: &dyn crate::ExternValue) -> bool {
    crate::registry::find_type(value.type_name()).is_some_and(|t| t.key_capable)
}

/// The canonical invalid-map-key error (→ `E0007`), shared by both backends and the static
/// checker's wording: raised for a non-string, non-key-capable key.
pub fn map_key_error(type_name: &str) -> crate::StdError {
    crate::StdError {
        kind: crate::ErrorKind::ArgType,
        message: format!(
            "map keys must be strings or key-capable types; `{type_name}` cannot key a map"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use equivalent::Equivalent;
    use std::hash::Hash;

    #[test]
    fn string_keys_hash_and_compare_like_bare_strs() {
        let key = MapKey::from("alpha");
        assert!("alpha".equivalent(&key));
        assert!(!"beta".equivalent(&key));
        // Content-only hash: the probe's hash equals the stored key's.
        fn fx(h: impl Hash) -> u64 {
            use std::hash::BuildHasher;
            std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
                .hash_one(h)
        }
        assert_eq!(fx("alpha"), fx(&key));
    }

    #[test]
    fn extern_keys_key_by_content_and_order_below_nothing_above_strings() {
        let a = MapKey::Extern(crate::ExternBox::new(crate::id::v4(1, 2)));
        let b = MapKey::Extern(crate::ExternBox::new(crate::id::v4(1, 2)));
        let c = MapKey::Extern(crate::ExternBox::new(crate::id::v4(9, 9)));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
        assert!(MapKey::from("zzz") < a, "Str orders below Extern, fixed");
        // The heterogeneous extern probe finds the stored key.
        let u = crate::id::v4(1, 2);
        let probe = ExternKeyRef(&u);
        assert!(probe.equivalent(&a));
        assert!(!probe.equivalent(&c));
        assert!(!probe.equivalent(&MapKey::from("alpha")));
    }

    #[test]
    fn render_quotes_strings_but_not_extern_keys() {
        assert_eq!(MapKey::from("k").render(), "\"k\"");
        let u = crate::id::v4(0, 0);
        assert_eq!(
            MapKey::Extern(crate::ExternBox::new(u)).render(),
            "00000000-0000-4000-8000-000000000000"
        );
    }

    #[test]
    fn key_capability_reads_the_registry() {
        // `Uuid` is registered key-capable; `Response` (also an extern type) is not.
        assert!(extern_key_capable(&crate::id::v4(1, 2)));
        assert!(
            map_key_error("Response")
                .message
                .contains("cannot key a map")
        );
    }
}
