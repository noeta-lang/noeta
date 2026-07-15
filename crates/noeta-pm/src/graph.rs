//! Transitive dependency resolution (package-manager P2.4) — the graph walk that turns a root
//! manifest's `[dependencies]` into the full set of packages the loader links, keyed for the flat
//! link pool.
//!
//! **Why a walk.** [`crate::manifest::dependency_packages`] was *flat*: it materialized only the
//! entry's direct dependencies. A real dependency has its own `noeta.toml` with its own
//! `[dependencies]`, so linking it means materializing its whole reachable subgraph. This module does
//! that: it fetches/reads each package, recurses into its dependencies, and dedups by **package
//! identity** (`company/package`) so a package shared by several dependents is materialized once.
//!
//! **The determinism boundary still holds.** Fetching (git `ls-remote` + clone) is real host IO done
//! here, *before* compilation; the output is a set of on-disk source trees + a [`DepPackage`] list.
//! The loader/compiler then run deterministically over those, outside the differential oracle.
//!
//! **Global segments (why linking stays collision-free).** Each package refers to *its own*
//! dependencies by *its own* local keys, and those collide across packages. So every resolved package
//! identity is assigned a **globally-unique segment**: a direct dependency keeps the root's dep-table
//! key (so the entry's `use <key>.…` needs no rewrite), and a transitive-only package gets a
//! synthesized unique segment. Each package's own `use <local-dep-key>.…` is then rewritten to the
//! target package's global segment via [`DepPackage::dep_renames`] (see [`crate::graph::assemble`]).
//!
//! **The resolver seam.** With git/path sources every dependency is pinned exactly (a tag = one
//! version, a path = one tree), so version *selection* is trivial today; the walk itself detects the
//! one conflict our flat model can't allow — the same identity required at two different versions. The
//! PubGrub resolver ([`crate::resolve`]) is still run as the authoritative selection/validation pass
//! over the materialized versions, so when the registry lands (P2.5) real version *ranges* flow
//! through the same call unchanged.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};

use crate::manifest::{Dependency, Manifest};
use crate::store::{Store, hash_tree};

/// The resolved dependency graph: the packages the loader links (each a re-rooted [`DepPackage`]),
/// plus the pinned coordinates for the lockfile (P2.4c).
#[derive(Debug)]
pub struct ResolvedGraph {
    /// One entry per resolved package identity, ready for [`noeta_loader::link_with_deps`], sorted by
    /// global segment for a deterministic link + cache order.
    pub packages: Vec<noeta_loader::DepPackage>,
    /// The pinned packages, keyed by identity, for `noeta.lock` (consumed in P2.4c).
    #[allow(dead_code)]
    pub locked: Vec<LockedPackage>,
    /// Every native **entry crate** the graph carries (package-manager Phase 3, N3.1) — what the
    /// composed-toolchain build (N3.2) compiles in. Empty for a pure-Noeta graph, which is the
    /// signal that no composition is needed. Sorted by identity (deterministic compose key).
    pub native_crates: Vec<NativeCrate>,
    /// The **namespace root segments** of the native packages the root app authorized to contribute
    /// CLI commands (`[trust].commands`, package-manager Phase 4). The composer bakes these into the
    /// shim so `run_cli` registers a dependency's `noeta <cmd>` only when its package is
    /// command-trusted; std's own commands (root `"std"`) are always allowed. Sorted + deduped.
    pub trusted_command_roots: Vec<String>,
    /// scope (`company`) → the trust root established for it during the walk (provenance, Phase 4
    /// #2 / Phase 5) — a registry-served Ed25519 key or a keyless-verified OIDC identity — to be
    /// **pinned** in `noeta.lock` (trust-on-first-use). Empty when no registry dependency carried
    /// provenance.
    pub scope_trust: BTreeMap<String, crate::lock::ScopeTrust>,
    /// The **root** package's effective language [`Edition`] (follow-on F1) — the edition the merged
    /// compilation unit compiles under. [`Edition::DEFAULT`] for a bare script with no `[package]`.
    /// Per-dependency editions live on each [`LockedPackage`]; this is the one the front-end reads.
    pub root_edition: crate::edition::Edition,
}

/// A resolved package's native entry crate (Phase 3, N3.1): where the composed build finds its
/// `Cargo.toml`, validated to exist at resolve time.
#[derive(Debug, Clone)]
pub struct NativeCrate {
    /// The owning package's global identity `company/package`.
    pub identity: String,
    /// The crate directory, absolute (package root + the manifest's relative `native` dir).
    pub crate_dir: PathBuf,
}

/// A resolved package pinned for the lockfile (package-manager P2.4c).
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the lockfile writer (P2.4c)
pub struct LockedPackage {
    /// The global identity `company/package`.
    pub identity: String,
    pub version: Version,
    /// The content hash of the materialized source tree (integrity).
    pub content_hash: String,
    /// Where it came from — a local path or a git tag pinned to a commit SHA.
    pub source: ResolvedSource,
    /// The manifest's relative native-crate dir, recorded in the lock as declared (Phase 3).
    pub native: Option<String>,
    /// The package's effective language [`Edition`] (follow-on F1), pinned so a rebuild reproduces
    /// the exact edition each dependency compiled under.
    pub edition: crate::edition::Edition,
}

/// A resolved dependency's origin (package-manager P2.4).
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the lockfile writer (P2.4c)
pub enum ResolvedSource {
    /// A local source tree, recorded as written in the manifest.
    Path { path: PathBuf },
    /// A git ref (tag, branch, or default-branch HEAD) pinned to the commit SHA it resolved to.
    Git {
        url: String,
        git_ref: crate::manifest::GitRef,
        sha: String,
    },
}

/// A materialized package during the walk — its identity, version, its own namespace root segment,
/// its on-disk tree, and its dependency edges (local key → child identity).
struct Instance {
    version: Version,
    edition: crate::edition::Edition,
    root_segment: String,
    dir: PathBuf,
    content_hash: String,
    source: ResolvedSource,
    /// The manifest's relative native-crate dir, validated against `dir` (Phase 3, N3.1).
    native: Option<String>,
    /// This package's own `[dependencies]`: local key → resolved child identity.
    edges: BTreeMap<String, String>,
}

/// Resolve the full dependency graph rooted at `entry`'s manifest (package-manager P2.4). Returns an
/// empty graph when there is no manifest or no `[dependencies]` (a bare script). Every failure — an
/// unreadable/invalid manifest, a git fetch error, a registry dependency (pending P2.5), or a version
/// conflict — is a human-readable `Err`.
pub fn resolve_graph(entry: &Path) -> Result<ResolvedGraph, String> {
    resolve_graph_for(entry, None)
}

