//! The project manifest (`noeta.toml`) — build **profiles** (object-model slice 6g).
//!
//! A *profile* names which dev-tiers are live in a build and which package provides each — the
//! Cargo-profile / MSBuild-configuration axis. A `--profile` selects a tier set; the front-end tier
//! filter (`noeta run`) and the tier runners (`noeta test`/`bench`/`doc`) consume that resolved
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

use noeta_check::BUILTIN_TIERS;
use noeta_fmt::FmtConfig;

/// The manifest file name, discovered at or above the entry file's directory.
pub const MANIFEST_NAME: &str = "noeta.toml";

/// The sole tier provider available before the package system: the built-in/stdlib tiers. The
/// provider-string grammar accepts only this; naming any other package is an error until packages
/// (and their dependency resolution) exist.
const BUILTIN_PROVIDER: &str = "std";

/// A parsed `noeta.toml`: the package's identity (`[package]`, absent for a bare script), its
/// declared dependencies (`[dependencies]`, keyed by **import root**), and its build profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    package: Option<PackageMeta>,
    dependencies: BTreeMap<String, Dependency>,
    profiles: BTreeMap<String, Profile>,
}

/// The `[package]` table — a package's global identity and version (package-manager P2.0). Absent for
/// a bare entry script that declares no `[package]`.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageMeta {
    /// The global identity `company/package` — what the registry indexes and git coords map to.
    pub name: PackageName,
    pub version: semver::Version,
    /// The language edition, if pinned (reserved; not yet consumed).
    pub edition: Option<String>,
}

/// A global package identity `company/package` (package-manager P2.0). The slash is deliberately
/// **not** an identifier, so this global id is decoupled from the local import root (the
/// dependency-table key) — mirroring Rust's `foo = { package = "real-name" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageName {
    pub company: String,
    pub package: String,
}

impl PackageName {
    /// Parse `company/package`: exactly one `/`, each side a non-empty identifier
    /// (`[A-Za-z_][A-Za-z0-9_]*`). The `package` half is the package's **root namespace segment** —
    /// what a consumer's dep-key re-roots at the package boundary (see phase-2 plan).
    pub fn parse(s: &str) -> Result<PackageName, String> {
        let (company, package) = s.split_once('/').ok_or_else(|| {
            format!("package name `{s}` must be `company/package` (missing `/`)")
        })?;
        if package.contains('/') {
            return Err(format!(
                "package name `{s}` must have exactly one `/` (found more)"
            ));
        }
        if !is_identifier(company) || !is_identifier(package) {
            return Err(format!(
                "package name `{s}`: `company` and `package` must each be identifiers \
                 (letters, digits, `_`; not starting with a digit)"
            ));
        }
        Ok(PackageName {
            company: company.to_string(),
            package: package.to_string(),
        })
    }

    /// The package's **root namespace segment** — the `package` half. A consumer's dependency-table
    /// key re-roots the package's modules from this segment at link time.
    pub fn root(&self) -> &str {
        &self.package
    }
}

/// One `[dependencies]` entry's **source** (package-manager P2.0). The table *key* is the local import
/// root (an identifier), decoupled from the resolved package's global `company/package` identity.
#[derive(Debug, Clone, PartialEq)]
pub enum Dependency {
    /// A local source tree — `dep = { path = "…" }`. Needs no network or resolver (P2.1).
    Path { path: PathBuf },
    /// A git repository pinned to a **tag** (= a released version) — `dep = { git = "…", tag = "…" }`.
    /// Sources are git + tagged releases only (user decision); the lockfile pins the resolved SHA.
    Git { url: String, tag: String },
    /// A registry dependency by SemVer requirement — `dep = "^1.2"` or `dep = { version = "^1.2" }`.
    /// The registry index resolves the name→git-coords (P2.5).
    Registry { req: semver::VersionReq },
}

#[derive(Debug, Clone, PartialEq)]
struct Profile {
    /// The base profile this one inherits tiers from (`extends = "dev"`), if any.
    extends: Option<String>,
    /// This profile's own tier → provider entries (overlaid on the base's during resolution).
    tiers: BTreeMap<String, String>,
}

