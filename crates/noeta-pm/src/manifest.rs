//! The project manifest (`noeta.toml`) — build **targets** (object-model slice 6g).
//!
//! A *target* is a named build recipe: it says which dev-tiers are live in the build and which
//! package provides each — the axis Cargo calls a profile and MSBuild a configuration, under the
//! noun users actually reach for (you build "dev" or "prod"). A `--target` selects a tier set; the
//! front-end tier filter (`noeta run`) and the tier runners (`noeta test`/`bench`/`doc`) consume
//! that resolved *active-tier set*, not caring whether it came from a target, a `--tier` flag, or a
//! default. The table shape leaves room for a target to absorb the rest of the recipe later
//! (platform/artifact keys — the resolution of "target" the platform word, by subsumption).
//!
//! ```toml
//! [targets.dev.tiers]
//! test  = "std"                 # provider = the built-in stdlib tier
//! bench = { package = "std" }   # table form (room for target-level options later)
//! debug = "std"
//!
//! [targets.ci]
//! extends = "dev"               # inherit dev's tiers…
//! [targets.ci.tiers]
//! debug = "std"                 # …and override / add
//! ```
//!
//! A tier's provider is the built-in `"std"` or a declared `[dependencies]` key (P2.6) — an
//! undeclared name is an error. A target's *active tiers* are the tier names in its
//! (inheritance-merged) map.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// The `[fmt]` grammar lives in `noeta-fmt` (dev tooling), so reading a manifest's formatter config
// pulls in the formatter crate — gated behind `fmt-config` (dev-deps D3c) so a lean runtime that only
// needs tier/dependency resolution never links it.
#[cfg(feature = "fmt-config")]
use noeta_fmt::FmtConfig;

/// The manifest file name, discovered at or above the entry file's directory.
pub const MANIFEST_NAME: &str = "noeta.toml";

/// The built-in/stdlib tier provider — always available; every other provider must be a declared
/// `[dependencies]` key.
const BUILTIN_PROVIDER: &str = "std";

/// A parsed `noeta.toml`: the package's identity (`[package]`, absent for a bare script), its
/// declared dependencies (`[dependencies]`, keyed by **import root**), and its build targets.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    package: Option<PackageMeta>,
    dependencies: BTreeMap<String, Dependency>,
    targets: BTreeMap<String, Target>,
    trust: Trust,
}

/// The `[trust]` table (package-manager Phase 4) — the **complete, auditable set of every elevated
/// authority** a consumer grants its dependencies. Empty by default: pulling a dependency gives it
/// sandboxed library code and nothing more. Authority is granted here, at the **root**, keyed by
/// **package identity** (`company/package`) — never inherited from a dependency, so a transitive
/// package can't authorize itself (the anti-supply-chain invariant).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trust {
    /// Packages permitted to compile + run their native Rust crate (which also runs its `cargo`
    /// build — `build.rs`/proc-macros). Any native-declaring package not listed here is refused.
    pub native: std::collections::BTreeSet<String>,
    /// Packages permitted to contribute `noeta <subcommand>` CLI commands. A command from an
    /// unlisted package is silently omitted (a capability the user never asked for).
    pub commands: std::collections::BTreeSet<String>,
    /// Which registry **scopes** (the `company` segment) this project demands carry verified
    /// provenance (namespace-protection #1, require-provenance). A dependency resolved from a required
    /// scope whose release is unsigned is a hard resolve error — the consumer's own guarantee, held
    /// independently of whether the scope itself set a require-provenance policy.
    pub require_provenance: RequireProvenance,
    /// Whether every registry dependency must be publicly recorded in the registry's **transparency
    /// log** (namespace-protection #1, TLog): resolution verifies each registry release's inclusion
    /// under a signed checkpoint and that the log is an append-only extension of the one pinned in
    /// `noeta.lock` — so a compromised registry can't serve an unlogged or history-rewritten release.
    /// Default `false` (gradual adoption).
    pub require_transparency: bool,
}

/// The consumer's `[trust].require_provenance` policy: demand verified provenance from no scope
/// (default), every scope, or a named set of scopes. The `company` segment is the scope.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum RequireProvenance {
    /// Unset — a scope's releases are accepted unsigned (gradual-adoption default).
    #[default]
    None,
    /// `require_provenance = true` — every registry dependency must carry verified provenance.
    All,
    /// `require_provenance = ["acme", "para"]` — only these scopes must.
    Scopes(std::collections::BTreeSet<String>),
}

