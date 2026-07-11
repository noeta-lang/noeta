//! Wasm stapling (P-WASM W1.2): inject a `.noeb` bundle into the wasm runner binary, producing
//! the single-artifact `noeta build --wasm` output.
//!
//! The runner compiles in a magic-tagged, zero-initialized slot static
//! (`noeta-wasm-runner/src/embedded.rs`); this patcher rewrites the runner module to make that
//! slot point at the bundle:
//!
//! 1. **Place the bundle at the old memory end.** The new active data segment starts at
//!    `initial_pages * 64KiB` and the memory minimum is bumped to cover it. This needs no layout
//!    knowledge and cannot collide with anything: static data and the shadow stack live below
//!    the old minimum, and Rust's wasm allocator acquires pages via `memory.grow`, whose first
//!    call returns the *new* minimum — so the heap starts strictly above the bundle.
//! 2. **Patch the slot.** Find [`WASM_SLOT_MAGIC`] in the existing data segments (the runner
//!    guarantees exactly one copy) and write the bundle's address and length as the two
//!    little-endian `u32`s that follow it.
//!
//! The rewrite is a **section-level walk of the raw binary** — the wasm top level is just
//! `(id, size, contents)*` — touching only the memory, data-count, and data sections and copying
//! every other section byte-for-byte. Deliberately not a full-module IR round-trip (a
//! walrus-class rewrite re-encodes every function body: ~1000× the work for zero benefit here,
//! measured at ~1.2 s per staple vs ~1 ms) and deliberately dependency-free, so the patcher
//! rides `noeta-bundle` everywhere the decode side does (walrus remains a dev-dependency, to
//! build and re-validate test modules). The contract is tiny — a 16-byte marker and two
//! integers — so any future runner works as long as it keeps the slot.

use crate::WASM_SLOT_MAGIC;

/// Why a staple failed. These are toolchain-facing (surfaced by `noeta build --wasm`), so each
/// message says what to do about it.
#[derive(Debug)]
pub enum WasmStapleError {
    /// The runner bytes are not a parseable wasm module (or use an encoding this walker does not
    /// handle — see the message).
    Parse(String),
    /// The bundle bytes are not a `.noeb` (the caller passed the wrong thing).
    NotABundle,
    /// No slot magic in the module — not a runner build, or the slot was optimized away.
    SlotMissing,
    /// More than one magic occurrence — ambiguous; refuse rather than guess.
    SlotAmbiguous,
    /// The module's memory layout cannot take the bundle (no memory, or a maximum too small).
    Memory(String),
}

impl std::fmt::Display for WasmStapleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmStapleError::Parse(e) => write!(f, "cannot parse the wasm runner: {e}"),
            WasmStapleError::NotABundle => write!(f, "the payload is not a `.noeb` bundle"),
            WasmStapleError::SlotMissing => write!(
                f,
                "the wasm runner has no bundle slot (is it a `noeta-wasm-runner` build?)"
            ),
            WasmStapleError::SlotAmbiguous => write!(
                f,
                "the wasm runner contains more than one bundle-slot marker; refusing to patch"
            ),
            WasmStapleError::Memory(e) => write!(f, "cannot place the bundle: {e}"),
        }
    }
}

impl std::error::Error for WasmStapleError {}

const PAGE: u64 = 65536;
const SEC_MEMORY: u8 = 5;
const SEC_DATA: u8 = 11;
const SEC_DATA_COUNT: u8 = 12;