/// As [`resolve_graph`], but resolving the graph for a specific build **target** (dev-deps arc): the
/// root's dependency set is [`Manifest::active_dependencies`] for `target` — the global
/// `[dependencies]` plus that target's own and inherited `[targets.<name>.dependencies]`. `None`
/// (the [`resolve_graph`] default) is the global set, so every existing caller is unchanged. A
/// dependency's *own* target-scoped deps never apply — a dep contributes only its `[dependencies]`.
pub fn resolve_graph_for(entry: &Path, target: Option<&str>) -> Result<ResolvedGraph, String> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(manifest_path) = crate::manifest::find(dir) else {
        return Ok(ResolvedGraph {
            packages: Vec::new(),
            locked: Vec::new(),
            native_crates: Vec::new(),
            trusted_command_roots: Vec::new(),
            scope_trust: BTreeMap::new(),
            root_edition: crate::edition::Edition::DEFAULT,
        });
    };
    let manifest = read_manifest(&manifest_path)?;
    let root_deps = manifest.active_dependencies(target)?;
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    // The lock is consulted during the walk (git deps fetched by their pinned SHA — offline when
    // already stored) and refreshed afterwards.
    let lock = crate::lock::Lock::read(&manifest_dir);
    // The root manifest's `[trust]` is the sole authority (Phase 4): a native-declaring package
    // anywhere in the tree must be listed here or resolution refuses it. A dependency's own trust
    // never applies — authority flows top-down from the human.
    let native_trust = &manifest.trust().native;
    let registries = manifest.registries();
    let mut walker = Walker {
        instances: BTreeMap::new(),
        store: None,
        lock: &lock,
        indexes: BTreeMap::new(),
        registries,
        native_trust,
        solution: BTreeMap::new(),
        scope_trust: BTreeMap::new(),
    };
    // Phase 4, S5b: first *select versions* — gather the candidate graph (materialize the path/git
    // spine, query the index for every registry candidate + its deps) and run PubGrub. This backtracks
    // over version ranges, so a solvable diamond resolves to a compatible set instead of a greedy
    // false conflict. The walk then materializes exactly the solved versions.
    walker.solve(&root_deps, &manifest_dir)?;
    let mut root_edges = BTreeMap::new();
    walker.walk(&root_deps, &manifest_dir, &mut root_edges)?;

    let scope_trust = walker.scope_trust;
    // The root package's edition governs the merged compilation unit (per-package editions of the
    // dependencies are pinned individually in the lock). A bare manifest with no `[package]` compiles
    // under the default edition.
    let root_edition = manifest.package().map(|p| p.edition()).unwrap_or_default();
    let graph = assemble(
        walker.instances,
        &root_edges,
        &manifest.trust().commands,
        scope_trust,
        root_edition,
    );

    // Refresh the lockfile (best-effort: a read-only project must not fail a build). Skipped for a
    // manifest with no resolved dependencies, so a bare-`[targets]` project grows no lock.
    if !graph.locked.is_empty() {
        let _ = crate::lock::write(&manifest_dir, &graph.locked, &graph.scope_trust);
    }
    Ok(graph)
}

/// Carries the walk's growing state: the deduped package instances, the lazily-opened package store
/// (opened only when a git dependency is first encountered), and the lockfile consulted for git pins.
struct Walker<'a> {
    instances: BTreeMap<String, Instance>,
    store: Option<Store>,
    lock: &'a crate::lock::Lock,
    /// Registry indexes, opened on first use and cached by source key (private-registries arc): with a
    /// `[registries]` map a scope may resolve from a different registry than the default, so there can
    /// be more than one live index. Keyed so two scopes pointing at the same source share one client.
    indexes: BTreeMap<String, Box<dyn crate::registry::Index>>,
    /// The root manifest's `[registries]` — which registry each scope resolves from.
    registries: &'a crate::manifest::Registries,
    /// The root manifest's `[trust].native` — package identities allowed to run native code (Phase 4).
    native_trust: &'a std::collections::BTreeSet<String>,
    /// The resolved `identity → version` map (Phase 4, S5b), computed by [`Walker::solve`] before the
    /// walk. The walk materializes each registry dependency at *its* selected version rather than
    /// greedily picking the highest; empty until `solve` runs (a pure path/git graph leaves registry
    /// selection unused).
    solution: BTreeMap<String, Version>,
    /// scope → the trust root established while materializing registry deps (provenance, Phase 4
    /// #2 / Phase 5); pinned into `noeta.lock` afterwards.
    scope_trust: BTreeMap<String, crate::lock::ScopeTrust>,
}

