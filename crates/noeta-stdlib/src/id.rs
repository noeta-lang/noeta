//! The first-class `Uuid` extern type and its constructors (id-entropy U2 → extern-types X2).
//!
//! Construction is pure functions from raw bits/time to a [`Uuid`] — the host supplies the
//! inputs ([`crate::host::Entropy`] for random bits, [`crate::host::Clock::clock_unix_ms`] for
//! the v7 timestamp), so the same code produces deterministic UUIDs on the sandbox and real ones
//! on the real host, and the differential holds by shared dispatch like every other registry
//! module. The byte assembly itself is the `uuid` crate's RFC 9562 [`uuid::Builder`] — compiled
//! WITHOUT the self-generating `v4`/`v7` features, so no entropy or time source exists outside
//! the Host seam.
//!
//! [`Uuid`] is a newtype around [`uuid::Uuid`], registered through the extern-type seam
//! ([`crate::ExternValue`] below): one type both backends host, ordered by its bytes (a v7
//! therefore sorts by time), key-capable, displayed in the canonical lowercase hyphenated form
//! `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (8-4-4-4-12). The newtype is required because the
//! extern-value contract (`ExternValue`) now lives in the `noeta-native` ABI crate while the
//! `uuid` crate is foreign to it — the orphan rule forbids `impl ExternValue for uuid::Uuid`, so
//! we wrap it, exactly as a third-party extension would wrap any foreign type it wants to expose.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::ops::Deref;

use crate::extern_value::ExternValue;

/// The registered extern-type name (`ExtType::name`, `type_of`, `x is Uuid`).
pub const TYPE_NAME: &str = "Uuid";

/// `Uuid`'s qualified runtime identity (`{namespace}.{name}` of the `ExtType` registration) —
/// what [`crate::ExternValue::type_identity`] returns; pre-joined, never formatted at dispatch.
pub const TYPE_IDENTITY: &str = "std.id.Uuid";

/// The first-class `Uuid` value — a newtype around [`uuid::Uuid`] carrying the [`ExternValue`]
/// contract. [`Deref`]s to the inner `uuid::Uuid` so its accessors (`to_string`, `as_bytes`,
/// `get_version_num`, `get_timestamp`) are reached directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uuid(pub uuid::Uuid);

impl Deref for Uuid {
    type Target = uuid::Uuid;
    fn deref(&self) -> &uuid::Uuid {
        &self.0
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `uuid::Uuid`'s `Display` is exactly the canonical hyphenated lowercase form.
        write!(f, "{}", self.0)
    }
}

/// A v4 (random) UUID from 128 raw bits: 122 of them survive; the version nibble (`4`) and the
/// variant bits (`10`) overwrite their fixed positions.
pub fn v4(hi: u64, lo: u64) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    Uuid(uuid::Builder::from_random_bytes(bytes).into_uuid())
}

/// A v7 (time-ordered) UUID: 48 bits of unix milliseconds, then the version nibble (`7`),
/// 12 random bits (`rand_a`, from `ra`'s low bits), the variant bits (`10`), and 62 random bits
/// (`rand_b`, from `rb`). Millisecond ties sort by their random tail — fine for an id (v7 orders
/// by *time*; sub-millisecond ordering is what `next_id` is for).
pub fn v7(unix_ms: u64, ra: u64, rb: u64) -> Uuid {
    // rand_a's 12 low bits land in counter bytes 0-1 (the version nibble overwrites byte 0's
    // high half); rand_b's 64 bits land in bytes 2-9 (the variant bits overwrite the top two,
    // leaving 62). Identical bit placement to the id-entropy arc's hand-rolled layout — the
    // exact-value conformance pins (`std/id_uuid.noe`) hold across the swap.
    let mut tail = [0u8; 10];
    tail[0] = (ra >> 8) as u8;
    tail[1] = ra as u8;
    tail[2..].copy_from_slice(&rb.to_be_bytes());
    Uuid(uuid::Builder::from_unix_timestamp_millis(unix_ms, &tail).into_uuid())
}

/// A v5 (name-based, RFC 9562 §5.5) UUID — pure, no Host input: the SHA-1 of the namespace's
/// bytes followed by the name, with the version nibble (`5`) and variant bits overwriting their
/// fixed positions. Same namespace + same name = same UUID, everywhere, forever — the point.
/// The digest comes from our own `sha1` dep (crypto arc C5) rather than the uuid crate's `v5`
/// feature, which would drag in a second SHA-1 implementation (`sha1_smol`).
pub fn v5(ns: &Uuid, name: &str) -> Uuid {
    let mut input = Vec::with_capacity(16 + name.len());
    input.extend_from_slice(ns.as_bytes());
    input.extend_from_slice(name.as_bytes());
    let digest = crate::crypto::sha1(&input);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid(uuid::Builder::from_sha1_bytes(bytes).into_uuid())
}