/// Discover the nearest `noeta.toml` at or above `start_dir`, walking up to the filesystem root.
pub fn find(start_dir: &Path) -> Option<PathBuf> {
    for dir in start_dir.ancestors() {
        let candidate = dir.join(MANIFEST_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the active-tier set for `profile` from the `noeta.toml` discovered at or above `entry`'s
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

/// Gather the entry's **dependency packages** as loader [`DepPackage`]s (package-manager P2.1/P2.3):
/// discover the nearest `noeta.toml`, and for each `[dependencies]` entry materialize its sources so
/// the loader can link them under the consumer's key. No manifest, or no `[dependencies]`, yields an
/// empty list (a bare script has no deps).
///
/// A `path` dependency is a local tree; a `git` dependency is **fetched** (`ls-remote` for the tag's
/// SHA, then a cached checkout into the package store — see [`crate::git`]). A `registry` dependency
/// still errors, pending the registry index (P2.5). Each dependency must carry a `[package]` table —
/// its `package` name segment is the **root** the loader re-roots to the consumer's key.
///
/// This layer is **flat** (direct dependencies only): a dependency's *own* transitive dependencies
/// are not yet resolved — that (plus the `noeta.lock` pin that avoids an `ls-remote` per run) is P2.4.
pub fn dependency_packages(entry: &Path) -> Result<Vec<noeta_loader::DepPackage>, String> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(manifest_path) = find(dir) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|err| format!("cannot read `{}`: {err}", manifest_path.display()))?;
    let manifest =
        Manifest::parse(&text).map_err(|err| format!("invalid `{}`: {err}", manifest_path.display()))?;
    let manifest_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));

    // Open the package store once, only if a git dependency actually needs it.
    let needs_store = manifest
        .dependencies()
        .values()
        .any(|d| matches!(d, Dependency::Git { .. }));
    let store = if needs_store {
        Some(crate::store::Store::open().ok_or_else(|| {
            "cannot open the package store (no writable cache directory) — needed for git \
             dependencies"
                .to_string()
        })?)
    } else {
        None
    };

    let mut packages = Vec::new();
    for (key, dep) in manifest.dependencies() {
        let dep_dir = match dep {
            Dependency::Path { path } => manifest_dir.join(path),
            Dependency::Git { url, tag } => {
                let store = store.as_ref().expect("store opened when a git dep is present");
                crate::git::fetch(url, tag, store)
                    .map_err(|err| format!("dependency `{key}`: {err}"))?
                    .path
            }
            Dependency::Registry { .. } => {
                return Err(format!(
                    "dependency `{key}` is a registry dependency — resolving name→git-coords needs \
                     the registry index (package-manager P2.5); use a `path` or `git` dependency \
                     today"
                ));
            }
        };
        packages.push(dep_package_from_dir(key, &dep_dir)?);
    }
    Ok(packages)
}

/// Build a [`noeta_loader::DepPackage`] from a materialized package directory `dir` (a local path dep
/// or a fetched git checkout): read its `[package]` for the root namespace segment, and gather its
/// `.noe` sources recursively.
fn dep_package_from_dir(key: &str, dir: &Path) -> Result<noeta_loader::DepPackage, String> {
    let manifest_path = dir.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&manifest_path).map_err(|err| {
        format!(
            "dependency `{key}` at `{}`: cannot read its `{MANIFEST_NAME}`: {err}",
            dir.display()
        )
    })?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| format!("dependency `{key}`: invalid `{MANIFEST_NAME}`: {err}"))?;
    let pkg = manifest.package().ok_or_else(|| {
        format!(
            "dependency `{key}` at `{}` has no `[package]` table (needed for its namespace root)",
            dir.display()
        )
    })?;
    let root = pkg.name.root().to_string();
    let modules = noeta_loader::read_package_sources(dir).map_err(|err| {
        format!(
            "dependency `{key}`: cannot read sources under `{}`: {err}",
            dir.display()
        )
    })?;
    Ok(noeta_loader::DepPackage {
        key: key.to_string(),
        root,
        modules,
    })
}

