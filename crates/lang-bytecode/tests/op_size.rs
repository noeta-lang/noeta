//! Structural guard for `size_of::<Op>()` (P-VMT-OPSZ, S4).
//!
//! The bytecode stream is large and cache-hostile, so every byte of an `Op` is streamed through
//! the VM's dispatch loop. Before S4 an `Op` was 128 bytes — two cache lines per instruction —
//! inflated by inline `String` names (interned to `NameId` in S4.1) and wide inline payloads
//! (`TypeRecipe`/`TypeRepr`/`NarrowTarget`, boxed in S4.2). This test pins the win: an `Op` must
//! stay within a single 64-byte cache line. Tighten the bound if a future slice shrinks it further;
//! a regression past 64 means an instruction once again straddles two lines and should be caught.

use lang_bytecode::Op;
use std::mem::size_of;

#[test]
fn op_fits_in_one_cache_line() {
    let size = size_of::<Op>();
    assert!(
        size <= 64,
        "size_of::<Op>() = {size} B; it must stay within one 64-byte cache line (P-VMT-OPSZ). \
         A new or widened variant regressed it — intern names to `NameId` or box the wide payload."
    );
}
