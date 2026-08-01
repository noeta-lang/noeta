//! Language **editions** — a per-package pin of the language/ABI semantics a package's source is
//! written against, declared `edition = "2026"` in its `[package]` table.
//!
//! Editions exist so a first-party-but-out-of-tree package can evolve on its own cadence yet stay
//! buildable by a newer toolchain: the toolchain keeps understanding every past edition and applies
//! *that* edition's rules to *that* package, so a language change that would otherwise be breaking
//! ships behind a new edition instead of splitting the ecosystem. This is the same contract as Rust
//! editions — opt-in, per-package, and never silently changing an existing package's meaning.
//!
//! **This is the lowest crate in the pipeline** — it depends on nothing, so every layer that must
//! name an edition (the lexer, the parser, the checker, the package manager) can depend on it
//! without pulling in the package-manager stack. `noeta-pm` re-exports it as `noeta_pm::edition`
//! for source compatibility with the resolution-side arc that introduced the type.
//!
//! **What the editions arc lands is the seam, not a divergence.** There is exactly one edition today
//! ([`Edition::E2026`]), so no two editions yet compile differently. What is real now: the value is
//! *validated* (an unknown edition is a manifest error, not a silently-accepted string), *pinned* in
//! `noeta.lock` for reproducibility, folded into the **startup-cache key**, and — as of the compiler
//! arc's S0 — *threaded into the front-end entry points* (`noeta_lexer::lex_in` /
//! `noeta_parser::parse_in`) so the day a future edition *does* change syntax or lints, the value
//! is already at the point that would consult it. The first edition-gated *behaviour* is a later,
//! separately-scoped slice; it reads the edition the toolchain already threads.
//!
//! **Granularity is per-package.** The data model records each resolved package's own edition
//! (manifest + lock + `DepPackage.edition`), so a dependency graph may mix editions. Applying each
//! package's own edition across the single merged program is the compiler arc's later work; this
//! crate is the shared vocabulary that work is written in.

use std::collections::HashMap;
use std::fmt;

use noeta_span::{SourceId, Span};

/// A pinned language edition. A closed set: the toolchain knows every edition it ever shipped, so an
/// unknown value in a manifest is a hard error (a typo or a package needing a newer toolchain), never
/// a silently-accepted free string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edition {
    /// The inaugural edition — today's language. Every package that omits `edition` is compiled
    /// under it, so introducing the field changes no existing package's meaning.
    E2026,
}

impl Edition {
    /// The edition an omitted `edition` key defaults to — the current language.
    pub const DEFAULT: Edition = Edition::E2026;