impl RequireProvenance {
    /// Whether releases from `scope` (a `company` segment) must carry verified provenance.
    pub fn requires(&self, scope: &str) -> bool {
        match self {
            RequireProvenance::None => false,
            RequireProvenance::All => true,
            RequireProvenance::Scopes(scopes) => scopes.contains(scope),
        }
    }
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
    /// The relative directory of this package's native Rust **entry crate** (package-manager
    /// Phase 3, N3.1): `native = "native"` points at a `Cargo.toml` whose crate exports the
    /// package's extension units (one crate, any number of units — std's own shape). `None` for a
    /// pure-Noeta package. Declaring native code is deliberately explicit — it pulls arbitrary
    /// Rust into a consumer's build, which should never be triggered by the mere presence of a
    /// directory.
    pub native: Option<String>,
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
        let (company, package) = s
            .split_once('/')
            .ok_or_else(|| format!("package name `{s}` must be `company/package` (missing `/`)"))?;
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
    /// A registry dependency by SemVer requirement — `dep = "^1.2"` or
    /// `dep = { version = "^1.2", package = "company/pkg" }`. The registry index resolves
    /// name→git-coords (P2.5). `package` is the registry identity (decoupled from the import-root
    /// key, like Rust's `foo = { package = "real" }`); it is **required** to resolve — the bare
    /// shorthand leaves it `None` and errors at resolution with a pointer to add it.
    Registry {
        package: Option<PackageName>,
        req: semver::VersionReq,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Target {
    /// The base target this one inherits tiers and dependencies from (`extends = "dev"`), if any.
    extends: Option<String>,
    /// This target's own tier → provider entries (overlaid on the base's during resolution).
    tiers: BTreeMap<String, String>,
    /// This target's own **target-scoped dependencies** — packages present only when building this
    /// target (dev-deps arc). Overlaid on the base's (via `extends`) and on the global
    /// `[dependencies]` during resolution. A dev tool a package ships lives here under `dev`, so it
    /// is simply absent from a `prod` build.
    dependencies: BTreeMap<String, Dependency>,
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

/// Read + parse the manifest at `manifest_path` (package-manager Phase 4, backing `noeta audit`).
/// Errors (tagged with the path) on an unreadable or invalid manifest.
pub fn load(manifest_path: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|err| format!("cannot read `{}`: {err}", manifest_path.display()))?;
    Manifest::parse(&text).map_err(|err| format!("invalid `{}`: {err}", manifest_path.display()))
}

/// The `[package]` identity (`company/package`) and version of the manifest at `manifest_path`
/// (package-manager P2.5, backing `noeta publish`). Errors when the manifest can't be read/parsed or
/// declares no `[package]` (a bare script can't be published).
pub fn current_package(manifest_path: &Path) -> Result<(String, semver::Version), String> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|err| format!("cannot read `{}`: {err}", manifest_path.display()))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| format!("invalid `{}`: {err}", manifest_path.display()))?;
    let pkg = manifest.package().ok_or_else(|| {
        format!(
            "`{}` has no `[package]` table — only a package (with a name + version) can be published",
            manifest_path.display()
        )
    })?;
    Ok((
        format!("{}/{}", pkg.name.company, pkg.name.package),
        pkg.version.clone(),
    ))
}

/// Add a dependency `key = <value_toml>` to the `[dependencies]` table of the manifest at
/// `manifest_path` (package-manager P2.4d, backing `noeta add`). `value_toml` is the raw TOML value
/// (`{ path = "../x" }`, `{ git = "…", tag = "…" }`, or `"^1.2"`). The edit is **format-preserving**:
/// the new entry is inserted under an existing `[dependencies]` header (or a new section is appended),
/// leaving the rest of the file — comments, ordering, whitespace — untouched. The result is re-parsed
/// before writing, so a malformed value or an unknown source never corrupts the manifest, and the
/// write is atomic. Errors if `key` is not an identifier or is already a dependency.
pub fn add_dependency(manifest_path: &Path, key: &str, value_toml: &str) -> Result<(), String> {
    if !is_identifier(key) {
        return Err(format!(
            "dependency key `{key}` must be an identifier (it becomes the import root — `use {key}.…`)"
        ));
    }
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|err| format!("cannot read `{}`: {err}", manifest_path.display()))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| format!("invalid `{}`: {err}", manifest_path.display()))?;
    if manifest.dependencies().contains_key(key) {
        return Err(format!("dependency `{key}` is already in the manifest"));
    }

    let entry = format!("{key} = {value_toml}");
    let updated = insert_dependency_entry(&text, &entry);
    // Re-parse the edited manifest so a bad value/source fails here rather than corrupting the file.
    Manifest::parse(&updated)
        .map_err(|err| format!("`noeta add {key}` would make `{MANIFEST_NAME}` invalid: {err}"))?;

    let dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".{MANIFEST_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &updated)
        .and_then(|()| std::fs::rename(&tmp, manifest_path))
        .map_err(|err| format!("cannot write `{}`: {err}", manifest_path.display()))
}

/// Insert `entry` (a `key = value` line) into `text`'s `[dependencies]` table: right after an
/// existing `[dependencies]` header line, or — if there is none — as a new section appended at the
/// end. Comments and the rest of the file are preserved (a purely textual edit).
fn insert_dependency_entry(text: &str, entry: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    if let Some(pos) = lines.iter().position(|l| l.trim() == "[dependencies]") {
        lines.insert(pos + 1, entry);
        let mut out = lines.join("\n");
        if text.ends_with('\n') {
            out.push('\n');
        }
        out
    } else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("\n[dependencies]\n{entry}\n"));
        out
    }
}

/// Resolve the active-tier set for `target` from the `noeta.toml` discovered at or above `entry`'s
/// directory: load the manifest, follow `extends`, and return the live tier names (sorted). Every
/// failure — no manifest, parse error, unknown target/tier, unavailable provider, inheritance
/// cycle — is a human-readable `Err` the caller prints.
pub fn resolve_active_tiers(entry: &Path, target: &str) -> Result<Vec<String>, String> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let path = find(dir).ok_or_else(|| {
        format!(
            "no `{MANIFEST_NAME}` found at or above `{}` (needed for `--target {target}`)",
            dir.display()
        )
    })?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    let manifest =
        Manifest::parse(&text).map_err(|err| format!("invalid `{}`: {err}", path.display()))?;
    manifest.active_tiers(target)
}

/// Resolve the active tier → **provider** map for `target` (see
/// [`Manifest::active_tier_providers`]) — the same discovery/parse path as
/// [`resolve_active_tiers`], returning who provides each live tier: `"std"` (the extension
/// declaration, with its native runner for the built-ins) or a declared dependency's import-root
/// key (that package's `@tier` declaration). The tier-execution layer dispatches on this.
pub fn resolve_active_tier_providers(
    entry: &Path,
    target: &str,
) -> Result<BTreeMap<String, String>, String> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let path = find(dir).ok_or_else(|| {
        format!(
            "no `{MANIFEST_NAME}` found at or above `{}` (needed for `--target {target}`)",
            dir.display()
        )
    })?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    let manifest =
        Manifest::parse(&text).map_err(|err| format!("invalid `{}`: {err}", path.display()))?;
    manifest.active_tier_providers(target)
}