impl Walker<'_> {
    /// Materialize and recurse into every dependency in `deps` (a manifest's `[dependencies]`),
    /// relative to `base_dir` (for path deps), recording each `key → child identity` edge into
    /// `edges`. Dedups by identity; a second sighting of an identity at a *different* version is a
    /// conflict (our flat link model permits one version per package).
    fn walk(
        &mut self,
        deps: &BTreeMap<String, Dependency>,
        base_dir: &Path,
        edges: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        for (key, dep) in deps {
            let (dir, source) = self.materialize(key, dep, base_dir)?;
            let child_manifest = read_manifest(&dir.join(crate::manifest::MANIFEST_NAME))
                .map_err(|err| format!("dependency `{key}`: {err}"))?;
            let pkg = child_manifest.package().ok_or_else(|| {
                format!(
                    "dependency `{key}` at `{}` has no `[package]` table (needed for its identity \
                     and namespace root)",
                    dir.display()
                )
            })?;
            let identity = format!("{}/{}", pkg.name.company, pkg.name.package);
            edges.insert(key.clone(), identity.clone());

            if let Some(existing) = self.instances.get(&identity) {
                if existing.version != pkg.version {
                    return Err(format!(
                        "dependency conflict: `{identity}` is required at both {} and {} — a \
                         package may appear at only one version (they share one flat namespace)",
                        existing.version, pkg.version
                    ));
                }
                continue; // already materialized and its subtree walked
            }

            // A declared native crate must exist where the manifest points (Phase 3, N3.1) —
            // checked here, where the materialized package root is known, so a git dep's typo'd
            // `native` fails at resolve time with the dependency named, not at compose time.
            if let Some(native) = &pkg.native {
                // Phase 4 authority gate: a native-declaring package runs arbitrary Rust (its
                // `cargo` build + the composed code), so it is refused unless the **root** app's
                // `[trust].native` lists its identity — even when reached transitively. Authority is
                // never inherited from a dependency, so a package can't smuggle native code in
                // through its own sub-dependencies; the human sees the whole native footprint.
                if !self.native_trust.contains(&identity) {
                    return Err(format!(
                        "dependency `{key}` (`{identity}`) ships native code (`native = \
                         \"{native}\"`), which runs arbitrary Rust at build + run time. It is not \
                         authorized: add `{identity}` to the `[trust].native` list in your \
                         `noeta.toml` to allow it (this grant is deliberately explicit — a \
                         dependency, even a transitive one, can never authorize its own native code)."
                    ));
                }
                validate_native_crate(&dir, native)
                    .map_err(|err| format!("dependency `{key}` (`{identity}`): {err}"))?;
            }
            let content_hash = hash_tree(&dir)
                .map_err(|err| format!("dependency `{key}`: hashing `{}`: {err}", dir.display()))?;
            // A git source is immutable, so a lock-recorded hash must match — a mismatch means the
            // stored tree drifted from what the lock pinned. A path source is a mutable local tree,
            // so its hash legitimately changes as the developer edits it; it is not verified.
            if matches!(source, ResolvedSource::Git { .. })
                && let Some(locked) = self.lock.content_hash(&identity)
                && locked != content_hash
            {
                return Err(format!(
                    "dependency `{key}` (`{identity}`) content hash does not match `{}` — the \
                     stored source drifted from the lock; run `noeta update` to re-pin",
                    crate::lock::LOCK_NAME
                ));
            }
            // Insert before recursing so a dependency cycle terminates (a back-edge sees the
            // in-progress instance and dedups).
            self.instances.insert(
                identity.clone(),
                Instance {
                    version: pkg.version.clone(),
                    edition: pkg.edition(),
                    root_segment: pkg.name.root().to_string(),
                    dir: dir.clone(),
                    content_hash,
                    source,
                    native: pkg.native.clone(),
                    edges: BTreeMap::new(),
                },
            );
            let mut child_edges = BTreeMap::new();
            self.walk(child_manifest.dependencies(), &dir, &mut child_edges)?;
            self.instances
                .get_mut(&identity)
                .expect("just inserted")
                .edges = child_edges;
        }
        Ok(())
    }

    /// Materialize one dependency to an on-disk directory (package-manager P2.4): a path dep is its
    /// local tree (relative to `base_dir`); a git dep is fetched into the store (its tag resolved to a
    /// commit SHA); a registry dep errors, pending the registry index (P2.5). Returns the directory
    /// and its pinned source coordinates.
    fn materialize(
        &mut self,
        key: &str,
        dep: &Dependency,
        base_dir: &Path,
    ) -> Result<(PathBuf, ResolvedSource), String> {
        match dep {
            Dependency::Path { path } => {
                // Canonicalize the joined directory so module names/spans (and the editor URIs built
                // from them) are clean absolute paths, not `…/app/../dep/…`. The manifest-relative
                // `path` is kept verbatim in the lock entry.
                let joined = base_dir.join(path);
                let dir = joined.canonicalize().unwrap_or(joined);
                Ok((dir, ResolvedSource::Path { path: path.clone() }))
            }
            Dependency::Git { url, git_ref } => self.fetch_git(key, url, git_ref, None),
            Dependency::Registry { package, .. } => {
                // Materialize the **resolver-selected** version (Phase 4, S5b): the PubGrub solve
                // already chose one compatible version per identity, so look up the coordinates of
                // `solution[identity]` in the index rather than greedily re-picking the highest.
                let package = package.as_ref().ok_or_else(|| {
                    format!(
                        "dependency `{key}` is a registry dependency but names no package — add \
                         `package = \"company/pkg\"` (the registry identity, decoupled from the \
                         import-root key)"
                    )
                })?;
                let name = format!("{}/{}", package.company, package.package);
                let version = self.solution.get(&name).cloned().ok_or_else(|| {
                    format!("dependency `{key}` (`{name}`) is not in the resolved version set")
                })?;
                let scope = package.company.clone();
                let scope_key = self
                    .index_for(&scope)?
                    .scope_key(&scope)
                    .map_err(|err| format!("dependency `{key}`: {err}"))?;
                let release = self
                    .index_for(&scope)?
                    .releases(&name)
                    .map_err(|err| format!("dependency `{key}`: {err}"))?
                    .into_iter()
                    .find(|r| r.version == version)
                    .ok_or_else(|| {
                        format!("dependency `{key}` (`{name}`): resolved version {version} is not in the index")
                    })?;
                // Provenance (Phase 4 #2 / Phase 5): pin the scope's trust root on first use,
                // reject a changed key / changed identity / downgraded root, and verify the
                // signature or keyless bundle (under the `provenance`/`keyless` features).
                self.check_provenance(key, &name, &release, scope_key.as_deref())?;
                let coords = release.coords;
                // The registry pins the SHA (Phase 4, S2), so a first resolve fetches by it rather
                // than trusting the tag's current target. A published release is always a tag.
                let git_ref = crate::manifest::GitRef::Tag(coords.tag.clone());
                self.fetch_git(key, &coords.url, &git_ref, Some(&coords.sha))
            }
        }
    }

    /// Materialize a git `url`@`tag` into the store (package-manager P2.4). Shared by a direct `git`
    /// dependency (`registry_sha = None`) and a resolved registry dependency (`registry_sha = Some`,
    /// the index-pinned commit). The SHA to fetch is, in precedence: the **lockfile** pin (the
    /// reproducibility authority once written) → the **registry** pin (closes trust-on-first-use on a
    /// first registry resolve) → an `ls-remote` of the tag (a bare `git` dep's first fetch). A pinned
    /// SHA already in the store needs no network at all.
    fn fetch_git(
        &mut self,
        key: &str,
        url: &str,
        git_ref: &crate::manifest::GitRef,
        registry_sha: Option<&str>,
    ) -> Result<(PathBuf, ResolvedSource), String> {
        let pin = self
            .lock
            .git_pin(url, git_ref)
            .or(registry_sha)
            .map(str::to_string);
        let store = self.store()?;
        let fetched = match &pin {
            Some(sha) => crate::git::fetch_pinned(url, git_ref, sha, store),
            None => crate::git::fetch(url, git_ref, store),
        }
        .map_err(|err| format!("dependency `{key}`: {err}"))?;
        Ok((
            fetched.path,
            ResolvedSource::Git {
                url: url.to_string(),
                git_ref: git_ref.clone(),
                sha: fetched.sha,
            },
        ))
    }

    /// Provenance check for a resolved registry release (Phase 4 #2 / Phase 5). Three layers:
    ///  1. **Trust-root TOFU** (always, no crypto — [`provenance_decision`]): a scope's trust root
    ///     (key or keyless identity) is pinned in `noeta.lock` on first use. A later registry
    ///     serving a *different* key, or switching a scope's root in either direction, or serving
    ///     a key-signed/unsigned release for a keyless-pinned scope (**downgrade**), is rejected —
    ///     this is what defends a registry compromised *after* first use.
    ///  2. **Key verification** (`provenance` feature — the CLI; the LSP does the trust-shape
    ///     checks but skips crypto): a key-signed release must verify against the pinned key.
    ///  3. **Keyless verification** (`keyless` feature): a bundle must verify offline — chain,
    ///     SCT, inclusion proof + checkpoint — and prove exactly the pinned identity (first use:
    ///     whatever identity it proves is what gets pinned).
    ///
    /// An **unsigned** release from an *unpinned or key-pinned* scope is allowed (unverified,
    /// gradual adoption); `noeta audit` surfaces which dependencies are verified.
    fn check_provenance(
        &mut self,
        key: &str,
        name: &str,
        release: &crate::registry::Release,
        served_key: Option<&str>,
    ) -> Result<(), String> {
        let scope = name.split('/').next().unwrap_or(name);
        let action = provenance_decision(
            self.lock.scope_trust(scope),
            release.signature.as_deref(),
            release.bundle.as_deref(),
            served_key,
        )
        .map_err(|reason| format!("dependency `{key}` (`{name}`): {reason}"))?;
        #[cfg(any(feature = "provenance", feature = "keyless"))]
        let (version, sha) = (&release.version, release.coords.sha.as_str());
        match action {
            ProvenanceAction::AllowUnverified => {}
            ProvenanceAction::Key {
                key: public_key,
                signature,
            } => {
                #[cfg(feature = "provenance")]
                if let Some(sig) = &signature {
                    let attestation = crate::provenance::Attestation { name, version, sha };
                    crate::provenance::verify(&attestation, sig, &public_key).map_err(|err| {
                        format!("dependency `{key}` (`{name}`): provenance {err}")
                    })?;
                }
                #[cfg(not(feature = "provenance"))]
                let _ = signature;
                // Pin on first use / re-pin (stable) — after verification, so a bad signature
                // never establishes a pin.
                self.scope_trust
                    .insert(scope.to_string(), crate::lock::ScopeTrust::Key(public_key));
            }
            ProvenanceAction::Keyless { bundle, pinned } => {
                #[cfg(feature = "keyless")]
                {
                    let attestation = crate::provenance::Attestation { name, version, sha };
                    let digest = crate::keyless::attested_digest(&attestation);
                    let policy =
                        pinned
                            .as_ref()
                            .map(|(issuer, identity)| crate::keyless::IdentityPolicy {
                                issuer: issuer.clone(),
                                identity: identity.clone(),
                            });
                    let verified = crate::keyless::verify_bundle(&bundle, &digest, policy.as_ref())
                        .map_err(|err| format!("dependency `{key}` (`{name}`): {err}"))?;
                    // Pin what verification *proved* (== the pin when one existed, since the
                    // policy enforced it); on first use this is the TOFU identity pin.
                    self.scope_trust.insert(
                        scope.to_string(),
                        crate::lock::ScopeTrust::Keyless {
                            issuer: verified.issuer,
                            identity: verified.identity,
                        },
                    );
                }
                #[cfg(not(feature = "keyless"))]
                {
                    let _ = bundle;
                    // No crypto (the LSP): the downgrade/shape checks above still ran. Keep an
                    // existing pin stable; a *first* pin needs verification to learn the identity,
                    // so it waits for a CLI resolve.
                    if let Some((issuer, identity)) = pinned {
                        self.scope_trust.insert(
                            scope.to_string(),
                            crate::lock::ScopeTrust::Keyless { issuer, identity },
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// The package store, opened on first use (only a git dependency needs it).
    fn store(&mut self) -> Result<&Store, String> {
        if self.store.is_none() {
            self.store = Some(Store::open().ok_or_else(|| {
                "cannot open the package store (no writable cache directory) — needed for git \
                 dependencies"
                    .to_string()
            })?);
        }
        Ok(self.store.as_ref().expect("just opened"))
    }

    /// The registry index for `company`'s packages, opened on first use and cached (private-registries
    /// arc). The `[registries]` map routes the scope to its source — a specific hosted registry, a
    /// GitHub org, or (unmapped) the environment default (`NOETA_REGISTRY_URL` + `registry-http`, else
    /// the local index). Two scopes on the same source share one client.
    fn index_for(&mut self, company: &str) -> Result<&dyn crate::registry::Index, String> {
        let source = self.registries.source_for(company);
        let key = registry_cache_key(source);
        if !self.indexes.contains_key(&key) {
            let index = crate::registry::open_source(source)?;
            self.indexes.insert(key.clone(), index);
        }
        Ok(self.indexes.get(&key).expect("just inserted").as_ref())
    }

    /// Select one compatible version per package (Phase 4, S5b) and store it in `self.solution`.
    /// Gathers the candidate graph — the **path/git spine** (materialized to learn each package's
    /// identity, version, and dependency edges) plus every reachable **registry candidate** (queried
    /// from the index, which serves per-version deps, so no cloning) — then runs PubGrub, which
    /// backtracks over version ranges. A local/git source **overrides** the registry for that identity
    /// (a single pinned version), matching Cargo's source precedence.
    fn solve(
        &mut self,
        root_manifest_deps: &BTreeMap<String, crate::manifest::Dependency>,
        manifest_dir: &Path,
    ) -> Result<(), String> {
        let mut path_git: BTreeMap<String, PathGitCandidate> = BTreeMap::new();
        let mut registry: BTreeMap<String, Vec<crate::registry::Release>> = BTreeMap::new();
        let mut registry_queue: Vec<String> = Vec::new();

        // Root's direct dependencies as resolver requirements; path/git deps are materialized here to
        // learn their identities + edges, registry identities are queued for index loading.
        let root_deps = self.gather(
            root_manifest_deps,
            manifest_dir,
            &mut path_git,
            &mut registry_queue,
        )?;

        // Transitively load every registry candidate (and the identities its releases depend on) from
        // the index — a path/git-overridden identity is skipped (its single version already wins).
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(identity) = registry_queue.pop() {
            if path_git.contains_key(&identity) || !seen.insert(identity.clone()) {
                continue;
            }
            let company = identity.split('/').next().unwrap_or(&identity).to_string();
            let releases = self
                .index_for(&company)?
                .releases(&identity)
                .map_err(|err| format!("registry package `{identity}`: {err}"))?;
            for release in &releases {
                for dep in &release.deps {
                    if !path_git.contains_key(&dep.package) && !seen.contains(&dep.package) {
                        registry_queue.push(dep.package.clone());
                    }
                }
            }
            registry.insert(identity, releases);
        }

        let candidates = Candidates {
            path_git: &path_git,
            registry: &registry,
        };
        // The synthetic root identity can't collide with a real `company/package` (no slash).
        self.solution =
            crate::resolve::resolve(&candidates, "\u{0}root", &Version::new(0, 0, 0), &root_deps)?;
        Ok(())
    }

    /// Recursively materialize the path/git dependencies in `deps` (learning each one's identity,
    /// version, and edges) and collect registry identities into `registry_queue`. Returns this level's
    /// requirements as `(child identity, requirement)` for the resolver — a path/git edge is an exact
    /// `=version`, a registry edge is its declared range. Part of [`Walker::solve`].
    fn gather(
        &mut self,
        deps: &BTreeMap<String, Dependency>,
        base_dir: &Path,
        path_git: &mut BTreeMap<String, PathGitCandidate>,
        registry_queue: &mut Vec<String>,
    ) -> Result<Vec<(String, VersionReq)>, String> {
        let mut reqs = Vec::new();
        for (key, dep) in deps {
            match dep {
                Dependency::Path { .. } | Dependency::Git { .. } => {
                    let (dir, _source) = self.materialize(key, dep, base_dir)?;
                    let child_manifest = read_manifest(&dir.join(crate::manifest::MANIFEST_NAME))
                        .map_err(|err| format!("dependency `{key}`: {err}"))?;
                    let pkg = child_manifest.package().ok_or_else(|| {
                        format!(
                            "dependency `{key}` at `{}` has no `[package]` table",
                            dir.display()
                        )
                    })?;
                    let identity = format!("{}/{}", pkg.name.company, pkg.name.package);
                    reqs.push((identity.clone(), exact_req(&pkg.version)));
                    if !path_git.contains_key(&identity) {
                        // Insert a placeholder before recursing so a dependency cycle terminates.
                        path_git.insert(
                            identity.clone(),
                            PathGitCandidate {
                                version: pkg.version.clone(),
                                deps: Vec::new(),
                            },
                        );
                        let child_reqs = self.gather(
                            child_manifest.dependencies(),
                            &dir,
                            path_git,
                            registry_queue,
                        )?;
                        path_git.get_mut(&identity).expect("just inserted").deps = child_reqs;
                    }
                }
                Dependency::Registry { package, req } => {
                    let package = package.as_ref().ok_or_else(|| {
                        format!(
                            "dependency `{key}` is a registry dependency but names no package — add \
                             `package = \"company/pkg\"`"
                        )
                    })?;
                    let identity = format!("{}/{}", package.company, package.package);
                    reqs.push((identity.clone(), req.clone()));
                    registry_queue.push(identity);
                }
            }
        }
        Ok(reqs)
    }
}

/// An exact `=x.y.z` requirement — how a path/git pin presents to the resolver.
fn exact_req(version: &Version) -> VersionReq {
    VersionReq::parse(&format!("={version}")).expect("=<version> is always a valid requirement")
}

/// A stable cache key for a `[registries]` source (private-registries arc), so two scopes routed to the
/// same registry share one opened index. `None` (the environment default) is one shared bucket.
fn registry_cache_key(source: Option<&crate::manifest::RegistrySource>) -> String {
    match source {
        None => "default".to_string(),
        Some(crate::manifest::RegistrySource::Hosted(url)) => format!("hosted:{url}"),
        Some(crate::manifest::RegistrySource::GitHub(org)) => format!("github:{org}"),
    }
}

/// A path/git package as a resolver candidate (Phase 4, S5b): its single pinned version and its
/// dependency edges. A local/git source is authoritative, so it offers exactly one version.
struct PathGitCandidate {
    version: Version,
    /// `(child identity, requirement)` — a path/git child is `=version`, a registry child its range.
    deps: Vec<(String, VersionReq)>,
}

/// The candidate graph fed to PubGrub (Phase 4, S5b): path/git packages (each a single pinned
/// version, overriding the registry for that identity) and registry packages (all published versions
/// from the index, with their per-version deps). This is what lets the resolver backtrack over ranges.
struct Candidates<'a> {
    path_git: &'a BTreeMap<String, PathGitCandidate>,
    registry: &'a BTreeMap<String, Vec<crate::registry::Release>>,
}

impl crate::resolve::Registry for Candidates<'_> {
    fn versions(&self, package: &str) -> Vec<Version> {
        if let Some(candidate) = self.path_git.get(package) {
            vec![candidate.version.clone()] // a local/git source overrides the registry
        } else {
            self.registry
                .get(package)
                .map(|releases| releases.iter().map(|r| r.version.clone()).collect())
                .unwrap_or_default()
        }
    }

    fn dependencies(&self, package: &str, version: &Version) -> Vec<(String, VersionReq)> {
        if let Some(candidate) = self.path_git.get(package) {
            if &candidate.version == version {
                candidate.deps.clone()
            } else {
                Vec::new()
            }
        } else if let Some(releases) = self.registry.get(package) {
            releases
                .iter()
                .find(|r| &r.version == version)
                .map(|r| {
                    r.deps
                        .iter()
                        .map(|d| (d.package.clone(), d.req.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

/// What [`provenance_decision`] tells [`Walker::check_provenance`] to do for one release — the
/// crypto-free half of the trust check, split out so the trust matrix is directly unit-testable
/// (the crypto half is a thin, feature-gated wrapper around the verified seams).
#[derive(Debug, PartialEq, Eq)]
enum ProvenanceAction {
    /// No provenance and no pin constraining it: allowed, unverified, nothing pinned.
    AllowUnverified,
    /// The key root: verify `signature` (when present) against `key`, then pin `Key(key)`.
    /// `signature: None` = an unsigned release from a key-pinned/key-served scope — allowed
    /// (gradual adoption), the key still pins.
    Key {
        key: String,
        signature: Option<String>,
    },
    /// The keyless root: verify `bundle` offline; `pinned` is the identity it must prove
    /// (`None` = first use — pin whatever identity verification establishes).
    Keyless {
        bundle: String,
        pinned: Option<(String, String)>,
    },
}

/// The **trust-root decision** for one resolved release (Phase 4 #2 / Phase 5): given the scope's
/// pinned root (if any) and the release's provenance shape, decide what to verify and what to pin —
/// or reject. Pure (no IO, no crypto), so every cell of the trust matrix is unit-tested:
///
/// | pinned ↓ / release → | bundle              | signature            | unsigned             |
/// |----------------------|---------------------|----------------------|----------------------|
/// | Keyless(identity)    | verify against pin  | **reject** (downgrade) | **reject** (downgrade) |
/// | Key(k)               | **reject** (switch) | verify against k     | allow, keep pin      |
/// | (none)               | verify, TOFU-pin    | verify vs served key, TOFU-pin | allow    |
///
/// Root *switches* are never implicit in either direction: keyless→key is a downgrade an attacker
/// wants, and key→keyless would let anyone with *any* OIDC identity re-pin a stolen scope. Both
/// require the consumer's explicit `noeta update` (which re-resolves with no pin and re-TOFUs).
fn provenance_decision(
    pinned: Option<&crate::lock::ScopeTrust>,
    signature: Option<&str>,
    bundle: Option<&str>,
    served_key: Option<&str>,
) -> Result<ProvenanceAction, String> {
    use crate::lock::ScopeTrust;
    // Defense in depth: publish already rejects a release carrying both roots
    // (`Release::check_provenance_shape`), but a malicious registry can serve anything.
    if signature.is_some() && bundle.is_some() {
        return Err(
            "the registry served a release carrying both a key signature and a keyless bundle — \
             malformed provenance (a release has exactly one trust root)"
                .to_string(),
        );
    }
    match (pinned, signature, bundle) {
        // Both-set was rejected above; spelled out so the match stays provably exhaustive.
        (_, Some(_), Some(_)) => unreachable!("rejected by the both-roots check"),
        // ── Scope pinned keyless ───────────────────────────────────────────────────────────────
        (Some(ScopeTrust::Keyless { issuer, identity }), None, Some(bundle)) => {
            Ok(ProvenanceAction::Keyless {
                bundle: bundle.to_string(),
                pinned: Some((issuer.clone(), identity.clone())),
            })
        }
        (Some(ScopeTrust::Keyless { .. }), ..) => Err(format!(
            "this scope is pinned to keyless (Sigstore) provenance in `{}`, but the registry \
             served a release without a keyless bundle — a downgrade to a weaker trust root, which \
             is how a compromised registry would smuggle a forged release past the transparency \
             log. If the maintainer really stepped back from keyless signing, reconcile with them, \
             then run `noeta update` to re-pin.",
            crate::lock::LOCK_NAME
        )),
        // ── Scope pinned to a key ──────────────────────────────────────────────────────────────
        (Some(ScopeTrust::Key(_)), None, Some(_)) => Err(format!(
            "this scope is pinned to a signing key in `{}`, but the registry served a \
             keyless-signed release — a trust-root switch is never implicit (any OIDC identity \
             would otherwise be able to take over a key-pinned scope). If the maintainer migrated \
             to keyless signing, reconcile with them, then run `noeta update` to re-pin.",
            crate::lock::LOCK_NAME
        )),
        (Some(ScopeTrust::Key(pinned_key)), signature, None) => {
            if let Some(served) = served_key
                && served != pinned_key
            {
                return Err(format!(
                    "the registry's public key for this scope changed from the one pinned in \
                     `{}` — a moved or compromised signing key. Reconcile with the maintainer, \
                     then run `noeta update` to re-pin.",
                    crate::lock::LOCK_NAME
                ));
            }
            Ok(ProvenanceAction::Key {
                key: pinned_key.clone(),
                signature: signature.map(str::to_string),
            })
        }
        // ── No pin: first use ──────────────────────────────────────────────────────────────────
        (None, None, Some(bundle)) => Ok(ProvenanceAction::Keyless {
            bundle: bundle.to_string(),
            pinned: None,
        }),
        (None, Some(sig), None) => match served_key {
            Some(served) => Ok(ProvenanceAction::Key {
                key: served.to_string(),
                signature: Some(sig.to_string()),
            }),
            None => Err(
                "the release is signed, but the scope has no public key to verify it against"
                    .to_string(),
            ),
        },
        (None, None, None) => match served_key {
            // A served key with an unsigned release still pins (the existing Phase 4 behavior):
            // the scope's key is known, so a later key change is detectable even before the
            // maintainer signs releases.
            Some(served) => Ok(ProvenanceAction::Key {
                key: served.to_string(),
                signature: None,
            }),
            None => Ok(ProvenanceAction::AllowUnverified),
        },
    }
}

/// Turn the walked instances into the loader's [`DepPackage`] list + the lockfile pins. Assigns each
/// identity its global segment (a direct dependency keeps the root's dep-table key; a transitive-only
/// package gets a synthesized unique segment) and rewrites each package's local dependency keys to the
/// global segments of the packages they resolve to ([`DepPackage::dep_renames`]).
fn assemble(
    instances: BTreeMap<String, Instance>,
    root_edges: &BTreeMap<String, String>,
    trusted_commands: &std::collections::BTreeSet<String>,
    scope_trust: BTreeMap<String, crate::lock::ScopeTrust>,
    root_edition: crate::edition::Edition,
) -> ResolvedGraph {
    // Global segment per identity. Direct dependencies keep the consumer's key (so the entry's
    // `use <key>.…` needs no rewrite); transitive-only packages get a unique synthesized segment.
    let mut global: BTreeMap<String, String> = BTreeMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for (key, identity) in root_edges {
        // First root key wins if the same identity is aliased under several keys.
        global
            .entry(identity.clone())
            .or_insert_with(|| key.clone());
        used.insert(key.clone());
    }
    // Deterministic assignment order for synthesized segments.
    for identity in instances.keys() {
        if !global.contains_key(identity) {
            let seg = unique_segment(&instances[identity].root_segment, &mut used);
            global.insert(identity.clone(), seg);
        }
    }

    let mut packages = Vec::with_capacity(instances.len());
    let mut locked = Vec::with_capacity(instances.len());
    let mut native_crates = Vec::new();
    // A native package's commands register only if the root app command-trusts its identity; record
    // the namespace root segment the composer/`run_cli` filters on (Phase 4).
    let mut trusted_command_roots: Vec<String> = Vec::new();
    for (identity, inst) in &instances {
        let key = global[identity].clone();
        let dep_renames: BTreeMap<String, String> = inst
            .edges
            .iter()
            .map(|(local_key, child)| (local_key.clone(), global[child].clone()))
            .collect();
        let modules = noeta_loader::read_package_sources(&inst.dir).unwrap_or_default();
        packages.push(noeta_loader::DepPackage {
            key,
            root: inst.root_segment.clone(),
            modules,
            dep_renames,
            // A native package's modules live in its Rust extension (composed in downstream), not the
            // link pool — so the loader retains, rather than flags, a `use` under its key.
            native: inst.native.is_some(),
        });
        locked.push(LockedPackage {
            identity: identity.clone(),
            version: inst.version.clone(),
            content_hash: inst.content_hash.clone(),
            source: inst.source.clone(),
            native: inst.native.clone(),
            edition: inst.edition,
        });
        if let Some(native) = &inst.native {
            native_crates.push(NativeCrate {
                identity: identity.clone(),
                crate_dir: inst.dir.join(native),
            });
            // Commands only exist inside a native package; grant its commands only if command-trusted.
            if trusted_commands.contains(identity) {
                trusted_command_roots.push(inst.root_segment.clone());
            }
        }
    }
    // Sort by global segment so the loader's SourceId assignment and the startup-cache key are
    // deterministic regardless of walk order.
    packages.sort_by(|a, b| a.key.cmp(&b.key));
    trusted_command_roots.sort();
    trusted_command_roots.dedup();
    ResolvedGraph {
        packages,
        locked,
        native_crates,
        trusted_command_roots,
        scope_trust,
        root_edition,
    }
}

/// Check a declared native entry crate exists: `<package root>/<native>/Cargo.toml` must be a
/// file (Phase 3, N3.1). The manifest parser already rejected absolute/`..` values.
fn validate_native_crate(package_dir: &Path, native: &str) -> Result<(), String> {
    let crate_dir = package_dir.join(native);
    if !crate_dir.join("Cargo.toml").is_file() {
        return Err(format!(
            "`package.native = \"{native}\"` names no Rust crate — expected `{}`",
            crate_dir.join("Cargo.toml").display()
        ));
    }
    Ok(())
}

/// A segment unique among `used`: the preferred `base`, else `base_2`, `base_3`, … Reserves the
/// chosen segment in `used`.
fn unique_segment(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}_{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Read and parse a manifest at `path`, tagging IO/parse errors with the path.
fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read `{}`: {err}", path.display()))?;
    Manifest::parse(&text).map_err(|err| format!("invalid `{}`: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out an app + one path dep under a fresh temp base; the dep declares `native = "native"`
    /// when `with_crate` says to create the crate dir (Phase 3, N3.1). When `trusted`, the app's
    /// `[trust].native` authorizes `acme/imgfx` (Phase 4) — otherwise resolution refuses the native.
    fn native_dep_project(name: &str, with_crate: bool, trusted: bool) -> PathBuf {
        let base = std::env::temp_dir().join(format!("noeta_graph_test_{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        let dep = base.join("imgfx");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(dep.join("native")).unwrap();
        let trust = if trusted {
            "\n[trust]\nnative = [\"acme/imgfx\"]\n"
        } else {
            ""
        };
        std::fs::write(
            app.join("noeta.toml"),
            format!(
                "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
                 [dependencies]\nfx = {{ path = \"../imgfx\" }}\n{trust}"
            ),
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        std::fs::write(
            dep.join("noeta.toml"),
            "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
        )
        .unwrap();
        std::fs::write(
            dep.join("fx.noe"),
            "namespace imgfx.fx;\npub fn one(): int { return 1; }\n",
        )
        .unwrap();
        if with_crate {
            std::fs::write(
                dep.join("native").join("Cargo.toml"),
                "[package]\nname = \"imgfx-native\"\nversion = \"1.0.0\"\n",
            )
            .unwrap();
        }
        app.join("main.noe")
    }

    #[test]
    fn a_target_scoped_dependency_resolves_only_for_its_target() {
        // An app with a runtime dep `fx` and a dev-only dep `tool` (dev-deps arc): the global graph
        // sees `fx`; `--target dev` also sees `tool`.
        let base = std::env::temp_dir().join("noeta_graph_test_target_deps");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        for (name, ver) in [("fx", "1.0.0"), ("tool", "1.0.0")] {
            let d = base.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("noeta.toml"),
                format!("[package]\nname = \"acme/{name}\"\nversion = \"{ver}\"\n"),
            )
            .unwrap();
            std::fs::write(
                d.join(format!("{name}.noe")),
                format!("namespace {name}.m;\npub fn one(): int {{ return 1; }}\n"),
            )
            .unwrap();
        }
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nfx = { path = \"../fx\" }\n\
             [targets.dev.dependencies]\ntool = { path = \"../tool\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        let entry = app.join("main.noe");

        let names = |g: &ResolvedGraph| -> Vec<String> {
            g.locked.iter().map(|l| l.identity.clone()).collect()
        };
        let global = resolve_graph(&entry).expect("resolves");
        assert!(names(&global).contains(&"acme/fx".to_string()));
        assert!(
            !names(&global).contains(&"acme/tool".to_string()),
            "dev dep leaked into globals"
        );

        let dev = resolve_graph_for(&entry, Some("dev")).expect("resolves");
        assert!(names(&dev).contains(&"acme/fx".to_string()));
        assert!(
            names(&dev).contains(&"acme/tool".to_string()),
            "dev dep missing under --target dev"
        );
    }

    #[test]
    fn registries_route_a_scope_to_its_configured_source() {
        // private-registries S2: a `[registries]` mapping sends `acme/*` to a github source. Resolving
        // an `acme` registry dep must reach that source (here the S3 stub error), proving the router
        // picked the per-scope registry rather than the default index.
        let base = std::env::temp_dir().join("noeta_graph_test_registry_routing");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"me/app\"\nversion = \"0.1.0\"\n\
             [registries]\nacme = \"github:acme\"\n\
             [dependencies]\nthing = { version = \"^1.0\", package = \"acme/thing\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        let err = resolve_graph(&app.join("main.noe")).expect_err("github source is a stub in S2");
        assert!(
            err.contains("github:acme") && err.contains("not implemented"),
            "routing should have reached the github source: {err}"
        );
    }

    #[test]
    fn the_resolved_graph_carries_per_package_and_root_editions() {
        // An app pinning edition 2026 explicitly, with a dependency that omits `edition` (so it
        // defaults). The root edition is the app's; each package's own edition is on its LockedPackage.
        let base = std::env::temp_dir().join("noeta_graph_test_editions");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        let dep = base.join("dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\nedition = \"2026\"\n\
             [dependencies]\nd = { path = \"../dep\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        std::fs::write(
            dep.join("noeta.toml"),
            "[package]\nname = \"acme/dep\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dep.join("d.noe"),
            "namespace dep.m;\npub fn one(): int { return 1; }\n",
        )
        .unwrap();
        let entry = app.join("main.noe");

        let graph = resolve_graph(&entry).expect("resolves");
        // Root edition = the app's explicit pin.
        assert_eq!(graph.root_edition, crate::edition::Edition::E2026);
        // The dependency omitted `edition`, so its LockedPackage records the default.
        let dep_lock = graph
            .locked
            .iter()
            .find(|l| l.identity == "acme/dep")
            .expect("dep locked");
        assert_eq!(dep_lock.edition, crate::edition::Edition::DEFAULT);
    }

    #[test]
    fn a_native_dep_surfaces_its_entry_crate_and_lock_records_it() {
        let entry = native_dep_project("native_ok", true, true);
        let graph = resolve_graph(&entry).expect("resolves");
        assert_eq!(graph.native_crates.len(), 1);
        let nc = &graph.native_crates[0];
        assert_eq!(nc.identity, "acme/imgfx");
        assert!(
            nc.crate_dir.join("Cargo.toml").is_file(),
            "absolute crate dir"
        );
        let locked = graph
            .locked
            .iter()
            .find(|l| l.identity == "acme/imgfx")
            .expect("locked");
        assert_eq!(locked.native.as_deref(), Some("native"));
        // The lockfile text carries the declaration.
        let lock_text =
            std::fs::read_to_string(entry.parent().unwrap().join(crate::lock::LOCK_NAME)).unwrap();
        assert!(lock_text.contains("native = \"native\""), "{lock_text}");
    }

    #[test]
    fn a_missing_native_crate_fails_at_resolve_time_naming_the_dep() {
        // Trusted, so it clears the authority gate and reaches the crate-existence check.
        let entry = native_dep_project("native_missing", false, true);
        let err = resolve_graph(&entry).expect_err("must fail");
        assert!(err.contains("acme/imgfx"), "{err}");
        assert!(err.contains("Cargo.toml"), "{err}");
    }

    #[test]
    fn command_trust_gates_which_native_roots_may_add_commands() {
        // A native dep trusted for native but NOT for commands contributes no trusted command root;
        // adding it to `[trust].commands` surfaces its namespace root for the composer's filter.
        let base = std::env::temp_dir().join("noeta_graph_test_cmd_trust");
        let make = |commands_trust: bool| -> Vec<String> {
            let _ = std::fs::remove_dir_all(&base);
            let app = base.join("app");
            let dep = base.join("imgfx");
            std::fs::create_dir_all(&app).unwrap();
            std::fs::create_dir_all(dep.join("native")).unwrap();
            let commands = if commands_trust {
                "commands = [\"acme/imgfx\"]\n"
            } else {
                ""
            };
            std::fs::write(
                app.join("noeta.toml"),
                format!(
                    "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
                     [dependencies]\nfx = {{ path = \"../imgfx\" }}\n\
                     [trust]\nnative = [\"acme/imgfx\"]\n{commands}"
                ),
            )
            .unwrap();
            std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
            std::fs::write(
                dep.join("noeta.toml"),
                "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
            )
            .unwrap();
            std::fs::write(
                dep.join("fx.noe"),
                "namespace imgfx.fx;\npub fn one(): int { return 1; }\n",
            )
            .unwrap();
            std::fs::write(
                dep.join("native").join("Cargo.toml"),
                "[package]\nname = \"imgfx-native\"\nversion = \"1.0.0\"\n",
            )
            .unwrap();
            resolve_graph(&app.join("main.noe"))
                .expect("resolves")
                .trusted_command_roots
        };
        // Native-trusted but not command-trusted → the package composes, but no command root.
        assert!(make(false).is_empty());
        // Command-trusted → its namespace root (`imgfx`) is surfaced for the command filter.
        assert_eq!(make(true), vec!["imgfx".to_string()]);
    }

    #[test]
    fn an_untrusted_native_dep_is_refused() {
        // Phase 4: a native-declaring dependency the app did not authorize in `[trust].native` is
        // refused — the mere presence of native code no longer runs arbitrary Rust.
        let entry = native_dep_project("native_untrusted", true, false);
        let err = resolve_graph(&entry).expect_err("must be refused");
        assert!(err.contains("acme/imgfx"), "{err}");
        assert!(err.contains("[trust].native"), "points at the fix: {err}");
        assert!(err.contains("native code"), "{err}");
    }

    #[test]
    fn a_transitive_native_dep_needs_root_trust() {
        // The anti-supply-chain invariant: a native package reached *transitively* (app → mid →
        // imgfx) is still refused unless the ROOT app trusts it — a dependency can't authorize its
        // own native sub-dependency.
        let base = std::env::temp_dir().join("noeta_graph_test_native_transitive");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        let mid = base.join("mid");
        let dep = base.join("imgfx");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&mid).unwrap();
        std::fs::create_dir_all(dep.join("native")).unwrap();
        // app trusts NOTHING, and depends on a pure `mid` which itself pulls the native `imgfx`.
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nm = { path = \"../mid\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        // `mid` "trusts" imgfx in its OWN manifest — which must be ignored (authority isn't inherited).
        std::fs::write(
            mid.join("noeta.toml"),
            "[package]\nname = \"acme/mid\"\nversion = \"1.0.0\"\n\
             [dependencies]\nfx = { path = \"../imgfx\" }\n\
             [trust]\nnative = [\"acme/imgfx\"]\n",
        )
        .unwrap();
        std::fs::write(
            mid.join("m.noe"),
            "namespace mid.core;\npub fn v(): int { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            dep.join("noeta.toml"),
            "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
        )
        .unwrap();
        std::fs::write(
            dep.join("fx.noe"),
            "namespace imgfx.fx;\npub fn one(): int { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            dep.join("native").join("Cargo.toml"),
            "[package]\nname = \"imgfx-native\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();

        let err = resolve_graph(&app.join("main.noe")).expect_err("root must authorize native");
        assert!(err.contains("acme/imgfx"), "{err}");
        assert!(err.contains("[trust].native"), "{err}");
    }

    #[test]
    fn a_pure_graph_has_no_native_crates() {
        let entry = native_dep_project("native_pure", true, false);
        // Rewrite the dep manifest without the `native` key.
        let dep_manifest = entry
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("imgfx")
            .join("noeta.toml");
        std::fs::write(
            &dep_manifest,
            "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let graph = resolve_graph(&entry).expect("resolves");
        assert!(graph.native_crates.is_empty());
    }

    // ── The trust-root decision matrix (Phase 4 #2 / Phase 5) ─────────────────────────────────
    // `provenance_decision` is the crypto-free half of `check_provenance`; every cell of its
    // matrix is pinned here. The crypto halves are covered in `provenance`/`keyless` tests.

    use crate::lock::ScopeTrust;

    fn keyless_pin() -> ScopeTrust {
        ScopeTrust::Keyless {
            issuer: "https://token.actions.githubusercontent.com".to_string(),
            identity: "https://github.com/acme/imgfx/.github/workflows/r.yaml@refs/heads/main"
                .to_string(),
        }
    }

    #[test]
    fn trust_first_use_pins_whichever_root_the_release_carries() {
        // Bundle, no pin → verify keyless with no identity constraint (TOFU establishes it).
        assert_eq!(
            provenance_decision(None, None, Some("{bundle}"), None).unwrap(),
            ProvenanceAction::Keyless {
                bundle: "{bundle}".to_string(),
                pinned: None,
            }
        );
        // Signature + served key, no pin → verify against the served key (which then pins).
        assert_eq!(
            provenance_decision(None, Some("sig"), None, Some("key")).unwrap(),
            ProvenanceAction::Key {
                key: "key".to_string(),
                signature: Some("sig".to_string()),
            }
        );
        // Unsigned but the scope serves a key → the key still pins (change detection starts now).
        assert_eq!(
            provenance_decision(None, None, None, Some("key")).unwrap(),
            ProvenanceAction::Key {
                key: "key".to_string(),
                signature: None,
            }
        );
        // Nothing at all → allowed, unverified, unpinned (gradual adoption).
        assert_eq!(
            provenance_decision(None, None, None, None).unwrap(),
            ProvenanceAction::AllowUnverified
        );
        // Signed but no key anywhere → fail closed (a signature nobody can check proves nothing).
        let err = provenance_decision(None, Some("sig"), None, None).unwrap_err();
        assert!(err.contains("no public key"), "{err}");
    }

    #[test]
    fn trust_a_keyless_pin_holds_the_release_to_that_identity() {
        let pin = keyless_pin();
        let ScopeTrust::Keyless { issuer, identity } = pin.clone() else {
            unreachable!()
        };
        assert_eq!(
            provenance_decision(Some(&pin), None, Some("{bundle}"), None).unwrap(),
            ProvenanceAction::Keyless {
                bundle: "{bundle}".to_string(),
                pinned: Some((issuer, identity)),
            }
        );
    }

    #[test]
    fn trust_downgrade_from_keyless_is_rejected() {
        let pin = keyless_pin();
        // Key-signed release for a keyless-pinned scope: the downgrade a compromised registry
        // would use to smuggle a forged release past the transparency log.
        let err = provenance_decision(Some(&pin), Some("sig"), None, Some("key")).unwrap_err();
        assert!(err.contains("downgrade"), "{err}");
        // Unsigned release for a keyless-pinned scope: same rejection.
        let err = provenance_decision(Some(&pin), None, None, None).unwrap_err();
        assert!(err.contains("downgrade"), "{err}");
    }

    #[test]
    fn trust_switch_from_key_to_keyless_is_never_implicit() {
        // Anyone owns *some* OIDC identity, so a key-pinned scope accepting any keyless bundle
        // would hand takeover to whoever compromises the registry. Explicit `noeta update` only.
        let pin = ScopeTrust::Key("key".to_string());
        let err = provenance_decision(Some(&pin), None, Some("{bundle}"), None).unwrap_err();
        assert!(err.contains("never implicit"), "{err}");
    }

    #[test]
    fn trust_a_changed_key_is_rejected_and_a_stable_key_verifies() {
        let pin = ScopeTrust::Key("old-key".to_string());
        let err = provenance_decision(Some(&pin), Some("sig"), None, Some("new-key")).unwrap_err();
        assert!(err.contains("changed"), "{err}");
        // Same served key (or none served): verify against the pinned key.
        assert_eq!(
            provenance_decision(Some(&pin), Some("sig"), None, Some("old-key")).unwrap(),
            ProvenanceAction::Key {
                key: "old-key".to_string(),
                signature: Some("sig".to_string()),
            }
        );
        assert_eq!(
            provenance_decision(Some(&pin), Some("sig"), None, None).unwrap(),
            ProvenanceAction::Key {
                key: "old-key".to_string(),
                signature: Some("sig".to_string()),
            }
        );
        // Unsigned from a key-pinned scope stays allowed (gradual adoption), pin kept.
        assert_eq!(
            provenance_decision(Some(&pin), None, None, Some("old-key")).unwrap(),
            ProvenanceAction::Key {
                key: "old-key".to_string(),
                signature: None,
            }
        );
    }

    #[test]
    fn trust_both_roots_on_one_release_is_malformed() {
        for pin in [None, Some(keyless_pin()), Some(ScopeTrust::Key("k".into()))] {
            let err =
                provenance_decision(pin.as_ref(), Some("sig"), Some("{bundle}"), None).unwrap_err();
            assert!(err.contains("exactly one trust root"), "{err}");
        }
    }
}
