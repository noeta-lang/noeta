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
}

/// A resolved dependency's origin (package-manager P2.4).
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the lockfile writer (P2.4c)
pub enum ResolvedSource {
    /// A local source tree, recorded as written in the manifest.
    Path { path: PathBuf },
    /// A git tag pinned to the commit SHA it resolved to.
    Git {
        url: String,
        tag: String,
        sha: String,
    },
}

/// A materialized package during the walk — its identity, version, its own namespace root segment,
/// its on-disk tree, and its dependency edges (local key → child identity).
struct Instance {
    version: Version,
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
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(manifest_path) = crate::manifest::find(dir) else {
        return Ok(ResolvedGraph {
            packages: Vec::new(),
            locked: Vec::new(),
            native_crates: Vec::new(),
            trusted_command_roots: Vec::new(),
        });
    };
    let manifest = read_manifest(&manifest_path)?;
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
    let mut walker = Walker {
        instances: BTreeMap::new(),
        store: None,
        lock: &lock,
        index: None,
        native_trust,
        solution: BTreeMap::new(),
    };
    // Phase 4, S5b: first *select versions* — gather the candidate graph (materialize the path/git
    // spine, query the index for every registry candidate + its deps) and run PubGrub. This backtracks
    // over version ranges, so a solvable diamond resolves to a compatible set instead of a greedy
    // false conflict. The walk then materializes exactly the solved versions.
    walker.solve(&manifest, &manifest_dir)?;
    let mut root_edges = BTreeMap::new();
    walker.walk(manifest.dependencies(), &manifest_dir, &mut root_edges)?;

    let graph = assemble(walker.instances, &root_edges, &manifest.trust().commands);

    // Refresh the lockfile (best-effort: a read-only project must not fail a build). Skipped for a
    // manifest with no resolved dependencies, so a bare-`[profiles]` project grows no lock.
    if !graph.locked.is_empty() {
        let _ = crate::lock::write(&manifest_dir, &graph.locked);
    }
    Ok(graph)
}