/// Gather the entry's **dependency packages** as loader [`DepPackage`]s (package-manager P2.1/P2.4):
/// resolve the full transitive dependency graph and hand back the re-rooted packages the loader links.
/// No manifest, or no `[dependencies]`, yields an empty list (a bare script has no deps). The graph
/// walk ([`crate::graph`]) materializes each package (a `path` tree, a fetched `git` tag; a `registry`
/// dependency errors pending P2.5), dedups by identity, and assigns global segments so transitive
/// `use`s link without key collision.
pub fn dependency_packages(entry: &Path) -> Result<Vec<noeta_loader::DepPackage>, String> {
    Ok(crate::graph::resolve_graph(entry)?.packages)
}

/// The `[package] name` of a **cargo** manifest — what a composed-toolchain shim writes into its
/// dependency line for a native entry crate (package-manager Phase 3, N3.2). Kept here because
/// `noeta-pm` owns the toml dependency; this reads cargo's manifest, not ours.
pub fn cargo_package_name(crate_dir: &Path) -> Result<String, String> {
    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|err| format!("`{}` is not valid TOML: {err}", path.display()))?;
    table
        .get("package")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("`{}` has no `[package] name`", path.display()))
}

/// The feature names a **cargo** manifest declares under `[features]` (dev-deps D5b). A composed
/// **dev toolchain** consults this to turn on a mixed crate's conventional dev-capability feature
/// (e.g. `fmt`, gating a tier's `body_formatters`) — but only if the crate actually declares it, so
/// enabling an absent feature never makes cargo error. A missing/empty `[features]` table yields the
/// empty set. (A shipped runner/AOT base never calls this: it pulls each crate at default features,
/// so the formatter and its parser stay uncompiled — the whole point of the split.)
pub fn cargo_features(crate_dir: &Path) -> Result<Vec<String>, String> {
    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|err| format!("`{}` is not valid TOML: {err}", path.display()))?;
    Ok(table
        .get("features")
        .and_then(|f| f.as_table())
        .map(|f| f.keys().cloned().collect())
        .unwrap_or_default())
}

/// Resolve the [`FmtConfig`] for a target directory: discover the nearest `noeta.toml`, read its
/// optional `[fmt]` table, and overlay any values on the defaults. A missing manifest or missing
/// `[fmt]` table yields [`FmtConfig::default`] (so `noeta fmt` works with zero configuration).
/// Returns `Err` only when a present `[fmt]` table is malformed (wrong types / unknown arrow style),
/// so a typo surfaces rather than being silently ignored.
#[cfg(feature = "fmt-config")]
pub fn resolve_fmt_config(file: &Path) -> Result<FmtConfig, String> {
    // Precedence: built-in defaults, then `.editorconfig` (walked up from the file), then the
    // manifest's `[fmt]` table — so an explicit `noeta.toml` setting wins over `.editorconfig`, which
    // wins over the defaults. (CLI flags, applied by the caller, win over all.)
    let mut config = FmtConfig::default();
    config.overlay_editorconfig(file);
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    if let Some(path) = find(dir) {
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
        // The `[fmt]` grammar lives in `noeta-fmt` (shared with the LSP formatter); the CLI adds the
        // manifest path to any error.
        config
            .overlay_toml(&text)
            .map_err(|err| format!("invalid `{}`: {err}", path.display()))?;
    }
    Ok(config)
}

impl Manifest {
    /// Parse a `noeta.toml`'s text into a [`Manifest`], validating every tier name (a built-in tier)
    /// and provider (only `"std"` for now). Unknown keys outside `[targets]` and unknown
    /// target-level keys are ignored, leaving room for later codegen knobs.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let table: toml::Table = text.parse().map_err(|err| format!("{err}"))?;
        let package = parse_package(&table)?;
        let dependencies = parse_dependencies(&table)?;
        let trust = parse_trust(&table)?;
        let mut targets = BTreeMap::new();

        let Some(targets_value) = table.get("targets") else {
            return Ok(Manifest {
                package,
                dependencies,
                targets,
                trust,
            });
        };
        let targets_table = targets_value
            .as_table()
            .ok_or("`targets` must be a table")?;