/// The v7 timestamp read back as unix milliseconds — `some` iff the version carries a timestamp
/// (`u.timestamp_ms()`; the Option IS the version distinction, surfaced where it matters).
pub fn timestamp_ms(u: &Uuid) -> Option<u64> {
    u.get_timestamp().map(|ts| {
        let (secs, nanos) = ts.to_unix();
        secs * 1000 + u64::from(nanos) / 1_000_000
    })
}

/// The `Uuid` extern-value contract: ordered by bytes (v7 = time order), content-hashed,
/// canonical lowercase hyphenated display. `key_capable` — no mutating methods.
impl ExternValue for Uuid {
    fn type_identity(&self) -> &'static str {
        TYPE_IDENTITY
    }

    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Uuid>() == Some(self)
    }

    fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<Uuid>()
            .map(|o| self.as_bytes().cmp(o.as_bytes()))
    }

    fn hash_value(&self) -> u64 {
        // Stable and content-derived (the map-key contract): fold the two halves. The map's
        // FxHasher re-mixes, so a plain fold is enough.
        let (hi, lo) = self.as_u64_pair();
        hi ^ lo.rotate_left(32)
    }

    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "{self}")
    }

    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(*self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nibble(uuid: &Uuid, index: usize) -> char {
        uuid.to_string()
            .chars()
            .filter(|c| *c != '-')
            .nth(index)
            .unwrap()
    }

    #[test]
    fn v4_is_canonical_with_version_and_variant_pinned() {
        // The exact values the id-entropy arc's hand-rolled builder produced — the crate swap
        // is bit-identical (the sandbox conformance pins depend on it).
        let uuid = v4(0xFFFF_FFFF_FFFF_FFFF, 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(uuid.to_string(), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let uuid = v4(0, 0);
        assert_eq!(uuid.to_string(), "00000000-0000-4000-8000-000000000000");
        // Version nibble is hex digit 12; variant nibble is digit 16.
        let uuid = v4(0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210);
        assert_eq!(nibble(&uuid, 12), '4');
        assert!(matches!(nibble(&uuid, 16), '8' | '9' | 'a' | 'b'));
        assert_eq!(uuid.get_version_num(), 4);
    }

    #[test]
    fn v7_leads_with_the_timestamp_and_orders_by_time() {
        // 0x0123456789ab ms → the first 12 hex digits, then version 7.
        let uuid = v7(0x0123_4567_89AB, 0, 0);
        assert_eq!(uuid.to_string(), "01234567-89ab-7000-8000-000000000000");
        assert_eq!(nibble(&uuid, 12), '7');
        // Later millisecond → later id under the value ordering, regardless of random tails —
        // through the extern contract, i.e. exactly how sets/`compare` will order them.
        let earlier = v7(1_000, u64::MAX, u64::MAX);
        let later = v7(1_001, 0, 0);
        assert_eq!(earlier.cmp_value(&later), Some(std::cmp::Ordering::Less));
    }

    #[test]
    fn random_bits_land_intact_around_the_pinned_fields() {
        // rand_a = 12 bits: 0xABC → high nibble into the version byte's low half, rest verbatim.
        let uuid = v7(0, 0xABC, 0x3FFF_FFFF_FFFF_FFFF);
        assert_eq!(uuid.to_string(), "00000000-0000-7abc-bfff-ffffffffffff");
    }

    #[test]
    fn timestamp_reads_back_from_v7_and_none_from_v4() {
        assert_eq!(
            timestamp_ms(&v7(1_767_225_600_005, 1, 2)),
            Some(1_767_225_600_005)
        );
        assert_eq!(timestamp_ms(&v4(1, 2)), None);
    }

    /// The RFC 9562 v5 example (DNS namespace, "www.example.com") — independently confirmed
    /// against Python's `uuid.uuid5`. Deterministic by definition, so an exact pin.
    #[test]
    fn v5_matches_the_rfc_9562_example() {
        let u = v5(&Uuid(uuid::Uuid::NAMESPACE_DNS), "www.example.com");
        assert_eq!(u.to_string(), "2ed6657d-e927-568b-95e1-2665a8aea6a2");
        assert_eq!(u.get_version_num(), 5);
        assert_eq!(u, v5(&Uuid(uuid::Uuid::NAMESPACE_DNS), "www.example.com"));
    }

    #[test]
    fn the_extern_contract_is_content_equality_and_display_is_canonical() {
        let a = v4(1, 2);
        let b = v4(1, 2);
        let c = v4(1, 3);
        assert!(a.eq_value(&b));
        assert!(!a.eq_value(&c));
        assert_eq!(a.hash_value(), b.hash_value());
        let dyn_a: &dyn ExternValue = &a;
        assert_eq!(dyn_a.display_string(), a.to_string());
        assert_eq!(dyn_a.type_identity(), "std.id.Uuid");
        assert_eq!(dyn_a.type_display_name(), "Uuid");
    }
}
