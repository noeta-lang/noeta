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
//! Two axes, kept apart. **Which provider** declares each tier a package uses is a per-package name
//! table, `[tiers]` — the tier counterpart of `[directives]` (`local = "provider[:exported]"`, the
//! provider being `"std"` or a `[dependencies]` key). **Which of those are live** in a given build is
//! the target's `[targets.<t>.tiers]` — an activation live-set of the package's own local tier names:
//!
//! ```toml
//! [tiers]
//! test  = "std"                 # the std-declared `test` tier, named `@test` locally
//! debug = "std"
//! bench = "std"
//! fuzz  = "fuzzkit"             # a dependency's `@tier`, named `@fuzz` locally
//! crit  = "criterion:bench"     # …renamed, so it does not collide with std's `bench`
//!
//! [targets.dev.tiers]
//! test  = true                  # live in `dev`
//! debug = true
//!
//! [targets.ci]
//! extends = "dev"               # inherit dev's live set…
//! [targets.ci.tiers]
//! debug = false                 # …turn one off, and
//! bench = true                  # …turn one on
//! ```
//!
//! There are no ambient built-in tiers: `test`/`bench`/`doc`/`debug` are ordinary `std`
//! [`ExtTier`](noeta_ext_abi::registry::ExtTier) declarations a package names in `[tiers]` like any
//! other provider's. A target's *active tiers* are the local names its (inheritance-merged) live-set
//! marks `true`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::PmError;

// The `[fmt]` grammar lives in `noeta-fmt` (dev tooling), so reading a manifest's formatter config
// pulls in the formatter crate — gated behind `fmt-config` (dev-deps D3c) so a lean runtime that only
// needs tier/dependency resolution never links it.
#[cfg(feature = "fmt-config")]
use noeta_fmt::FmtConfig;

/// The manifest file name, discovered at or above the entry file's directory.
///
/// Defined once, in `noeta-loader`: the loader needs the *name* as the pruning predicate for the
/// package walk (a subdirectory holding one is a different package), and two spellings of the file
/// that decides what a package is could drift. Everything the manifest *means* is still here.
pub use noeta_loader::MANIFEST_NAME;

/// The built-in/stdlib tier provider — always available; every other provider must be a declared
/// `[dependencies]` key.
pub(crate) const BUILTIN_PROVIDER: &str = "std";

/// A parsed `noeta.toml`: the package's identity (`[package]`, absent for a bare script), its
/// declared dependencies (`[dependencies]`, keyed by **import root**), and its build targets.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    package: Option<PackageMeta>,
    dependencies: BTreeMap<String, Dependency>,
    /// The `[directives]` table: the extension `@`-directives this package's own source uses, each a
    /// local `@name` → the dependency (by this package's import-root key) that provides it and the name
    /// it exported. Per-package (see [`UseBinding`]); resolution of a `@name` is scoped to the package
    /// that wrote it. Authorization to run the provider's native code stays root-only (`[trust].native`).
    directives: BTreeMap<String, UseBinding>,
    /// The `[tiers]` table: the dev-tiers this package's own source writes as `@name { … }`, each a
    /// local `@name` → the provider that declares it and the name it exported. The provider is one of
    /// **this package's** `[dependencies]` keys or the built-in `"std"` (which declares `test`/`bench`/
    /// `doc`/`debug` as ordinary [`ExtTier`](noeta_ext_abi::registry::ExtTier)s — there are no ambient
    /// built-in tiers; a package names the std tiers it uses here, like any other provider). Per-package
    /// (see [`UseBinding`]) so two packages can name the same provider's tier differently, or rename to
    /// avoid a collision between two providers' same-named tiers. Which of these are *live* in a build is
    /// a separate axis: `[targets.<t>.tiers]` (a live-set of these local names). Authorization to run a
    /// provider's native tier runner stays root-only (`[trust].native`); this table only names.
    tiers: BTreeMap<String, UseBinding>,
    targets: BTreeMap<String, Target>,
    trust: Trust,
    registries: Registries,
    db: DbConfig,
    /// The `[patch]` table (dev-time path override): package identity (`company/package`) → the
    /// local tree that replaces it. Parsed for every manifest, but **honored only from the root
    /// app's** — the resolver never reads a dependency's `[patch]` (no inheritance, the same
    /// top-down authority rule as `[trust]`).
    patch: BTreeMap<String, PathBuf>,
}

/// The `[registries]` table (private-registries arc) — a map from a **scope** (`company`) to the
/// registry that scope's packages resolve from, plus an optional `default` for every other scope. Lets
/// a project mix the public hosted registry with private ones (e.g. a whole GitHub org) without making
/// everything private: `acme/*` can come from `github:acme` while everything else stays on the default.
/// Empty = the single default registry for everything — the built-in hosted index at
/// `registry.noeta.dev`, or the `NOETA_REGISTRY_URL`/`NOETA_REGISTRY_DIR` environment overrides.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registries {
    /// The fallback for scopes with no explicit mapping. `None` = the environment default.
    default: Option<RegistrySource>,
    /// scope (`company`) → its registry source.
    by_scope: BTreeMap<String, RegistrySource>,
}

impl Registries {
    /// The registry source for `scope` (a `company` segment): its explicit mapping, else the `default`
    /// mapping, else `None` (meaning "use the environment default registry").
    pub fn source_for(&self, scope: &str) -> Option<&RegistrySource> {
        self.by_scope.get(scope).or(self.default.as_ref())
    }

    /// Every distinct source configured (for auditing/UX).
    pub fn all(&self) -> impl Iterator<Item = (&str, &RegistrySource)> {
        self.default
            .iter()
            .map(|s| ("default", s))
            .chain(self.by_scope.iter().map(|(k, v)| (k.as_str(), v)))
    }

    /// Whether any mapping is configured.
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.by_scope.is_empty()
    }
}

/// Where a scope's packages resolve from (private-registries arc). Crypto-/IO-free: the manifest layer
/// only parses the *shape*; opening the concrete index happens in the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySource {
    /// A hosted noeta-registry service at this base URL (a bare `http(s)://…`).
    Hosted(String),
    /// A **git forge** used as a registry — any git host (GitHub, GitLab, Gitea/Forgejo, a bare git
    /// server). `company/package` → `<base>/<package>`, versions = semver git tags. The stored string
    /// is the org/group **base URL** (e.g. `https://github.com/acme`); the shorthands `github:<owner>`,
    /// `gitlab:<group>`, and `git:<url>` all parse to this.
    GitForge(String),
}

impl RegistrySource {
    /// Parse a `[registries]` value string. Four forms:
    /// - `github:<owner>` → the git forge `https://github.com/<owner>`
    /// - `gitlab:<group>` → the git forge `https://gitlab.com/<group>` (nested groups allowed)
    /// - `git:<url>` → the git forge at that base URL verbatim (self-hosted Gitea/GitLab/Forgejo, SSH,
    ///   or a `file://`/local path for tests)
    /// - a bare `http(s)://…` → a hosted noeta-registry service
    pub fn parse(s: &str) -> Result<RegistrySource, PmError> {
        Self::parse_inner(s).map_err(PmError::Manifest)
    }

    /// The string-formatting body of [`Self::parse`] — a manifest-value validator, so every
    /// failure is one kind ([`PmError::Manifest`]), applied once at the public boundary.
    fn parse_inner(s: &str) -> Result<RegistrySource, String> {
        let s = s.trim();
        if let Some(owner) = s.strip_prefix("github:") {
            Ok(RegistrySource::GitForge(format!(
                "https://github.com/{}",
                forge_owner("github", owner)?
            )))
        } else if let Some(group) = s.strip_prefix("gitlab:") {
            // GitLab groups may nest (`group/subgroup`), so a slash is allowed here.
            Ok(RegistrySource::GitForge(format!(
                "https://gitlab.com/{}",
                forge_group("gitlab", group)?
            )))
        } else if let Some(base) = s.strip_prefix("git:") {
            let base = base.trim().trim_end_matches('/');
            if base.is_empty() || base.contains(char::is_whitespace) {
                return Err(format!("`git:` registry needs a base URL (got `{s}`)"));
            }
            Ok(RegistrySource::GitForge(base.to_string()))
        } else if s.starts_with("http://") || s.starts_with("https://") {
            Ok(RegistrySource::Hosted(s.trim_end_matches('/').to_string()))
        } else {
            Err(format!(
                "registry source `{s}` must be `github:<owner>`, `gitlab:<group>`, `git:<url>`, or an \
                 http(s):// URL"
            ))
        }
    }
}

/// Validate a forge owner/user segment (no slashes) for a shorthand.
fn forge_owner(scheme: &str, owner: &str) -> Result<String, String> {
    let owner = owner.trim();
    if owner.is_empty()
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "`{scheme}:` registry needs a valid owner name (got `{owner}`)"
        ));
    }
    Ok(owner.to_string())
}

/// Validate a forge group path (slashes allowed, for nested groups) for a shorthand.
fn forge_group(scheme: &str, group: &str) -> Result<String, String> {
    let group = group.trim().trim_matches('/');
    let ok = !group.is_empty()
        && group.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if !ok {
        return Err(format!(
            "`{scheme}:` registry needs a valid group path (got `{group}`)"
        ));
    }
    Ok(group.to_string())
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
    /// The commands a dependency may contribute, each bound to the **local name** it appears under
    /// (`noeta <local>`). Keyed by that local name → the providing package and the command it
    /// exported (`[trust.commands]` / `migrate = "para/db"`, or `undo = "para/db:rollback"` to
    /// rename). The binding *is* the grant: an entry both authorizes the provider to contribute this
    /// one command and fixes the name it appears under, so two packages exporting the same command
    /// name coexist under distinct local names rather than colliding. A command from a package with
    /// no entry here is never registered (a capability the user never asked for).
    pub commands: std::collections::BTreeMap<String, Binding>,
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
    /// A **publish cooldown** window in seconds (namespace-protection #1): a registry release published
    /// more recently than this is not *newly selected* during resolution — so an advisory or a yank can
    /// catch a compromised release before it auto-propagates to consumers. An existing lockfile pin is
    /// unaffected (already your choice); only fresh selection is held back. `None` = off (default).
    pub publish_cooldown: Option<u64>,
    /// Per-advisory-tier policy (advisory-intake arc, tier 5): whether an advisory of a given intake
    /// tier (`operator`/`publisher`/`imported`) makes `noeta audit` **fail** the build, merely **warn**,
    /// or is ignored (`off`). Default: every tier warns; a project opts a tier up to `fail` for CI.
    pub advisories: AdvisoryPolicy,
}

/// A single entry in a `[trust.commands]` (or, from Slice 2, `[trust.directives]`) table: the
/// providing package and the name that package exported the command/directive under. The table
/// **key** is the local name the project uses; this is the resolved right-hand side. Written
/// `local = "company/package"` (exported name == local name) or `local = "company/package:exported"`
/// to rename — the first `:` splits the identity (which contains a `/`, never a `:`) from the
/// exported name, so the exported half may itself contain any character (a command name may hold a
/// space, e.g. `remote add`). The binding is both the authorization and the name mapping in one
/// entry, so a package is never named twice to be trusted and bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The providing package identity (`company/package`) — the same identity `[trust].native`
    /// authorizes. Validated as a [`PackageName`] at parse time so a typo fails loudly.
    pub provider: String,
    /// The name the provider declared the command/directive under (its `ExtCommand`/`ExtDirective`
    /// `name`). Equal to the local key unless a `:exported` suffix renamed it.
    pub exported: String,
}

/// A per-package `[directives]` / `[tiers]` binding: the local `@name` a package's own source writes,
/// resolved to a provider named by one of **this package's** dependency import-root keys, and the name
/// that provider exported. Written `local = "para"` (exported name == local) or `local = "para:html"`
/// to rename.
///
/// Unlike [`Binding`] (`[trust.commands]`: root-only, keyed by global package **identity**), these are
/// **per-package** and keyed by the using package's own dependency **keys** — the same context a
/// `use <key>.…` resolves in. So two packages can each name the tier/directive they use from a shared
/// provider differently, and a package renames to avoid a collision between two providers' same-named
/// tiers. Authorization is separate and stays root-only (`[trust].native`); this table only names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseBinding {
    /// A dependency import-root key of the package this table belongs to (a `[dependencies]` key), or
    /// the built-in provider `"std"` where the table permits it (`[tiers]`, for `test`/`debug`/…).
    pub provider_key: String,
    /// The tier/directive name the provider declared. Equal to the local key unless `:exported` renamed it.
    pub exported: String,
}