        for (name, value) in targets_table {
            let target_table = value
                .as_table()
                .ok_or_else(|| format!("target `{name}` must be a table"))?;

            let extends = match target_table.get("extends") {
                None => None,
                Some(v) => Some(
                    v.as_str()
                        .ok_or_else(|| format!("target `{name}`: `extends` must be a string"))?
                        .to_string(),
                ),
            };

            // Target-scoped dependencies (dev-deps arc): parsed before tiers so a tier this target
            // declares may name a target-scoped dep as its provider.
            let target_deps =
                match target_table.get("dependencies") {
                    None => BTreeMap::new(),
                    Some(v) => parse_dependency_map(v.as_table().ok_or_else(|| {
                        format!("target `{name}`: `dependencies` must be a table")
                    })?)?,
                };

            let mut tiers = BTreeMap::new();
            if let Some(tiers_value) = target_table.get("tiers") {
                let tiers_table = tiers_value
                    .as_table()
                    .ok_or_else(|| format!("target `{name}`: `tiers` must be a table"))?;
                for (tier, provider_value) in tiers_table {
                    // The tier name-space is open (tier-providers T3): a name may be a built-in
                    // tier or one a dependency declares with `@tier`. The manifest cannot see the
                    // dependency's source, so it validates only the *shape* here — an identifier —
                    // and resolution happens where the tier is used (activation validates the name
                    // against the linked program's registry, E0036).
                    if !is_identifier(tier) {
                        return Err(format!(
                            "target `{name}`: tier `{tier}` is not a valid tier name (an identifier)"
                        ));
                    }
                    let provider = provider_of(name, tier, provider_value)?;
                    // A tier's provider is the built-in stdlib (`"std"`) or a **declared
                    // dependency** — named by its `[dependencies]` key, the same import root used
                    // elsewhere (package-manager P2.6). An undeclared name is an error pointing the
                    // user to add the dependency.
                    if provider != BUILTIN_PROVIDER
                        && !dependencies.contains_key(&provider)
                        && !target_deps.contains_key(&provider)
                    {
                        return Err(format!(
                            "target `{name}`: tier `{tier}` names provider `{provider}`, which is \
                             neither the built-in `\"{BUILTIN_PROVIDER}\"` nor a declared \
                             dependency — add `{provider}` to `[dependencies]` or \
                             `[targets.{name}.dependencies]` to provide this tier"
                        ));
                    }
                    tiers.insert(tier.clone(), provider);
                }
            }

            targets.insert(
                name.clone(),
                Target {
                    extends,
                    tiers,
                    dependencies: target_deps,
                },
            );
        }

        Ok(Manifest {
            package,
            dependencies,
            targets,
            trust,
        })
    }

    /// The package's identity, if it declares a `[package]` table (a bare entry script has none).
    pub fn package(&self) -> Option<&PackageMeta> {
        self.package.as_ref()
    }

    /// The `[trust]` grants — the authority this manifest extends to its dependencies (Phase 4).
    pub fn trust(&self) -> &Trust {
        &self.trust
    }

    /// The declared dependencies, keyed by local **import root** (the dependency-table key).
    pub fn dependencies(&self) -> &BTreeMap<String, Dependency> {
        &self.dependencies
    }

    /// The active tier names for `target`, merging inherited tiers (`extends`) under this target's
    /// own (which win), returned sorted. Errors on an unknown target or an `extends` cycle.
    pub fn active_tiers(&self, target: &str) -> Result<Vec<String>, String> {
        let mut chain = Vec::new();
        let merged = self.resolve(target, &mut chain)?;
        Ok(merged.into_keys().collect())
    }

    /// The active tier → **provider** map for `target` (package-manager P2.6): each live tier mapped
    /// to the package providing it — the built-in `"std"` or a declared dependency's import-root key.
    /// The tier-execution layer dispatches on this: `"std"` runs the built-in native runner, a
    /// dependency key runs that package's `@tier` runner (`resolve_active_tier_providers`).
    pub fn active_tier_providers(&self, target: &str) -> Result<BTreeMap<String, String>, String> {
        let mut chain = Vec::new();
        self.resolve(target, &mut chain)
    }

    /// Resolve a target's effective tier map by walking its `extends` chain base-first, overlaying
    /// each target's own tiers on top. `chain` records the targets visited along the current path
    /// to detect a cycle.
    /// The active dependency set for `target`: the global `[dependencies]` overlaid with the target's
    /// own and inherited (`extends`) `[targets.<name>.dependencies]` (dev-deps arc). A target-scoped
    /// key shadows a global one of the same name. `None` (no `--target`) yields just the globals; an
    /// **unknown** target is lenient — it contributes no scoped deps — since target-scoped deps are
    /// optional and a project need not declare a `[targets.*]` block to build a bare `--target` name.
    pub fn active_dependencies(
        &self,
        target: Option<&str>,
    ) -> Result<BTreeMap<String, Dependency>, String> {
        let mut merged = self.dependencies.clone();
        if let Some(name) = target
            && self.targets.contains_key(name)
        {
            let mut chain = Vec::new();
            for (key, dep) in self.resolve_deps(name, &mut chain)? {
                merged.insert(key, dep);
            }
        }
        Ok(merged)
    }

    /// The extends-merged target-scoped dependency map for `name` (base's first, then this target's,
    /// so a nearer target shadows an inherited one). Mirrors [`Self::resolve`] for tiers, including
    /// its inheritance-cycle guard.
    fn resolve_deps(
        &self,
        name: &str,
        chain: &mut Vec<String>,
    ) -> Result<BTreeMap<String, Dependency>, String> {
        if chain.iter().any(|p| p == name) {
            chain.push(name.to_string());
            return Err(format!("target inheritance cycle: {}", chain.join(" -> ")));
        }
        let target = self
            .targets
            .get(name)
            .ok_or_else(|| format!("unknown target `{name}`"))?;
        chain.push(name.to_string());
        let mut merged = match &target.extends {
            Some(base) => self.resolve_deps(base, chain)?,
            None => BTreeMap::new(),
        };
        chain.pop();
        for (key, dep) in &target.dependencies {
            merged.insert(key.clone(), dep.clone());
        }
        Ok(merged)
    }

    fn resolve(
        &self,
        name: &str,
        chain: &mut Vec<String>,
    ) -> Result<BTreeMap<String, String>, String> {
        if chain.iter().any(|p| p == name) {
            chain.push(name.to_string());
            return Err(format!("target inheritance cycle: {}", chain.join(" -> ")));
        }
        let target = self
            .targets
            .get(name)
            .ok_or_else(|| format!("unknown target `{name}`"))?;

        chain.push(name.to_string());
        let mut merged = match &target.extends {
            Some(base) => self.resolve(base, chain)?,
            None => BTreeMap::new(),
        };
        chain.pop();

        for (tier, provider) in &target.tiers {
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
    let native = match pkg.get("native") {
        None => None,
        Some(v) => {
            let dir = v
                .as_str()
                .ok_or("`package.native` must be a string (a relative directory)")?;
            Some(validate_native_dir(dir)?)
        }
    };
    Ok(Some(PackageMeta {
        name,
        version,
        edition,
        native,
    }))
}

/// Syntactic validation of a `package.native` value (Phase 3, N3.1): a non-empty **relative**
/// directory that stays inside the package tree. A git/registry package's tree is materialized
/// into the shared content-addressed store, so a `..` escape would point at other packages'
/// content; existence of the directory (and its `Cargo.toml`) is checked at resolve time, where
/// the package root is known.
fn validate_native_dir(dir: &str) -> Result<String, String> {
    if dir.is_empty() {
        return Err("`package.native` must not be empty".to_string());
    }
    let path = std::path::Path::new(dir);
    if path.is_absolute() {
        return Err(format!(
            "`package.native` must be a relative directory (inside the package), got `{dir}`"
        ));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "`package.native` must not leave the package tree (`..` in `{dir}`)"
        ));
    }
    Ok(dir.to_string())
}

