//! Pointing the generator at the `.noeb` container: real modules, and corruptions of them.
//!
//! # Why corrupted-valid rather than random bytes
//!
//! This is the one place in the toolchain where *byte*-level fuzzing is the right instrument — a
//! bundle is a binary container read from disk, and the property is that no byte string makes the
//! reader panic. But random bytes are still nearly worthless here, for the same reason they are
//! against the parser: `read` checks a 4-byte magic first, so ~all of them are rejected at offset 0
//! and nothing past the header is ever exercised.
//!
//! So the input is a *valid* bundle with something broken in it. That reaches the length fields,
//! the inflate, and `Module::decode` — which is where an attacker-controlled length or a malformed
//! varint would actually bite. [`reach`] measures how deep each input got and the test asserts a
//! floor, exactly as the program generator asserts its parse rate: a corruption strategy that
//! drifted into only ever breaking the magic would leave the suite green and vacuous.
//!
//! # What a bundle is, and is not
//!
//! It is a **local build artifact**, not something the package manager fetches — `noeta-pm` never
//! touches `.noeb`. It is read by `noeta run app.noeb`, by the startup cache on every run, by a
//! stapled executable's own tail, and by the wasm runner at the edge. So the realistic corruption
//! is a truncated or garbled file (a half-written cache entry, an interrupted download, a bad
//! disk), not an adversary with an oracle. That makes "never panics, always explains itself" the
//! property worth having, and it is worth having precisely because the startup cache reads one on
//! every single run.

use noeta_bytecode::Module;
use noeta_span::{Source, SourceId};

/// Compile the generated program that `(seed, nonce)` denotes into a bytecode module.
///
/// `None` when the program does not lex, parse, **type-check**, or compile.
///
/// # The checker is a precondition, not a formality
///
/// The generator emits syntactically valid programs that are mostly *not* type-correct, and feeding
/// those straight to `noeta_compiler::compile` panics for about 4% of them — a duplicate class name,
/// for instance, re-registers the type so the first declaration's methods are missing from its
/// dispatch table and the lookup indexes a key that is not there. That is not a compiler bug
/// reachable by a user: measured over 2,000 programs, **zero** both pass the checker and panic the
/// compiler, so `noeta run` never gets there. The compiler assumes a checked program, and a harness
/// that skips the checker is testing a contract nobody offers.
///
/// So this runs `check_all` and requires it clean. That costs yield — under a tenth of generated
/// programs type-check — but the modules it does produce are the ones the compiler is actually
/// asked to build, which is the population the round-trip property is about.
pub fn module_for(seed: u64, nonce: u32) -> Option<Module> {
    // The compiler resolves `std.*` through the process-wide extension registry and *panics* if
    // none is installed — linking `noeta-stdlib` is not enough, its provider has to be registered.
    // Same seeding the conformance harness does, and idempotent, so calling it per module is free.
    noeta_stdlib::registry::default_seeded();
    let src = crate::generate::program(&crate::seed_bytes(seed, nonce));
    let source = Source::new(SourceId(0), "bundle.noe", src);
    let lexed = noeta_lexer::lex(&source);
    if !lexed.diagnostics.is_empty() {
        return None;
    }
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    if !parsed.diagnostics.is_empty() {
        return None;
    }
    if !noeta_check::check_all(&parsed.program)
        .diagnostics
        .is_empty()
    {
        return None;
    }
    noeta_compiler::compile(&parsed.program).ok()
}

/// How far into `read` an input got before being rejected — the depth metric the anti-vacuity
/// assertion is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reach {
    /// Rejected at the 4-byte magic, or too short to have one. The shallowest possible outcome:
    /// nothing but the first comparison ran.
    Header,
    /// Past the magic, rejected on the version/flags/length fields or a bad runtime string.
    Envelope,
    /// Past the envelope: the payload was de-scrambled and inflated, or failed to. This is where
    /// a decompression bomb or a malformed deflate stream lands.
    Payload,
    /// Inflated cleanly and reached `Module::decode`, which rejected it. The deepest rejection.
    Decode,
    /// Decoded to a module.
    Loaded,
}

/// Classify how deep `bytes` got. Mirrors `noeta_bundle::read`'s own order of checks; it is a
/// separate implementation on purpose, so a change to that order shows up as a shift in the reach
/// histogram rather than silently agreeing with itself.
pub fn reach(bytes: &[u8]) -> Reach {
    use noeta_bundle::BundleError;
    match noeta_bundle::read(bytes) {
        Ok(_) => Reach::Loaded,
        Err(BundleError::BadMagic) => Reach::Header,
        Err(BundleError::Truncated) if bytes.len() < 7 => Reach::Header,
        Err(
            BundleError::Truncated
            | BundleError::UnsupportedFormat { .. }
            | BundleError::UnsupportedTransform { .. }
            | BundleError::VersionMismatch { .. },
        ) => Reach::Envelope,
        // `Decode` covers both a failed inflate and a failed postcard decode; the payload length
        // separates "there was something to inflate" from "there was not".
        Err(BundleError::Decode) if bytes.len() > 7 => Reach::Decode,
        Err(BundleError::Decode) => Reach::Payload,
    }
}

/// Damage `bundle` in one of several ways, chosen by `bytes`.
///
/// Every strategy keeps the result *plausible* — the magic survives unless the strategy is
/// explicitly about the magic — because an input rejected at offset 0 tests nothing. The strategies
/// aim at the fields a real corruption would hit: the declared runtime-string length, the flags
/// byte, the compressed payload, and the tail.
pub fn corrupt(bundle: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut out = bundle.to_vec();
    if out.is_empty() {
        return out;
    }
    let at = |i: usize| bytes.get(i).copied().unwrap_or(0);
    let pos = |i: usize, len: usize| {
        if len == 0 {
            0
        } else {
            (usize::from(at(i)) | (usize::from(at(i + 1)) << 8)) % len
        }
    };
    match at(0) % 8 {
        // Truncate. The commonest real corruption by far: a half-written cache entry.
        0 => {
            let keep = pos(1, out.len());
            out.truncate(keep);
        }
        // Flip one bit anywhere.
        1 => {
            let p = pos(1, out.len());
            out[p] ^= 1 << (at(3) % 8);
        }
        // Overwrite one byte with an arbitrary value.
        2 => {
            let p = pos(1, out.len());
            out[p] = at(3);
        }
        // Tamper with the declared runtime-string length (byte 6) — a length field the reader
        // trusts to slice with.
        3 => {
            if out.len() > 6 {
                out[6] = at(1);
            }
        }
        // Tamper with the flags byte, including setting reserved/unknown transform bits.
        4 => {
            if out.len() > 5 {
                out[5] = at(1);
            }
        }
        // Corrupt a run inside the compressed payload, past the header.
        5 => {
            let start = 7 + pos(1, out.len().saturating_sub(7).max(1));
            let end = (start + 1 + usize::from(at(3))).min(out.len());
            for (k, b) in out
                .get_mut(start..end)
                .unwrap_or(&mut [])
                .iter_mut()
                .enumerate()
            {
                *b ^= at(4).wrapping_add(k as u8);
            }
        }
        // Append trailing garbage — a bundle with something concatenated after it.
        6 => {
            let extra = usize::from(at(1)) % 64;
            out.extend((0..extra).map(|k| at(2).wrapping_add(k as u8)));
        }
        // Break the magic itself, so the shallow path stays covered too.
        _ => {
            out[0] ^= 0xFF;
        }
    }
    out
}
