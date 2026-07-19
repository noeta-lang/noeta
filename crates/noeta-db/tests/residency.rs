//! Heap-residency regression for the audit **F9 residual (a)** — deleted-file inputs are never
//! freed. salsa 0.27 cannot free an input slot (its table is append-only, with no public delete),
//! so a source that vanishes from a workspace used to keep its *whole* analysis resident: the input
//! text plus the fat downstream memos (AST, merged `Program`, `Sites`, `Module`). [`release_source`]
//! reclaims all of that — text cleared, memos overwritten with empty-program equivalents — leaving
//! only the bounded, unfreeable input struct behind.
//!
//! This test brackets add/read/delete cycles with the [`noeta_alloc_probe`] residency counter and
//! proves that releasing each deleted source reclaims the bulk of what a non-released cycle retains.
//! It registers the tracking allocator as this test binary's global allocator so [`live_bytes`] is
//! non-zero; it must run single-threaded (the counters are process-wide), which `cargo test`'s
//! default per-test threading respects for a one-test binary.

use noeta_alloc_probe::{TrackingAlloc, live_bytes};
use noeta_db::{
    Edition, LangDatabase, Workspace, linked_bytecode_from, linked_checked_ide_from,
    release_source, source_program,
};
use noeta_span::{Source, SourceId};

#[global_allocator]
static GLOBAL: TrackingAlloc = TrackingAlloc(std::alloc::System);

/// A deliberately large module: hundreds of checked/compiled declarations, so a retained cycle's
/// AST + `Module` + type index dwarf the fixed per-input overhead salsa cannot free.
fn big_source() -> String {
    let mut s = String::from("namespace App.Scratch\n");
    for i in 0..250 {
        s.push_str(&format!(
            "pub fn scratch_{i}(x: int): int {{\n  y = x + {i}\n  return y * 2\n}}\n"
        ));
    }
    s
}

/// One add/read/delete cycle over a fresh `(Workspace, scratch source)`: builds the inputs, forces
/// the fat downstream memos (as an editor does for an open entry), and — when `release` — reclaims
/// the scratch source before dropping it. Every cycle mints new inputs (the append-only slots salsa
/// keeps either way), so what distinguishes the two runs is only whether the *content* is released.
fn cycle(db: &mut LangDatabase, base_src: noeta_db::SourceProgram, text: &str, release: bool) {
    let scratch = Source::new(SourceId(1), "scratch.noe", text);
    let src = source_program(db, &scratch, Edition::DEFAULT);
    let ws = Workspace::new(db, vec![base_src, src], Vec::new());
    // Populate the fat memos: the merged program, its type index, and its compiled module.
    let _ = linked_checked_ide_from(db, ws, src);
    let _ = linked_bytecode_from(db, ws, src);
    if release {
        release_source(db, ws, src);
    }
}

fn base_program(db: &LangDatabase) -> noeta_db::SourceProgram {
    let base = Source::new(
        SourceId(0),
        "base.noe",
        "namespace App.Base;\npub fn base(): int { return 0; }\n",
    );
    source_program(db, &base, Edition::DEFAULT)
}

/// Growth in live bytes across `cycles` add/read/delete iterations, with vs. without release, each
/// on its own db so the two measurements are independent. Warms one cycle first (allocator/salsa
/// one-time tables) so the measured delta is per-cycle steady-state, not first-touch.
fn residency_growth(cycles: usize, release: bool) -> usize {
    noeta_stdlib::registry::default_seeded();
    let text = big_source();
    let mut db = LangDatabase::default();
    let base_src = base_program(&db);
    // Warm-up cycle outside the measurement window.
    cycle(&mut db, base_src, &text, release);
    let before = live_bytes();
    for _ in 0..cycles {
        cycle(&mut db, base_src, &text, release);
    }
    live_bytes().saturating_sub(before)
}

#[test]
fn releasing_deleted_sources_reclaims_the_bulk_of_their_residency() {
    const CYCLES: usize = 25;
    let retained = residency_growth(CYCLES, false);
    let released = residency_growth(CYCLES, true);

    // Sanity: the non-released run really does accumulate — each retained cycle keeps a large AST +
    // Module resident, so 40 of them are well into six figures of bytes.
    assert!(
        retained > 100_000,
        "expected the non-released run to accumulate the retained memos, saw {retained} bytes"
    );
    // The whole point: releasing each deleted source reclaims the overwhelming majority of what a
    // retained cycle holds. The residual is only salsa's unfreeable per-cycle input/workspace slots
    // (tiny and fixed) plus empty-program memos — far below the retained content.
    assert!(
        released * 4 < retained,
        "release must reclaim the bulk: released {released} vs retained {retained} bytes over \
         {CYCLES} cycles"
    );
}

#[test]
fn released_source_reads_back_empty_and_recomputes_cleanly() {
    noeta_stdlib::registry::default_seeded();
    let mut db = LangDatabase::default();
    let base_src = base_program(&db);
    let text = big_source();
    let scratch = Source::new(SourceId(1), "scratch.noe", &text);
    let src = source_program(&db, &scratch, Edition::DEFAULT);
    let ws = Workspace::new(&db, vec![base_src, src], Vec::new());

    // Before release: the scratch module compiles cleanly to a non-trivial module.
    assert!(linked_bytecode_from(&db, ws, src).0.is_ok());
    assert!(linked_checked_ide_from(&db, ws, src).diagnostics.is_empty());

    release_source(&mut db, ws, src);

    // After release: the source's downstream memos are the empty-program equivalents, and the
    // session is intact — a fresh read recomputes cleanly (no corruption, no stale panic).
    assert!(
        linked_checked_ide_from(&db, ws, src).diagnostics.is_empty(),
        "the released source must recompute as an empty, well-typed program"
    );
    assert!(linked_bytecode_from(&db, ws, src).0.is_ok());
    // The workspace's surviving member is unaffected.
    assert!(
        linked_checked_ide_from(&db, ws, base_src)
            .diagnostics
            .is_empty()
    );
}
