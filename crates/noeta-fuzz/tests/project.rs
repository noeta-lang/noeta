//! The two front-ends, fuzzed against each other: `noeta check` and `noeta run` must agree on
//! whether a project is statically acceptable.
//!
//! # The gap this closes
//!
//! `noeta check` does not share a front-end with `noeta run`. It goes through
//! `noeta_project::project_check` — the same entry the LSP's `workspace/diagnostic` and the MCP `check`
//! tool use — which drives the **salsa** workspace (`noeta-db`); `run` goes through the loader's
//! compile front-end. Every other oracle in this crate (`run`, `typed`, `jit`, `leaks`, `bundle`,
//! `fmt`) drives the second one only, so the first had no sweep at all.
//!
//! That is not a hypothetical. Four defects were found and fixed by hand on the check path, all
//! invisible to 250,000+ generated *programs*, and three of them were literally "check says X, run
//! says Y" — a re-derivation that never learned the migrations/seeds exception, a linker gate
//! narrower than the pass it guarded, and a `file:` URI where a path belonged. Each is a fact about
//! the project's **layout**, which is why a generator that emits one buffer of source could not
//! reach them, and why the input here is a package on disk.
//!
//! [`noeta_fuzz::project_target`] documents the invariant and what it deliberately does not
//! assert.
//!
//! # The floors, and why they are assertions
//!
//! The invariant is a boolean equality, so a sweep in which every project came out the same way has
//! proved one implication and skipped the other — and that looks exactly like a clean sweep. Both
//! [`MIN_ACCEPTED`] and [`MIN_REJECTED`] are therefore asserted rather than printed, as is the
//! per-layout materialization: a [`Layout`] no project takes is a seam no project covers, and the
//! three fixed defects each lived in exactly one of them.
//!
//! # Coverage, stated rather than assumed
//!
//! [`NONCES`] is what runs in the gate, not the limit of the technique:
//! `cargo run --release -p noeta-fuzz --example projectscan -- scan 20000` runs the identical
//! oracle over as many projects as you care to wait for, and is what should be run after touching
//! `noeta-project`, `noeta-db`, the loader's package walk, or module-path derivation. The first 36-
//! project probe this target ever ran found a live divergence — `noeta check` reporting every entry
//! of a migrations-only package unreadable — which was carried here as an argued exception until
//! `noeta-ide` was fixed, at which point the exception's own from-below assertion failed and it was
//! deleted.

use std::collections::BTreeMap;

use noeta_fuzz::project_target::{self as target, Layout, Reach};

/// The front end recurses over nesting the default ~2 MiB test-thread stack cannot hold, exactly as
/// in the run and formatter suites.
const DEEP_STACK: usize = 64 * 1024 * 1024;

/// Projects swept by the gate — 100 per [`Layout`].
///
/// Measured at 16.6 s for 1,800 projects in an unoptimized build on this repository's reference
/// box, so this is ~8 s: the same order as the neighbouring oracles' sweeps, and the cost is real
/// filesystem work (a package is written, walked twice, and deleted) rather than the per-program
/// arithmetic they pay. A deeper run is one command away (see the module docs), which is why this
/// is a floor and not a claim about how much is enough.
const NONCES: u32 = 900;

/// The floor on projects **both** front-ends accepted. Measured at 334 of 900 (~37%), so this is
/// set well below that: it is here to catch the sweep going hollow, not to pin a rate.
///
/// Without it the "`check` clean ⇒ the run side is clean" half is vacuous — every project could be
/// refused by both for an ordinary typing reason and the suite would stay green while proving
/// nothing about the direction the worst defects take.
///
/// Raised from 180 when the `DataDirOnly` divergence was fixed. That is not a tune: those 50
/// projects were always accepted by the *run* side and are now accepted by both, so leaving the
/// floor where it was would have handed the detector 50 projects of new slack it did not earn — the
/// same margin it had before (≈63% of the measured rate) is 211, and 200 is the round number below
/// it. The rejected half did not move, so [`MIN_REJECTED`] does not.
const MIN_ACCEPTED: u32 = 200;

/// The floor on projects both front-ends refused. Measured at ~63%. The mirror of
/// [`MIN_ACCEPTED`]: without it the "the run side clean ⇒ `check` is clean" half is the vacuous one.
const MIN_REJECTED: u32 = 400;