/// Resolve the [`FmtConfig`] for a target directory: discover the nearest `noeta.toml`, read its
/// optional `[fmt]` table, and overlay any values on the defaults. A missing manifest or missing
/// `[fmt]` table yields [`FmtConfig::default`] (so `noeta fmt` works with zero configuration).
/// Returns `Err` only when a present `[fmt]` table is malformed (wrong types / unknown arrow style),
/// so a typo surfaces rather than being silently ignored.
pub fn resolve_fmt_config(start_dir: &Path) -> Result<FmtConfig, String> {
    let Some(path) = find(start_dir) else {
        return Ok(FmtConfig::default());
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    // The `[fmt]` grammar lives in `noeta-fmt` (shared with the LSP formatter); the CLI adds the
    // manifest path to any error.
    FmtConfig::from_toml(&text).map_err(|err| format!("invalid `{}`: {err}", path.display()))
}

impl Manifest {
    /// Parse a `noeta.toml`'s text into a [`Manifest`], validating every tier name (a built-in tier)
    /// and provider (only `"std"` for now). Unknown keys outside `[profiles]` and unknown
    /// profile-level keys are ignored, leaving room for later codegen knobs.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let table: toml::Table = text.parse().map_err(|err| format!("{err}"))?;
        let package = parse_package(&table)?;
        let dependencies = parse_dependencies(&table)?;
        let mut profiles = BTreeMap::new();

        let Some(profiles_value) = table.get("profiles") else {
            return Ok(Manifest {
                package,
                dependencies,
                profiles,
            });
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

        Ok(Manifest {
            package,
            dependencies,
            profiles,
        })
    }

    /// The package's identity, if it declares a `[package]` table (a bare entry script has none).
    pub fn package(&self) -> Option<&PackageMeta> {
        self.package.as_ref()
    }

    /// The declared dependencies, keyed by local **import root** (the dependency-table key).
    pub fn dependencies(&self) -> &BTreeMap<String, Dependency> {
        &self.dependencies
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

/// Parse the optional `[package]` table into a [`PackageMeta`]. Absent table → `None` (a bare entry
/// script). A present table requires `name` (`company/package`) and `version` (SemVer); `edition` is
/// optional. Unknown keys are ignored (room for later fields).
fn parse_package(table: &toml::Table) -> Result<Option<PackageMeta>, String> {
    let Some(value) = table.get("package") else {
        return Ok(None);
    };
    let pkg = value.as_table().ok_or("`package` must be a table")?;
    let name_str = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("`package` must have a string `name` (`\"company/package\"`)")?;
    let name = PackageName::parse(name_str)?;
    let version_str = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("`package` must have a string `version` (SemVer, e.g. `\"1.2.0\"`)")?;
    let version = semver::Version::parse(version_str)
        .map_err(|err| format!("`package.version` `{version_str}` is not valid SemVer: {err}"))?;
    let edition = match pkg.get("edition") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or("`package.edition` must be a string")?
                .to_string(),
        ),
    };
    Ok(Some(PackageMeta {
        name,
        version,
        edition,
    }))
}

/// Parse the optional `[dependencies]` table. Each key is a local **import root** (must be an
/// identifier — it becomes a `use <key>.…` path segment); each value is one dependency source.
fn parse_dependencies(table: &toml::Table) -> Result<BTreeMap<String, Dependency>, String> {
    let Some(value) = table.get("dependencies") else {
        return Ok(BTreeMap::new());
    };
    let deps = value.as_table().ok_or("`dependencies` must be a table")?;
    let mut out = BTreeMap::new();
    for (key, value) in deps {
        if !is_identifier(key) {
            return Err(format!(
                "dependency key `{key}` must be an identifier (it is the local import root — \
                 `use {key}.…`)"
            ));
        }
        out.insert(key.clone(), parse_dependency(key, value)?);
    }
    Ok(out)
}

/// Parse one dependency value: a bare SemVer string (`dep = "^1.2"`, the registry shorthand) or a
/// table with exactly one source key — `path`, `git` (+ required `tag`), or `version`.
fn parse_dependency(key: &str, value: &toml::Value) -> Result<Dependency, String> {
    if let Some(req) = value.as_str() {
        let req = semver::VersionReq::parse(req).map_err(|err| {
            format!("dependency `{key}` version requirement `{req}` is not valid SemVer: {err}")
        })?;
        return Ok(Dependency::Registry { req });
    }
    let table = value.as_table().ok_or_else(|| {
        format!("dependency `{key}` must be a SemVer string or a table (`{{ path/git/version = … }}`)")
    })?;
    let has = |k: &str| table.contains_key(k);
    match (has("path"), has("git"), has("version")) {
        (true, false, false) => {
            let path = table["path"]
                .as_str()
                .ok_or_else(|| format!("dependency `{key}`: `path` must be a string"))?;
            Ok(Dependency::Path {
                path: PathBuf::from(path),
            })
        }
        (false, true, false) => {
            let url = table["git"]
                .as_str()
                .ok_or_else(|| format!("dependency `{key}`: `git` must be a string"))?;
            let tag = table
                .get("tag")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    format!(
                        "dependency `{key}`: a `git` dependency requires a string `tag` \
                         (sources are git + tagged releases only)"
                    )
                })?;
            Ok(Dependency::Git {
                url: url.to_string(),
                tag: tag.to_string(),
            })
        }
        (false, false, true) => {
            let req = table["version"]
                .as_str()
                .ok_or_else(|| format!("dependency `{key}`: `version` must be a string"))?;
            let req = semver::VersionReq::parse(req).map_err(|err| {
                format!("dependency `{key}` version requirement `{req}` is not valid SemVer: {err}")
            })?;
            Ok(Dependency::Registry { req })
        }
        (false, false, false) => Err(format!(
            "dependency `{key}` table must name a source: `path`, `git` (+ `tag`), or `version`"
        )),
        _ => Err(format!(
            "dependency `{key}` names more than one source — use exactly one of `path`, `git`, \
             or `version`"
        )),
    }
}