    /// The canonical string form, as written in `noeta.toml` / `noeta.lock` and mixed into the
    /// startup-cache key.
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::E2026 => "2026",
        }
    }

    /// Parse the `edition` value from a manifest. An unrecognised edition is rejected with a message
    /// listing the editions this toolchain understands — the actionable failure for a package pinned
    /// to a newer edition than the toolchain, or a plain typo.
    pub fn parse(value: &str) -> Result<Edition, String> {
        match value {
            "2026" => Ok(Edition::E2026),
            other => Err(format!(
                "`package.edition` `{other}` is not a language edition this toolchain understands \
                 (known: {}). Update the toolchain, or pin a known edition.",
                Edition::KNOWN
                    .iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Every edition this toolchain understands, oldest first — the closed set `parse` accepts and
    /// the error path enumerates.
    pub const KNOWN: &'static [Edition] = &[Edition::E2026];
}

impl Default for Edition {
    fn default() -> Self {
        Edition::DEFAULT
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which language [`Edition`] each source of a merged program was written against, keyed by
/// [`SourceId`].
///
/// The loader merges every package's modules into **one** `Program` compiled as a unit, so a
/// per-package edition cannot be a single scalar on the program — the merged statement list mixes
/// declarations from many sources. Each declaration's span already carries the `SourceId` of the
/// file it came from, so this side-table recovers "which edition governs this declaration?" from any
/// span, exactly as `noeta-loader`'s `SourceMap` recovers "which file does this span render
/// against?". This is the carrier the checker (and later, lowering) consults per declaration.
///
/// **An absent source is [`Edition::DEFAULT`].** A source not recorded — a single-file check, a
/// synthetic REPL fragment, the one-edition world — is governed by the default edition, so
/// [`Self::at`] never fails. An empty map therefore means "everything is the default edition", which
/// is exactly what a plain `check_all` / single-file `parse` wants.
/// **Where a merged program's sources came from — one value, never three.**
///
/// A linked program carries three facts about every source in it: which language [`Edition`] governs
/// it, which **package** wrote it, and which extension `@name`s that package binds. The loader and
/// the package manager resolve all three in one pass, from one dependency graph, and every rule that
/// consults one of them consults the others: the checker recovers a declaration's package from its
/// span, then reads *that* package's `@name` table; tier activation does the same.
///
/// They were three independent fields on [`CheckOptions`] until 2026-08-01, and the shape of that
/// mistake is worth keeping written down, because it cost four surfaces a wrong answer each and one
/// of them was inside this crate. Empty `packages` means "provenance unknown", and every provenance
/// rule correctly stands down — the right answer for a lone file. Empty `uses` does **not** mean
/// unknown: it means *no package binds any `@name`*, so a project that renames a directive
/// (`[directives] gen = "para:openapi"`) sees it resolve to nothing and gets a spurious `E0036` on
/// code the compiler accepts. A caller that knows enough to fill in `packages` has `uses` in hand
/// from the very same resolve, so half-supplying was never a decision anyone made — it was
/// `..Default::default()` answering a question nobody asked.
///
/// Making the triple one value removes the choice. There is no `Default`, for the same reason
/// `LowerOptions` has none: an unattributed program is a *decision* ([`Provenance::unattributed`]),
/// not a fallback.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Provenance {
    /// Which language [`Edition`] governs each source, keyed by `SourceId` (editions arc). Empty
    /// means every declaration is at [`Edition::DEFAULT`].
    pub editions: EditionMap,
    /// Which **package** each source was read from, keyed by `SourceId` (the package orphan rule).
    /// Empty means provenance is unknown everywhere and the orphan rule stands down — the right
    /// answer for a single-file check or a synthetic program, neither of which has a package graph
    /// to judge against.
    pub packages: noeta_span::PackageMap,
    /// Per-package `@`-name resolution tables (`[directives]`, `[tiers]`), keyed by
    /// [`PackageOrigin`](noeta_span::PackageOrigin): the loader/pm builds them from each package's
    /// manifest in that package's own dependency context. Empty means no package binds any extension
    /// `@name`, so only built-in directives and program-declared tiers resolve.
    pub uses: noeta_span::PackageUses,
}

impl Provenance {
    /// A program with no package graph and no per-source editions: a lone file, a synthetic program
    /// the CLI assembled, a REPL entry. Every provenance rule stands down and only built-in
    /// directives and program-declared tiers resolve — which is correct here, because there is no
    /// manifest that could have said otherwise.
    pub fn unattributed() -> Provenance {
        Provenance {
            editions: EditionMap::default(),
            packages: noeta_span::PackageMap::default(),
            uses: noeta_span::PackageUses::default(),
        }
    }

    /// Per-source editions, still no package graph — a multi-source program linked without a
    /// manifest (the salsa single-document queries).
    pub fn of_sources(editions: EditionMap) -> Provenance {
        Provenance {
            editions,
            ..Provenance::unattributed()
        }
    }

    /// The full triple, as the loader resolved it for a linked workspace program. The three
    /// arguments have three distinct types, so they cannot be transposed.
    pub fn of(
        editions: EditionMap,
        packages: noeta_span::PackageMap,
        uses: noeta_span::PackageUses,
    ) -> Provenance {
        Provenance {
            editions,
            packages,
            uses,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditionMap {
    by_source: HashMap<SourceId, Edition>,
}

impl EditionMap {
    /// An empty map — every source resolves to [`Edition::DEFAULT`] via [`Self::at`].
    pub fn new() -> EditionMap {
        EditionMap::default()
    }

    /// Record that the source `id` is written against `edition`.
    pub fn set(&mut self, id: SourceId, edition: Edition) {
        self.by_source.insert(id, edition);
    }

    /// The edition governing the source `id` — its recorded edition, or [`Edition::DEFAULT`] if the
    /// source was never recorded (a single-file/synthetic source, or the one-edition world).
    pub fn source_edition(&self, id: SourceId) -> Edition {
        self.by_source.get(&id).copied().unwrap_or(Edition::DEFAULT)
    }

    /// The edition governing the declaration a `span` belongs to — [`Self::source_edition`] of the
    /// span's owning source. The per-span lookup the checker uses to apply each declaration's own
    /// edition across a merged program.
    pub fn at(&self, span: Span) -> Edition {
        self.source_edition(span.source)
    }

    /// Whether nothing has been recorded — i.e. every source is the default edition.
    pub fn is_empty(&self) -> bool {
        self.by_source.is_empty()
    }

    /// How many sources have a recorded edition.
    pub fn len(&self) -> usize {
        self.by_source.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_known_edition() {
        assert_eq!(Edition::parse("2026").unwrap(), Edition::E2026);
    }

    #[test]
    fn rejects_an_unknown_edition_actionably() {
        let err = Edition::parse("2030").unwrap_err();
        assert!(err.contains("2030"), "names the offending value");
        assert!(err.contains("2026"), "enumerates the known editions");
    }

    #[test]
    fn default_is_the_inaugural_edition() {
        assert_eq!(Edition::default(), Edition::E2026);
        assert_eq!(Edition::DEFAULT.as_str(), "2026");
    }

    #[test]
    fn round_trips_through_its_string_form() {
        for &e in Edition::KNOWN {
            assert_eq!(Edition::parse(e.as_str()).unwrap(), e);
        }
    }

    #[test]
    fn edition_map_defaults_an_unrecorded_source() {
        let map = EditionMap::new();
        assert!(map.is_empty());
        // An empty map governs every source with the default edition — the one-edition world.
        assert_eq!(map.source_edition(SourceId(0)), Edition::DEFAULT);
        assert_eq!(map.source_edition(SourceId(7)), Edition::DEFAULT);
        assert_eq!(map.at(Span::new_in(SourceId(3), 0, 1)), Edition::DEFAULT);
    }

    #[test]
    fn edition_map_recovers_a_recorded_source_and_span() {
        let mut map = EditionMap::new();
        map.set(SourceId(0), Edition::E2026);
        map.set(SourceId(2), Edition::E2026);
        assert_eq!(map.len(), 2);
        assert!(!map.is_empty());
        // A recorded source resolves to its edition; a span resolves via its owning `SourceId`.
        assert_eq!(map.source_edition(SourceId(0)), Edition::E2026);
        assert_eq!(map.at(Span::new_in(SourceId(2), 4, 8)), Edition::E2026);
        // An unrecorded source still falls back to the default.
        assert_eq!(map.source_edition(SourceId(9)), Edition::DEFAULT);
    }
}
