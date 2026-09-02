//! Structural guard for `size_of::<Op>()`.
//!
//! The bytecode stream is large and cache-hostile, so every byte of an `Op` is streamed through
//! the VM's dispatch loop. An `Op` must stay within a single 64-byte cache line, which is what
//! keeps instruction names interned to `NameId` and the wide payloads
//! (`TypeRecipe`/`TypeRepr`/`NarrowTarget`) boxed rather than inline. Tighten the bound if a
//! change shrinks it further; a regression past 64 means an instruction straddles two lines.

use noeta_bytecode::Op;
use std::mem::size_of;

#[test]
fn op_fits_in_one_cache_line() {
    let size = size_of::<Op>();
    assert!(
        size <= 64,
        "size_of::<Op>() = {size} B; it must stay within one 64-byte cache line. \
         A new or widened variant regressed it — intern names to `NameId` or box the wide payload."
    );
}
