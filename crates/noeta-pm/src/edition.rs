//! Language **editions** (follow-on arc F1) — a per-package pin of the language/ABI semantics a
//! package's source is written against, declared `edition = "2026"` in its `[package]` table.
//!
//! Editions exist so a first-party-but-out-of-tree package can evolve on its own cadence yet stay
//! buildable by a newer toolchain: the toolchain keeps understanding every past edition and applies
//! *that* edition's rules to *that* package, so a language change that would otherwise be breaking
//! ships behind a new edition instead of splitting the ecosystem. This is the same contract as Rust
//! editions — opt-in, per-package, and never silently changing an existing package's meaning.
//!
//! **What this arc lands is the seam, not a divergence.** There is exactly one edition today
//! ([`Edition::E2026`]), so no two editions yet compile differently. What is real now: the value is
//! *validated* (an unknown edition is a manifest error, not a silently-accepted string), *pinned* in
//! `noeta.lock` for reproducibility, and folded into the **startup-cache key** — so the moment a
//! future edition *does* change compilation, switching a package's edition already invalidates its
//! cached bytecode rather than serving a stale artifact. The first edition-gated *behaviour* is a
//! later, separately-scoped change; it reads the edition the toolchain already threads here.
//!
//! **Granularity is per-package.** The data model records each resolved package's own edition
//! (manifest + lock), so a dependency graph may mix editions. The compilation *unit* the front-end
//! currently consumes is the merged program, whose edition is the **root** package's (a merged
//! program has no per-declaration edition switch yet); per-declaration divergence within one
//! compilation is the future refinement the per-package model leaves room for.

use std::fmt;

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
}
