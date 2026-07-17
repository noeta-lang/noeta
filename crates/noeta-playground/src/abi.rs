//! The wasm export surface — a hand-rolled `(ptr, len)` C ABI over the JSON core.
//!
//! Deliberately **no wasm-bindgen**: three string→string exports do not justify a codegen tool
//! whose CLI must stay in version lock-step with the crate. The embedder's glue is ~30 lines of
//! JS (see `web/playground/`): write UTF-8 source into a buffer from [`noeta_alloc`], call an
//! entry point, read the **length-prefixed** result (`[len: u32 LE][bytes]`), then release it
//! with [`noeta_free_result`].
//!
//! This module is the crate's whole `unsafe` surface (with the getrandom hook below), and it is
//! exactly the seam miri cannot cover — the caller is JavaScript. The wasm differential/browser
//! smoke tests gate it instead; the JSON core underneath is safe and natively unit-tested.

#![allow(unsafe_code)]

/// Allocate `len` bytes for the caller to write source text into. Released by the entry point
/// that consumes it (pass the same `len`).
#[unsafe(no_mangle)]
pub extern "C" fn noeta_alloc(len: usize) -> *mut u8 {
    let mut buf = vec![0u8; len.max(1)];
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Release a buffer from [`noeta_alloc`] without consuming it (an embedder abandoning a call).
///
/// # Safety
/// `ptr`/`len` must be exactly a live [`noeta_alloc`] allocation, not yet released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_dealloc(ptr: *mut u8, len: usize) {
    unsafe { drop(Vec::from_raw_parts(ptr, len.max(1), len.max(1))) }
}

/// [`crate::check_source`] over the ABI: consumes the input buffer, returns a length-prefixed
/// JSON buffer (release with [`noeta_free_result`]).
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_check(ptr: *mut u8, len: usize) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::check_source(&text))
}

/// [`crate::run_source`] over the ABI. Safety: as [`noeta_check`].
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_run(ptr: *mut u8, len: usize) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::run_source(&text))
}

/// [`crate::fmt_source`] over the ABI. Safety: as [`noeta_check`].
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_fmt(ptr: *mut u8, len: usize) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::fmt_source(&text))
}

/// [`crate::debug_source`] over the ABI — the debug run (W2.4). Unlike the plain entries the
/// input is a JSON **request** (`{"source", "breakpoints": [line…], "stop_on_entry"}`), and the
/// embedder MUST supply the `js_debug_pause` import (module `noeta_host`): every pause calls it
/// with the captured-stack JSON and blocks until it returns the resume-command buffer.
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_debug_run(ptr: *mut u8, len: usize) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::debug_source(&text))
}

/// [`crate::run_source_browser`] over the ABI — the "real host" run (W3.0). Requires the
/// embedder to supply the `noeta_host` imports at instantiation. Safety: as [`noeta_check`].
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_run_browser(ptr: *mut u8, len: usize) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::run_source_browser(&text))
}

/// [`crate::run_source_browser_async`] over the ABI — the JSPI-pumped run (W3.1). The embedder
/// MUST have wrapped the `noeta_host` imports with `WebAssembly.Suspending` and this export with
/// `WebAssembly.promising`; calling it plainly would trap at the first suspension.
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_run_browser_async(ptr: *mut u8, len: usize) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::run_source_browser_async(&text))
}

/// [`crate::hover_source`] over the ABI: `line`/`character` are a zero-based UTF-16 position
/// (the LSP convention — see `ide.rs`).
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_hover(
    ptr: *mut u8,
    len: usize,
    line: u32,
    character: u32,
) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::hover_source(&text, line, character))
}

/// [`crate::definition_source`] over the ABI. Safety and position convention: as [`noeta_hover`].
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_definition(
    ptr: *mut u8,
    len: usize,
    line: u32,
    character: u32,
) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::definition_source(&text, line, character))
}

/// [`crate::complete_source`] over the ABI. Safety and position convention: as [`noeta_hover`].
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_complete(
    ptr: *mut u8,
    len: usize,
    line: u32,
    character: u32,
) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::complete_source(&text, line, character))
}

/// [`crate::signature_source`] over the ABI. Safety and position convention: as [`noeta_hover`].
///
/// # Safety
/// `ptr`/`len` must be a live [`noeta_alloc`] allocation holding UTF-8 (lossily decoded if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_signature(
    ptr: *mut u8,
    len: usize,
    line: u32,
    character: u32,
) -> *mut u8 {
    let text = unsafe { take_input(ptr, len) };
    pack(crate::signature_source(&text, line, character))
}

/// Release a result buffer handed out by an entry point.
///
/// # Safety
/// `ptr` must be exactly a live result from [`noeta_check`]/[`noeta_run`]/[`noeta_fmt`], not yet
/// released (the length prefix is read back to reconstruct the allocation).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noeta_free_result(ptr: *mut u8) {
    unsafe {
        let mut len_bytes = [0u8; 4];
        std::ptr::copy_nonoverlapping(ptr, len_bytes.as_mut_ptr(), 4);
        let len = u32::from_le_bytes(len_bytes) as usize;
        drop(Vec::from_raw_parts(ptr, 4 + len, 4 + len));
    }
}

/// Reclaim and decode the caller's input buffer (consuming the [`noeta_alloc`] allocation).
unsafe fn take_input(ptr: *mut u8, len: usize) -> String {
    let buf = unsafe { Vec::from_raw_parts(ptr, len.max(1), len.max(1)) };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Encode a result as `[len: u32 LE][bytes]` in one exact-size allocation.
fn pack(s: String) -> *mut u8 {
    let bytes = s.into_bytes();
    let mut buf = Vec::with_capacity(4 + bytes.len());
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&bytes);
    debug_assert_eq!(buf.len(), buf.capacity());
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// getrandom's `custom` backend hook (the workspace builds wasm32-unknown-unknown with
/// `--cfg getrandom_backend="custom"`, see `.cargo/config.toml`). Honestly unsupported: the only
/// getrandom consumer in the tree is `bcrypt`'s salt path, which is unreachable — salts arrive
/// as arguments drawn from the Host `Entropy` capability, and the playground's `SandboxHost`
/// provides deterministic entropy. If a future dependency ever reaches this, it fails loudly
/// instead of silently handing out a guessable stream.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[unsafe(no_mangle)]
extern "Rust" fn __getrandom_v03_custom(
    _dest: *mut u8,
    _len: usize,
) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}
