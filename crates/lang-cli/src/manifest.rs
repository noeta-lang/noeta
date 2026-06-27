//! The project manifest (`lang.toml`) — build **profiles** (object-model slice 6g).
//!
//! A *profile* names which dev-tiers are live in a build and which package provides each — the
//! Cargo-profile / MSBuild-configuration axis. A `--profile` selects a tier set; the front-end tier
//! filter (`lang run`) and the tier runners (`lang test`/`bench`/`doc`) consume that resolved
//! *active-tier set*, not caring whether it came from a profile, a `--tier` flag, or a default.
//!
//! ```toml
//! [profiles.dev.tiers]
//! test  = "std"                 # provider = the built-in stdlib tier
//! bench = { package = "std" }   # table form (room for profile-level options later)
//! debug = "std"
//!
//! [profiles.ci]
//! extends = "dev"               # inherit dev's tiers…
//! [profiles.ci.tiers]
//! debug = "std"                 # …and override / add
//! ```
//!
//! The provider-string grammar is parsed and validated **now** so the manifest shape is locked, but
//! the only provider available before the package system is the built-in `"std"` — any other
//! provider is an error (cross-package resolution lands with packages). A profile's *active tiers*
//! are the tier names in its (inheritance-merged) map.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lang_check::BUILTIN_TIERS;

/// The manifest file name, discovered at or above the entry file's directory.
pub const MANIFEST_NAME: &str = "lang.toml";

/// The sole tier provider available before the package system: the built-in/stdlib tiers. The
/// provider-string grammar accepts only this; naming any other package is an error until packages
/// (and their dependency resolution) exist.
const BUILTIN_PROVIDER: &str = "std";

/// A parsed `lang.toml`: its build profiles, each a (possibly inheriting) tier provider-map.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, PartialEq)]
struct Profile {
    /// The base profile this one inherits tiers from (`extends = "dev"`), if any.
    extends: Option<String>,
    /// This profile's own tier → provider entries (overlaid on the base's during resolution).
    tiers: BTreeMap<String, String>,
}

/// Discover the nearest `lang.toml` at or above `start_dir`, walking up to the filesystem root.
pub fn find(start_dir: &Path) -> Option<PathBuf> {
    for dir in start_dir.ancestors() {
        let candidate = dir.join(MANIFEST_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the active-tier set for `profile` from the `lang.toml` discovered at or above `entry`'s
/// directory: load the manifest, follow `extends`, and return the live tier names (sorted). Every
/// failure — no manifest, parse error, unknown profile/tier, unavailable provider, inheritance
/// cycle — is a human-readable `Err` the caller prints.
pub fn resolve_active_tiers(entry: &Path, profile: &str) -> Result<Vec<String>, String> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let path = find(dir).ok_or_else(|| {
        format!(
            "no `{MANIFEST_NAME}` found at or above `{}` (needed for `--profile {profile}`)",
            dir.display()
        )
    })?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    let manifest =
        Manifest::parse(&text).map_err(|err| format!("invalid `{}`: {err}", path.display()))?;
    manifest.active_tiers(profile)
}

impl Manifest {
    /// Parse a `lang.toml`'s text into a [`Manifest`], validating every tier name (a built-in tier)
    /// and provider (only `"std"` for now). Unknown keys outside `[profiles]` and unknown
    /// profile-level keys are ignored, leaving room for later codegen knobs.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let table: toml::Table = text.parse().map_err(|err| format!("{err}"))?;
        let mut profiles = BTreeMap::new();

        let Some(profiles_value) = table.get("profiles") else {
            return Ok(Manifest { profiles });
        };
        let profiles_table = profiles_value
            .as_table()
            .ok_or("`profiles` must be a table")?;

        for (name, value) in profiles_table {
            let profile_table = value
                .as_table()
                .ok_or_else(|| format!("profile `{name}` must be a table"))?;

            let extends = match profile_table.get("extends") {
                None => None,
                Some(v) => Some(
                    v.as_str()
                        .ok_or_else(|| format!("profile `{name}`: `extends` must be a string"))?
                        .to_string(),
                ),
            };

            let mut tiers = BTreeMap::new();
            if let Some(tiers_value) = profile_table.get("tiers") {
                let tiers_table = tiers_value
                    .as_table()
                    .ok_or_else(|| format!("profile `{name}`: `tiers` must be a table"))?;
                for (tier, provider_value) in tiers_table {
                    if !BUILTIN_TIERS.contains(&tier.as_str()) {
                        return Err(format!(
                            "profile `{name}`: unknown tier `{tier}` (built-in tiers are {})",
                            builtin_tier_list()
                        ));
                    }
                    let provider = provider_of(name, tier, provider_value)?;
                    if provider != BUILTIN_PROVIDER {
                        return Err(format!(
                            "profile `{name}`: tier `{tier}` names provider `{provider}`, which is \
                             not available — no package system yet, so only `\"{BUILTIN_PROVIDER}\"` \
                             is provided"
                        ));
                    }
                    tiers.insert(tier.clone(), provider);
                }
            }

            profiles.insert(name.clone(), Profile { extends, tiers });
        }

        Ok(Manifest { profiles })
    }

    /// The active tier names for `profile`, merging inherited tiers (`extends`) under this profile's
    /// own (which win), returned sorted. Errors on an unknown profile or an `extends` cycle.
    pub fn active_tiers(&self, profile: &str) -> Result<Vec<String>, String> {
        let mut chain = Vec::new();
        let merged = self.resolve(profile, &mut chain)?;
        Ok(merged.into_keys().collect())
    }

    /// Resolve a profile's effective tier map by walking its `extends` chain base-first, overlaying
    /// each profile's own tiers on top. `chain` records the profiles visited along the current path
    /// to detect a cycle.
    fn resolve(
        &self,
        name: &str,
        chain: &mut Vec<String>,
    ) -> Result<BTreeMap<String, String>, String> {
        if chain.iter().any(|p| p == name) {
            chain.push(name.to_string());
            return Err(format!("profile inheritance cycle: {}", chain.join(" -> ")));
        }
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| format!("unknown profile `{name}`"))?;

        chain.push(name.to_string());
        let mut merged = match &profile.extends {
            Some(base) => self.resolve(base, chain)?,
            None => BTreeMap::new(),
        };
        chain.pop();

        for (tier, provider) in &profile.tiers {
            merged.insert(tier.clone(), provider.clone());
        }
        Ok(merged)
    }
}