/// The `[db]` table — a project's default database wiring for `noeta migrate` (and any tooling that
/// wants a declared DSN). Every key is optional: `url` is the connection string (the same dsn schemes
/// `db.connect` accepts), `migrations` is the directory holding the `.sql` migration files (default
/// `migrations/`), and `seeds` is the directory holding the re-runnable `.sql` seed files (default
/// `seeds/`). The CLI layers a `--db`/`--dir`/`--seeds-dir` flag and the `DATABASE_URL` env var over
/// these, so a project can declare a default here and still override per-invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbConfig {
    /// The database connection string, if declared. `None` = not configured here (resolve from a flag
    /// or `DATABASE_URL`).
    pub url: Option<String>,
    /// The migrations directory, if declared. `None` = the default (`migrations/`).
    pub migrations: Option<String>,
    /// The seeds directory, if declared. `None` = the default (`seeds/`).
    pub seeds: Option<String>,
}

/// What a matched advisory of a given intake tier does to an `noeta audit` run (advisory-intake arc,
/// tier 5). Default [`AdvisoryAction::Warn`] — surfaced, but not a build failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdvisoryAction {
    /// A matched advisory of this tier is printed but does not fail the audit.
    #[default]
    Warn,
    /// A matched advisory of this tier fails the audit (a non-zero exit — a CI gate).
    Fail,
    /// Advisories of this tier are ignored entirely (neither printed as a hit nor counted).
    Off,
}

impl AdvisoryAction {
    fn parse(s: &str) -> Result<AdvisoryAction, String> {
        match s {
            "warn" => Ok(AdvisoryAction::Warn),
            "fail" => Ok(AdvisoryAction::Fail),
            "off" => Ok(AdvisoryAction::Off),
            other => Err(format!(
                "`{other}` must be one of \"fail\", \"warn\", \"off\""
            )),
        }
    }
}

/// The consumer's per-tier advisory policy (advisory-intake arc, tier 5). Which intake tiers act as
/// build **failures** versus **warnings** in `noeta audit`. Configured under `[trust.advisories]`
/// (per-tier keys) or a bare `advisories = "fail"` (all tiers). Default: all three warn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdvisoryPolicy {
    pub operator: AdvisoryAction,
    pub publisher: AdvisoryAction,
    pub imported: AdvisoryAction,
}

impl AdvisoryPolicy {
    /// The action for an advisory whose intake tier is the wire string `tier` (`operator`/`publisher`/
    /// `imported`). An unknown/absent tier is treated as `operator` (the default tier a feed serves).
    pub fn action_for(&self, tier: &str) -> AdvisoryAction {
        match tier {
            "publisher" => self.publisher,
            "imported" => self.imported,
            _ => self.operator,
        }
    }
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
    /// The pinned language [`Edition`] (follow-on arc F1). `None` when the package omits `edition`,
    /// which the toolchain treats as [`Edition::DEFAULT`]; the value is validated at parse time
    /// (an unknown edition is a manifest error), so a present value is always a known edition.
    pub edition: Option<crate::edition::Edition>,
    /// The **minimum toolchain** this package works with (`toolchain = ">=0.2"`) — a SemVer
    /// requirement the *running `noeta` binary's* version must satisfy at resolve time. Without
    /// it, a too-old binary composing a too-new package dies deep in a Rust compile (or a checker
    /// error) instead of saying "upgrade noeta". `None` means the package makes no claim — it is
    /// an author-side promise, not a sandbox: the real compatibility contract is the extension
    /// ABI, this field only turns a violation into a clear, early message.
    pub toolchain: Option<semver::VersionReq>,
    /// The relative directory of this package's native Rust **entry crate** (package-manager
    /// Phase 3, N3.1): `native = "native"` points at a `Cargo.toml` whose crate exports the
    /// package's extension units (one crate, any number of units — std's own shape). `None` for a
    /// pure-Noeta package. Declaring native code is deliberately explicit — it pulls arbitrary
    /// Rust into a consumer's build, which should never be triggered by the mere presence of a
    /// directory.
    pub native: Option<String>,
    /// The declared license as an **SPDX expression** (`license = "MIT OR Apache-2.0"`). Sent with
    /// a publish and recorded in the registry's immutable release record (and its transparency-log
    /// leaf). `None` when the package declares none. Shape-checked at parse time; the claim is the
    /// publisher's — consumers can check the SHA-pinned source's LICENSE file.
    pub license: Option<String>,
    /// Discovery keywords (`keywords = ["image", "simd"]`) — up to [`MAX_KEYWORDS`] topic tags the
    /// registry indexes so a package is findable by what it is *for*, not just by its name. Sent
    /// with a publish and part of the immutable release record, but — unlike [`license`] — **not**
    /// bound into the transparency-log leaf: tampering with a keyword mis-files a package in a
    /// listing, it cannot redirect a build or misrepresent a legal claim.
    ///
    /// A set: empty when the package declares none, and stored deduplicated and sorted so the
    /// declared order never matters. Shape-checked at parse time.
    ///
    /// [`license`]: PackageMeta::license
    pub keywords: Vec<String>,
    /// A one-line description (`description = "Fast image effects for Noeta"`) — the blurb the
    /// registry shows in package-search results. Sent with a publish and part of the immutable
    /// release record, but — like [`keywords`] and unlike [`license`] — **not** bound into the
    /// transparency-log leaf: it is discovery prose, not a claim a consumer resolves against.
    /// `None` when the package declares none. Shape-checked at parse time (single line, bounded).
    ///
    /// [`license`]: PackageMeta::license
    /// [`keywords`]: PackageMeta::keywords
    pub description: Option<String>,
}

impl PackageMeta {
    /// The **effective** language edition this package compiles under — its pinned [`Edition`], or
    /// [`Edition::DEFAULT`] when it declared none. The one place the rest of the toolchain reads an
    /// edition, so the default is applied consistently.
    pub fn edition(&self) -> crate::edition::Edition {
        self.edition.unwrap_or_default()
    }
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
    pub fn parse(s: &str) -> Result<PackageName, PmError> {
        let (company, package) = s.split_once('/').ok_or_else(|| {
            PmError::Manifest(format!(
                "package name `{s}` must be `company/package` (missing `/`)"
            ))
        })?;
        if package.contains('/') {
            return Err(PmError::Manifest(format!(
                "package name `{s}` must have exactly one `/` (found more)"
            )));
        }
        if !is_identifier(company) || !is_identifier(package) {
            return Err(PmError::Manifest(format!(
                "package name `{s}`: `company` and `package` must each be identifiers \
                 (letters, digits, `_`; not starting with a digit)"
            )));
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
/// Which git reference a `git` dependency tracks. A **tag** is the release model (a published,
/// immutable version); a **branch** or bare **HEAD** tracks a moving ref — for an in-development or
/// bundled package that isn't cut into tagged releases yet (follow-on: `git` deps without a tag). In
/// every case the lockfile pins the resolved commit SHA, so a build reproduces exactly; `noeta
/// update` re-resolves a branch/HEAD ref to its latest commit (a tag re-resolves to the same commit
/// unless it moved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRef {
    /// A tagged release — `dep = { git = "…", tag = "v1.2.0" }`.
    Tag(String),
    /// A branch's current tip — `dep = { git = "…", branch = "main" }`.
    Branch(String),
    /// The remote's default-branch `HEAD` — `dep = { git = "…" }` with no tag/branch (the
    /// tag-free in-dev/bundled case).
    Head,
}

impl GitRef {
    /// A short human description for messages and the lockfile ref comment (`v1.2.0`, `branch main`,
    /// `HEAD`).
    pub fn describe(&self) -> String {
        match self {
            GitRef::Tag(t) => t.clone(),
            GitRef::Branch(b) => format!("branch {b}"),
            GitRef::Head => "HEAD".to_string(),
        }
    }

    /// The lockfile-pin key component (paired with the url) — a stable, kind-prefixed string so a tag
    /// named `main` and the branch `main` never share a pin. Recomputed identically on lock read and
    /// on the resolve-time pin lookup, so a locked SHA is found again.
    pub fn lock_key(&self) -> String {
        match self {
            GitRef::Tag(t) => format!("tag:{t}"),
            GitRef::Branch(b) => format!("branch:{b}"),
            GitRef::Head => "head".to_string(),
        }
    }
}

/// root (an identifier), decoupled from the resolved package's global `company/package` identity.
#[derive(Debug, Clone, PartialEq)]
pub enum Dependency {
    /// A local source tree — `dep = { path = "…" }`. Needs no network or resolver (P2.1).
    Path { path: PathBuf },
    /// A git repository pinned to a [`GitRef`] — a `tag` (a released version), a `branch`, or the
    /// default-branch `HEAD` (`dep = { git = "…" }`, the tag-free in-dev/bundled case). The lockfile
    /// pins the resolved SHA either way, so a build reproduces exactly.
    Git { url: String, git_ref: GitRef },
    /// A registry dependency by SemVer requirement — `dep = "^1.2"` or
    /// `dep = { version = "^1.2", package = "company/pkg" }`. The registry index resolves
    /// name→git-coords (P2.5). `package` is the registry identity (decoupled from the import-root
    /// key, like Rust's `foo = { package = "real" }`); it is **required** to resolve — the bare
    /// shorthand leaves it `None` and errors at resolution with a pointer to add it.
    Registry {
        package: Option<PackageName>,
        req: semver::VersionReq,
    },
    /// A **scope** dependency — `para = [ { path = … }, { path = … } ]` — several packages that share
    /// one namespace scope, bound under one import-root key. This is what lets an app depend on more
    /// than one package of the same scope (`para/aether` *and* `para/db`) without two colliding TOML
    /// keys: the key is the shared scope root, and each member is a package under it. Every member
    /// package must share a single `company` segment (the scope), and that scope re-roots to the key —
    /// so the members address as `<key>.<member-package>.…` in the flat link pool (an identity re-root
    /// when the key already *is* the scope, the usual case; an alias otherwise). A member is any
    /// non-scope source (`path`/`git`/`version`); scopes do not nest.
    Scope(Vec<Dependency>),
}

#[derive(Debug, Clone, PartialEq)]
struct Target {
    /// The base target this one inherits tiers and dependencies from (`extends = "dev"`), if any.
    extends: Option<String>,
    /// This target's own tier **activation** entries — a local `[tiers]` name → live (`true`) or
    /// explicitly off (`false`, to turn off a tier an `extends` base left on). Overlaid on the base's
    /// during resolution; the provider each name resolves to lives in the package's `[tiers]` table,
    /// not here (that separation is the point — activation is a live-set, not a provider map).
    tiers: BTreeMap<String, bool>,
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
pub fn load(manifest_path: &Path) -> Result<Manifest, PmError> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", manifest_path.display())))?;
    Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", manifest_path.display())))
}

/// The `[package]` identity (`company/package`) and version of the manifest at `manifest_path`
/// (package-manager P2.5, backing `noeta publish`). Errors when the manifest can't be read/parsed or
/// declares no `[package]` (a bare script can't be published).
pub fn current_package(manifest_path: &Path) -> Result<(String, semver::Version), PmError> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", manifest_path.display())))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", manifest_path.display())))?;
    let pkg = manifest.package().ok_or_else(|| {
        PmError::Manifest(format!(
            "`{}` has no `[package]` table — only a package (with a name + version) can be published",
            manifest_path.display()
        ))
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
/// write is atomic. Errors if `key` is not an identifier.
///
/// When `key` is **already** a dependency, the entry is widened into a
/// [scope dependency][`Dependency::Scope`] rather than refused: the existing value and `value_toml`
/// become the first two members of a `key = [ … ]` array (and a key that is already an array gains
/// one more member). That is the manifest shape for binding several packages of one scope —
/// `para/aether` *and* `para/db` — under a single import root, so `noeta add para --package para/db`
/// after `noeta add para --package para/aether` does the expected thing. Adding the *same* source
/// twice is still refused.
pub fn add_dependency(manifest_path: &Path, key: &str, value_toml: &str) -> Result<(), PmError> {
    if !is_identifier(key) {
        return Err(PmError::Manifest(format!(
            "dependency key `{key}` must be an identifier (it becomes the import root — `use {key}.…`)"
        )));
    }
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", manifest_path.display())))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", manifest_path.display())))?;

    let updated = if manifest.dependencies().contains_key(key) {
        extend_scope_entry(&text, key, value_toml)?
    } else {
        insert_dependency_entry(&text, &format!("{key} = {value_toml}"))
    };
    // Re-parse the edited manifest so a bad value/source fails here rather than corrupting the file.
    Manifest::parse(&updated).map_err(|err| {
        err.map_msg(|m| format!("`noeta add {key}` would make `{MANIFEST_NAME}` invalid: {m}"))
    })?;

    let dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".{MANIFEST_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &updated)
        .and_then(|()| std::fs::rename(&tmp, manifest_path))
        .map_err(|err| PmError::Io(format!("cannot write `{}`: {err}", manifest_path.display())))
}