/// Rewrite `runner` (a `noeta-wasm-runner` wasm binary) so it carries and runs `bundle` — the
/// single-artifact `noeta build --wasm` output.
pub fn staple_wasm(runner: &[u8], bundle: &[u8]) -> Result<Vec<u8>, WasmStapleError> {
    if !crate::is_bundle(bundle) {
        return Err(WasmStapleError::NotABundle);
    }

    // Pass 1: the bundle's address comes from the memory section, which precedes the data
    // section — read the old minimum before emitting anything.
    let sections = split_sections(runner)?;
    let memory_contents = sections
        .iter()
        .find(|s| s.id == SEC_MEMORY)
        .map(|s| s.contents)
        .ok_or_else(|| WasmStapleError::Memory("the module has no memory section".into()))?;
    let (initial, maximum) = parse_first_memory(memory_contents)?;
    let addr = initial * PAGE;
    let new_initial = initial + bundle.len().div_ceil(PAGE as usize) as u64;
    if let Some(max) = maximum
        && max < new_initial
    {
        return Err(WasmStapleError::Memory(format!(
            "the module's memory maximum ({max} pages) is below the {new_initial} pages needed"
        )));
    }
    let addr_u32 = u32::try_from(addr)
        .map_err(|_| WasmStapleError::Memory("the bundle address exceeds wasm32 memory".into()))?;

    // Pass 2: re-emit, transforming exactly three sections and copying the rest verbatim.
    let mut out = Vec::with_capacity(runner.len() + bundle.len() + 64);
    out.extend_from_slice(&runner[..8]); // magic + version
    let mut patched = 0usize;
    let mut saw_data = false;
    for section in &sections {
        match section.id {
            SEC_MEMORY => {
                let body = bump_first_memory(section.contents, new_initial, maximum)?;
                push_section(&mut out, SEC_MEMORY, &body);
            }
            SEC_DATA_COUNT => {
                let (count, rest) = read_leb_u64(section.contents)
                    .ok_or_else(|| parse_err("truncated data-count section"))?;
                if !rest.is_empty() {
                    return Err(parse_err("trailing bytes in the data-count section"));
                }
                let mut body = Vec::new();
                write_leb_u64(&mut body, count + 1);
                push_section(&mut out, SEC_DATA_COUNT, &body);
            }
            SEC_DATA => {
                saw_data = true;
                let body = rewrite_data_section(section.contents, bundle, addr_u32, &mut patched)?;
                push_section(&mut out, SEC_DATA, &body);
            }
            id => {
                push_section(&mut out, id, section.contents);
            }
        }
    }
    if !saw_data {
        return Err(WasmStapleError::SlotMissing);
    }
    match patched {
        0 => Err(WasmStapleError::SlotMissing),
        1 => Ok(out),
        _ => Err(WasmStapleError::SlotAmbiguous),
    }
}

fn parse_err(msg: &str) -> WasmStapleError {
    WasmStapleError::Parse(msg.to_string())
}

/// One top-level section: `(id, size, contents)` with the contents borrowed from the input.
struct Section<'a> {
    id: u8,
    contents: &'a [u8],
}

/// Split a wasm binary into its top-level sections (after validating the 8-byte header).
fn split_sections(binary: &[u8]) -> Result<Vec<Section<'_>>, WasmStapleError> {
    if binary.len() < 8 || &binary[0..4] != b"\0asm" || binary[4..8] != [1, 0, 0, 0] {
        return Err(parse_err("not a wasm module (bad magic/version)"));
    }
    let mut rest = &binary[8..];
    let mut sections = Vec::new();
    while !rest.is_empty() {
        let id = rest[0];
        let (size, after) =
            read_leb_u64(&rest[1..]).ok_or_else(|| parse_err("truncated section header"))?;
        let size = size as usize;
        if after.len() < size {
            return Err(parse_err("section extends past the end of the module"));
        }
        sections.push(Section {
            id,
            contents: &after[..size],
        });
        rest = &after[size..];
    }
    Ok(sections)
}

/// Append `(id, size, contents)` to `out`.
fn push_section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
    out.push(id);
    write_leb_u64(out, contents.len() as u64);
    out.extend_from_slice(contents);
}

/// Read the first memory's `(initial, maximum)` from a memory section's contents.
fn parse_first_memory(contents: &[u8]) -> Result<(u64, Option<u64>), WasmStapleError> {
    let (count, rest) =
        read_leb_u64(contents).ok_or_else(|| parse_err("truncated memory section"))?;
    if count != 1 {
        return Err(WasmStapleError::Memory(format!(
            "expected exactly one linear memory, found {count}"
        )));
    }
    let (_, initial, maximum, _) = parse_limits(rest)?;
    Ok((initial, maximum))
}