/// The provider package for a tier entry: a bare string (`test = "std"`) or a table whose `package`
/// key carries it (`bench = { package = "std", samples = 100 }`); other table keys are profile-level
/// options, ignored for now.
fn provider_of(profile: &str, tier: &str, value: &toml::Value) -> Result<String, String> {
    if let Some(s) = value.as_str() {
        return Ok(s.to_string());
    }
    if let Some(table) = value.as_table() {
        let package = table
            .get("package")
            .and_then(|p| p.as_str())
            .ok_or_else(|| {
                format!("profile `{profile}`: tier `{tier}` table must have a string `package`")
            })?;
        return Ok(package.to_string());
    }
    Err(format!(
        "profile `{profile}`: tier `{tier}` must be a provider string or a `{{ package = … }}` table"
    ))
}

/// The built-in tiers rendered as a comma-separated backticked list, for diagnostics.
fn builtin_tier_list() -> String {
    BUILTIN_TIERS
        .iter()
        .map(|t| format!("`{t}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_resolves_a_simple_profile() {
        let m = Manifest::parse(
            "[profiles.dev.tiers]\n\
             test = \"std\"\n\
             debug = \"std\"\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["debug", "test"]);
    }

    #[test]
    fn table_provider_form_is_accepted() {
        let m = Manifest::parse(
            "[profiles.dev.tiers]\n\
             bench = { package = \"std\", samples = 100 }\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["bench"]);
    }

    #[test]
    fn extends_merges_base_then_overrides() {
        let m = Manifest::parse(
            "[profiles.base.tiers]\n\
             test = \"std\"\n\
             doc = \"std\"\n\
             [profiles.ci]\n\
             extends = \"base\"\n\
             [profiles.ci.tiers]\n\
             bench = \"std\"\n",
        )
        .expect("valid manifest");
        // ci inherits test+doc from base and adds bench.
        assert_eq!(m.active_tiers("ci").unwrap(), vec!["bench", "doc", "test"]);
        // A minimalist profile opts into nothing.
        let empty = Manifest::parse("[profiles.prod]\n").expect("valid manifest");
        assert!(empty.active_tiers("prod").unwrap().is_empty());
    }

    #[test]
    fn unknown_tier_is_rejected() {
        let err = Manifest::parse("[profiles.dev.tiers]\ntset = \"std\"\n").unwrap_err();
        assert!(err.contains("unknown tier `tset`"), "{err}");
    }

    #[test]
    fn unavailable_provider_is_rejected() {
        let err =
            Manifest::parse("[profiles.dev.tiers]\nbench = \"criterion-lang\"\n").unwrap_err();
        assert!(err.contains("not available"), "{err}");
    }

    #[test]
    fn unknown_profile_is_an_error() {
        let m = Manifest::parse("[profiles.dev.tiers]\ntest = \"std\"\n").unwrap();
        assert!(
            m.active_tiers("nope")
                .unwrap_err()
                .contains("unknown profile")
        );
    }

    #[test]
    fn inheritance_cycle_is_detected() {
        let m = Manifest::parse("[profiles.a]\nextends = \"b\"\n[profiles.b]\nextends = \"a\"\n")
            .unwrap();
        assert!(m.active_tiers("a").unwrap_err().contains("cycle"));
    }
}