/// Parse the optional `[dependencies]` table. Each key is a local **import root** (must be an
/// identifier — it becomes a `use <key>.…` path segment); each value is one dependency source.
fn parse_dependencies(table: &toml::Table) -> Result<BTreeMap<String, Dependency>, String> {
    let Some(value) = table.get("dependencies") else {
        return Ok(BTreeMap::new());
    };
    let deps = value.as_table().ok_or("`dependencies` must be a table")?;
    parse_dependency_map(deps)
}

/// Parse a `[…dependencies]` sub-table (shared by the global `[dependencies]` and a target's
/// `[targets.<name>.dependencies]`): each key is an import root, each value a [`Dependency`].
fn parse_dependency_map(deps: &toml::Table) -> Result<BTreeMap<String, Dependency>, String> {
    let mut out = BTreeMap::new();
    for (key, value) in deps {
        if !is_identifier(key) {
            return Err(format!(
                "dependency key `{key}` must be an identifier (it is the local import root — \
                 `use {key}.…`)"
            ));
        }
        // The key is the local import root: binding a dependency under a built-in root
        // (`std`/`noeta`/`core`) would make `use std.…` resolve to that package instead of the
        // compiler's built-in — an import-layer shadowing vector (namespace-protection #2/#3).
        // Refuse it here so even a hand-edited manifest can't capture a core import root.
        if crate::reserved::is_builtin(key) {
            return Err(format!(
                "dependency key `{key}` is a built-in import root (`use {key}.…` is the compiler's \
                 own `{key}` namespace) and cannot be bound to a dependency — choose another key"
            ));
        }
        out.insert(key.clone(), parse_dependency(key, value)?);
    }
    Ok(out)
}

/// Parse the optional `[trust]` table (package-manager Phase 4): `native` and `commands`, each an
/// array of package identities (`company/package`) the consumer authorizes for that escalation. Each
/// entry is validated as an identity (so a typo'd grant is a hard error, not a silently ineffective
/// one). An absent `[trust]` table yields empty grants — the safe default (no dependency may run
/// native code or add a command).
fn parse_trust(table: &toml::Table) -> Result<Trust, String> {
    let Some(value) = table.get("trust") else {
        return Ok(Trust::default());
    };
    let trust_table = value.as_table().ok_or("`trust` must be a table")?;
    let parse_list = |field: &str| -> Result<std::collections::BTreeSet<String>, String> {
        let Some(list) = trust_table.get(field) else {
            return Ok(std::collections::BTreeSet::new());
        };
        let array = list
            .as_array()
            .ok_or_else(|| format!("`trust.{field}` must be an array of `\"company/package\"`"))?;
        let mut out = std::collections::BTreeSet::new();
        for entry in array {
            let s = entry
                .as_str()
                .ok_or_else(|| format!("`trust.{field}` entries must be strings"))?;
            // Validate the identity shape so a typo (`acme/imagefx` for `acme/imgfx`) fails loudly
            // rather than silently granting nothing.
            PackageName::parse(s).map_err(|err| format!("`trust.{field}`: {err}"))?;
            out.insert(s.to_string());
        }
        Ok(out)
    };
    // `require_provenance` is `true` (every scope), `false`/absent (none), or an array of scope
    // (`company`) strings. A scope entry is validated as an identifier so a typo fails loudly.
    let require_provenance = match trust_table.get("require_provenance") {
        None => RequireProvenance::None,
        Some(v) => {
            if let Some(b) = v.as_bool() {
                if b {
                    RequireProvenance::All
                } else {
                    RequireProvenance::None
                }
            } else if let Some(array) = v.as_array() {
                let mut scopes = std::collections::BTreeSet::new();
                for entry in array {
                    let s = entry.as_str().ok_or(
                        "`trust.require_provenance` entries must be scope strings (a `company`)",
                    )?;
                    if !is_identifier(s) {
                        return Err(format!(
                            "`trust.require_provenance`: `{s}` is not a scope (a `company` identifier)"
                        ));
                    }
                    scopes.insert(s.to_string());
                }
                RequireProvenance::Scopes(scopes)
            } else {
                return Err(
                    "`trust.require_provenance` must be a boolean or an array of scope strings"
                        .to_string(),
                );
            }
        }
    };
    let require_transparency = match trust_table.get("require_transparency") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or("`trust.require_transparency` must be a boolean")?,
    };
    Ok(Trust {
        native: parse_list("native")?,
        commands: parse_list("commands")?,
        require_provenance,
        require_transparency,
    })
}