/// Re-encode a memory section's contents with the first memory's minimum set to `new_initial`.
fn bump_first_memory(
    contents: &[u8],
    new_initial: u64,
    maximum: Option<u64>,
) -> Result<Vec<u8>, WasmStapleError> {
    let (_, rest) = read_leb_u64(contents).ok_or_else(|| parse_err("truncated memory section"))?;
    let (flags, _, _, after) = parse_limits(rest)?;
    if !after.is_empty() {
        return Err(parse_err("trailing bytes in the memory section"));
    }
    let mut body = Vec::new();
    write_leb_u64(&mut body, 1); // count
    body.push(flags);
    write_leb_u64(&mut body, new_initial);
    if let Some(max) = maximum {
        write_leb_u64(&mut body, max);
    }
    Ok(body)
}

/// A parsed limits encoding: the flags byte, minimum, optional maximum, and the bytes after it.
type Limits<'a> = (u8, u64, Option<u64>, &'a [u8]);

/// Parse a wasm limits encoding. Only the 32-bit non-shared forms (flags 0x00/0x01) exist in a
/// wasip1 runner; anything else is refused loudly.
fn parse_limits(bytes: &[u8]) -> Result<Limits<'_>, WasmStapleError> {
    let (&flags, rest) = bytes
        .split_first()
        .ok_or_else(|| parse_err("truncated memory limits"))?;
    let (min, rest) = read_leb_u64(rest).ok_or_else(|| parse_err("truncated memory minimum"))?;
    match flags {
        0x00 => Ok((flags, min, None, rest)),
        0x01 => {
            let (max, rest) =
                read_leb_u64(rest).ok_or_else(|| parse_err("truncated memory maximum"))?;
            Ok((flags, min, Some(max), rest))
        }
        other => Err(parse_err(&format!(
            "unsupported memory limits flags 0x{other:02x} (shared/memory64?)"
        ))),
    }
}

/// Rewrite a data section's contents: patch the slot wherever the magic occurs (counting into
/// `patched` — the caller enforces exactly one) and append the bundle as a new active segment at
/// `addr`. Existing segments are copied with their payloads intact.
fn rewrite_data_section(
    contents: &[u8],
    bundle: &[u8],
    addr: u32,
    patched: &mut usize,
) -> Result<Vec<u8>, WasmStapleError> {
    let (count, mut rest) =
        read_leb_u64(contents).ok_or_else(|| parse_err("truncated data section"))?;
    let mut body = Vec::with_capacity(contents.len() + bundle.len() + 16);
    write_leb_u64(&mut body, count + 1);
    for _ in 0..count {
        let (flags, after) = read_leb_u64(rest).ok_or_else(|| parse_err("truncated segment"))?;
        write_leb_u64(&mut body, flags);
        let after = match flags {
            // Active, memory 0: an init expr precedes the payload.
            0 => copy_const_expr(after, &mut body)?,
            // Passive: payload only.
            1 => after,
            // Active with an explicit memory index.
            2 => {
                let (mem, after) =
                    read_leb_u64(after).ok_or_else(|| parse_err("truncated memory index"))?;
                write_leb_u64(&mut body, mem);
                copy_const_expr(after, &mut body)?
            }
            other => {
                return Err(parse_err(&format!(
                    "unsupported data-segment flags {other}"
                )));
            }
        };
        let (size, after) =
            read_leb_u64(after).ok_or_else(|| parse_err("truncated segment size"))?;
        let size = size as usize;
        if after.len() < size {
            return Err(parse_err("data segment extends past the section"));
        }
        write_leb_u64(&mut body, size as u64);
        let payload_at = body.len();
        body.extend_from_slice(&after[..size]);
        rest = &after[size..];

        // Patch the slot in the copied payload: `[magic][ptr: u32 LE][len: u32 LE]`.
        let payload = &mut body[payload_at..];
        let mut search_from = 0;
        while let Some(at) = find(&payload[search_from..], &WASM_SLOT_MAGIC) {
            let start = search_from + at + WASM_SLOT_MAGIC.len();
            if payload.len() < start + 8 {
                // A truncated tail match cannot be the slot (its two u32s would not fit).
                break;
            }
            payload[start..start + 4].copy_from_slice(&addr.to_le_bytes());
            payload[start + 4..start + 8].copy_from_slice(&(bundle.len() as u32).to_le_bytes());
            *patched += 1;
            search_from = start + 8;
        }
    }
    if !rest.is_empty() {
        return Err(parse_err("trailing bytes in the data section"));
    }

    // The appended segment: active on memory 0, `i32.const addr`, the bundle bytes.
    body.push(0); // flags
    body.push(0x41); // i32.const
    write_sleb_i64(&mut body, i64::from(addr as i32));
    body.push(0x0B); // end
    write_leb_u64(&mut body, bundle.len() as u64);
    body.extend_from_slice(bundle);
    Ok(body)
}