fn on_deep_stack<R: Send>(body: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, body)
            .expect("spawn deep-stack worker")
            .join()
            .expect("deep-stack worker panicked")
    })
}

/// What one layout's projects came out as.
#[derive(Default, Clone, Copy)]
struct Row {
    accepted: u32,
    rejected: u32,
}

impl Row {
    fn total(self) -> u32 {
        self.accepted + self.rejected
    }
}

#[test]
fn check_and_run_agree_on_whether_a_generated_project_is_acceptable() {
    on_deep_stack(|| {
        let mut rows: BTreeMap<Layout, Row> = BTreeMap::new();
        for nonce in 0..NONCES {
            // A per-process fixture root. Never a fixed name under the system temp dir: this
            // repository is routinely worked in several worktrees at once, and a shared fixture
            // path is deleted out from under a concurrent run.
            let dir = noeta_test_temp::TempDir::new("project-oracle");
            let layout = target::materialize(&dir, target::BASE_SEED, nonce);
            let evaluated = match target::evaluate_total(&dir) {
                Ok(evaluated) => evaluated,
                Err(violation) => panic!(
                    "nonce {nonce} [{layout:?}]: {violation}\n\
                     replay: cargo run --release -p noeta-fuzz --example projectscan -- show {nonce}\n\
                     --- files ---\n{}",
                    describe(&dir)
                ),
            };
            // Two preconditions, asserted per project rather than trusted. The first: `check`
            // sweeps one shape per code tier an entry's blocks name and the run side compiles the
            // shipping shape only, so a generator arm that started emitting `@tier { … }` would
            // make the two entry-shape sets differ and every disagreement ambiguous.
            assert!(
                evaluated.tiers_checked.is_empty(),
                "nonce {nonce} [{layout:?}]: the project declares code tiers {:?}. `check` sweeps a \
                 shape per tier that the run side never compiles, so the accept/reject comparison \
                 is no longer between the same two programs — fix the generator rather than the \
                 assertion.",
                evaluated.tiers_checked
            );
            // The second: the check side must actually have looked at something. A project it
            // reports checking zero files of agrees with everything.
            assert!(
                evaluated.files_checked > 0,
                "nonce {nonce} [{layout:?}]: `project_check` reported checking 0 files, so its \
                 verdict is about nothing.\n--- files ---\n{}",
                describe(&dir)
            );
            let row = rows.entry(layout).or_default();
            match evaluated.reach {
                Reach::Accepted => row.accepted += 1,
                Reach::Rejected => row.rejected += 1,
            }
        }

        let total = |pick: fn(Row) -> u32| rows.values().copied().map(pick).sum::<u32>();
        let accepted = total(|r| r.accepted);
        let rejected = total(|r| r.rejected);
        eprintln!(
            "project oracle: {accepted} accepted by both, {rejected} refused by both, \
             of {NONCES} projects"
        );
        // Every project reaches both sides (each is asked in full) and the two agreed on all of
        // them, so `accepted` is what BOTH front-ends accepted — there is no longer a column in
        // which one of them alone did.
        for layout in target::LAYOUTS {
            let row = rows.get(layout).copied().unwrap_or_default();
            eprintln!(
                "  {layout:?}: {} accepted, {} rejected",
                row.accepted, row.rejected
            );
        }

        // Every layout materialized, in the count the cycle promises. A layout that silently
        // stopped being produced — a `materialize` arm that writes nothing, a walk that stops
        // finding its files — takes a whole seam out of the sweep, and the three fixed defects each
        // lived in exactly one seam.
        let per_layout = NONCES / target::LAYOUTS.len() as u32;
        for layout in target::LAYOUTS {
            let row = rows.get(layout).copied().unwrap_or_default();
            assert_eq!(
                row.total(),
                per_layout,
                "{layout:?} produced {} projects, not the {per_layout} the layout cycle promises — \
                 a seam has dropped out of the sweep",
                row.total()
            );
        }

        assert!(
            accepted >= MIN_ACCEPTED,
            "only {accepted} of {NONCES} generated projects were accepted by both front-ends \
             (floor {MIN_ACCEPTED}). The 'check clean ⇒ run clean' half of this oracle is \
             conditioned on a project `check` accepts, so this sweep proved almost nothing in the \
             direction the worst defects take. Something upstream is refusing projects it used to \
             accept."
        );
        assert!(
            rejected >= MIN_REJECTED,
            "only {rejected} of {NONCES} generated projects were refused by both front-ends \
             (floor {MIN_REJECTED}). The 'run clean ⇒ check clean' half is then the vacuous one — \
             a sweep that only ever sees acceptance cannot see a check that stopped refusing."
        );

        // The shape the module-path refusal lives in must actually reach the refusal. This is the
        // anti-hollowing assertion with the sharpest history behind it: the salsa linker once
        // skipped its whole derivation pass on a workspace with no legally-derived member, so
        // `check` accepted a package whose only module is `src/my-utils.noe` while `run` refused
        // it. If that stops being refused *by both*, either the defect is back (the sweep says so
        // above) or this layout has stopped producing the shape that holds it.
        let unspellable = rows
            .get(&Layout::UnspellableOnly)
            .copied()
            .unwrap_or_default();
        assert_eq!(
            unspellable.rejected, per_layout,
            "{} of {per_layout} `UnspellableOnly` projects were refused by both front-ends. A \
             package whose only module has a name no `use` can spell is refused by construction, \
             so anything else means this layout is no longer building that shape.",
            unspellable.rejected
        );
    });
}

