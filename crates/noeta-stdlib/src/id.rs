//! The first-class `Uuid` extern type and its constructors (id-entropy U2 → extern-types X2).
//!
//! Construction is pure functions from raw bits/time to a [`uuid::Uuid`] — the host supplies the
//! inputs ([`crate::host::Entropy`] for random bits, [`crate::host::Clock::clock_unix_ms`] for
//! the v7 timestamp), so the same code produces deterministic UUIDs on the sandbox and real ones
//! on the real host, and the differential holds by shared dispatch like every other registry
//! module. The byte assembly itself is the `uuid` crate's RFC 9562 [`uuid::Builder`] — compiled
//! WITHOUT the self-generating `v4`/`v7` features, so no entropy or time source exists outside
//! the Host seam.
//!
//! The value is `uuid::Uuid` itself, registered through the extern-type seam
//! ([`crate::ExternValue`] below): one type both backends host, ordered by its bytes (a v7
//! therefore sorts by time), key-capable, displayed in the canonical lowercase hyphenated form
//! `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (8-4-4-4-12).

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;

use crate::extern_value::ExternValue;

/// The registered extern-type name (`ExtType::name`, `type_of`, `x is Uuid`).
pub const TYPE_NAME: &str = "Uuid";

/// A v4 (random) UUID from 128 raw bits: 122 of them survive; the version nibble (`4`) and the
/// variant bits (`10`) overwrite their fixed positions.
pub fn v4(hi: u64, lo: u64) -> uuid::Uuid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

/// A v7 (time-ordered) UUID: 48 bits of unix milliseconds, then the version nibble (`7`),
/// 12 random bits (`rand_a`, from `ra`'s low bits), the variant bits (`10`), and 62 random bits
/// (`rand_b`, from `rb`). Millisecond ties sort by their random tail — fine for an id (v7 orders
/// by *time*; sub-millisecond ordering is what `next_id` is for).
pub fn v7(unix_ms: u64, ra: u64, rb: u64) -> uuid::Uuid {
    // rand_a's 12 low bits land in counter bytes 0-1 (the version nibble overwrites byte 0's
    // high half); rand_b's 64 bits land in bytes 2-9 (the variant bits overwrite the top two,
    // leaving 62). Identical bit placement to the id-entropy arc's hand-rolled layout — the
    // exact-value conformance pins (`std/id_uuid.noe`) hold across the swap.
    let mut tail = [0u8; 10];
    tail[0] = (ra >> 8) as u8;
    tail[1] = ra as u8;
    tail[2..].copy_from_slice(&rb.to_be_bytes());
    uuid::Builder::from_unix_timestamp_millis(unix_ms, &tail).into_uuid()
}

/// The v7 timestamp read back as unix milliseconds — `some` iff the version carries a timestamp
/// (`u.timestamp_ms()`; the Option IS the version distinction, surfaced where it matters).
pub fn timestamp_ms(u: &uuid::Uuid) -> Option<u64> {
    u.get_timestamp().map(|ts| {
        let (secs, nanos) = ts.to_unix();
        secs * 1000 + u64::from(nanos) / 1_000_000
    })
}

/// The `Uuid` extern-value contract: ordered by bytes (v7 = time order), content-hashed,
/// canonical lowercase hyphenated display. `key_capable` — no mutating methods.
impl ExternValue for uuid::Uuid {
    fn type_name(&self) -> &'static str {
        TYPE_NAME
    }

    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<uuid::Uuid>() == Some(self)
    }

    fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<uuid::Uuid>()
            .map(|o| self.as_bytes().cmp(o.as_bytes()))
    }

    fn hash_value(&self) -> u64 {
        // Stable and content-derived (the map-key contract): fold the two halves. The map's
        // FxHasher re-mixes, so a plain fold is enough.
        let (hi, lo) = self.as_u64_pair();
        hi ^ lo.rotate_left(32)
    }

    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        // `uuid::Uuid`'s `Display` is exactly the canonical hyphenated lowercase form.
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

    fn nibble(uuid: &uuid::Uuid, index: usize) -> char {
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
        assert_eq!(timestamp_ms(&v7(1_767_225_600_005, 1, 2)), Some(1_767_225_600_005));
        assert_eq!(timestamp_ms(&v4(1, 2)), None);
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
        assert_eq!(dyn_a.type_name(), "Uuid");
    }
}
