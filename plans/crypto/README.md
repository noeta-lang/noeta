# std.crypto — hashing, HMAC, bcrypt, and `id.uuid_v5` (the third seam client)

**Status: ARC COMPLETE (2026-07-07).** C0 `4cbdafb`, C1 `6d5fd4e`, C2 `445cadf`, C3 `b128b63`,
C4 `4557cf2`, C5 `aa11b98`, C6 docs+plan+memory, C7 constant-time verification
(`hmac_sha256_verify`/`hmac_sha512_verify` on the hmac crate's `Mac::verify_slice`, plus
`constant_time_eq` via `subtle` — the deferred hmac_verify row, pulled forward on user request).
Branch `std-crypto` (off local main `6d069ca`).
Gates: all published vectors pinned (NIST FIPS 180, RFC 4231, RFC 9562, openwall bcrypt —
digest/HMAC values cross-checked against Python hashlib, v5 against Python uuid5); C2/C3/C5
landed with **zero backend edits** (the registry seam carried a new module, a new extern type,
and new module functions alone); bcrypt sandbox hash exact-pinned by the differential +
real-entropy CLI round-trip. Follow-ons in `plans/deferred.md` §Crypto (blake3, argon2,
encryption, constant-time hmac_verify).

A tier-2 stdlib module for the everyday cryptographic primitives an application language needs:
content digests (sha256/sha512, plus sha1/md5 for interop), keyed digests (HMAC), password
hashing (bcrypt via the Host entropy seam), and crypto-grade random bytes. The incremental
`Hasher` becomes the **third extern-type seam client**, exercising the one corner of the
{pure, mutable} × {host-free, effectful} matrix no type has hit yet: **mutable + host-free**
(Uuid = pure+host-free, FileHandle = mutable+effectful).

RustCrypto crates do the math (`sha1`, `sha2`, `md-5`, `hmac`) plus `bcrypt`; we never
hand-roll primitives. All are pure-Rust, no default features beyond what's needed.

## Design decisions

1. **Digests are `bytes`; hex is a rendering.** Every digest fn returns `bytes` (the packed-types
   buffer). `bytes` gains a `to_hex()` method — digests stay composable (HMAC of a digest,
   digest as key material) and `sha256(s).to_hex()` covers the common case.
2. **Inputs are `str|bytes` — via a new `SigType::Union`.** The language has declared unions;
   the stdlib signature vocabulary doesn't. Rather than `Dyn` (loses static checking) or
   `_bytes` twins (API noise), grow `SigType::Union(&'static [SigType])` mapping onto
   `Type::union_of`. Strings hash as their UTF-8 bytes. This is the enabling slice — future
   stdlib sigs get unions for free.
3. **One `Hasher` extern type, per-algorithm constructors.** `crypto.sha256_hasher()` etc.
   return `Hasher` (an enum over algorithm states inside one ExternValue — one type, not four).
   `update(data: str|bytes)` mutates the receiver (the seam corner); `digest()` is
   **non-destructive** (clones the state, finalizes the clone) so a hasher can report interim
   digests and keep accepting updates — deterministic and least surprising. `key_capable: false`
   (mutable). Equality = same algorithm + same absorbed state? No — hasher states don't expose
   comparison; `eq_value` is identity-flavored false-unless-same-object is not available to a
   value type, so eq = both sides' *current digest* + algorithm (well-defined, testable).
4. **bcrypt salts come from the Host.** `bcrypt_hash(password, cost)` draws a 16-byte salt from
   `entropy_u64()` ×2 — deterministic (exact-string pinnable) in the sandbox, OS entropy on the
   real host. `bcrypt_verify(password, hash)` is pure. Cost validated to bcrypt's 4..=31.
5. **`crypto.random_bytes(n)`** — crypto-grade random bytes off the Entropy capability (tokens,
   salts, key material). Distinct from `std.random` (seeded PRNG stream) on purpose, same
   split as `id.uuid` vs `random.int`.
6. **`id.uuid_v5(ns, name)` lands in `std.id`,** not crypto — it's an id concern that merely
   uses sha1 internally (via our `sha1` dep + `uuid::Builder::from_sha1_bytes`; no extra uuid
   crate feature/dep). Well-known namespaces ship as zero-arg fns: `id.namespace_dns()` /
   `namespace_url()` / `namespace_oid()` / `namespace_x500()` (uuid crate constants).
7. **Out of scope (deferred, recorded in `plans/deferred.md`):** blake3 (asm/simd dep story),
   argon2/scrypt (bcrypt covers M2; add when password-hash pluggability is designed), AES/
   encryption (key-management design first), HTTP-signature helpers (belongs to the HTTP arc).

## Slices

- **C0** — this plan + workspace deps (`sha1`, `sha2`, `md-5`, `hmac`, `bcrypt`).
- **C1** — enabling surface: `SigType::Union` (+ checker mapping/arg check) and `bytes.to_hex()`
  in both backends + checker. Conformance: to_hex exact values, union arg mismatch = E0007.
- **C2** — `std.crypto` digest fns: `sha256`/`sha512`/`sha1`/`md5` and `hmac_sha256`/
  `hmac_sha512` (`(key: str|bytes, data: str|bytes)`), all `-> bytes`, pure/host-free.
  Conformance pins published test vectors (RFC/NIST) as exact hex.
- **C3** — `Hasher` extern type: `sha256_hasher()`/`sha512_hasher()`, `update()`/`digest()`.
  Conformance: incremental == one-shot, interim digests, `is Hasher` narrowing, eq contract.
- **C4** — `bcrypt_hash`/`bcrypt_verify` + `random_bytes` on the Host entropy seam. Sandbox
  conformance pins the exact hash string (seeded entropy ⇒ fixed salt; cost 4 for speed);
  real-executor CLI test verifies a real hash round-trips; cost-range + arg errors.
- **C5** — `id.uuid_v5(ns: Uuid, name: str) -> Uuid` + namespace fns. Conformance pins the
  RFC 9562 DNS/"www.example.com" vector.
- **C6** — docs (Standard-Library-Modules crypto section + id additions, Native-Extensions
  gains the mutable+host-free Hasher row), plan outcome, deferred entries, memory.

## Gates

Per slice: workspace tests + full conformance corpus (differential + leak + doc-samples) green;
commit per green slice. No hot-path structures are touched (registry tables and checker arg
paths are cold), so no bench gate — if that changes, bench it.