/// The generator's own health check, in the shape `tests/generator.rs` asserts the parse rate: the
/// layouts a sweep believes it is covering are the layouts that appear on disk.
///
/// A `materialize` arm that wrote its files to the wrong place, or a walk that stopped descending
/// into subdirectories, would not fail loudly — the sweep would keep passing over projects that no
/// longer hold the shape their name claims. This fails instead.
#[test]
fn every_layout_materializes_the_shape_its_name_claims() {
    for (index, layout) in target::LAYOUTS.iter().enumerate() {
        let nonce = index as u32;
        assert_eq!(
            target::layout_for(nonce),
            *layout,
            "the layout cycle does not start at nonce 0"
        );
        let dir = noeta_test_temp::TempDir::new("project-layouts");
        assert_eq!(target::materialize(&dir, target::BASE_SEED, nonce), *layout);
        let entries = target::entries(&dir);
        assert!(
            !entries.is_empty(),
            "{layout:?} materialized no `.noe` file at all"
        );
        let names: Vec<String> = entries
            .iter()
            .map(|p| {
                p.strip_prefix(&*dir)
                    .unwrap_or(p)
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();
        let manifest = dir.join("noeta.toml").is_file();
        let expected_manifest = !matches!(layout, Layout::Script | Layout::ScriptPair);
        assert_eq!(
            manifest, expected_manifest,
            "{layout:?}: manifest present = {manifest}, expected {expected_manifest}"
        );
        // The distinguishing file of each layout — the one that makes it a different seam from its
        // neighbours. Spelled out here rather than derived from `materialize`, so a change to that
        // function has to come past this list.
        let required: &[&str] = match layout {
            Layout::Script => &["main.noe"],
            Layout::ScriptPair => &["main.noe", "helper.noe"],
            Layout::PackageSrc => &["src/main.noe"],
            Layout::PackageFlat => &["main.noe"],
            Layout::PackageNested => &["src/main.noe", "src/deep/helper.noe"],
            Layout::DataDir => &["src/main.noe", "migrations/20260719000002_more_users.noe"],
            Layout::DataDirOnly => &["migrations/20260719000002_more_users.noe"],
            Layout::UnspellableOnly => &["src/my-utils.noe"],
            Layout::UnspellableSibling => &["src/main.noe", "src/my-utils.noe"],
        };
        for want in required {
            assert!(
                names.iter().any(|n| n == want),
                "{layout:?} is missing {want}; it materialized {names:?}"
            );
        }
        assert_eq!(
            names.len(),
            required.len(),
            "{layout:?} materialized {names:?}, but its shape is {required:?} — an extra file \
             changes which pool the entries group into, and therefore what the sweep is testing"
        );
    }
}

/// Every file of a failing project, so a regression report is a paste-ready reproduction rather
/// than a path into a directory the test already deleted.
fn describe(root: &std::path::Path) -> String {
    let mut out = String::new();
    if let Ok(manifest) = std::fs::read_to_string(root.join("noeta.toml")) {
        out.push_str(&format!("--- noeta.toml ---\n{manifest}"));
    }
    for entry in target::entries(root) {
        let name = entry.strip_prefix(root).unwrap_or(&entry).display();
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        out.push_str(&format!("--- {name} ---\n{text}"));
    }
    out
}