/// Widen `text`'s existing `[dependencies]` entry for `key` into a **scope array** carrying
/// `value_toml` as one more member (see [`add_dependency`]). A single-source entry becomes a
/// two-member array; an entry that is already an array gains an element. The existing value's own
/// text is reused verbatim, so member formatting and any trailing comment survive; only the one
/// entry is rewritten.
fn extend_scope_entry(text: &str, key: &str, value_toml: &str) -> Result<String, PmError> {
    let (span, value_text) = find_dependency_entry(text, key).ok_or_else(|| {
        // The parse said the key is a dependency, so it is in a `[dependencies]` table we could not
        // locate textually — a dotted-key or inline-table spelling we do not rewrite.
        PmError::Manifest(format!(
            "dependency `{key}` is already in the manifest, and its entry is not in a form \
             `noeta add` can extend — edit `{MANIFEST_NAME}` by hand to make `{key}` a scope array \
             (`{key} = [ {{ … }}, {value_toml} ]`)"
        ))
    })?;

    // Re-adding the identical source is a mistake, not a scope: an array with two equal members
    // would resolve one package twice under one root.
    let members: Vec<String> = match toml::from_str::<toml::Table>(&format!("v = {value_text}"))
        .ok()
        .and_then(|t| t.get("v").and_then(|v| v.as_array()).cloned())
    {
        // Already an array: keep each member's own text by re-splitting the value, then append.
        Some(_) => split_array_members(&value_text),
        None => vec![value_text.trim().to_string()],
    };
    if members.iter().any(|m| m == value_toml.trim()) {
        return Err(PmError::Manifest(format!(
            "dependency `{key}` already binds this exact source — nothing to add"
        )));
    }

    let added = value_toml.trim().to_string();
    let mut entry = format!("{key} = [\n");
    for member in members.iter().chain(std::iter::once(&added)) {
        entry.push_str(&format!("    {member},\n"));
    }
    entry.push(']');

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines.splice(span.clone(), entry.lines().map(str::to_string));
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Locate `key`'s entry in `text`'s top-level `[dependencies]` table: its line range and the raw
/// text of its value (everything right of the `=`, joined across lines for a multi-line array).
/// `None` when there is no such plain `key = value` entry — including one under a
/// `[targets.<name>.dependencies]` table, which is a different table and never rewritten here.
fn find_dependency_entry(text: &str, key: &str) -> Option<(std::ops::Range<usize>, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut in_dependencies = false;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]";
            continue;
        }
        if !in_dependencies {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(value_start) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        // Accumulate lines until the value parses — the cheap way to find where a multi-line array
        // ends without re-implementing TOML's grammar.
        let mut value = value_start.trim().to_string();
        for (j, more) in lines.iter().enumerate().skip(i + 1) {
            if toml::from_str::<toml::Table>(&format!("v = {value}")).is_ok() {
                return Some((i..j, value));
            }
            value.push('\n');
            value.push_str(more);
        }
        if toml::from_str::<toml::Table>(&format!("v = {value}")).is_ok() {
            return Some((i..lines.len(), value));
        }
        return None;
    }
    None
}

/// Split a TOML array's raw text into its members' own raw texts, so an existing scope array can be
/// rewritten with one more member without re-serializing (and reformatting) the ones already there.
/// Commas inside a member's braces, brackets or strings do not split.
fn split_array_members(array_text: &str) -> Vec<String> {
    let inner = array_text.trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inner);
    let mut members = Vec::new();
    let mut depth = 0usize;
    let mut in_string: Option<char> = None;
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match in_string {
            Some(quote) => {
                if ch == '\\' && quote == '"' {
                    current.push(ch);
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                    continue;
                }
                if ch == quote {
                    in_string = None;
                }
            }
            None => match ch {
                '"' | '\'' => in_string = Some(ch),
                '{' | '[' => depth += 1,
                '}' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    let member = current.trim();
                    if !member.is_empty() {
                        members.push(member.to_string());
                    }
                    current.clear();
                    continue;
                }
                _ => {}
            },
        }
        current.push(ch);
    }
    let member = current.trim();
    if !member.is_empty() {
        members.push(member.to_string());
    }
    members
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
pub fn resolve_active_tiers(entry: &Path, target: &str) -> Result<Vec<String>, PmError> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let path = find(dir).ok_or_else(|| {
        PmError::NoManifest(format!(
            "no `{MANIFEST_NAME}` found at or above `{}` (needed for `--target {target}`)",
            dir.display()
        ))
    })?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", path.display())))?;
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
) -> Result<BTreeMap<String, String>, PmError> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let path = find(dir).ok_or_else(|| {
        PmError::NoManifest(format!(
            "no `{MANIFEST_NAME}` found at or above `{}` (needed for `--target {target}`)",
            dir.display()
        ))
    })?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", path.display())))?;
    manifest.active_tier_providers(target)
}

/// The root package's `[tiers]` provider map for the `noeta.toml` discovered at or above `entry`
/// ([`Manifest::tier_provider_map`]) — who provides each tier the root names, target-independent. A
/// bare script with no manifest has no `[tiers]`, so this yields an empty map (its tiers resolve
/// ambiently). This is what the tier-execution layer dispatches on and the compile cache key folds in.
pub fn resolve_tier_providers(entry: &Path) -> Result<BTreeMap<String, String>, PmError> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(path) = find(dir) else {
        return Ok(BTreeMap::new());
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
    let manifest = Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", path.display())))?;
    Ok(manifest.tier_provider_map())
}

/// Gather the entry's **dependency packages** as loader [`DepPackage`]s (package-manager P2.1/P2.4):
/// resolve the full transitive dependency graph and hand back the re-rooted packages the loader links.
/// No manifest, or no `[dependencies]`, yields an empty list (a bare script has no deps). The graph
/// walk ([`crate::graph`]) materializes each package (a `path` tree, a fetched `git` tag; a `registry`
/// dependency errors pending P2.5), dedups by identity, and assigns global segments so transitive
/// `use`s link without key collision.
pub fn dependency_packages(entry: &Path) -> Result<Vec<noeta_loader::DepPackage>, PmError> {
    Ok(crate::graph::resolve_graph(entry)?.packages)
}

/// As [`dependency_packages`], but resolving for a build **target** (dev-deps D2): the root's
/// dependency set is [`Manifest::active_dependencies`] — the global `[dependencies]` plus the
/// target's own and inherited `[targets.<name>.dependencies]`. This is what makes a declared
/// dev-only dependency actually LINK under `--target dev`; `None` is the global set.
pub fn dependency_packages_for(
    entry: &Path,
    target: Option<&str>,
) -> Result<Vec<noeta_loader::DepPackage>, PmError> {
    Ok(crate::graph::resolve_graph_for(entry, target)?.packages)
}

/// As [`dependency_packages_for`], but also returns the whole program's per-package `@`-name tables
/// (`[directives]`; `[tiers]` later) — the [`noeta_span::PackageUses`] the checker resolves `@name`
/// through. One resolve produces both, so the front-end need not resolve the graph twice.
pub fn dependency_selection_for(
    entry: &Path,
    target: Option<&str>,
) -> Result<(Vec<noeta_loader::DepPackage>, noeta_span::PackageUses), PmError> {
    let g = crate::graph::resolve_graph_for(entry, target)?;
    Ok((g.packages, g.package_uses))
}

/// As [`dependency_packages`], but a pure **query** ([`crate::graph::resolve_graph_query`]): no
/// lockfile refresh. What the IDE calls — opening a file in the editor must not rewrite
/// `noeta.lock` (or silently re-pin versions) as a side effect of making hover/completions work.
pub fn dependency_packages_query(entry: &Path) -> Result<Vec<noeta_loader::DepPackage>, PmError> {
    Ok(crate::graph::resolve_graph_query(entry)?.packages)
}

/// The **effective language edition** the entry compiles under (follow-on F1) — its own
/// `[package].edition`, or [`Edition::DEFAULT`] when it declares none or has no manifest at all (a
/// bare script). This is the entry's *own* package edition, independent of its dependency graph
/// (each dependency's edition is pinned separately in `noeta.lock`), so it is a cheap manifest read,
/// not a graph walk — the edition the compile boundary folds into the startup-cache key so a future
/// edition that changes compilation already invalidates stale bytecode.
pub fn root_edition(entry: &Path) -> crate::edition::Edition {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(path) = find(dir) else {
        return crate::edition::Edition::DEFAULT;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return crate::edition::Edition::DEFAULT;
    };
    match Manifest::parse(&text) {
        Ok(m) => m.package().map(|p| p.edition()).unwrap_or_default(),
        // A corrupt manifest is NOT silently accepted overall (cross-cutting audit finding 4):
        // every invocation that calls this also resolves dependencies through the same manifest,
        // and that parse fails loudly (`PmError::Manifest` → CLI error / IDE diagnostic). This
        // cheap pre-read just must not duplicate that report or fail a never-fail caller (fmt,
        // hover), so it defaults and lets the authoritative path speak. A *valid* manifest with a
        // bad edition value never reaches here — `Manifest::parse` hard-errors on it.
        Err(_) => crate::edition::Edition::DEFAULT,
    }
}