/// Whether `s` is a Noeta identifier (`[A-Za-z_][A-Za-z0-9_]*`) — the shape a package-name segment
/// and a dependency import-root key must both have.
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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

    // --- `[package]` + `[dependencies]` (package-manager P2.0) ---------------------------------

    #[test]
    fn parses_a_package_table() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.4.2\"\n\
             edition = \"2026\"\n",
        )
        .expect("valid");
        let pkg = m.package().expect("package present");
        assert_eq!(pkg.name.company, "acme");
        assert_eq!(pkg.name.package, "widgets");
        assert_eq!(pkg.name.root(), "widgets");
        assert_eq!(pkg.version, semver::Version::parse("1.4.2").unwrap());
        assert_eq!(pkg.edition.as_deref(), Some("2026"));
    }

    #[test]
    fn a_bare_script_has_no_package() {
        let m = Manifest::parse("[profiles.dev.tiers]\ntest = \"std\"\n").expect("valid");
        assert!(m.package().is_none());
        assert!(m.dependencies().is_empty());
    }

    #[test]
    fn package_name_requires_company_slash_package() {
        assert!(Manifest::parse("[package]\nname = \"widgets\"\nversion = \"1.0.0\"\n").is_err());
        assert!(
            Manifest::parse("[package]\nname = \"a/b/c\"\nversion = \"1.0.0\"\n").is_err()
        );
        assert!(Manifest::parse("[package]\nname = \"1bad/x\"\nversion = \"1.0.0\"\n").is_err());
    }

    #[test]
    fn package_version_must_be_semver() {
        assert!(Manifest::parse("[package]\nname = \"a/b\"\nversion = \"not-semver\"\n").is_err());
    }

    #[test]
    fn parses_the_three_dependency_forms() {
        let m = Manifest::parse(
            "[dependencies]\n\
             local = { path = \"../local\" }\n\
             http = { git = \"https://example.com/guzzle/http\", tag = \"v1.2.0\" }\n\
             json = { version = \"^1.2\" }\n\
             shorthand = \"^0.4\"\n",
        )
        .expect("valid");
        let deps = m.dependencies();
        assert_eq!(
            deps["local"],
            Dependency::Path {
                path: PathBuf::from("../local")
            }
        );
        assert_eq!(
            deps["http"],
            Dependency::Git {
                url: "https://example.com/guzzle/http".to_string(),
                tag: "v1.2.0".to_string(),
            }
        );
        assert_eq!(
            deps["json"],
            Dependency::Registry {
                req: semver::VersionReq::parse("^1.2").unwrap()
            }
        );
        assert_eq!(
            deps["shorthand"],
            Dependency::Registry {
                req: semver::VersionReq::parse("^0.4").unwrap()
            }
        );
    }

    #[test]
    fn a_git_dependency_requires_a_tag() {
        assert!(
            Manifest::parse("[dependencies]\nhttp = { git = \"https://x/y\" }\n").is_err()
        );
    }

    #[test]
    fn a_dependency_names_exactly_one_source() {
        assert!(
            Manifest::parse(
                "[dependencies]\nx = { path = \"../p\", version = \"^1\" }\n"
            )
            .is_err()
        );
        assert!(Manifest::parse("[dependencies]\nx = {}\n").is_err());
    }

    #[test]
    fn a_dependency_key_must_be_an_identifier() {
        // The key is the local import root (`use bad-key.…` is not a valid path).
        assert!(Manifest::parse("[dependencies]\n\"bad-key\" = \"^1\"\n").is_err());
    }

    #[test]
    fn package_and_dependencies_and_profiles_coexist() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/app\"\n\
             version = \"0.1.0\"\n\
             [dependencies]\n\
             http = { git = \"https://x/guzzle/http\", tag = \"v1.0.0\" }\n\
             [profiles.dev.tiers]\n\
             test = \"std\"\n",
        )
        .expect("valid");
        assert_eq!(m.package().unwrap().name.company, "acme");
        assert!(m.dependencies().contains_key("http"));
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["test"]);
    }

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
