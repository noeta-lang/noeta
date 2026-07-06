//! UUID construction for the `id` module (id-entropy U2).
//!
//! Pure functions from raw bits/time to canonical strings — the host supplies the inputs
//! ([`crate::host::Entropy`] for random bits, [`crate::host::Clock::clock_unix_ms`] for the v7
//! timestamp), so the same code produces deterministic UUIDs on the sandbox and real ones on the
//! real host, and the differential holds by shared dispatch like every other registry module.
//!
//! Layouts per RFC 9562. Both versions render in the canonical form: lowercase hex,
//! `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (8-4-4-4-12).

/// A v4 (random) UUID from 128 raw bits: 122 of them survive; the version nibble (`4`) and the
/// variant bits (`10`) overwrite their fixed positions.
pub fn v4(hi: u64, lo: u64) -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant 10xx
    format_uuid(&bytes)
}

/// A v7 (time-ordered) UUID: 48 bits of unix milliseconds, then the version nibble (`7`),
/// 12 random bits (`rand_a`, from `ra`'s low bits), the variant bits (`10`), and 62 random bits
/// (`rand_b`, from `rb`). Millisecond ties sort by their random tail — fine for an id (v7 orders
/// by *time*; sub-millisecond ordering is what `next_id` is for).
pub fn v7(unix_ms: u64, ra: u64, rb: u64) -> String {
    let mut bytes = [0u8; 16];
    // The timestamp's low 48 bits, big-endian — the top of the id, so string order is time order.
    bytes[..6].copy_from_slice(&(unix_ms << 16).to_be_bytes()[..6]);
    bytes[6] = 0x70 | ((ra >> 8) & 0x0F) as u8; // version 7 + rand_a high 4 bits
    bytes[7] = (ra & 0xFF) as u8; // rand_a low 8 bits
    let rb_bytes = rb.to_be_bytes();
    bytes[8..].copy_from_slice(&rb_bytes);
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant 10xx (rand_b keeps 62 bits)
    format_uuid(&bytes)
}

/// Canonical hyphenated lowercase rendering (8-4-4-4-12).
fn format_uuid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nibble(uuid: &str, index: usize) -> char {
        uuid.chars().filter(|c| *c != '-').nth(index).unwrap()
    }

    #[test]
    fn v4_is_canonical_with_version_and_variant_pinned() {
        let uuid = v4(0xFFFF_FFFF_FFFF_FFFF, 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(uuid, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let uuid = v4(0, 0);
        assert_eq!(uuid, "00000000-0000-4000-8000-000000000000");
        // Version nibble is hex digit 12; variant nibble is digit 16.
        let uuid = v4(0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210);
        assert_eq!(nibble(&uuid, 12), '4');
        assert!(matches!(nibble(&uuid, 16), '8' | '9' | 'a' | 'b'));
    }

    #[test]
    fn v7_leads_with_the_timestamp_and_orders_by_time() {
        // 0x0123456789ab ms → the first 12 hex digits, then version 7.
        let uuid = v7(0x0123_4567_89AB, 0, 0);
        assert_eq!(uuid, "01234567-89ab-7000-8000-000000000000");
        assert_eq!(nibble(&uuid, 12), '7');
        // Later millisecond → lexicographically later id, regardless of the random tails.
        let earlier = v7(1_000, u64::MAX, u64::MAX);
        let later = v7(1_001, 0, 0);
        assert!(earlier < later);
    }

    #[test]
    fn random_bits_land_intact_around_the_pinned_fields() {
        // rand_a = 12 bits: 0xABC → high nibble into the version byte's low half, rest verbatim.
        let uuid = v7(0, 0xABC, 0x3FFF_FFFF_FFFF_FFFF);
        assert_eq!(uuid, "00000000-0000-7abc-bfff-ffffffffffff");
    }
}
