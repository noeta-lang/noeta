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
pub struct ResolvedGraph {
    /// One entry per resolved package identity, ready for [`noeta_loader::link_with_deps`], sorted by
    /// global segment for a deterministic link + cache order.
    pub packages: Vec<noeta_loader::DepPackage>,
    /// The pinned packages, keyed by identity, for `noeta.lock` (consumed in P2.4c).
    #[allow(dead_code)]
    pub locked: Vec<LockedPackage>,
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
    let mut walker = Walker {
        instances: BTreeMap::new(),
        store: None,
        lock: &lock,
        index: None,
    };
    let mut root_edges = BTreeMap::new();
    walker.walk(manifest.dependencies(), &manifest_dir, &mut root_edges)?;

    validate_with_resolver(&walker.instances, &root_edges)?;
    let graph = assemble(walker.instances, &root_edges);

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
    index: Option<crate::registry::LocalIndex>,
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
                let dir = base_dir.join(path);
                Ok((
                    dir,
                    ResolvedSource::Path {
                        path: path.clone(),
                    },
                ))
            }
            Dependency::Git { url, tag } => self.fetch_git(key, url, tag),
            Dependency::Registry { package, req } => {
                // Resolve the registry identity + requirement to git coordinates, then materialize
                // exactly as a `git` dependency (the registry is a name→coords index, not a store).
                let package = package.as_ref().ok_or_else(|| {
                    format!(
                        "dependency `{key}` is a registry dependency but names no package — add \
                         `package = \"company/pkg\"` (the registry identity, decoupled from the \
                         import-root key)"
                    )
                })?;
                let name = format!("{}/{}", package.company, package.package);
                let index = self.index()?;
                let (_version, coords) = crate::registry::resolve_coords(index, &name, req)
                    .map_err(|err| format!("dependency `{key}`: {err}"))?;
                self.fetch_git(key, &coords.url, &coords.tag)
            }
        }
    }

    /// Materialize a git `url`@`tag` into the store, honoring the lockfile pin (package-manager P2.4).
    /// Shared by a direct `git` dependency and a resolved registry dependency. If the lock pins the
    /// SHA and its tree is stored, no network is touched (offline); otherwise the tag is resolved at
    /// the remote.
    fn fetch_git(
        &mut self,
        key: &str,
        url: &str,
        tag: &str,
    ) -> Result<(PathBuf, ResolvedSource), String> {
        let pin = self.lock.git_pin(url, tag).map(str::to_string);
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

    /// The registry index, opened on first use (only a registry dependency needs it).
    fn index(&mut self) -> Result<&crate::registry::LocalIndex, String> {
        if self.index.is_none() {
            self.index = Some(crate::registry::LocalIndex::open()?);
        }
        Ok(self.index.as_ref().expect("just opened"))
    }
}

/// Run the PubGrub resolver over the materialized graph as the authoritative selection/validation
/// pass (package-manager P2.4). With exact git/path pins each identity has a single candidate version
/// and an edge is an exact `=version` requirement, so this confirms a consistent solution and surfaces
/// PubGrub's explainable report on the (walk-already-caught, but defensively re-checked) conflict case.
/// When the registry lands (P2.5) the same call performs real range selection.
fn validate_with_resolver(
    instances: &BTreeMap<String, Instance>,
    root_edges: &BTreeMap<String, String>,
) -> Result<(), String> {
    let registry = GraphRegistry { instances };
    let root_deps: Vec<(String, VersionReq)> = root_edges
        .values()
        .map(|identity| (identity.clone(), exact_req(&instances[identity].version)))
        .collect();
    // The synthetic root identity can't collide with a real `company/package` (no slash).
    crate::resolve::resolve(&registry, "\u{0}root", &Version::new(0, 0, 0), &root_deps).map(|_| ())
}

/// An exact `=x.y.z` requirement — how a git/path pin presents to the resolver.
fn exact_req(version: &Version) -> VersionReq {
    VersionReq::parse(&format!("={version}")).expect("=<version> is always a valid requirement")
}

/// A [`crate::resolve::Registry`] backed by the walk's materialized instances: each identity offers
/// exactly its one pinned version, whose dependencies are exact-pinned edges.
struct GraphRegistry<'a> {
    instances: &'a BTreeMap<String, Instance>,
}

impl crate::resolve::Registry for GraphRegistry<'_> {
    fn versions(&self, package: &str) -> Vec<Version> {
        self.instances
            .get(package)
            .map(|i| vec![i.version.clone()])
            .unwrap_or_default()
    }

    fn dependencies(&self, package: &str, _version: &Version) -> Vec<(String, VersionReq)> {
        self.instances
            .get(package)
            .map(|i| {
                i.edges
                    .values()
                    .map(|child| (child.clone(), exact_req(&self.instances[child].version)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Turn the walked instances into the loader's [`DepPackage`] list + the lockfile pins. Assigns each
/// identity its global segment (a direct dependency keeps the root's dep-table key; a transitive-only
/// package gets a synthesized unique segment) and rewrites each package's local dependency keys to the
/// global segments of the packages they resolve to ([`DepPackage::dep_renames`]).
fn assemble(
    instances: BTreeMap<String, Instance>,
    root_edges: &BTreeMap<String, String>,
) -> ResolvedGraph {
    // Global segment per identity. Direct dependencies keep the consumer's key (so the entry's
    // `use <key>.…` needs no rewrite); transitive-only packages get a unique synthesized segment.
    let mut global: BTreeMap<String, String> = BTreeMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for (key, identity) in root_edges {
        // First root key wins if the same identity is aliased under several keys.
        global.entry(identity.clone()).or_insert_with(|| key.clone());
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
        });
    }
    // Sort by global segment so the loader's SourceId assignment and the startup-cache key are
    // deterministic regardless of walk order.
    packages.sort_by(|a, b| a.key.cmp(&b.key));
    ResolvedGraph { packages, locked }
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