/// The extra native driver **rings** a `noeta build --native` binary should include beyond its import
/// footprint — `[native] rings = ["ring-postgres"]`. A native package with several drivers behind one
/// module (e.g. `para/db`'s SQLite + PostgreSQL) picks the driver at runtime from the dsn, which the
/// static footprint scan cannot see; so a **non-default** driver is requested here explicitly. The
/// composer enables only rings an entry crate actually declares, so an unknown/undeclared name is
/// harmlessly ignored. Empty when there is no manifest or no `[native]` table (today's behavior — the
/// default `ring-sqlite` driver still rides the entry crate's own defaults). A standalone read (like
/// [`root_edition`]) so it stays a cheap manifest peek, not a graph walk.
pub fn native_rings(entry: &Path) -> Vec<String> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(path) = find(dir) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    table
        .get("native")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("rings"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The `[package] name` of a **cargo** manifest — what a composed-toolchain shim writes into its
/// dependency line for a native entry crate (package-manager Phase 3, N3.2). Kept here because
/// `noeta-pm` owns the toml dependency; this reads cargo's manifest, not ours.
pub fn cargo_package_name(crate_dir: &Path) -> Result<String, PmError> {
    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
    let table: toml::Table = text.parse().map_err(|err| {
        PmError::Manifest(format!("`{}` is not valid TOML: {err}", path.display()))
    })?;
    table
        .get("package")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .ok_or_else(|| PmError::Manifest(format!("`{}` has no `[package] name`", path.display())))
}

/// The feature names a **cargo** manifest declares under `[features]` (dev-deps D5b). A composed
/// **dev toolchain** consults this to turn on a mixed crate's conventional dev-capability feature
/// (e.g. `fmt`, gating a tier's `body_formatters`) — but only if the crate actually declares it, so
/// enabling an absent feature never makes cargo error. A missing/empty `[features]` table yields the
/// empty set. (A shipped runner/AOT base never calls this: it pulls each crate at default features,
/// so the formatter and its parser stay uncompiled — the whole point of the split.)
pub fn cargo_features(crate_dir: &Path) -> Result<Vec<String>, PmError> {
    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
    let table: toml::Table = text.parse().map_err(|err| {
        PmError::Manifest(format!("`{}` is not valid TOML: {err}", path.display()))
    })?;
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
pub fn resolve_fmt_config(file: &Path) -> Result<FmtConfig, PmError> {
    // Precedence: built-in defaults, then `.editorconfig` (walked up from the file), then the
    // manifest's `[fmt]` table — so an explicit `noeta.toml` setting wins over `.editorconfig`, which
    // wins over the defaults. (CLI flags, applied by the caller, win over all.)
    let mut config = FmtConfig::default();
    config.overlay_editorconfig(file);
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    if let Some(path) = find(dir) {
        let text = std::fs::read_to_string(&path)
            .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
        // The `[fmt]` grammar lives in `noeta-fmt` (shared with the LSP formatter); the CLI adds the
        // manifest path to any error.
        config
            .overlay_toml(&text)
            .map_err(|err| PmError::Manifest(format!("invalid `{}`: {err}", path.display())))?;
    }
    Ok(config)
}

/// As [`resolve_fmt_config`], but **lenient** — the editor path (cross-cutting #14: this crate
/// owns the one manifest-discovery walk; `noeta-fmt` used to duplicate it because the optional
/// `noeta-pm → noeta-fmt` edge forbids the reverse dependency). A missing manifest, an unreadable
/// file, or a malformed `[fmt]` table all yield what could be resolved so far (defaults +
/// `.editorconfig`): formatting in an editor must never fail on a config problem. The CLI uses
/// [`resolve_fmt_config`] so it can report the error instead.
#[cfg(feature = "fmt-config")]
pub fn resolve_fmt_config_lenient(file: &Path) -> FmtConfig {
    let mut config = FmtConfig::default();
    config.overlay_editorconfig(file);
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    if let Some(path) = find(dir)
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        let _ = config.overlay_toml(&text);
    }
    config
}

impl Manifest {
    /// Parse a `noeta.toml`'s text into a [`Manifest`], validating the shape of the `[tiers]`/
    /// `[directives]` provider bindings and the `[targets.*.tiers]` activation live-set. Unknown keys
    /// outside `[targets]` and unknown target-level keys are ignored, leaving room for later knobs.
    pub fn parse(text: &str) -> Result<Manifest, PmError> {
        Self::parse_inner(text).map_err(PmError::Manifest)
    }

    /// The string-formatting body of [`Self::parse`]: every failure here is a manifest
    /// parse/validation problem, so the one classification ([`PmError::Manifest`]) is applied
    /// once at the public boundary and the internal helpers keep their plain strings.
    fn parse_inner(text: &str) -> Result<Manifest, String> {
        let table: toml::Table = text.parse().map_err(|err| format!("{err}"))?;
        let package = parse_package(&table)?;
        let dependencies = parse_dependencies(&table)?;
        let global_dep_keys: std::collections::BTreeSet<String> =
            dependencies.keys().cloned().collect();
        let directives = parse_use_bindings(&table, "directives", &global_dep_keys, false)?;
        // `[tiers]` permits the built-in `"std"` provider (it declares `test`/`bench`/`doc`/`debug`);
        // a directive always comes from a dependency, so `[directives]` does not. A tier's provider may
        // also be a **target-scoped** dependency (a dev-only tier), so validate against the union of
        // the global and every target's dependency keys.
        let mut tier_provider_keys = global_dep_keys.clone();
        tier_provider_keys.extend(target_scoped_dep_keys(&table)?);
        let tiers = parse_use_bindings(&table, "tiers", &tier_provider_keys, true)?;
        // A `@name` is one namespace per package: source cannot tell a directive apart from a tier
        // until resolution, so a local name may not name both a `[directives]` and a `[tiers]` entry.
        if let Some(dup) = directives.keys().find(|k| tiers.contains_key(*k)) {
            return Err(format!(
                "`{dup}` is declared in both `[directives]` and `[tiers]` — a local `@{dup}` names \
                 one provider; rename one (`local = \"provider:{dup}\"`)"
            ));
        }
        let trust = parse_trust(&table)?;
        let registries = parse_registries(&table)?;
        let db = parse_db(&table)?;
        let patch = parse_patch(&table)?;
        let mut targets = BTreeMap::new();

        let Some(targets_value) = table.get("targets") else {
            return Ok(Manifest {
                package,
                dependencies,
                directives,
                tiers,
                targets,
                trust,
                registries,
                db,
                patch,
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
                let tiers_table = tiers_value.as_table().ok_or_else(|| {
                    format!(
                        "target `{name}`: `tiers` must be a table of `local = true` activation flags \
                         (the provider each tier resolves to lives in the top-level `[tiers]` table)"
                    )
                })?;
                for (tier, active_value) in tiers_table {
                    // `[targets.*.tiers]` is a live-set: it names local tier names (from the
                    // package's own `[tiers]`) and switches each on/off for this build. It carries
                    // no provider — that separation is the point (a provider-string here was the old
                    // shape). The manifest validates only the name shape and the boolean; whether the
                    // name resolves to a live provider is checked where the tier is used (E0036).
                    if !is_identifier(tier) {
                        return Err(format!(
                            "target `{name}`: tier `{tier}` is not a valid tier name (an identifier)"
                        ));
                    }
                    let active = active_value.as_bool().ok_or_else(|| {
                        format!(
                            "target `{name}`: tier `{tier}` must be `true` or `false` — \
                             `[targets.{name}.tiers]` activates local tier names, it no longer names \
                             a provider (move the provider to the top-level `[tiers]` table: \
                             `{tier} = \"…\"`)"
                        )
                    })?;
                    tiers.insert(tier.clone(), active);
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
            directives,
            tiers,
            targets,
            trust,
            registries,
            db,
            patch,
        })
    }

    /// The package's identity, if it declares a `[package]` table (a bare entry script has none).
    pub fn package(&self) -> Option<&PackageMeta> {
        self.package.as_ref()
    }

    /// The `[registries]` mapping (private-registries arc) — which registry each scope resolves from.
    pub fn registries(&self) -> &Registries {
        &self.registries
    }

    /// The `[trust]` grants — the authority this manifest extends to its dependencies (Phase 4).
    pub fn trust(&self) -> &Trust {
        &self.trust
    }

    /// The `[db]` table — the project's default database wiring (`noeta migrate`).
    pub fn db(&self) -> &DbConfig {
        &self.db
    }

    /// The declared dependencies, keyed by local **import root** (the dependency-table key).
    pub fn dependencies(&self) -> &BTreeMap<String, Dependency> {
        &self.dependencies
    }

    /// The `[directives]` table — the extension `@`-directives this package's source uses, each local
    /// `@name` → the dependency (this package's import-root key) that provides it and its exported name.
    pub fn directives(&self) -> &BTreeMap<String, UseBinding> {
        &self.directives
    }

    /// The `[tiers]` table — the dev-tiers this package's source writes as `@name { … }`, each local
    /// `@name` → the provider (`"std"` or a dependency import-root key) that declares it and its
    /// exported name. Separate from `[targets.*.tiers]`, which selects which of these are *live*.
    pub fn tiers(&self) -> &BTreeMap<String, UseBinding> {
        &self.tiers
    }

    /// The root's `[tiers]` as a plain local-name → provider-key map — who provides each tier this
    /// package names, **independent of any build target** (a tier's provider is package-level; the
    /// target only selects which are live). This is what the tier-execution layer dispatches on and
    /// what the compile cache key folds in.
    pub fn tier_provider_map(&self) -> BTreeMap<String, String> {
        self.tiers
            .iter()
            .map(|(local, b)| (local.clone(), b.provider_key.clone()))
            .collect()
    }

    /// The `[patch]` overrides (dev-time path override): package identity → the local tree that
    /// replaces it. Honored by the resolver **only when this is the root app's manifest**.
    pub fn patch(&self) -> &BTreeMap<String, PathBuf> {
        &self.patch
    }

    /// The active tier (local) names for `target`, merging the inherited live-set (`extends`) under
    /// this target's own (a nearer `false` turns an inherited tier off), returned sorted. Errors on an
    /// unknown target or an `extends` cycle.
    pub fn active_tiers(&self, target: &str) -> Result<Vec<String>, PmError> {
        let mut chain = Vec::new();
        let merged = self
            .resolve(target, &mut chain)
            .map_err(PmError::Manifest)?;
        Ok(merged
            .into_iter()
            .filter_map(|(tier, active)| active.then_some(tier))
            .collect())
    }

    /// The active tier → **provider** map for `target`: each live tier (a local name from the
    /// activation live-set) mapped to the provider its `[tiers]` entry names — the built-in `"std"` or
    /// a declared dependency's import-root key. This joins the two axes (the target's live-set and the
    /// package's `[tiers]` table) for this *root* package's own tiers; a dependency's tiers resolve
    /// through its own `[tiers]` (the per-package [`noeta_span::PackageUses`] the graph builds). An
    /// active tier this package's `[tiers]` never mapped is an error pointing the user to add it.
    ///
    /// The tier-execution layer dispatches on this: `"std"` runs the built-in native runner, a
    /// dependency key runs that package's `@tier` runner (`resolve_active_tier_providers`).
    pub fn active_tier_providers(&self, target: &str) -> Result<BTreeMap<String, String>, PmError> {
        let active = self.active_tiers(target)?;
        let mut providers = BTreeMap::new();
        for tier in active {
            let binding = self.tiers.get(&tier).ok_or_else(|| {
                PmError::Manifest(format!(
                    "target `{target}` activates tier `{tier}`, but no `[tiers]` entry maps it to a \
                     provider — add `{tier} = \"std\"` (or a dependency key) to `[tiers]`"
                ))
            })?;
            providers.insert(tier, binding.provider_key.clone());
        }
        Ok(providers)
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
    ) -> Result<BTreeMap<String, Dependency>, PmError> {
        let mut merged = self.dependencies.clone();
        if let Some(name) = target
            && self.targets.contains_key(name)
        {
            let mut chain = Vec::new();
            for (key, dep) in self
                .resolve_deps(name, &mut chain)
                .map_err(PmError::Manifest)?
            {
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
    ) -> Result<BTreeMap<String, bool>, String> {
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

        for (tier, active) in &target.tiers {
            merged.insert(tier.clone(), *active);
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
        Some(v) => {
            let s = v.as_str().ok_or("`package.edition` must be a string")?;
            Some(crate::edition::Edition::parse(s)?)
        }
    };
    let toolchain = match pkg.get("toolchain") {
        None => None,
        Some(v) => {
            let req = v.as_str().ok_or(
                "`package.toolchain` must be a string (a SemVer requirement, e.g. `\">=0.2\"`)",
            )?;
            Some(semver::VersionReq::parse(req).map_err(|err| {
                format!("`package.toolchain` `{req}` is not a valid SemVer requirement: {err}")
            })?)
        }
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
    let license = match pkg.get("license") {
        None => None,
        Some(v) => {
            let expr = v
                .as_str()
                .ok_or("`package.license` must be a string (an SPDX expression)")?;
            Some(validate_license(expr)?)
        }
    };
    let keywords = match pkg.get("keywords") {
        None => Vec::new(),
        Some(v) => {
            let list = v
                .as_array()
                .ok_or("`package.keywords` must be an array of strings")?;
            validate_keywords(list)?
        }
    };
    let description = match pkg.get("description") {
        None => None,
        Some(v) => {
            let text = v.as_str().ok_or("`package.description` must be a string")?;
            Some(validate_description(text)?)
        }
    };
    Ok(Some(PackageMeta {
        name,
        version,
        edition,
        toolchain,
        native,
        license,
        keywords,
        description,
    }))
}

/// Syntactic validation of a `package.license` value: an SPDX license expression like
/// `MIT OR Apache-2.0`. Shape only (SPDX charset, non-empty, bounded — mirrors the registry's
/// check), not a full SPDX parser: the goal is catching typos and garbage at `noeta check` time,
/// not adjudicating license law.
fn validate_license(expr: &str) -> Result<String, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty()
        || expr.len() > 120
        || !expr
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " .+()-".contains(c))
    {
        return Err(format!(
            "`package.license` `{expr}` is not an SPDX license expression (letters, digits, ` .+()-`, ≤ 120 chars)"
        ));
    }
    Ok(trimmed.to_string())
}

/// The most keywords a release may carry. Enough to place a package; few enough that a keyword
/// still narrows a listing to something worth reading. MUST match the registry's `MAX_KEYWORDS`.
pub const MAX_KEYWORDS: usize = 5;

/// Syntactic validation of a `package.keywords` value: up to [`MAX_KEYWORDS`] tags, each 1–20 chars
/// of lowercase `a–z`, `0–9` and `-`, starting alphanumeric. Mirrors the registry's `KEYWORD` check,
/// so a publish that would be rejected server-side fails at `noeta check` time instead.
///
/// The narrow charset is the point rather than an accident: one canonical spelling per tag is what
/// makes a keyword listing *group*, instead of fragmenting a topic across `Aether`, `aether_` and
/// `AEther`. Returns the tags deduplicated and sorted — they are a set, so the order declared in the
/// manifest carries no meaning and is not worth preserving through the wire and the index.
fn validate_keywords(list: &[toml::Value]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for value in list {
        let kw = value
            .as_str()
            .ok_or("`package.keywords` must be an array of strings")?;
        let ok = !kw.is_empty()
            && kw.len() <= 20
            && kw.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && kw
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok {
            return Err(format!(
                "`package.keywords` `{kw}` is not a keyword (1–20 chars of lowercase `a-z`, `0-9` \
                 and `-`, starting alphanumeric)"
            ));
        }
        if !out.iter().any(|seen| seen == kw) {
            out.push(kw.to_string());
        }
    }
    if out.len() > MAX_KEYWORDS {
        return Err(format!(
            "`package.keywords` declares {} keywords; at most {MAX_KEYWORDS} are allowed",
            out.len()
        ));
    }
    out.sort();
    Ok(out)
}

/// The longest a `package.description` may be. A one-line search blurb — long enough to be useful,
/// short enough to stay a single result-card row. MUST match the registry's `MAX_DESCRIPTION`.
pub const MAX_DESCRIPTION: usize = 200;

/// Syntactic validation of a `package.description` value: a single line, trimmed, non-empty, at most
/// [`MAX_DESCRIPTION`] characters, with no control characters (which rules out newlines and tabs).
/// Mirrors the registry's `description` check, so a publish rejected server-side fails at
/// `noeta check` time instead. Returns the trimmed value.
fn validate_description(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_DESCRIPTION
        || trimmed.chars().any(|c| c.is_control())
    {
        return Err(format!(
            "`package.description` must be a single line of 1–{MAX_DESCRIPTION} characters"
        ));
    }
    Ok(trimmed.to_string())
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
/// Parse the optional `[db]` table into a [`DbConfig`]. Both keys (`url`, `migrations`) are optional
/// strings; a present-but-wrong-typed value fails loudly rather than being silently ignored.
fn parse_db(table: &toml::Table) -> Result<DbConfig, String> {
    let Some(value) = table.get("db") else {
        return Ok(DbConfig::default());
    };
    let db_table = value.as_table().ok_or("`db` must be a table")?;
    let string_key = |key: &str| -> Result<Option<String>, String> {
        match db_table.get(key) {
            None => Ok(None),
            Some(v) => Ok(Some(
                v.as_str()
                    .ok_or_else(|| format!("`db.{key}` must be a string"))?
                    .to_string(),
            )),
        }
    };
    Ok(DbConfig {
        url: string_key("url")?,
        migrations: string_key("migrations")?,
        seeds: string_key("seeds")?,
    })
}

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
    let publish_cooldown = match trust_table.get("publish_cooldown") {
        None => None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or("`trust.publish_cooldown` must be a duration string like \"24h\"")?;
            Some(parse_duration(s)?)
        }
    };
    let advisories = parse_advisory_policy(trust_table)?;
    Ok(Trust {
        native: parse_list("native")?,
        commands: parse_binding_table(trust_table, "commands")?,
        require_provenance,
        require_transparency,
        publish_cooldown,
        advisories,
    })
}

/// Parse a `[trust.<field>]` binding table (`local = "company/package[:exported]"`) into a
/// local-name → [`Binding`] map. This is the shape both `[trust.commands]` and `[trust.directives]`
/// use: the key is the local name the project addresses the capability under, the value names the
/// providing package and (optionally, after a `:`) the name that package exported it under.
///
/// The pre-mapping array form (`commands = ["company/package"]`) is refused with a migration
/// message — it granted every command a package shipped, blanket, which is exactly what the local
/// binding replaces.
fn parse_binding_table(
    trust_table: &toml::Table,
    field: &str,
) -> Result<BTreeMap<String, Binding>, String> {
    let Some(value) = trust_table.get(field) else {
        return Ok(BTreeMap::new());
    };
    // A leftover array is the old blanket-by-package form — name the replacement rather than the
    // generic "must be a table", since this is a migration a reader is likely mid-way through.
    if value.is_array() {
        return Err(format!(
            "`trust.{field}` is now a table mapping each local name to its provider — write \
             `[trust.{field}]` with `name = \"company/package\"` entries (add `:exported` to rename), \
             not an array of package identities"
        ));
    }
    let table = value.as_table().ok_or_else(|| {
        format!("`trust.{field}` must be a table of `local = \"company/package[:exported]\"`")
    })?;
    let mut out = BTreeMap::new();
    for (local, spec_value) in table {
        let spec = spec_value.as_str().ok_or_else(|| {
            format!("`trust.{field}.{local}` must be a string `\"company/package[:exported]\"`")
        })?;
        // The identity is `company/package` (a `/`, never a `:`), so the FIRST colon unambiguously
        // begins the exported name; everything after it is that name verbatim. No colon → the
        // exported name is the local key (the common, no-rename case).
        let (provider, exported) = match spec.split_once(':') {
            Some((p, e)) => (p, e),
            None => (spec, local.as_str()),
        };
        PackageName::parse(provider).map_err(|err| format!("`trust.{field}.{local}`: {err}"))?;
        if exported.is_empty() {
            return Err(format!(
                "`trust.{field}.{local}`: the exported name after `:` is empty — drop the `:` to \
                 bind the provider's own `{local}`, or name the command it exports"
            ));
        }
        out.insert(
            local.clone(),
            Binding {
                provider: provider.to_string(),
                exported: exported.to_string(),
            },
        );
    }
    Ok(out)
}

/// Parse a per-package `[directives]` / `[tiers]` table (`local = "dep-key[:exported]"`) into a
/// local-name → [`UseBinding`] map. The provider is named by one of **this package's** dependency
/// import-root keys (`valid_providers`), so a `@name` resolves in the same context a `use <key>.…`
/// does. `allow_builtin` permits the built-in `"std"` provider (for `[tiers]`, whose `test`/`debug`/…
/// come from the stdlib); `[directives]` passes `false` — a directive always comes from a dependency.
///
/// `valid_providers` is the set of legal provider keys: the global `[dependencies]` for `[directives]`,
/// and those *plus every target-scoped dependency key* for `[tiers]` (a dev-only tier provider is
/// declared under `[targets.<t>.dependencies]`, and the package-level `[tiers]` table must still be
/// able to name it).
/// Every dependency key declared under any `[targets.<t>.dependencies]` (dev-deps arc) — the extra
/// provider keys a package-level `[tiers]` table may name beyond the global `[dependencies]`. Read
/// straight off the raw TOML (the targets are not parsed into [`Target`]s until later), tolerating a
/// malformed `[targets]`/target/`dependencies` shape here (the targets loop reports it precisely).
fn target_scoped_dep_keys(
    table: &toml::Table,
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut keys = std::collections::BTreeSet::new();
    let Some(targets) = table.get("targets").and_then(|v| v.as_table()) else {
        return Ok(keys);
    };
    for target in targets.values() {
        let Some(deps) = target
            .as_table()
            .and_then(|t| t.get("dependencies"))
            .and_then(|d| d.as_table())
        else {
            continue;
        };
        keys.extend(deps.keys().cloned());
    }
    Ok(keys)
}

fn parse_use_bindings(
    table: &toml::Table,
    field: &str,
    valid_providers: &std::collections::BTreeSet<String>,
    allow_builtin: bool,
) -> Result<BTreeMap<String, UseBinding>, String> {
    let Some(value) = table.get(field) else {
        return Ok(BTreeMap::new());
    };
    let entries = value
        .as_table()
        .ok_or_else(|| format!("`{field}` must be a table of `local = \"dep-key[:exported]\"`"))?;
    let mut out = BTreeMap::new();
    for (local, spec_value) in entries {
        let spec = spec_value.as_str().ok_or_else(|| {
            format!("`{field}.{local}` must be a string `\"dep-key[:exported]\"`")
        })?;
        // First `:` splits the provider key from the exported name; no colon → exported == local.
        let (provider_key, exported) = match spec.split_once(':') {
            Some((p, e)) => (p, e),
            None => (spec, local.as_str()),
        };
        if provider_key.is_empty() {
            return Err(format!(
                "`{field}.{local}`: the provider dependency key is empty"
            ));
        }
        if exported.is_empty() {
            return Err(format!(
                "`{field}.{local}`: the exported name after `:` is empty — drop the `:` to bind the \
                 provider's own `{local}`, or name the {} it exports",
                field.trim_end_matches('s')
            ));
        }
        let is_builtin = allow_builtin && provider_key == BUILTIN_PROVIDER;
        if !is_builtin && !valid_providers.contains(provider_key) {
            let std_hint = if allow_builtin {
                format!(" (or the built-in `\"{BUILTIN_PROVIDER}\"`)")
            } else {
                String::new()
            };
            return Err(format!(
                "`{field}.{local}` names provider `{provider_key}`, which is not a `[dependencies]` \
                 key{std_hint} — add `{provider_key}` to `[dependencies]` to use its {field}"
            ));
        }
        out.insert(
            local.clone(),
            UseBinding {
                provider_key: provider_key.to_string(),
                exported: exported.to_string(),
            },
        );
    }
    Ok(out)
}

/// Parse `[trust].advisories` (advisory-intake arc, tier 5): either a bare action string applied to
/// every tier (`advisories = "fail"`), or a sub-table with per-tier keys
/// (`[trust.advisories]` / `operator = "fail"`, `publisher = "warn"`, `imported = "off"`). Absent →
/// every tier warns (the default).
fn parse_advisory_policy(trust_table: &toml::Table) -> Result<AdvisoryPolicy, String> {
    let Some(value) = trust_table.get("advisories") else {
        return Ok(AdvisoryPolicy::default());
    };
    // A bare string sets every tier at once.
    if let Some(s) = value.as_str() {
        let action =
            AdvisoryAction::parse(s).map_err(|err| format!("`trust.advisories`: {err}"))?;
        return Ok(AdvisoryPolicy {
            operator: action,
            publisher: action,
            imported: action,
        });
    }
    let table = value
        .as_table()
        .ok_or("`trust.advisories` must be an action string (\"fail\"/\"warn\"/\"off\") or a table of per-tier actions")?;
    let mut policy = AdvisoryPolicy::default();
    for (key, v) in table {
        let s = v.as_str().ok_or_else(|| {
            format!("`trust.advisories.{key}` must be a string (\"fail\"/\"warn\"/\"off\")")
        })?;
        let action =
            AdvisoryAction::parse(s).map_err(|err| format!("`trust.advisories.{key}`: {err}"))?;
        match key.as_str() {
            "operator" => policy.operator = action,
            "publisher" => policy.publisher = action,
            "imported" => policy.imported = action,
            other => {
                return Err(format!(
                    "`trust.advisories.{other}` is not a tier (use `operator`, `publisher`, or `imported`)"
                ));
            }
        }
    }
    Ok(policy)
}

/// Parse the `[registries]` table (private-registries arc): a map of scope (`company`) → source string
/// (`github:<org>` or an http(s):// URL), plus an optional reserved `default` key applied to unmapped
/// scopes. A non-string value, an unknown source syntax, or a non-identifier scope key is an error.
fn parse_registries(table: &toml::Table) -> Result<Registries, String> {
    let Some(value) = table.get("registries") else {
        return Ok(Registries::default());
    };
    let reg_table = value.as_table().ok_or("`registries` must be a table")?;
    let mut registries = Registries::default();
    for (key, val) in reg_table {
        let s = val
            .as_str()
            .ok_or_else(|| format!("`registries.{key}` must be a source string"))?;
        let source =
            RegistrySource::parse(s).map_err(|err| format!("`registries.{key}`: {err}"))?;
        if key == "default" {
            registries.default = Some(source);
        } else {
            // A scope key must be a bare `company` identifier (not `company/package`).
            if !is_identifier(key) {
                return Err(format!(
                    "`registries.{key}`: a registry key must be a scope (`company`) identifier or \
                     `default`"
                ));
            }
            registries.by_scope.insert(key.clone(), source);
        }
    }
    Ok(registries)
}

/// Parse the optional `[patch]` table (dev-time path override — Noeta's analog of Cargo's
/// `[patch]`): each key is a full **package identity** (`"company/package"`, quoted since it
/// contains a slash), each value a `{ path = "…" }` table naming the local tree that replaces the
/// identity's declared source during resolution. Only `path` overrides exist — a `git` or
/// `version` patch is refused with a pointer at the supported form, so the advanced Cargo shapes
/// stay deliberately out of scope. A built-in scope (`std`/`noeta`/`core`) is served by the
/// compiler and can never be patched.
fn parse_patch(table: &toml::Table) -> Result<BTreeMap<String, PathBuf>, String> {
    let Some(value) = table.get("patch") else {
        return Ok(BTreeMap::new());
    };
    let patch_table = value
        .as_table()
        .ok_or("`patch` must be a table of `\"company/package\" = { path = \"…\" }` overrides")?;
    let mut out = BTreeMap::new();
    for (key, val) in patch_table {
        // Validate the identity shape so a typo'd key fails loudly instead of silently patching
        // nothing (the same rule as a `[trust]` grant).
        let name = PackageName::parse(key).map_err(|err| format!("`patch.\"{key}\"`: {err}"))?;
        if crate::reserved::is_builtin(&name.company) {
            return Err(format!(
                "`patch.\"{key}\"`: `{}` is a built-in scope served by the compiler and cannot be \
                 patched",
                name.company
            ));
        }
        let override_table = val.as_table().ok_or_else(|| {
            format!(
                "`patch.\"{key}\"` must be a `{{ path = \"…\" }}` table — only local path \
                 overrides are supported"
            )
        })?;
        for k in override_table.keys() {
            if k != "path" {
                return Err(format!(
                    "`patch.\"{key}\"` has an unsupported key `{k}` — only `path` overrides are \
                     supported (no git or version patches)"
                ));
            }
        }
        let path = override_table
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| {
                format!(
                    "`patch.\"{key}\"` must carry a string `path` — only local path overrides are \
                     supported"
                )
            })?;
        out.insert(key.clone(), PathBuf::from(path));
    }
    Ok(out)
}

/// Parse a human duration into seconds for `[trust].publish_cooldown`: an integer with an optional unit
/// suffix `s`/`m`/`h`/`d` (seconds/minutes/hours/days), e.g. `"24h"`, `"30m"`, `"7d"`, `"3600s"`. A
/// bare number is seconds. Zero is allowed (a no-op window). Rejects a negative, empty, or malformed
/// value so a typo can't silently disable the cooldown.
fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("`trust.publish_cooldown` is empty".to_string());
    }
    let (digits, unit_secs) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1u64),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86_400),
        c if c.is_ascii_digit() => (s, 1),
        _ => {
            return Err(format!(
                "`trust.publish_cooldown` = \"{s}\" has an unknown unit (use s, m, h, or d)"
            ));
        }
    };
    let n: u64 = digits.trim().parse().map_err(|_| {
        format!("`trust.publish_cooldown` = \"{s}\" is not a whole number of time units")
    })?;
    n.checked_mul(unit_secs)
        .ok_or_else(|| format!("`trust.publish_cooldown` = \"{s}\" overflows"))
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
    // An array value is a **scope** dependency: several packages sharing the scope `key`, each an
    // ordinary (non-array) source. Empty arrays and nested scopes are rejected so the shape stays a
    // flat list of member packages.
    if let Some(array) = value.as_array() {
        if array.is_empty() {
            return Err(format!(
                "dependency `{key}` is an empty array — a scope dependency must list at least one \
                 member package (`{key} = [ {{ path = … }}, … ]`)"
            ));
        }
        let mut members = Vec::with_capacity(array.len());
        for element in array {
            if element.is_array() {
                return Err(format!(
                    "dependency `{key}`: a scope dependency's members must be package sources \
                     (`{{ path/git/version = … }}`), not nested arrays"
                ));
            }
            match parse_dependency(key, element)? {
                Dependency::Scope(_) => unreachable!("a nested array was already rejected"),
                member => members.push(member),
            }
        }
        return Ok(Dependency::Scope(members));
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
            // A `git` dependency tracks a `tag` (a release), a `branch`, or — with neither — the
            // remote's default-branch HEAD (the tag-free in-dev/bundled case). `tag` and `branch`
            // are mutually exclusive.
            let git_ref = match (
                table.get("tag").and_then(|v| v.as_str()),
                table.get("branch").and_then(|v| v.as_str()),
            ) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "dependency `{key}`: a `git` dependency takes `tag` OR `branch`, not both"
                    ));
                }
                (Some(tag), None) => GitRef::Tag(tag.to_string()),
                (None, Some(branch)) => GitRef::Branch(branch.to_string()),
                (None, None) => {
                    // Reject a non-string `tag`/`branch` explicitly rather than silently treating it
                    // as HEAD (a common typo like `tag = 1` should not become a HEAD dependency).
                    if table.contains_key("tag") {
                        return Err(format!("dependency `{key}`: `tag` must be a string"));
                    }
                    if table.contains_key("branch") {
                        return Err(format!("dependency `{key}`: `branch` must be a string"));
                    }
                    GitRef::Head
                }
            };
            Ok(Dependency::Git {
                url: url.to_string(),
                git_ref,
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- `[db]` (migration wiring) -------------------------------------------------------------

    #[test]
    fn parses_a_db_table() {
        let m = Manifest::parse(
            "[db]\n\
             url = \"postgres://u:p@h/db\"\n\
             migrations = \"db/migrations\"\n\
             seeds = \"db/seeds\"\n",
        )
        .expect("valid");
        assert_eq!(m.db().url.as_deref(), Some("postgres://u:p@h/db"));
        assert_eq!(m.db().migrations.as_deref(), Some("db/migrations"));
        assert_eq!(m.db().seeds.as_deref(), Some("db/seeds"));
    }

    #[test]
    fn a_db_table_without_seeds_defaults_to_none() {
        let m = Manifest::parse("[db]\nurl = \"sqlite:app.db\"\n").expect("valid");
        assert!(m.db().seeds.is_none());
    }

    #[test]
    fn a_missing_db_table_is_the_default() {
        let m = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"0.1.0\"\n").expect("valid");
        assert_eq!(m.db(), &DbConfig::default());
        assert!(m.db().url.is_none());
    }

    #[test]
    fn a_wrongly_typed_db_url_is_an_error() {
        let err = Manifest::parse("[db]\nurl = 5\n").unwrap_err();
        assert!(
            err.to_string().contains("`db.url` must be a string"),
            "{err}"
        );
    }

    // --- `[patch]` (dev-time path override) ----------------------------------------------------

    #[test]
    fn the_patch_table_parses_identity_keyed_path_overrides() {
        let m = Manifest::parse(
            "[dependencies]\ndb = { version = \"^1.0\", package = \"para/db\" }\n\
             [patch]\n\
             \"para/db\" = { path = \"../para-db\" }\n\
             \"acme/http\" = { path = \"/abs/http\" }\n",
        )
        .expect("valid");
        assert_eq!(m.patch().len(), 2);
        assert_eq!(m.patch()["para/db"], PathBuf::from("../para-db"));
        assert_eq!(m.patch()["acme/http"], PathBuf::from("/abs/http"));
        // No `[patch]` table → empty (the common case; parsing is purely additive).
        assert!(
            Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n")
                .unwrap()
                .patch()
                .is_empty()
        );
    }

    #[test]
    fn a_patch_override_must_be_a_path_table() {
        // A git patch is deliberately unsupported (only local path overrides exist).
        let err =
            Manifest::parse("[patch]\n\"para/db\" = { git = \"https://example.com/para/db\" }\n")
                .unwrap_err();
        assert!(
            err.to_string()
                .contains("only `path` overrides are supported"),
            "{err}"
        );
        // A bare version string is not an override either.
        let err = Manifest::parse("[patch]\n\"para/db\" = \"^2.0\"\n").unwrap_err();
        assert!(
            err.to_string().contains("only local path overrides"),
            "{err}"
        );
        // A `path` of the wrong type fails loudly.
        assert!(Manifest::parse("[patch]\n\"para/db\" = { path = 5 }\n").is_err());
    }

    #[test]
    fn a_patch_key_must_be_a_package_identity() {
        let err = Manifest::parse("[patch]\ndb = { path = \"../db\" }\n").unwrap_err();
        assert!(
            err.to_string().contains("must be `company/package`"),
            "the key is a full identity, not an import root: {err}"
        );
    }

    #[test]
    fn a_builtin_scope_cannot_be_patched() {
        let err = Manifest::parse("[patch]\n\"std/http\" = { path = \"../http\" }\n").unwrap_err();
        assert!(
            err.to_string().contains("built-in scope"),
            "core namespaces are never patchable: {err}"
        );
    }

    // --- `[registries]` (private-registries arc) -----------------------------------------------

    #[test]
    fn parses_a_registries_table() {
        let m = Manifest::parse(
            "[registries]\n\
             default = \"https://registry.noeta.dev\"\n\
             acme = \"github:acme\"\n\
             widgets = \"https://widgets.example/reg/\"\n",
        )
        .expect("valid");
        let r = m.registries();
        assert!(!r.is_empty());
        // A scope with an explicit mapping resolves to it (the `github:` shorthand → a forge base URL).
        assert_eq!(
            r.source_for("acme"),
            Some(&RegistrySource::GitForge(
                "https://github.com/acme".to_string()
            ))
        );
        // Trailing slash trimmed on hosted URLs.
        assert_eq!(
            r.source_for("widgets"),
            Some(&RegistrySource::Hosted(
                "https://widgets.example/reg".to_string()
            ))
        );
        // An unmapped scope falls back to `default`.
        assert_eq!(
            r.source_for("other"),
            Some(&RegistrySource::Hosted(
                "https://registry.noeta.dev".to_string()
            ))
        );
    }

    #[test]
    fn registries_default_is_optional_and_absent_means_env_default() {
        let m = Manifest::parse("[registries]\nacme = \"github:acme\"\n").unwrap();
        let r = m.registries();
        assert!(r.source_for("acme").is_some());
        assert_eq!(r.source_for("unmapped"), None); // no default → use the environment registry
        // No table at all → empty.
        assert!(
            Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n")
                .unwrap()
                .registries()
                .is_empty()
        );
    }

    #[test]
    fn rejects_a_bad_registry_source_or_key() {
        assert!(Manifest::parse("[registries]\nacme = \"ftp://nope\"\n").is_err());
        assert!(Manifest::parse("[registries]\nacme = \"github:\"\n").is_err());
        assert!(Manifest::parse("[registries]\n\"a/b\" = \"github:acme\"\n").is_err());
        assert!(Manifest::parse("[registries]\nacme = 5\n").is_err());
    }

    #[test]
    fn registry_source_parse_forms() {
        // Every git-forge shorthand + the generic `git:` normalize to one GitForge base URL.
        assert_eq!(
            RegistrySource::parse("github:my-org").unwrap(),
            RegistrySource::GitForge("https://github.com/my-org".to_string())
        );
        assert_eq!(
            RegistrySource::parse("gitlab:team/sub").unwrap(),
            RegistrySource::GitForge("https://gitlab.com/team/sub".to_string())
        );
        assert_eq!(
            RegistrySource::parse("git:https://git.example.com/org/").unwrap(),
            RegistrySource::GitForge("https://git.example.com/org".to_string())
        );
        assert_eq!(
            RegistrySource::parse("git:ssh://git@example.com/org").unwrap(),
            RegistrySource::GitForge("ssh://git@example.com/org".to_string())
        );
        // A bare http(s) URL stays a hosted noeta-registry service (not a forge).
        assert_eq!(
            RegistrySource::parse("https://x.example").unwrap(),
            RegistrySource::Hosted("https://x.example".to_string())
        );
        assert!(RegistrySource::parse("github:bad org").is_err());
        assert!(RegistrySource::parse("gitlab:").is_err());
        assert!(RegistrySource::parse("git:").is_err());
        assert!(RegistrySource::parse("just-a-string").is_err());
    }

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
        assert_eq!(pkg.edition, Some(crate::edition::Edition::E2026));
        assert_eq!(pkg.edition(), crate::edition::Edition::E2026);
    }

    #[test]
    fn defaults_edition_when_omitted_and_rejects_an_unknown_one() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n",
        )
        .expect("valid");
        let pkg = m.package().expect("package present");
        assert_eq!(pkg.edition, None);
        assert_eq!(pkg.edition(), crate::edition::Edition::DEFAULT);

        let err = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             edition = \"2030\"\n",
        )
        .expect_err("unknown edition rejected");
        assert!(
            err.message().contains("2030"),
            "names the offending value: {err}"
        );
    }

    #[test]
    fn parses_a_license_and_rejects_a_malformed_one() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             license = \"MIT OR Apache-2.0\"\n",
        )
        .expect("valid");
        assert_eq!(
            m.package().unwrap().license.as_deref(),
            Some("MIT OR Apache-2.0")
        );
        // Omitted → None (a license is optional, nudged at publish).
        let m =
            Manifest::parse("[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n").unwrap();
        assert_eq!(m.package().unwrap().license, None);
        // Not an SPDX-shaped expression → a manifest error naming the value.
        let err = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             license = \"<script>\"\n",
        )
        .expect_err("malformed license rejected");
        assert!(err.message().contains("SPDX"), "{err}");
    }

    #[test]
    fn parses_a_toolchain_requirement_and_rejects_a_malformed_one() {
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             toolchain = \">=0.2\"\n",
        )
        .expect("valid");
        let req = m.package().unwrap().toolchain.clone().unwrap();
        assert!(req.matches(&semver::Version::new(0, 2, 0)));
        assert!(!req.matches(&semver::Version::new(0, 1, 9)));
        // Omitted → None (the package makes no toolchain claim).
        let m =
            Manifest::parse("[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n").unwrap();
        assert_eq!(m.package().unwrap().toolchain, None);
        // Not a SemVer requirement → a manifest error naming the value.
        let err = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             toolchain = \"latest\"\n",
        )
        .expect_err("malformed toolchain requirement rejected");
        assert!(err.message().contains("SemVer requirement"), "{err}");
    }

    #[test]
    fn parses_keywords_deduped_and_sorted_and_rejects_bad_ones() {
        // Declared unsorted with a duplicate → stored as a sorted set.
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             keywords = [\"simd\", \"image\", \"simd\"]\n",
        )
        .expect("valid");
        assert_eq!(
            m.package().unwrap().keywords,
            vec!["image".to_string(), "simd".to_string()]
        );
        // Omitted → an empty set.
        let m =
            Manifest::parse("[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n").unwrap();
        assert!(m.package().unwrap().keywords.is_empty());
        // An uppercase tag is not a keyword (one canonical spelling per tag).
        let err = Manifest::parse(
            "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\nkeywords = [\"Image\"]\n",
        )
        .expect_err("malformed keyword rejected");
        assert!(err.message().contains("keyword"), "{err}");
        // Over the limit is rejected, naming the cap.
        let err = Manifest::parse(
            "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n\
             keywords = [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\"]\n",
        )
        .expect_err("too many keywords rejected");
        assert!(err.message().contains("at most"), "{err}");
    }

    #[test]
    fn parses_a_description_and_rejects_a_multiline_or_over_long_one() {
        // A declared blurb is trimmed and kept.
        let m = Manifest::parse(
            "[package]\n\
             name = \"acme/widgets\"\n\
             version = \"1.0.0\"\n\
             description = \"  Fast image effects for Noeta  \"\n",
        )
        .expect("valid");
        assert_eq!(
            m.package().unwrap().description.as_deref(),
            Some("Fast image effects for Noeta")
        );
        // Omitted → None.
        let m =
            Manifest::parse("[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n").unwrap();
        assert_eq!(m.package().unwrap().description, None);
        // A newline (a control character) is rejected — a description is a single line.
        let err = Manifest::parse(
            "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\ndescription = \"one\\ntwo\"\n",
        )
        .expect_err("multi-line description rejected");
        assert!(err.message().contains("single line"), "{err}");
        // Over the length cap is rejected.
        let long = "x".repeat(MAX_DESCRIPTION + 1);
        let err = Manifest::parse(&format!(
            "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\ndescription = \"{long}\"\n"
        ))
        .expect_err("over-long description rejected");
        assert!(err.message().contains("single line"), "{err}");
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
             [trust.commands]\n\
             scaffold = \"acme/scaffold\"\n\
             gen = \"acme/scaffold:generate\"\n",
        )
        .expect("valid");
        assert!(m.trust().native.contains("acme/imgfx"));
        assert!(m.trust().native.contains("acme/simd"));
        // The no-rename form: exported name defaults to the local key.
        let scaffold = m.trust().commands.get("scaffold").expect("bound");
        assert_eq!(scaffold.provider, "acme/scaffold");
        assert_eq!(scaffold.exported, "scaffold");
        // The rename form: `local = "provider:exported"`.
        let renamed = m.trust().commands.get("gen").expect("bound");
        assert_eq!(renamed.provider, "acme/scaffold");
        assert_eq!(renamed.exported, "generate");
        assert!(!m.trust().commands.contains_key("acme/imgfx"));
    }

    #[test]
    fn directives_bind_local_names_to_a_dependency_and_exported_name() {
        let m = Manifest::parse(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\npara = { path = \"../para\" }\n\
             [directives]\n\
             openapi = \"para\"\n\
             oapi = \"para:openapi\"\n",
        )
        .expect("valid");
        // No-rename: exported defaults to the local key.
        let openapi = m.directives().get("openapi").expect("bound");
        assert_eq!(openapi.provider_key, "para");
        assert_eq!(openapi.exported, "openapi");
        // Rename: `local = "dep-key:exported"`.
        let oapi = m.directives().get("oapi").expect("bound");
        assert_eq!(oapi.provider_key, "para");
        assert_eq!(oapi.exported, "openapi");
    }

    #[test]
    fn a_directive_naming_an_undeclared_dependency_is_an_error() {
        let err = Manifest::parse(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [directives]\nopenapi = \"para\"\n",
        )
        .expect_err("provider must be a declared dependency")
        .to_string();
        assert!(err.contains("directives.openapi"), "{err}");
        assert!(err.contains("[dependencies]"), "{err}");
    }

    #[test]
    fn trust_rejects_the_pre_mapping_command_array() {
        let err = Manifest::parse(
            "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n\
             [trust]\ncommands = [\"acme/scaffold\"]\n",
        )
        .expect_err("the array form is retired")
        .to_string();
        assert!(err.contains("trust.commands"), "{err}");
        assert!(err.contains("table"), "{err}");
    }

    #[test]
    fn advisory_policy_defaults_to_all_warn() {
        let m = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();
        let p = m.trust().advisories;
        assert_eq!(p.operator, AdvisoryAction::Warn);
        assert_eq!(p.publisher, AdvisoryAction::Warn);
        assert_eq!(p.imported, AdvisoryAction::Warn);
        assert_eq!(p.action_for("imported"), AdvisoryAction::Warn);
    }

    #[test]
    fn advisory_policy_bare_string_sets_every_tier() {
        let m = Manifest::parse(
            "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n[trust]\nadvisories = \"fail\"\n",
        )
        .unwrap();
        let p = m.trust().advisories;
        assert_eq!(p.operator, AdvisoryAction::Fail);
        assert_eq!(p.publisher, AdvisoryAction::Fail);
        assert_eq!(p.imported, AdvisoryAction::Fail);
    }

    #[test]
    fn advisory_policy_per_tier_table() {
        let m = Manifest::parse(
            "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n\
             [trust.advisories]\noperator = \"fail\"\npublisher = \"fail\"\nimported = \"off\"\n",
        )
        .unwrap();
        let p = m.trust().advisories;
        assert_eq!(p.operator, AdvisoryAction::Fail);
        assert_eq!(p.publisher, AdvisoryAction::Fail);
        assert_eq!(p.imported, AdvisoryAction::Off);
    }

    #[test]
    fn advisory_policy_rejects_a_bad_action_or_tier() {
        assert!(
            Manifest::parse(
                "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n[trust]\nadvisories = \"spicy\"\n"
            )
            .is_err()
        );
        assert!(
            Manifest::parse(
                "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n[trust.advisories]\nnope = \"fail\"\n",
            )
            .is_err()
        );
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
    fn trust_parses_publish_cooldown() {
        let off = Manifest::parse("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();
        assert_eq!(off.trust().publish_cooldown, None);
        assert_eq!(
            Manifest::parse("[trust]\npublish_cooldown = \"24h\"\n")
                .unwrap()
                .trust()
                .publish_cooldown,
            Some(86_400)
        );
        assert_eq!(
            Manifest::parse("[trust]\npublish_cooldown = \"7d\"\n")
                .unwrap()
                .trust()
                .publish_cooldown,
            Some(604_800)
        );
        // A bare number is seconds; a non-string or bad unit is an error, not a silent no-op.
        assert_eq!(
            Manifest::parse("[trust]\npublish_cooldown = \"90\"\n")
                .unwrap()
                .trust()
                .publish_cooldown,
            Some(90)
        );
        assert!(Manifest::parse("[trust]\npublish_cooldown = \"2w\"\n").is_err());
        assert!(Manifest::parse("[trust]\npublish_cooldown = 24\n").is_err());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30m").unwrap(), 1_800);
        assert_eq!(parse_duration("3600s").unwrap(), 3_600);
        assert_eq!(parse_duration("0").unwrap(), 0);
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("-5h").is_err());
    }

    #[test]
    fn trust_rejects_a_malformed_identity() {
        // A typo'd grant must fail loudly, not silently authorize nothing.
        assert!(Manifest::parse("[trust]\nnative = [\"not-an-identity\"]\n").is_err());
        assert!(Manifest::parse("[trust]\nnative = \"acme/x\"\n").is_err()); // must be an array
        // A command binding's provider is validated as an identity, and its value must be a string.
        assert!(
            Manifest::parse("[trust.commands]\nmigrate = \"not-an-identity\"\n").is_err(),
            "a malformed provider identity must fail"
        );
        assert!(
            Manifest::parse("[trust.commands]\nmigrate = 42\n").is_err(),
            "a non-string binding value must fail"
        );
        assert!(
            Manifest::parse("[trust.commands]\nmigrate = \"acme/db:\"\n").is_err(),
            "an empty exported name after `:` must fail"
        );
    }

    #[test]
    fn a_bare_script_has_no_package() {
        let m = Manifest::parse("[targets.dev.tiers]\ntest = true\n").expect("valid");
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
                git_ref: GitRef::Tag("v1.2.0".to_string()),
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
    fn a_git_dependency_tracks_a_tag_a_branch_or_head() {
        fn git_ref_of(toml: &str) -> GitRef {
            let m = Manifest::parse(toml).expect("valid");
            match &m.dependencies()["x"] {
                Dependency::Git { url, git_ref } => {
                    assert_eq!(url, "https://x/y");
                    git_ref.clone()
                }
                other => panic!("expected a git dependency, got {other:?}"),
            }
        }

        // A tag (the release form), a branch, or bare `git` — the default-branch HEAD (the tag-free
        // in-dev/bundled case).
        assert_eq!(
            git_ref_of("[dependencies]\nx = { git = \"https://x/y\", tag = \"v1.0.0\" }\n"),
            GitRef::Tag("v1.0.0".to_string())
        );
        assert_eq!(
            git_ref_of("[dependencies]\nx = { git = \"https://x/y\", branch = \"main\" }\n"),
            GitRef::Branch("main".to_string())
        );
        assert_eq!(
            git_ref_of("[dependencies]\nx = { git = \"https://x/y\" }\n"),
            GitRef::Head
        );
        // `tag` and `branch` together is a conflict.
        assert!(
            Manifest::parse(
                "[dependencies]\nx = { git = \"https://x/y\", tag = \"v1\", branch = \"main\" }\n"
            )
            .is_err()
        );
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
            assert!(
                err.message().contains("built-in import root"),
                "{key}: {err}"
            );
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
             [tiers]\n\
             test = \"std\"\n\
             [targets.dev.tiers]\n\
             test = true\n",
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
             test = true\n\
             debug = true\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["debug", "test"]);
    }

    #[test]
    fn an_inactive_tier_is_dropped_from_the_live_set() {
        // `false` is a live-set entry that turns a tier off — it lets a derived target (`extends`)
        // switch an inherited tier off without redeclaring the base.
        let m = Manifest::parse(
            "[targets.dev.tiers]\n\
             test = true\n\
             bench = false\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("dev").unwrap(), vec!["test"]);
    }

    #[test]
    fn a_provider_string_in_a_target_live_set_is_rejected() {
        // The old shape (`[targets.*.tiers]` naming a provider) is gone: the live-set takes booleans,
        // and the error points at the top-level `[tiers]` table where providers now live.
        let err = Manifest::parse("[targets.dev.tiers]\ntest = \"std\"\n").unwrap_err();
        assert!(err.message().contains("must be `true` or `false`"), "{err}");
        assert!(err.message().contains("[tiers]"), "{err}");
    }

    #[test]
    fn extends_merges_base_then_overrides() {
        let m = Manifest::parse(
            "[targets.base.tiers]\n\
             test = true\n\
             doc = true\n\
             [targets.ci]\n\
             extends = \"base\"\n\
             [targets.ci.tiers]\n\
             bench = true\n",
        )
        .expect("valid manifest");
        // ci inherits test+doc from base and adds bench.
        assert_eq!(m.active_tiers("ci").unwrap(), vec!["bench", "doc", "test"]);
        // A minimalist target opts into nothing.
        let empty = Manifest::parse("[targets.prod]\n").expect("valid manifest");
        assert!(empty.active_tiers("prod").unwrap().is_empty());
    }

    #[test]
    fn extends_can_turn_an_inherited_tier_off() {
        let m = Manifest::parse(
            "[targets.base.tiers]\n\
             test = true\n\
             debug = true\n\
             [targets.prod]\n\
             extends = \"base\"\n\
             [targets.prod.tiers]\n\
             debug = false\n",
        )
        .expect("valid manifest");
        assert_eq!(m.active_tiers("prod").unwrap(), vec!["test"]);
    }

    #[test]
    fn tier_names_are_open_but_must_be_identifiers() {
        // The activation live-set names local tier names; only the *shape* is validated here.
        let m = Manifest::parse("[targets.dev.tiers]\nfuzz = true\n").expect("open name-space");
        assert_eq!(m.active_tiers("dev").unwrap(), ["fuzz"]);
        let err = Manifest::parse("[targets.dev.tiers]\n\"fu zz\" = true\n").unwrap_err();
        assert!(err.message().contains("not a valid tier name"), "{err}");
    }

    #[test]
    fn an_undeclared_tier_provider_is_rejected() {
        // A `[tiers]` provider that is neither `std` nor a declared dependency is an error.
        let err = Manifest::parse("[tiers]\nbench = \"criterion\"\n").unwrap_err();
        assert!(err.message().contains("[dependencies]"), "{err}");
    }

    #[test]
    fn an_active_tier_without_a_tiers_entry_is_rejected() {
        // Joining the two axes: activating a tier the package's `[tiers]` never mapped is an error.
        let m = Manifest::parse("[targets.dev.tiers]\nbench = true\n").unwrap();
        let err = m.active_tier_providers("dev").unwrap_err();
        assert!(err.message().contains("no `[tiers]` entry"), "{err}");
    }

    #[test]
    fn a_declared_dependency_may_provide_a_tier() {
        // package-manager P2.6: a resolved dependency (`bench_kit`) is a valid tier provider, named
        // in `[tiers]`; the target's live-set activates the local names.
        let m = Manifest::parse(
            "[dependencies]\n\
             bench_kit = { path = \"../bench_kit\" }\n\
             [tiers]\n\
             bench = \"bench_kit\"\n\
             test = \"std\"\n\
             [targets.dev.tiers]\n\
             bench = true\n\
             test = true\n",
        )
        .expect("valid");
        let providers = m.active_tier_providers("dev").unwrap();
        assert_eq!(providers["bench"], "bench_kit"); // provided by the dependency
        assert_eq!(providers["test"], "std"); // the std-declared tier
    }

    #[test]
    fn a_tier_may_be_renamed_off_its_provider() {
        // Surface rename: a local `@crit` bound to `criterion`'s `bench`, so it does not collide
        // with std's own `bench`.
        let m = Manifest::parse(
            "[dependencies]\n\
             criterion = { path = \"../criterion\" }\n\
             [tiers]\n\
             bench = \"std\"\n\
             crit = \"criterion:bench\"\n",
        )
        .expect("valid");
        assert_eq!(m.tiers()["bench"].provider_key, "std");
        assert_eq!(m.tiers()["bench"].exported, "bench");
        assert_eq!(m.tiers()["crit"].provider_key, "criterion");
        assert_eq!(m.tiers()["crit"].exported, "bench");
    }

    #[test]
    fn a_local_name_cannot_be_both_a_directive_and_a_tier() {
        let err = Manifest::parse(
            "[dependencies]\n\
             para = { path = \"../para\" }\n\
             [directives]\n\
             foo = \"para\"\n\
             [tiers]\n\
             foo = \"para\"\n",
        )
        .unwrap_err();
        assert!(
            err.message().contains("both `[directives]` and `[tiers]`"),
            "{err}"
        );
    }

    #[test]
    fn unknown_target_is_an_error() {
        let m = Manifest::parse("[targets.dev.tiers]\ntest = true\n").unwrap();
        assert!(
            m.active_tiers("nope")
                .unwrap_err()
                .message()
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
    fn add_dependency_writes_and_widens_a_repeated_key_into_a_scope() {
        let dir = std::env::temp_dir().join("noeta_add_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MANIFEST_NAME);
        std::fs::write(&path, "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();

        add_dependency(&path, "http", "{ git = \"https://x/y\", tag = \"v1.0.0\" }").unwrap();
        let m = Manifest::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(matches!(m.dependencies()["http"], Dependency::Git { .. }));

        // A non-identifier key is rejected — it could never be an import root.
        assert!(add_dependency(&path, "bad-key", "\"^1\"").is_err());

        // A second package under one key widens the entry into a scope array rather than failing,
        // and a third appends to it — the shape that binds `para/aether` *and* `para/db`.
        let scoped = dir.join("scoped");
        std::fs::create_dir_all(&scoped).unwrap();
        let path = scoped.join(MANIFEST_NAME);
        std::fs::write(&path, "[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n").unwrap();
        add_dependency(
            &path,
            "para",
            "{ version = \"^0.1\", package = \"para/aether\" }",
        )
        .unwrap();
        add_dependency(
            &path,
            "para",
            "{ version = \"^0.1\", package = \"para/html\" }",
        )
        .unwrap();
        add_dependency(
            &path,
            "para",
            "{ version = \"^0.1\", package = \"para/db\" }",
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let m = Manifest::parse(&text).unwrap();
        let Dependency::Scope(members) = &m.dependencies()["para"] else {
            panic!(
                "expected a scope dependency, got {:?}",
                m.dependencies()["para"]
            );
        };
        assert_eq!(members.len(), 3);
        let packages: Vec<String> = members
            .iter()
            .map(|d| match d {
                Dependency::Registry { package, .. } => {
                    let p = package.as_ref().expect("package identity");
                    format!("{}/{}", p.company, p.package)
                }
                other => panic!("expected registry members, got {other:?}"),
            })
            .collect();
        assert_eq!(packages, ["para/aether", "para/html", "para/db"]);
        // The `[package]` table above it is untouched — the edit rewrites only the one entry.
        assert!(text.starts_with("[package]\nname = \"a/b\"\nversion = \"1.0.0\"\n"));

        // Re-adding an identical source is still refused: it would bind one package twice.
        assert!(
            add_dependency(
                &path,
                "para",
                "{ version = \"^0.1\", package = \"para/db\" }"
            )
            .is_err()
        );
    }

    #[test]
    fn native_rings_reads_the_native_table() {
        let dir = std::env::temp_dir().join("noeta_native_rings_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_NAME),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [native]\nrings = [\"ring-postgres\"]\n",
        )
        .unwrap();
        let entry = dir.join("main.noe");
        std::fs::write(&entry, "echo 1\n").unwrap();
        assert_eq!(native_rings(&entry), vec!["ring-postgres".to_string()]);

        // No `[native]` table → no extra rings (today's default behavior).
        std::fs::write(
            dir.join(MANIFEST_NAME),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(native_rings(&entry).is_empty());
    }

    #[test]
    fn parses_a_scope_dependency_array() {
        let m = Manifest::parse(
            "[dependencies]\n\
             para = [ { path = \"../para-aether\" }, { path = \"../para-db\" } ]\n",
        )
        .expect("valid");
        match &m.dependencies()["para"] {
            Dependency::Scope(members) => {
                assert_eq!(members.len(), 2);
                assert!(matches!(members[0], Dependency::Path { .. }));
                assert!(matches!(members[1], Dependency::Path { .. }));
            }
            other => panic!("expected a scope dependency, got {other:?}"),
        }
    }

    #[test]
    fn a_scope_dependency_rejects_empty_and_nested_arrays() {
        // An empty scope names no member packages.
        assert!(
            Manifest::parse("[dependencies]\npara = []\n")
                .unwrap_err()
                .message()
                .contains("at least one")
        );
        // A scope's members are package sources, not nested scopes.
        assert!(
            Manifest::parse("[dependencies]\npara = [ [ { path = \"x\" } ] ]\n")
                .unwrap_err()
                .message()
                .contains("nested")
        );
    }

    #[test]
    fn inheritance_cycle_is_detected() {
        let m = Manifest::parse("[targets.a]\nextends = \"b\"\n[targets.b]\nextends = \"a\"\n")
            .unwrap();
        assert!(m.active_tiers("a").unwrap_err().message().contains("cycle"));
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
        // A dev-only `@lint` tier: its provider is a dep declared only under `[targets.dev]`, named
        // in the package-level `[tiers]` table and activated in `dev`'s live-set.
        let m = Manifest::parse(
            "[targets.dev.dependencies]\nlinter = { path = \"../lint\" }\n\
             [tiers]\nlint = \"linter\"\n\
             [targets.dev.tiers]\nlint = true\n",
        );
        assert!(m.is_ok(), "target-scoped provider should validate: {m:?}");
        // Still rejected when the provider is declared nowhere.
        assert!(Manifest::parse("[tiers]\nlint = \"ghost\"\n").is_err());
    }
}