/// Copy a const init expr (`i32.const n end` or `global.get n end` — the forms a linker emits
/// for data offsets) from `bytes` into `out`, returning the rest.
fn copy_const_expr<'a>(bytes: &'a [u8], out: &mut Vec<u8>) -> Result<&'a [u8], WasmStapleError> {
    let (&op, rest) = bytes
        .split_first()
        .ok_or_else(|| parse_err("truncated init expr"))?;
    out.push(op);
    let rest = match op {
        0x41 => {
            let (value, len) =
                read_sleb_i64(rest).ok_or_else(|| parse_err("truncated i32.const"))?;
            write_sleb_i64(out, value);
            &rest[len..]
        }
        0x23 => {
            let (index, rest2) =
                read_leb_u64(rest).ok_or_else(|| parse_err("truncated global.get"))?;
            write_leb_u64(out, index);
            rest2
        }
        other => {
            return Err(parse_err(&format!(
                "unsupported data-offset init expr opcode 0x{other:02x}"
            )));
        }
    };
    match rest.split_first() {
        Some((&0x0B, rest)) => {
            out.push(0x0B);
            Ok(rest)
        }
        _ => Err(parse_err("init expr not terminated by `end`")),
    }
}

/// Read an unsigned LEB128, returning the value and the rest.
fn read_leb_u64(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in bytes.iter().enumerate() {
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, &bytes[i + 1..]));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// Read a signed LEB128, returning the value and its encoded length.
fn read_sleb_i64(bytes: &[u8]) -> Option<(i64, usize)> {
    let mut value = 0i64;
    let mut shift = 0u32;
    for (i, &byte) in bytes.iter().enumerate() {
        value |= i64::from(byte & 0x7F) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                value |= -1i64 << shift;
            }
            return Some((value, i + 1));
        }
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// Write an unsigned LEB128.
fn write_leb_u64(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Write a signed LEB128.
fn write_sleb_i64(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        let sign_clear = byte & 0x40 == 0;
        if (value == 0 && sign_clear) || (value == -1 && !sign_clear) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// First occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic "runner" (built with walrus — a dev-dependency, kept exactly for building and
    /// re-validating test modules): one 2-page memory and one data segment carrying `slots`
    /// copies of the slot (each magic + 8 zero bytes), padded either side.
    fn synthetic_runner(slots: usize) -> Vec<u8> {
        let mut module = walrus::Module::default();
        let memory = module.memories.add_local(false, false, 2, None, None);
        let mut value = vec![0xAAu8; 8];
        for _ in 0..slots {
            value.extend_from_slice(&WASM_SLOT_MAGIC);
            value.extend_from_slice(&[0u8; 8]);
        }
        value.extend_from_slice(&[0xBBu8; 8]);
        module.data.add(
            walrus::DataKind::Active {
                memory,
                offset: walrus::ConstExpr::Value(walrus::ir::Value::I32(16)),
            },
            value,
        );
        module.emit_wasm()
    }

    fn bundle() -> Vec<u8> {
        crate::write(&noeta_bytecode::Module::default())
    }

    #[test]
    fn staple_places_the_bundle_at_the_old_memory_end_and_patches_the_slot() {
        let bundle = bundle();
        let image = staple_wasm(&synthetic_runner(1), &bundle).expect("staples");
        // Re-parse with walrus: the emitted binary must be valid wasm with the expected shape.
        let module = walrus::Module::from_buffer(&image).expect("emits valid wasm");

        // Memory grew to cover the bundle placed at the old end (2 pages → +1 for a small blob).
        let memory = module.memories.iter().next().expect("one memory");
        assert_eq!(memory.initial, 3);

        // The new segment sits at the old memory end and carries exactly the bundle.
        let expected_addr = 2 * PAGE as i32;
        let appended = module
            .data
            .iter()
            .find(|d| d.value == bundle)
            .expect("the bundle rides a data segment");
        match &appended.kind {
            walrus::DataKind::Active { offset, .. } => {
                // `ConstExpr` has no `PartialEq`; match the shape.
                assert!(matches!(
                    offset,
                    walrus::ConstExpr::Value(walrus::ir::Value::I32(addr)) if *addr == expected_addr
                ));
            }
            walrus::DataKind::Passive => panic!("the bundle segment must be active"),
        }

        // The slot's two u32s now point at it.
        let slot_segment = module
            .data
            .iter()
            .find(|d| find(&d.value, &WASM_SLOT_MAGIC).is_some())
            .expect("the slot survives the rewrite");
        let at = find(&slot_segment.value, &WASM_SLOT_MAGIC).unwrap() + WASM_SLOT_MAGIC.len();
        let ptr = u32::from_le_bytes(slot_segment.value[at..at + 4].try_into().unwrap());
        let len = u32::from_le_bytes(slot_segment.value[at + 4..at + 8].try_into().unwrap());
        assert_eq!(ptr, expected_addr as u32);
        assert_eq!(len, bundle.len() as u32);
    }

    #[test]
    fn untouched_sections_are_copied_byte_for_byte() {
        // Beyond shape-validity: everything except the memory/data(/data-count) sections must
        // survive verbatim — the point of the section-level walk over an IR round-trip.
        let runner = synthetic_runner(1);
        let image = staple_wasm(&runner, &bundle()).expect("staples");
        let before = split_sections(&runner).expect("input splits");
        let after = split_sections(&image).expect("output splits");
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(after.iter()) {
            assert_eq!(a.id, b.id, "section order preserved");
            if a.id != SEC_MEMORY && a.id != SEC_DATA && a.id != SEC_DATA_COUNT {
                assert_eq!(a.contents, b.contents, "section {} copied verbatim", a.id);
            }
        }
    }

    #[test]
    fn refuses_zero_or_several_slots_and_a_non_bundle() {
        let bundle = bundle();
        assert!(matches!(
            staple_wasm(&synthetic_runner(0), &bundle),
            Err(WasmStapleError::SlotMissing)
        ));
        assert!(matches!(
            staple_wasm(&synthetic_runner(2), &bundle),
            Err(WasmStapleError::SlotAmbiguous)
        ));
        assert!(matches!(
            staple_wasm(&synthetic_runner(1), b"not a bundle"),
            Err(WasmStapleError::NotABundle)
        ));
        assert!(matches!(
            staple_wasm(b"not wasm", &bundle),
            Err(WasmStapleError::Parse(_))
        ));
    }

    #[test]
    fn a_memory_maximum_below_the_needed_pages_is_refused() {
        let bundle = bundle();
        let mut module = walrus::Module::default();
        let memory = module.memories.add_local(false, false, 2, Some(2), None);
        let mut value = WASM_SLOT_MAGIC.to_vec();
        value.extend_from_slice(&[0u8; 8]);
        module.data.add(
            walrus::DataKind::Active {
                memory,
                offset: walrus::ConstExpr::Value(walrus::ir::Value::I32(0)),
            },
            value,
        );
        assert!(matches!(
            staple_wasm(&module.emit_wasm(), &bundle),
            Err(WasmStapleError::Memory(_))
        ));
    }

    #[test]
    fn leb_round_trips() {
        for value in [0u64, 1, 127, 128, 300, 65536, u64::from(u32::MAX)] {
            let mut buf = Vec::new();
            write_leb_u64(&mut buf, value);
            assert_eq!(read_leb_u64(&buf), Some((value, &[][..])));
        }
        for value in [0i64, 1, -1, 63, 64, -64, -65, 131072, -2147483648] {
            let mut buf = Vec::new();
            write_sleb_i64(&mut buf, value);
            assert_eq!(read_sleb_i64(&buf), Some((value, buf.len())));
        }
    }
}
