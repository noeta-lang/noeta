//! P-PACK 2.5+ — proof that a `list[i].field` read actually *fuses* into a single `Rvalue::IndexField`
//! when the checker marks it, rather than silently falling back to the unfused `Index` + `Field` pair.
//!
//! The conformance corpus + differential already prove the fused op is *correct* (it reads exactly
//! what the unfused pair would, on both packed and boxed lists). This test closes the one gap those
//! cannot see: that the recorded site span matches the span lowering looks up — a mismatch would leave
//! fusion silently disabled while every output stayed identical. We assert on the lowered IR's shape.

use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

/// Lower `src` with the checker's site maps and return the pretty-printed Core IR.
fn lowered_ir(src: &str) -> String {
    let source = Source::new(SourceId::FIRST, "fuse.lang", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let checked = lang_check::check_all(&parsed.program);
    assert!(
        checked.diagnostics.is_empty(),
        "program must check cleanly: {:?}",
        checked.diagnostics
    );
    let ir = lang_ir::lower_with_sites(
        &parsed.program,
        &checked.packed_list_sites,
        &checked.index_field_sites,
    )
    .expect("lowering is total over the parsed language");
    lang_ir::dump(&ir)
}

#[test]
fn packed_indexed_field_lowers_to_a_fused_read() {
    // The fused form pretty-prints as `recv[idx].field` on one line; the unfused pair would be a
    // separate `recv[idx]` then `.field`, so the `].x` substring appears only when fusion fired.
    let ir = lowered_ir(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         ps = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo ps[0].x\n",
    );
    assert!(
        ir.contains("].x"),
        "expected a fused `[i].field` read in the IR, got:\n{ir}"
    );
}

#[test]
fn boxed_struct_indexed_field_also_fuses() {
    let ir = lowered_ir(
        "struct P { x: int; y: int }\n\
         ps = [P { x: 1, y: 2 }]\n\
         echo ps[0].y\n",
    );
    assert!(
        ir.contains("].y"),
        "expected a fused `[i].field` read in the IR, got:\n{ir}"
    );
}

#[test]
fn without_the_site_map_the_read_stays_unfused() {
    // The boxed-default `lower` (empty site maps) never fuses — proving the fusion is driven entirely
    // by the checker channel, not the lowering shape.
    let source = Source::new(SourceId::FIRST, "fuse.lang", "xs = [1]\necho xs\n");
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let ir = lang_ir::dump(&lang_ir::lower(&parsed.program).expect("total"));
    assert!(
        !ir.contains("]."),
        "the empty-map path must not fuse:\n{ir}"
    );
}