/// Parse one dependency value: a bare SemVer string (`dep = "^1.2"`, the registry shorthand) or a
/// table with exactly one source key — `path`, `git` (+ required `tag`), or `version`.
fn parse_dependency(key: &str, value: &toml::Value) -> Result<Dependency, String> {
    if let Some(req) = value.as_str() {
        let req = semver::VersionReq::parse(req).map_err(|err| {
            format!("dependency `{key}` version requirement `{req}` is not valid SemVer: {err}")
        })?;
        return Ok(Dependency::Registry { package: None, req });
    }
    let table = value.as_table().ok_or_else(|| {
        format!(
            "dependency `{key}` must be a SemVer string or a table (`{{ path/git/version = … }}`)"
        )
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
            let tag = table.get("tag").and_then(|v| v.as_str()).ok_or_else(|| {
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
            // The registry identity (`company/package`) — decoupled from the import-root key.
            let package = match table.get("package") {
                None => None,
                Some(v) => {
                    let s = v
                        .as_str()
                        .ok_or_else(|| format!("dependency `{key}`: `package` must be a string"))?;
                    Some(
                        PackageName::parse(s)
                            .map_err(|err| format!("dependency `{key}`: {err}"))?,
                    )
                }
            };
            Ok(Dependency::Registry { package, req })
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
/// key carries it (`bench = { package = "std", samples = 100 }`); other table keys are target-level
/// options, ignored for now.
fn provider_of(target: &str, tier: &str, value: &toml::Value) -> Result<String, String> {
    if let Some(s) = value.as_str() {
        return Ok(s.to_string());
    }
    if let Some(table) = value.as_table() {
        let package = table
            .get("package")
            .and_then(|p| p.as_str())
            .ok_or_else(|| {
                format!("target `{target}`: tier `{tier}` table must have a string `package`")
            })?;
        return Ok(package.to_string());
    }
    Err(format!(
        "target `{target}`: tier `{tier}` must be a provider string or a `{{ package = … }}` table"
    ))
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

    // --- cargo manifest introspection (composition: dev-deps D5b) ------------------------------

    #[test]
    fn reads_declared_cargo_features() {
        let dir = std::env::temp_dir().join(format!("noeta-pm-feat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"imgfx-native\"\nversion = \"0.1.0\"\n\n\
             [features]\nfmt = []\nextra = [\"fmt\"]\n",
        )
        .unwrap();
        let mut feats = cargo_features(&dir).unwrap();
        feats.sort();
        assert_eq!(feats, vec!["extra".to_string(), "fmt".to_string()]);
        // A crate with no `[features]` table yields the empty set (not an error) — a pure-runtime
        // crate the dev toolchain enables nothing extra on.
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(cargo_features(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- `package.native` (package-manager Phase 3, N3.1) --------------------------------------

    #[test]
    fn parses_a_native_entry_crate() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/imgfx\"\n\
             version = \"1.0.0\"\n\
             native = \"native\"\n",
        )
        .expect("valid");
        assert_eq!(m.package().unwrap().native.as_deref(), Some("native"));
    }

    #[test]
    fn native_is_absent_for_a_pure_package() {
        let m = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").expect("valid");
        assert_eq!(m.package().unwrap().native, None);
    }

    #[test]
    fn native_rejects_absolute_escape_and_empty() {
        let manifest = |native: &str| {
            format!("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\nnative = \"{native}\"\n")
        };
        assert!(Manifest::parse(&manifest("/abs/dir")).is_err());
        assert!(Manifest::parse(&manifest("../outside")).is_err());
        assert!(Manifest::parse(&manifest("ok/../../outside")).is_err());
        assert!(Manifest::parse(&manifest("")).is_err());
        // A nested relative dir is fine.
        assert!(Manifest::parse(&manifest("rust/imgfx")).is_ok());
    }

    // --- `[trust]` (package-manager Phase 4) ---------------------------------------------------

    #[test]
    fn trust_defaults_to_empty() {
        let m = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").expect("valid");
        assert!(m.trust().native.is_empty());
        assert!(m.trust().commands.is_empty());
    }

    #[test]
    fn trust_parses_native_and_command_grants() {
        let m = Manifest::parse(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [trust]\n\
             native = [\"acme/imgfx\", \"acme/simd\"]\n\
             commands = [\"acme/scaffold\"]\n",
        )
        .expect("valid");
        assert!(m.trust().native.contains("acme/imgfx"));
        assert!(m.trust().native.contains("acme/simd"));
        assert!(m.trust().commands.contains("acme/scaffold"));
        assert!(!m.trust().commands.contains("acme/imgfx"));
    }

    #[test]
    fn trust_parses_require_provenance() {
        // Default: no scope is required.
        let none = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();
        assert!(!none.trust().require_provenance.requires("acme"));
        // `true` → every scope required.
        let all = Manifest::parse("[trust]\nrequire_provenance = true\n").unwrap();
        assert!(all.trust().require_provenance.requires("acme"));
        assert!(all.trust().require_provenance.requires("anything"));
        // A scope list → only those scopes required.
        let some = Manifest::parse("[trust]\nrequire_provenance = [\"para\", \"acme\"]\n").unwrap();
        assert!(some.trust().require_provenance.requires("para"));
        assert!(some.trust().require_provenance.requires("acme"));
        assert!(!some.trust().require_provenance.requires("other"));
        // `false` is explicitly none.
        let off = Manifest::parse("[trust]\nrequire_provenance = false\n").unwrap();
        assert!(!off.trust().require_provenance.requires("acme"));
        // A malformed scope entry / wrong type fails loudly.
        assert!(Manifest::parse("[trust]\nrequire_provenance = [\"a/b\"]\n").is_err());
        assert!(Manifest::parse("[trust]\nrequire_provenance = 42\n").is_err());
    }

    #[test]
    fn trust_parses_require_transparency() {
        let off = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();
        assert!(!off.trust().require_transparency);
        let on = Manifest::parse("[trust]\nrequire_transparency = true\n").unwrap();
        assert!(on.trust().require_transparency);
        assert!(Manifest::parse("[trust]\nrequire_transparency = \"yes\"\n").is_err());
    }

    #[test]
    fn trust_rejects_a_malformed_identity() {
        // A typo'd grant must fail loudly, not silently authorize nothing.
        assert!(Manifest::parse("[trust]\nnative = [\"not-an-identity\"]\n").is_err());
        assert!(Manifest::parse("[trust]\ncommands = [42]\n").is_err());
        assert!(Manifest::parse("[trust]\nnative = \"acme/x\"\n").is_err()); // must be an array
    }

    #[test]
    fn a_bare_script_has_no_package() {
        let m = Manifest::parse("[targets.dev.tiers]\ntest = \"std\"\n").expect("valid");
        assert!(m.package().is_none());
        assert!(m.dependencies().is_empty());
    }

    #[test]
    fn package_name_requires_company_slash_package() {
        assert!(Manifest::parse("[package]\nname = \"widgets\"\nversion = \"1.0.0\"\n").is_err());
        assert!(Manifest::parse("[package]\nname = \"a/b/c\"\nversion = \"1.0.0\"\n").is_err());
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
                package: None,
                req: semver::VersionReq::parse("^1.2").unwrap()
            }
        );
        assert_eq!(
            deps["shorthand"],
            Dependency::Registry {
                package: None,
                req: semver::VersionReq::parse("^0.4").unwrap()
            }
        );
    }

    #[test]
    fn a_git_dependency_requires_a_tag() {
        assert!(Manifest::parse("[dependencies]\nhttp = { git = \"https://x/y\" }\n").is_err());
    }

    #[test]
    fn a_dependency_names_exactly_one_source() {
        assert!(
            Manifest::parse("[dependencies]\nx = { path = \"../p\", version = \"^1\" }\n").is_err()
        );
        assert!(Manifest::parse("[dependencies]\nx = {}\n").is_err());
    }

    #[test]
    fn a_dependency_key_must_be_an_identifier() {
        // The key is the local import root (`use bad-key.…` is not a valid path).
        assert!(Manifest::parse("[dependencies]\n\"bad-key\" = \"^1\"\n").is_err());
    }

    #[test]
    fn a_dependency_key_cannot_capture_a_builtin_import_root() {
        // namespace-protection #2/#3: binding a dep under `std`/`noeta`/`core` would shadow the
        // compiler's built-in namespace at the import layer — refused even by hand-editing.
        for key in ["std", "noeta", "core"] {
            let err = Manifest::parse(&format!("[dependencies]\n{key} = \"^1\"\n")).unwrap_err();
            assert!(err.contains("built-in import root"), "{key}: {err}");
        }
        // A near-miss that is *not* reserved is accepted (the guard is exact, not a prefix match).
        assert!(Manifest::parse("[dependencies]\nstdx = \"^1\"\n").is_ok());
    }

    #[test]
    fn package_and_dependencies_and_targets_coexist() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/app\"\n\
             version = \"0.1.0\"\n\
             [dependencies]\n\
             http = { git = \"https://x/guzzle/http\", tag = \"v1.0.0\" }\n\
             [targets.dev.tiers]\n\
             test = \"std\"\n",
        )
        .expect("valid");
        assert_eq!(m.package().unwrap().name.company, "acme");
        assert!(m.dependencies().contains_key("http"));
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["test"]);
    }

    #[test]
    fn parses_and_resolves_a_simple_target() {
        let m = Manifest::parse(
            "[targets.dev.tiers]\n\
             test = \"std\"\n\
             debug = \"std\"\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["debug", "test"]);
    }

    #[test]
    fn table_provider_form_is_accepted() {
        let m = Manifest::parse(
            "[targets.dev.tiers]\n\
             bench = { package = \"std\", samples = 100 }\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["bench"]);
    }

    #[test]
    fn extends_merges_base_then_overrides() {
        let m = Manifest::parse(
            "[targets.base.tiers]\n\
             test = \"std\"\n\
             doc = \"std\"\n\
             [targets.ci]\n\
             extends = \"base\"\n\
             [targets.ci.tiers]\n\
             bench = \"std\"\n",
        )
        .expect("valid manifest");
        // ci inherits test+doc from base and adds bench.
        assert_eq!(m.active_tiers("ci").unwrap(), vec!["bench", "doc", "test"]);
        // A minimalist target opts into nothing.
        let empty = Manifest::parse("[targets.prod]\n").expect("valid manifest");
        assert!(empty.active_tiers("prod").unwrap().is_empty());
    }

    #[test]
    fn tier_names_are_open_but_must_be_identifiers() {
        // The tier name-space is open (tier-providers T3): a non-builtin name is accepted — it may
        // be a tier a dependency declares with `@tier`; whether it resolves is activation's check.
        let m = Manifest::parse("[targets.dev.tiers]\nfuzz = \"std\"\n").expect("open name-space");
        assert_eq!(m.active_tiers("dev").unwrap(), ["fuzz"]);
        // Only the *shape* is validated here: a non-identifier key is rejected.
        let err = Manifest::parse("[targets.dev.tiers]\n\"fu zz\" = \"std\"\n").unwrap_err();
        assert!(err.contains("not a valid tier name"), "{err}");
    }

    #[test]
    fn an_undeclared_provider_is_rejected() {
        // A provider that is neither `std` nor a declared dependency is an error.
        let err = Manifest::parse("[targets.dev.tiers]\nbench = \"criterion\"\n").unwrap_err();
        assert!(err.contains("declared dependency"), "{err}");
    }

    #[test]
    fn a_declared_dependency_may_provide_a_tier() {
        // package-manager P2.6: a resolved dependency (`bench_kit`) is a valid tier provider.
        let m = Manifest::parse(
            "[dependencies]\n\
             bench_kit = { path = \"../bench_kit\" }\n\
             [targets.dev.tiers]\n\
             bench = \"bench_kit\"\n\
             test = \"std\"\n",
        )
        .expect("valid");
        let providers = m.active_tier_providers("dev").unwrap();
        assert_eq!(providers["bench"], "bench_kit"); // provided by the dependency
        assert_eq!(providers["test"], "std"); // still the built-in
    }

    #[test]
    fn unknown_target_is_an_error() {
        let m = Manifest::parse("[targets.dev.tiers]\ntest = \"std\"\n").unwrap();
        assert!(
            m.active_tiers("nope")
                .unwrap_err()
                .contains("unknown target")
        );
    }

    // --- `noeta add` manifest editing (package-manager P2.4d) ----------------------------------

    #[test]
    fn insert_uses_an_existing_dependencies_table() {
        let text = "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n\n[dependencies]\nx = \"^1\"\n";
        let out = insert_dependency_entry(text, "y = { path = \"../y\" }");
        // The new entry lands under the header, and both deps parse.
        let m = Manifest::parse(&out).expect("valid");
        assert!(m.dependencies().contains_key("x"));
        assert!(m.dependencies().contains_key("y"));
        assert!(out.contains("[dependencies]\ny = { path = \"../y\" }"));
    }

    #[test]
    fn insert_appends_a_new_section_when_absent() {
        let text = "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n";
        let out = insert_dependency_entry(text, "y = \"^2\"");
        let m = Manifest::parse(&out).expect("valid");
        assert_eq!(
            m.dependencies()["y"],
            Dependency::Registry {
                package: None,
                req: semver::VersionReq::parse("^2").unwrap()
            }
        );
    }

    #[test]
    fn a_registry_dependency_carries_an_optional_package_identity() {
        let m = Manifest::parse(
            "[dependencies]\nwebclient = { version = \"^1.2\", package = \"guzzle/http\" }\n",
        )
        .expect("valid");
        match &m.dependencies()["webclient"] {
            Dependency::Registry { package, req } => {
                let package = package.as_ref().expect("package present");
                assert_eq!(package.company, "guzzle");
                assert_eq!(package.package, "http");
                assert_eq!(req, &semver::VersionReq::parse("^1.2").unwrap());
            }
            other => panic!("expected a registry dependency, got {other:?}"),
        }
        // A malformed identity is rejected at parse time.
        assert!(
            Manifest::parse("[dependencies]\nx = { version = \"^1\", package = \"nope\" }\n")
                .is_err()
        );
    }

    #[test]
    fn add_dependency_writes_and_rejects_duplicates() {
        let dir = std::env::temp_dir().join("noeta_add_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MANIFEST_NAME);
        std::fs::write(&path, "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();

        add_dependency(&path, "http", "{ git = \"https://x/y\", tag = \"v1.0.0\" }").unwrap();
        let m = Manifest::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(matches!(m.dependencies()["http"], Dependency::Git { .. }));

        // A second add of the same key is rejected (and a non-identifier key too).
        assert!(add_dependency(&path, "http", "\"^1\"").is_err());
        assert!(add_dependency(&path, "bad-key", "\"^1\"").is_err());
    }

    #[test]
    fn inheritance_cycle_is_detected() {
        let m = Manifest::parse("[targets.a]\nextends = \"b\"\n[targets.b]\nextends = \"a\"\n")
            .unwrap();
        assert!(m.active_tiers("a").unwrap_err().contains("cycle"));
    }

    #[test]
    fn target_scoped_dependencies_overlay_the_globals() {
        let m = Manifest::parse(
            "[dependencies]\nliveview = { path = \"../liveview\" }\n\
             [targets.dev.dependencies]\nlinter = { git = \"https://x/lint\", tag = \"v1.0.0\" }\n",
        )
        .expect("valid");
        // No target → globals only.
        let base = m.active_dependencies(None).unwrap();
        assert!(base.contains_key("liveview") && !base.contains_key("linter"));
        // `dev` → globals + its scoped deps.
        let dev = m.active_dependencies(Some("dev")).unwrap();
        assert!(dev.contains_key("liveview") && dev.contains_key("linter"));
        // A prod target with no scoped deps (undeclared) is lenient → globals only.
        let prod = m.active_dependencies(Some("prod")).unwrap();
        assert!(prod.contains_key("liveview") && !prod.contains_key("linter"));
    }

    #[test]
    fn extends_inherits_target_scoped_dependencies() {
        let m = Manifest::parse(
            "[targets.dev.dependencies]\nlinter = { path = \"../lint\" }\n\
             [targets.ci]\nextends = \"dev\"\n[targets.ci.dependencies]\ncov = { path = \"../cov\" }\n",
        )
        .expect("valid");
        let ci = m.active_dependencies(Some("ci")).unwrap();
        assert!(ci.contains_key("linter"), "ci should inherit dev's linter");
        assert!(ci.contains_key("cov"), "ci should have its own cov");
    }

    #[test]
    fn a_tier_provider_may_be_a_target_scoped_dependency() {
        // `dev`'s `@lint` tier is provided by a dep declared only under `dev`.
        let m = Manifest::parse(
            "[targets.dev.dependencies]\nlinter = { path = \"../lint\" }\n\
             [targets.dev.tiers]\nlint = \"linter\"\n",
        );
        assert!(m.is_ok(), "target-scoped provider should validate: {m:?}");
        // Still rejected when the provider is declared nowhere.
        assert!(Manifest::parse("[targets.dev.tiers]\nlint = \"ghost\"\n").is_err());
    }
}