/// Carries the walk's growing state: the deduped package instances, the lazily-opened package store
/// (opened only when a git dependency is first encountered), and the lockfile consulted for git pins.
struct Walker<'a> {
    instances: BTreeMap<String, Instance>,
    store: Option<Store>,
    lock: &'a crate::lock::Lock,
    index: Option<Box<dyn crate::registry::Index>>,
    /// The root manifest's `[trust].native` — package identities allowed to run native code (Phase 4).
    native_trust: &'a std::collections::BTreeSet<String>,
    /// The resolved `identity → version` map (Phase 4, S5b), computed by [`Walker::solve`] before the
    /// walk. The walk materializes each registry dependency at *its* selected version rather than
    /// greedily picking the highest; empty until `solve` runs (a pure path/git graph leaves registry
    /// selection unused).
    solution: BTreeMap<String, Version>,
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
            Dependency::Git { url, tag } => self.fetch_git(key, url, tag, None),
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
                let index = self.index()?;
                let coords = index
                    .releases(&name)
                    .map_err(|err| format!("dependency `{key}`: {err}"))?
                    .into_iter()
                    .find(|r| r.version == version)
                    .map(|r| r.coords)
                    .ok_or_else(|| {
                        format!("dependency `{key}` (`{name}`): resolved version {version} is not in the index")
                    })?;
                // The registry pins the SHA (Phase 4, S2), so a first resolve fetches by it rather
                // than trusting the tag's current target.
                self.fetch_git(key, &coords.url, &coords.tag, Some(&coords.sha))
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
        tag: &str,
        registry_sha: Option<&str>,
    ) -> Result<(PathBuf, ResolvedSource), String> {
        let pin = self
            .lock
            .git_pin(url, tag)
            .or(registry_sha)
            .map(str::to_string);
        let store = self.store()?;
        let fetched = match &pin {
            Some(sha) => crate::git::fetch_pinned(url, tag, sha, store),
            None => crate::git::fetch(url, tag, store),
        }
        .map_err(|err| format!("dependency `{key}`: {err}"))?;
        Ok((
            fetched.path,
            ResolvedSource::Git {
                url: url.to_string(),
                tag: tag.to_string(),
                sha: fetched.sha,
            },
        ))
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

    /// The registry index, opened on first use (only a registry dependency needs it): the networked
    /// index when configured (`NOETA_REGISTRY_URL` + the `registry-http` feature), else the local one.
    fn index(&mut self) -> Result<&dyn crate::registry::Index, String> {
        if self.index.is_none() {
            self.index = Some(crate::registry::open_default()?);
        }
        Ok(self.index.as_deref().expect("just opened"))
    }

    /// Select one compatible version per package (Phase 4, S5b) and store it in `self.solution`.
    /// Gathers the candidate graph — the **path/git spine** (materialized to learn each package's
    /// identity, version, and dependency edges) plus every reachable **registry candidate** (queried
    /// from the index, which serves per-version deps, so no cloning) — then runs PubGrub, which
    /// backtracks over version ranges. A local/git source **overrides** the registry for that identity
    /// (a single pinned version), matching Cargo's source precedence.
    fn solve(&mut self, manifest: &Manifest, manifest_dir: &Path) -> Result<(), String> {
        let mut path_git: BTreeMap<String, PathGitCandidate> = BTreeMap::new();
        let mut registry: BTreeMap<String, Vec<crate::registry::Release>> = BTreeMap::new();
        let mut registry_queue: Vec<String> = Vec::new();

        // Root's direct dependencies as resolver requirements; path/git deps are materialized here to
        // learn their identities + edges, registry identities are queued for index loading.
        let root_deps =
            self.gather(manifest.dependencies(), manifest_dir, &mut path_git, &mut registry_queue)?;

        // Transitively load every registry candidate (and the identities its releases depend on) from
        // the index — a path/git-overridden identity is skipped (its single version already wins).
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(identity) = registry_queue.pop() {
            if path_git.contains_key(&identity) || !seen.insert(identity.clone()) {
                continue;
            }
            let releases = self
                .index()?
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
        self.solution = crate::resolve::resolve(
            &candidates,
            "\u{0}root",
            &Version::new(0, 0, 0),
            &root_deps,
        )?;
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
                    let child_manifest =
                        read_manifest(&dir.join(crate::manifest::MANIFEST_NAME))
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
                        let child_reqs =
                            self.gather(child_manifest.dependencies(), &dir, path_git, registry_queue)?;
                        path_git
                            .get_mut(&identity)
                            .expect("just inserted")
                            .deps = child_reqs;
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

/// Turn the walked instances into the loader's [`DepPackage`] list + the lockfile pins. Assigns each
/// identity its global segment (a direct dependency keeps the root's dep-table key; a transitive-only
/// package gets a synthesized unique segment) and rewrites each package's local dependency keys to the
/// global segments of the packages they resolve to ([`DepPackage::dep_renames`]).
fn assemble(
    instances: BTreeMap<String, Instance>,
    root_edges: &BTreeMap<String, String>,
    trusted_commands: &std::collections::BTreeSet<String>,
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
        });
        locked.push(LockedPackage {
            identity: identity.clone(),
            version: inst.version.clone(),
            content_hash: inst.content_hash.clone(),
            source: inst.source.clone(),
            native: inst.native.clone(),
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
            std::fs::write(dep.join("fx.noe"), "namespace imgfx.fx;\npub fn one(): int { return 1; }\n")
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
        std::fs::write(mid.join("m.noe"), "namespace mid.core;\npub fn v(): int { return 1; }\n")
            .unwrap();
        std::fs::write(
            dep.join("noeta.toml"),
            "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
        )
        .unwrap();
        std::fs::write(dep.join("fx.noe"), "namespace imgfx.fx;\npub fn one(): int { return 1; }\n")
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
}
