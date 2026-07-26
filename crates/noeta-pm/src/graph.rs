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

use crate::error::PmError;
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
    /// The **package identities** (`company/package`) of the native packages the root app authorized
    /// to contribute CLI commands (`[trust].commands`, package-manager Phase 4). The composer ties
    /// command registration to these entries' extension units in the shim, so `run_cli` registers a
    /// dependency's `noeta <cmd>` only when its *package* is command-trusted — never by namespace-root
    /// name matching, which would over-trust every package sharing a scope root (trusting `para/db`
    /// must not trust all of `para/*`). std's own commands are always allowed. Sorted + deduped.
    pub trusted_command_identities: Vec<String>,
    /// scope (`company`) → the trust root established for it during the walk (provenance, Phase 4
    /// #2 / Phase 5) — a registry-served Ed25519 key or a keyless-verified OIDC identity — to be
    /// **pinned** in `noeta.lock` (trust-on-first-use). Empty when no registry dependency carried
    /// provenance.
    pub scope_trust: BTreeMap<String, crate::lock::ScopeTrust>,
    /// The **root** package's effective language [`Edition`] (follow-on F1) — the edition the merged
    /// compilation unit compiles under. [`Edition::DEFAULT`] for a bare script with no `[package]`.
    /// Per-dependency editions live on each [`LockedPackage`]; this is the one the front-end reads.
    pub root_edition: crate::edition::Edition,
    /// The transparency-log head to pin in `noeta.lock` (namespace-protection #1, TLog) — the log key
    /// + last checkpoint verified during this resolve. `None` when transparency isn't engaged.
    pub log_trust: Option<crate::lock::LogTrust>,
    /// The identities resolved via a **registry** dependency (as opposed to a direct git/path source)
    /// — the set transparency enforcement applies to. Empty when no registry dependency was resolved.
    pub registry_identities: std::collections::BTreeSet<String>,
}

/// A resolved package's native entry crate (Phase 3, N3.1): where the composed build finds its
/// `Cargo.toml`, validated to exist at resolve time.
#[derive(Debug, Clone)]
pub struct NativeCrate {
    /// The owning package's global identity `company/package`.
    pub identity: String,
    /// The crate directory, absolute (package root + the manifest's relative `native` dir).
    pub crate_dir: PathBuf,
    /// The owning package's materialized-tree content hash (the same hash the lockfile pins).
    /// Folded into the compose cache key: a **path** dependency's `crate_dir` never changes on
    /// edit (unlike a store-materialized git/registry dep, whose dir is per-SHA), so without the
    /// content hash an edit to the crate's source would keep serving the stale composed binary.
    pub content_hash: String,
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

/// Which namespace root segment a dependency's modules re-root from ([`Walker::walk_one`]). A normal
/// dependency authored `namespace <package>.…`, so its root is the package half; a **scope** member
/// authored `namespace <scope>.<package>.…`, so its root is the company/scope segment.
#[derive(Clone, Copy)]
enum ScopeRoot {
    Package,
    Scope,
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
    /// This package's own `[dependencies]`: local key → the resolved child identities. A normal
    /// dependency contributes exactly one identity; a **scope** dependency (`key = [ … ]`) contributes
    /// one per member package, all sharing the scope, so a key may map to several.
    edges: BTreeMap<String, Vec<String>>,
}

/// Resolve the full dependency graph rooted at `entry`'s manifest (package-manager P2.4). Returns an
/// empty graph when there is no manifest or no `[dependencies]` (a bare script). Every failure — an
/// unreadable/invalid manifest, a git fetch error, a registry dependency (pending P2.5), or a version
/// conflict — is a human-readable `Err`.
pub fn resolve_graph(entry: &Path) -> Result<ResolvedGraph, PmError> {
    resolve_graph_for(entry, None)
}

/// As [`resolve_graph`], but **without the lockfile refresh** — for query-shaped consumers. A
/// resolve is a build-command step for `run`/`build`/`add`/`update` (their pins should persist),
/// but the IDE resolves the same graph just so hover/completions see dependency modules, and the
/// formatter scans it for text tiers: a query API must not mutate project state on disk (or
/// silently re-pin versions) because a file was opened or formatted. Same selection, same trust
/// enforcement; only the write is skipped.
pub fn resolve_graph_query(entry: &Path) -> Result<ResolvedGraph, PmError> {
    resolve_graph_impl(entry, None, LockRefresh::Skip)
}

/// As [`resolve_graph`], but resolving the graph for a specific build **target** (dev-deps arc): the
/// root's dependency set is [`Manifest::active_dependencies`] for `target` — the global
/// `[dependencies]` plus that target's own and inherited `[targets.<name>.dependencies]`. `None`
/// (the [`resolve_graph`] default) is the global set, so every existing caller is unchanged. A
/// dependency's *own* target-scoped deps never apply — a dep contributes only its `[dependencies]`.
pub fn resolve_graph_for(entry: &Path, target: Option<&str>) -> Result<ResolvedGraph, PmError> {
    resolve_graph_impl(entry, target, LockRefresh::Refresh)
}

/// Whether a resolve refreshes `noeta.lock` afterwards ([`resolve_graph_query`] skips it).
#[derive(Clone, Copy, PartialEq)]
enum LockRefresh {
    Refresh,
    Skip,
}

/// Whether the solve may adopt `noeta.lock`'s pinned versions without querying the index
/// (the lock fast path). [`LockPins::Ignore`] forces a live solve — used when
/// `[trust].require_transparency` is on, since transparency enforcement is by definition a
/// live check against the registry's log on every resolve.
#[derive(Clone, Copy, PartialEq)]
enum LockPins {
    Honor,
    Ignore,
}

fn resolve_graph_impl(
    entry: &Path,
    target: Option<&str>,
    refresh: LockRefresh,
) -> Result<ResolvedGraph, PmError> {
    let dir = entry.parent().unwrap_or_else(|| Path::new("."));
    let Some(manifest_path) = crate::manifest::find(dir) else {
        return Ok(ResolvedGraph {
            packages: Vec::new(),
            locked: Vec::new(),
            native_crates: Vec::new(),
            trusted_command_identities: Vec::new(),
            scope_trust: BTreeMap::new(),
            root_edition: crate::edition::Edition::DEFAULT,
            log_trust: None,
            registry_identities: std::collections::BTreeSet::new(),
        });
    };
    let manifest = read_manifest(&manifest_path)?;
    if let Some(pkg) = manifest.package() {
        check_toolchain_req(pkg, "this package")?;
    }
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
    // Same top-down authority: the consumer's require-provenance policy is the root's, applied to every
    // resolved dependency (namespace-protection #1). A dependency can't relax it for itself.
    let require_provenance = &manifest.trust().require_provenance;
    let publish_cooldown = manifest.trust().publish_cooldown;
    let mut walker = Walker {
        instances: BTreeMap::new(),
        store: None,
        lock: &lock,
        indexes: BTreeMap::new(),
        registries,
        native_trust,
        require_provenance,
        publish_cooldown,
        solution: BTreeMap::new(),
        scope_trust: BTreeMap::new(),
        registry_ids: std::collections::BTreeSet::new(),
        candidates: BTreeMap::new(),
        mat_memo: std::collections::HashMap::new(),
    };
    // Phase 4, S5b: first *select versions* — gather the candidate graph (materialize the path/git
    // spine, query the index for every registry candidate + its deps) and run PubGrub. This backtracks
    // over version ranges, so a solvable diamond resolves to a compatible set instead of a greedy
    // false conflict. The walk then materializes exactly the solved versions.
    // Transparency enforcement (below) is a live check against the registry's log by design, so
    // it forgoes the lock fast path; everything else honors the pin.
    let lock_pins = if manifest.trust().require_transparency {
        LockPins::Ignore
    } else {
        LockPins::Honor
    };
    walker.solve(&root_deps, &manifest_dir, lock_pins)?;
    let mut root_edges = BTreeMap::new();
    walker.walk(&root_deps, &manifest_dir, &mut root_edges)?;

    let scope_trust = walker.scope_trust;
    // The root package's edition governs the merged compilation unit (per-package editions of the
    // dependencies are pinned individually in the lock). A bare manifest with no `[package]` compiles
    // under the default edition.
    let root_edition = manifest.package().map(|p| p.edition()).unwrap_or_default();
    let registry_ids = walker.registry_ids;
    #[allow(unused_mut)]
    let mut graph = assemble(
        walker.instances,
        &root_edges,
        &manifest.trust().commands,
        scope_trust,
        root_edition,
        registry_ids,
    );

    // The root package's OWN native crate. A dependency's native crate becomes a `NativeCrate`
    // during the walk, but the root package is never walked as its own dependency — so a package
    // declaring `package.native` could not compose its own extension, and `noeta check`/`run` on a
    // file *inside* that package failed to resolve a `use` of its own namespace (`use para.db`
    // inside `para/db` itself). That is the reason such a package was checkable only as somebody
    // else's dependency, and the reason a defect there — a stale `dyn` workaround, say — could
    // survive unseen: CI structurally could not check the package on its own.
    //
    // No trust gate here, deliberately: `[trust].native` authorizes a *dependency's* native code,
    // protecting a consumer from code they did not write. The root author IS that consumer, so a
    // package trusting its own native code is redundant friction — the same reason cargo never asks
    // you to authorize your own `build.rs`.
    if let Some(pkg) = manifest.package()
        && let Some(native) = &pkg.native
    {
        let identity = format!("{}/{}", pkg.name.company, pkg.name.package);
        // Guard against double-linking if the root ever appears as its own instance (a
        // self-referential scope dependency).
        if !graph.native_crates.iter().any(|nc| nc.identity == identity) {
            validate_native_crate(&manifest_dir, native)?;
            let content_hash = hash_tree(&manifest_dir).map_err(|err| {
                PmError::Io(format!(
                    "hashing the root package at `{}`: {err}",
                    manifest_dir.display()
                ))
            })?;
            // Absolute: the composer writes each crate as a path dependency in a shim `Cargo.toml`
            // under its cache dir, so a relative `crate_dir` (which `manifest_dir` is whenever the
            // entry was passed as a relative path) would resolve against the cache dir and vanish.
            // A dependency's `inst.dir` is already materialized to an absolute path; the root's is
            // not, so it is the one case that must canonicalize.
            let crate_dir = manifest_dir.join(native);
            let crate_dir = crate_dir.canonicalize().unwrap_or(crate_dir);
            graph.native_crates.push(NativeCrate {
                identity,
                crate_dir,
                content_hash,
            });
        }
    }

    // Transparency-log enforcement (namespace-protection #1, TLog): when the consumer requires it,
    // every registry release must be publicly logged under a signed checkpoint that is an append-only
    // extension of the one pinned in `noeta.lock`. Feature-gated (the crypto + HTTP client), like
    // provenance verification; a lockfile-shape-only build (the LSP) skips the crypto.
    #[cfg(all(feature = "registry-http", feature = "provenance"))]
    if manifest.trust().require_transparency && !graph.registry_identities.is_empty() {
        graph.log_trust = Some(enforce_transparency(&graph, lock.log_trust())?);
    }

    // Refresh the lockfile (best-effort: a read-only project must not fail a build). Skipped for a
    // manifest with no resolved dependencies, so a bare-`[targets]` project grows no lock — and
    // for a query resolve ([`resolve_graph_query`]), which must not write project state at all.
    if refresh == LockRefresh::Refresh && !graph.locked.is_empty() {
        // Resolution doesn't touch the advisory feed (that's `noeta audit`), so preserve any advisory
        // pin already in the lock rather than erasing it on every build.
        let advisory_trust = crate::lock::Lock::read(&manifest_dir)
            .advisory_trust()
            .cloned();
        let _ = crate::lock::write(
            &manifest_dir,
            &graph.locked,
            &graph.scope_trust,
            graph.log_trust.as_ref(),
            advisory_trust.as_ref(),
        );
    }
    Ok(graph)
}

/// Enforce `[trust].require_transparency` (namespace-protection #1, TLog): verify the registry's
/// current signed checkpoint (against the pinned log key, TOFU on first use), prove it is an
/// append-only extension of the checkpoint pinned in the lock, and verify every registry-resolved
/// release is included at that checkpoint. Returns the new head to pin. Any failure — a changed log
/// key, a rewritten history, or a missing/forged inclusion — aborts the resolve.
#[cfg(all(feature = "registry-http", feature = "provenance"))]
fn enforce_transparency(
    graph: &ResolvedGraph,
    pinned: Option<&crate::lock::LogTrust>,
) -> Result<crate::lock::LogTrust, PmError> {
    use crate::transparency;
    // The registry follows the same default chain as resolution (`NOETA_REGISTRY_URL`, then
    // `NOETA_REGISTRY_DIR`, then the built-in hosted default) — `None` only when
    // `NOETA_REGISTRY_DIR` routes to the file-backed local index, which serves no log.
    let index = crate::registry::open_http()?.ok_or_else(|| {
        PmError::Trust(
            "`[trust].require_transparency` needs a hosted registry, but `NOETA_REGISTRY_DIR` \
             routes to the file-backed local index (which serves no transparency log) — unset it \
             or set `NOETA_REGISTRY_URL`"
                .to_string(),
        )
    })?;
    // The log key: the one pinned in the lock, or the served key on first use (TOFU).
    let key = match pinned {
        Some(p) => p.public_key.clone(),
        None => index.log_public_key()?.ok_or_else(|| {
            PmError::Trust("the registry serves no transparency-log public key".to_string())
        })?,
    };
    let cp = index.log_checkpoint()?;
    if !transparency::verify_checkpoint(&key, cp.tree_size, &cp.root_hash, &cp.signature)? {
        return Err(PmError::Trust(
            "the transparency-log checkpoint does not verify against the pinned log key — the \
             registry changed keys or is equivocating (reconcile, then `noeta update` to re-pin)"
                .to_string(),
        ));
    }
    // Append-only: the current checkpoint must be a consistent extension of the pinned one.
    if let Some(p) = pinned {
        if cp.tree_size < p.tree_size {
            return Err(PmError::Trust(
                "the transparency log shrank since it was pinned — the registry rewrote history"
                    .to_string(),
            ));
        }
        let cons = index.log_consistency(p.tree_size, cp.tree_size)?;
        let root_from = transparency::hex_to_array::<32>(&p.root_hash)
            .ok_or_else(|| PmError::Trust("malformed pinned root hash".to_string()))?;
        let root_to = transparency::hex_to_array::<32>(&cp.root_hash)
            .ok_or_else(|| PmError::Trust("malformed checkpoint root".to_string()))?;
        let proof = cons
            .proof
            .iter()
            .map(|h| transparency::hex_to_array::<32>(h))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| PmError::Trust("malformed consistency proof".to_string()))?;
        if !transparency::verify_consistency(
            p.tree_size as usize,
            cp.tree_size as usize,
            &proof,
            &root_from,
            &root_to,
        ) {
            return Err(PmError::Trust(
                "the transparency log is not an append-only extension of the pinned checkpoint — \
                 history was rewritten or the registry is equivocating"
                    .to_string(),
            ));
        }
    }
    // Every registry-resolved release must be included at this verified checkpoint.
    for pkg in &graph.locked {
        if !graph.registry_identities.contains(&pkg.identity) {
            continue;
        }
        let ResolvedSource::Git { url, git_ref, sha } = &pkg.source else {
            continue;
        };
        // A registry-resolved release is pinned to a tag; the transparency log is keyed by that tag.
        // (`git_ref` generalized `tag` to tag/branch/HEAD in the private-registries arc — a non-tag
        // git source is never a registry identity, so it is not logged.)
        let crate::manifest::GitRef::Tag(tag) = git_ref else {
            continue;
        };
        // `None` license: the lock doesn't carry one, so only the coordinates are checked — the
        // record's license field is still authenticated by inclusion, just not cross-checked here.
        let version = pkg.version.to_string();
        index.verify_inclusion_at(
            &crate::registry::ReleaseCoords {
                name: &pkg.identity,
                version: &version,
                url,
                tag,
                sha,
                license: None,
            },
            &cp,
        )?;
    }
    Ok(crate::lock::LogTrust {
        public_key: key,
        tree_size: cp.tree_size,
        root_hash: cp.root_hash,
    })
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
    /// The root manifest's `[trust].require_provenance` — scopes whose releases the consumer demands
    /// carry verified provenance (namespace-protection #1). An unsigned release from a required scope
    /// is refused during the walk.
    require_provenance: &'a crate::manifest::RequireProvenance,
    /// The root manifest's `[trust].publish_cooldown` in seconds (namespace-protection #1): a registry
    /// release published more recently than this is dropped from the candidate set before PubGrub, so a
    /// too-fresh version is never newly selected. `None` = off.
    publish_cooldown: Option<u64>,
    /// The resolved `identity → version` map (Phase 4, S5b), computed by [`Walker::solve`] before the
    /// walk. The walk materializes each registry dependency at *its* selected version rather than
    /// greedily picking the highest; empty until `solve` runs (a pure path/git graph leaves registry
    /// selection unused).
    solution: BTreeMap<String, Version>,
    /// scope → the trust root established while materializing registry deps (provenance, Phase 4
    /// #2 / Phase 5); pinned into `noeta.lock` afterwards.
    scope_trust: BTreeMap<String, crate::lock::ScopeTrust>,
    /// The identities materialized via a **registry** dependency (not a direct git/path source) —
    /// what transparency-log enforcement applies to (namespace-protection #1, TLog).
    registry_ids: std::collections::BTreeSet<String>,
    /// The registry candidate sets `solve` loaded from the index, kept so `materialize` reads the
    /// solved version's release from here instead of re-querying the index — the audit found every
    /// release list fetched twice per resolve (and a git-forge scope re-`git fetch`ing per query).
    candidates: BTreeMap<String, Vec<crate::registry::Release>>,
    /// Materialization memo: dependency source key → the materialized result. `gather` (the solve's
    /// path/git spine walk) and `walk` each materialize the same dependency once per resolve, and a
    /// tree hash is a full-tree SHA-256 — this halves both. Keyed by the *source* (path or
    /// url+ref), not identity, since gather learns the identity only after materializing.
    mat_memo: std::collections::HashMap<String, (PathBuf, ResolvedSource, Option<String>)>,
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
        edges: &mut BTreeMap<String, Vec<String>>,
    ) -> Result<(), PmError> {
        for (key, dep) in deps {
            match dep {
                // A scope dependency binds several member packages under one import root: materialize
                // each, require they all share one `company` segment (the scope the key stands for),
                // and record every member identity under the key. The shared company is what re-roots
                // to the key ([`assemble`]), so a `use <key>.<member>.…` reaches the right package.
                Dependency::Scope(members) => {
                    let mut scope_company: Option<String> = None;
                    for member in members {
                        let identity = self.walk_one(key, member, base_dir, ScopeRoot::Scope)?;
                        let company = identity.split('/').next().unwrap_or(&identity).to_string();
                        match &scope_company {
                            None => scope_company = Some(company),
                            Some(first) if *first != company => {
                                return Err(PmError::Manifest(format!(
                                    "scope dependency `{key}` mixes packages from different scopes \
                                     (`{first}` and `{company}`) — every member of a scope must share \
                                     one `company` segment (the scope the key `{key}` stands for)"
                                )));
                            }
                            _ => {}
                        }
                        edges.entry(key.clone()).or_default().push(identity);
                    }
                }
                _ => {
                    let identity = self.walk_one(key, dep, base_dir, ScopeRoot::Package)?;
                    edges.entry(key.clone()).or_default().push(identity);
                }
            }
        }
        Ok(())
    }

    /// Materialize and install one **leaf** dependency (a `path`/`git`/`registry` source, never a
    /// scope), recursing into its own subtree, and return its `company/package` identity. `root`
    /// selects the namespace root segment recorded for re-rooting: [`ScopeRoot::Package`] (a normal
    /// dependency) uses the package half — the package authored `namespace <package>.…`; a scope
    /// member uses the company/scope segment, since a scope package authored `namespace <scope>.…`.
    fn walk_one(
        &mut self,
        key: &str,
        dep: &Dependency,
        base_dir: &Path,
        root: ScopeRoot,
    ) -> Result<String, PmError> {
        let (dir, source, fetched_hash) = self.materialize(key, dep, base_dir)?;
        let child_manifest = read_manifest(&dir.join(crate::manifest::MANIFEST_NAME))
            .map_err(|err| err.map_msg(|m| format!("dependency `{key}`: {m}")))?;
        let pkg = child_manifest.package().ok_or_else(|| {
            PmError::Manifest(format!(
                "dependency `{key}` at `{}` has no `[package]` table (needed for its identity \
                 and namespace root)",
                dir.display()
            ))
        })?;
        let identity = format!("{}/{}", pkg.name.company, pkg.name.package);
        check_toolchain_req(pkg, &format!("dependency `{key}` (`{identity}`)"))?;
        let root_segment = match root {
            ScopeRoot::Package => pkg.name.root().to_string(),
            ScopeRoot::Scope => pkg.name.company.clone(),
        };

        if let Some(existing) = self.instances.get(&identity) {
            if existing.version != pkg.version {
                return Err(PmError::Conflict(format!(
                    "dependency conflict: `{identity}` is required at both {} and {} — a \
                     package may appear at only one version (they share one flat namespace)",
                    existing.version, pkg.version
                )));
            }
            return Ok(identity); // already materialized and its subtree walked
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
                return Err(PmError::Trust(format!(
                    "dependency `{key}` (`{identity}`) ships native code (`native = \
                     \"{native}\"`), which runs arbitrary Rust at build + run time. It is not \
                     authorized: add `{identity}` to the `[trust].native` list in your \
                     `noeta.toml` to allow it (this grant is deliberately explicit — a \
                     dependency, even a transitive one, can never authorize its own native code)."
                )));
            }
            validate_native_crate(&dir, native).map_err(|err| {
                err.map_msg(|m| format!("dependency `{key}` (`{identity}`): {m}"))
            })?;
        }
        // A git fetch already hashed its (immutable, store-materialized) tree — reuse it; a
        // path tree is mutable, so it hashes fresh.
        let content_hash = match fetched_hash {
            Some(hash) => hash,
            None => hash_tree(&dir).map_err(|err| {
                PmError::Io(format!(
                    "dependency `{key}`: hashing `{}`: {err}",
                    dir.display()
                ))
            })?,
        };
        // A git source is immutable, so a lock-recorded hash must match — a mismatch means the
        // stored tree drifted from what the lock pinned. A path source is a mutable local tree,
        // so its hash legitimately changes as the developer edits it; it is not verified.
        // The pin is per **version**: when a live re-solve deliberately selected a different
        // version (a changed requirement, `noeta update`), the old version's hash doesn't
        // apply — comparing it anyway turned every legitimate upgrade into a false "drifted"
        // error the moment an upstream published.
        if matches!(source, ResolvedSource::Git { .. })
            && self.lock.locked_version(&identity) == Some(&pkg.version)
            && let Some(locked) = self.lock.content_hash(&identity)
            && locked != content_hash
        {
            return Err(PmError::Lock(format!(
                "dependency `{key}` (`{identity}`) content hash does not match `{}` — the \
                 stored source drifted from the lock; run `noeta update` to re-pin",
                crate::lock::LOCK_NAME
            )));
        }
        // Insert before recursing so a dependency cycle terminates (a back-edge sees the
        // in-progress instance and dedups).
        self.instances.insert(
            identity.clone(),
            Instance {
                version: pkg.version.clone(),
                edition: pkg.edition(),
                root_segment,
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
        Ok(identity)
    }

    /// Materialize one dependency to an on-disk directory (package-manager P2.4): a path dep is its
    /// local tree (relative to `base_dir`); a git dep is fetched into the store (its tag resolved to a
    /// commit SHA); a registry dep materializes its solved release. Returns the directory, the pinned
    /// source coordinates, and — when the fetch already computed it — the tree content hash.
    ///
    /// Memoized per resolve: `gather` (the solve's path/git spine) and `walk` each reach every
    /// dependency once, so without the memo every tree was materialized (and, for git sources,
    /// full-tree-hashed) twice per resolve.
    fn materialize(
        &mut self,
        key: &str,
        dep: &Dependency,
        base_dir: &Path,
    ) -> Result<(PathBuf, ResolvedSource, Option<String>), PmError> {
        let memo_key = match dep {
            Dependency::Path { path } => format!("path:{}", base_dir.join(path).display()),
            Dependency::Git { url, git_ref } => format!("git:{url}@{}", git_ref.lock_key()),
            Dependency::Registry {
                package: Some(p), ..
            } => format!("reg:{}/{}", p.company, p.package),
            Dependency::Registry { package: None, .. } => String::new(), // errors below anyway
            Dependency::Scope(_) => String::new(), // refused below (never materialized as a leaf)
        };
        if !memo_key.is_empty()
            && let Some(hit) = self.mat_memo.get(&memo_key)
        {
            return Ok(hit.clone());
        }
        let out = self.materialize_uncached(key, dep, base_dir)?;
        if !memo_key.is_empty() {
            self.mat_memo.insert(memo_key, out.clone());
        }
        Ok(out)
    }

    fn materialize_uncached(
        &mut self,
        key: &str,
        dep: &Dependency,
        base_dir: &Path,
    ) -> Result<(PathBuf, ResolvedSource, Option<String>), PmError> {
        match dep {
            Dependency::Path { path } => {
                // Canonicalize the joined directory so module names/spans (and the editor URIs built
                // from them) are clean absolute paths, not `…/app/../dep/…`. The manifest-relative
                // `path` is kept verbatim in the lock entry.
                let joined = base_dir.join(path);
                let dir = joined.canonicalize().unwrap_or(joined);
                // No precomputed hash: a path tree is the developer's mutable working copy, so the
                // walk hashes it fresh each resolve.
                Ok((dir, ResolvedSource::Path { path: path.clone() }, None))
            }
            Dependency::Git { url, git_ref } => self.fetch_git(key, url, git_ref, None, None),
            // A scope is a group of member packages, not a single source — [`Walker::walk`] and
            // [`Walker::gather`] expand it into its members before ever materializing, so a bare
            // scope never reaches here.
            Dependency::Scope(_) => Err(PmError::Manifest(format!(
                "internal error: scope dependency `{key}` reached `materialize` unexpanded"
            ))),
            Dependency::Registry { package, .. } => {
                // Materialize the **resolver-selected** version (Phase 4, S5b): the PubGrub solve
                // already chose one compatible version per identity, so look up the coordinates of
                // `solution[identity]` in the index rather than greedily re-picking the highest.
                let package = package.as_ref().ok_or_else(|| {
                    PmError::Manifest(format!(
                        "dependency `{key}` is a registry dependency but names no package — add \
                         `package = \"company/pkg\"` (the registry identity, decoupled from the \
                         import-root key)"
                    ))
                })?;
                let name = format!("{}/{}", package.company, package.package);
                // This identity came from a registry dependency — transparency enforcement applies to
                // it (a direct git/path dep isn't logged, so it's out of scope).
                self.registry_ids.insert(name.clone());
                // Defense in depth: `solve` already refused built-in scopes before they entered the
                // candidate graph, so a solved version can't name one — but the walk is a public entry
                // too, so keep the invariant local rather than assume the solve ran.
                if crate::reserved::is_builtin(&package.company) {
                    return Err(PmError::Trust(format!(
                        "dependency `{key}`: {}",
                        crate::reserved::builtin_registry_refusal(&package.company, &name)
                    )));
                }
                let version = self.solution.get(&name).cloned().ok_or_else(|| {
                    PmError::Conflict(format!(
                        "dependency `{key}` (`{name}`) is not in the resolved version set"
                    ))
                })?;
                let scope = package.company.clone();
                // Lock fast path: the solved version IS the locked version → materialize from the
                // lock's pinned coordinates, no index round-trip (offline when the store holds the
                // tree). Trust holds without re-checking provenance here: the release was verified
                // when first resolved, the SHA pin + the walk's content-hash check guarantee the
                // tree is byte-identical to what was verified, and the TOFU scope pin is carried
                // forward from the lock so the rewrite can't drop it. A miss (no coords in the
                // lock, or a live solve picked a different version) falls through to the index.
                if let Some((url, tag, sha)) = self.lock.registry_coords(&name)
                    && self.lock.locked_version(&name) == Some(&version)
                {
                    if let Some(pin) = self.lock.scope_trust(&scope) {
                        self.scope_trust.insert(scope.clone(), pin.clone());
                    }
                    let (url, tag, sha) = (url.to_string(), tag.to_string(), sha.to_string());
                    let git_ref = crate::manifest::GitRef::Tag(tag);
                    return self.fetch_git(key, &url, &git_ref, Some(&sha), None);
                }
                let scope_key = self
                    .index_for(&scope)?
                    .scope_key(&scope)
                    .map_err(|err| err.map_msg(|m| format!("dependency `{key}`: {m}")))?;
                // The solved release comes from the candidate set `solve` already loaded — the
                // index is only re-queried if this identity wasn't in it (a walk without a solve,
                // e.g. a caller-driven partial resolve).
                let from_candidates = self
                    .candidates
                    .get(&name)
                    .and_then(|rs| rs.iter().find(|r| r.version == version).cloned());
                let release = match from_candidates {
                    Some(release) => release,
                    None => self
                        .index_for(&scope)?
                        .releases(&name)
                        .map_err(|err| err.map_msg(|m| format!("dependency `{key}`: {m}")))?
                        .into_iter()
                        .find(|r| r.version == version)
                        .ok_or_else(|| {
                            PmError::Network(format!(
                                "dependency `{key}` (`{name}`): resolved version {version} is not in the index"
                            ))
                        })?,
                };
                // Provenance (Phase 4 #2 / Phase 5): pin the scope's trust root on first use,
                // reject a changed key / changed identity / downgraded root, and verify the
                // signature or keyless bundle (under the `provenance`/`keyless` features).
                self.check_provenance(key, &name, &release, scope_key.as_deref())?;
                let coords = release.coords;
                // The registry pins the SHA (Phase 4, S2), so a first resolve fetches by it rather
                // than trusting the tag's current target. A published release is always a tag.
                let git_ref = crate::manifest::GitRef::Tag(coords.tag.clone());
                // Materialize from the index's already-fetched local clone when it has one (a git-forge
                // index), avoiding a second network clone; the lock still records `coords.url`.
                let local = self.index_for(&scope)?.local_repo(&name);
                self.fetch_git(
                    key,
                    &coords.url,
                    &git_ref,
                    Some(&coords.sha),
                    local.as_deref(),
                )
            }
        }
    }

    /// Materialize a git `url`@`tag` into the store (package-manager P2.4). Shared by a direct `git`
    /// dependency (`registry_sha = None`) and a resolved registry dependency (`registry_sha = Some`,
    /// the index-pinned commit). The SHA to fetch is, in precedence: the **lockfile** pin (the
    /// reproducibility authority once written) → the **registry** pin (closes trust-on-first-use on a
    /// first registry resolve) → an `ls-remote` of the tag (a bare `git` dep's first fetch). A pinned
    /// SHA already in the store needs no network at all.
    ///
    /// `url` is the **recorded** origin — the lock pin and `ResolvedSource` key, portable across
    /// machines. `local_repo`, when set (a git-forge index's already-fetched bare clone,
    /// private-registries arc), is where the tree is actually **fetched from**, so a git-forge release
    /// materializes offline from that clone instead of a second network clone; `url` is still what the
    /// lock records.
    fn fetch_git(
        &mut self,
        key: &str,
        url: &str,
        git_ref: &crate::manifest::GitRef,
        registry_sha: Option<&str>,
        local_repo: Option<&Path>,
    ) -> Result<(PathBuf, ResolvedSource, Option<String>), PmError> {
        let pin = self
            .lock
            .git_pin(url, git_ref)
            .or(registry_sha)
            .map(str::to_string);
        // Fetch from the local clone when the index provides one; the lock still keys on `url`.
        let source = local_repo.and_then(Path::to_str).unwrap_or(url);
        let store = self.store()?;
        let fetched = match &pin {
            Some(sha) => crate::git::fetch_pinned(source, git_ref, sha, store),
            None => crate::git::fetch(source, git_ref, store),
        }
        .map_err(|err| err.map_msg(|m| format!("dependency `{key}`: {m}")))?;
        Ok((
            fetched.path,
            ResolvedSource::Git {
                url: url.to_string(),
                git_ref: git_ref.clone(),
                sha: fetched.sha,
            },
            // The fetch already hashed the tree (`materialize_sha` does, store hit or miss) —
            // hand it up so the walk doesn't SHA-256 the whole tree a second time.
            Some(fetched.content_hash),
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
    ) -> Result<(), PmError> {
        let scope = name.split('/').next().unwrap_or(name);
        // Consumer require-provenance (namespace-protection #1): if this project demands `scope`
        // carry provenance, an unsigned release is refused outright — the guarantee holds even if the
        // scope set no policy of its own, and even on first use where TOFU would otherwise allow it.
        if self.require_provenance.requires(scope)
            && release.signature.is_none()
            && release.bundle.is_none()
        {
            return Err(PmError::Trust(format!(
                "dependency `{key}` (`{name}`): `[trust].require_provenance` demands verified \
                 provenance from scope `{scope}`, but this release is unsigned — refusing to resolve \
                 an unattested release"
            )));
        }
        let action = provenance_decision(
            self.lock.scope_trust(scope),
            release.signature.as_deref(),
            release.bundle.as_deref(),
            served_key,
        )
        .map_err(|reason| PmError::Trust(format!("dependency `{key}` (`{name}`): {reason}")))?;
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
                        err.map_msg(|m| format!("dependency `{key}` (`{name}`): provenance {m}"))
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
                        .map_err(|err| {
                            err.map_msg(|m| format!("dependency `{key}` (`{name}`): {m}"))
                        })?;
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
    fn store(&mut self) -> Result<&Store, PmError> {
        if self.store.is_none() {
            self.store = Some(Store::open().ok_or_else(|| {
                PmError::Io(
                    "cannot open the package store (no writable cache directory) — needed for git \
                     dependencies"
                        .to_string(),
                )
            })?);
        }
        Ok(self.store.as_ref().expect("just opened"))
    }

    /// The registry index for `company`'s packages, opened on first use and cached (private-registries
    /// arc). The `[registries]` map routes the scope to its source — a specific hosted registry, a
    /// GitHub org, or (unmapped) the default chain: `NOETA_REGISTRY_URL`, then `NOETA_REGISTRY_DIR`
    /// (the local index), then the built-in hosted registry at `registry.noeta.dev` (`registry-http`
    /// builds; without the HTTP client, always the local index). Two scopes on the same source share
    /// one client.
    fn index_for(&mut self, company: &str) -> Result<&dyn crate::registry::Index, PmError> {
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
        lock_pins: LockPins,
    ) -> Result<(), PmError> {
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

        // The **lockfile fast path**: when every registry requirement the local manifests declare —
        // the root's direct deps plus the freshly re-gathered path/git spine's edges — is satisfied
        // by the version `noeta.lock` pins, adopt the locked selection and never query the index.
        // This is what makes the lock an actual *pin*: a committed `noeta.lock` reproduces the same
        // versions on every machine, offline once the store holds the trees, and an upstream publish
        // (or yank, or cooldown window) cannot silently change an existing build — exactly the
        // documented "an existing lockfile pin bypasses the index entirely" semantics. Sound because
        // a registry release is immutable at `(identity, version)`: its declared dep ranges can't
        // change after publish, so a set that was mutually consistent when locked stays consistent —
        // only the local requirement frontier can drift, and that is exactly what is re-checked
        // here. Any miss (new dep, changed range, no lock, `noeta update` deleted it) falls through
        // to the live solve. Adopting the *whole* locked set is deliberate: the walk materializes
        // only what the manifests reference, so a stale extra pin is inert and drops out on rewrite.
        if lock_pins == LockPins::Honor {
            let mut frontier: Vec<(&String, &VersionReq)> = root_deps
                .iter()
                .filter(|(id, _)| !path_git.contains_key(id.as_str()))
                .map(|(id, req)| (id, req))
                .collect();
            for cand in path_git.values() {
                for (id, req) in &cand.deps {
                    if !path_git.contains_key(id) {
                        frontier.push((id, req));
                    }
                }
            }
            let lock = self.lock;
            if !frontier.is_empty()
                && frontier
                    .iter()
                    .all(|(id, req)| lock.locked_version(id).is_some_and(|v| req.matches(v)))
            {
                self.solution = lock
                    .locked_versions()
                    .filter(|(id, _)| !path_git.contains_key(id.as_str()))
                    .map(|(id, v)| (id.clone(), v.clone()))
                    .collect();
                return Ok(());
            }
        }

        // Versions the **root consumer** exact-pins (`dep = "=1.5.0"`) — a deliberate choice that
        // bypasses their own publish cooldown (namespace-protection #1). Only the root's *direct* deps
        // count: a transitive dependency can't exact-pin its way past the consumer's cooldown, so the
        // control stays sound against a malicious dep declaring `= <fresh version>`.
        let exact_pins = root_exact_pins(&root_deps);

        // Transitively load every registry candidate (and the identities its releases depend on) from
        // the index — a path/git-overridden identity is skipped (its single version already wins).
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(identity) = registry_queue.pop() {
            if path_git.contains_key(&identity) || !seen.insert(identity.clone()) {
                continue;
            }
            // Same supply-chain invariant, at the transitive frontier: a release from the index may
            // *declare* a dependency under a built-in scope. Refuse before querying the index for it,
            // so a compromised registry can't drag a forged `std/*` in behind a legitimate package.
            let scope = identity.split('/').next().unwrap_or(&identity);
            if crate::reserved::is_builtin(scope) {
                return Err(PmError::Trust(crate::reserved::builtin_registry_refusal(
                    scope, &identity,
                )));
            }
            let company = scope.to_string();
            let mut releases = self
                .index_for(&company)?
                .releases(&identity)
                .map_err(|err| err.map_msg(|m| format!("registry package `{identity}`: {m}")))?;
            // A yanked release is never *newly selected* (PROTOCOL.md, Go's model): drop it from the
            // candidate set here — the index still serves it, so an exact lock-pinned version keeps
            // materializing through the version lookup path.
            releases.retain(|r| !r.yanked);
            let releases = self.apply_cooldown(&identity, releases, &exact_pins)?;
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
        // Keep the loaded candidate sets: `materialize` reads the solved release from here rather
        // than re-querying the index (which for a git-forge scope means another `git fetch`).
        self.candidates = registry;
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
    ) -> Result<Vec<(String, VersionReq)>, PmError> {
        let mut reqs = Vec::new();
        for (key, dep) in deps {
            match dep {
                // A scope contributes each of its member packages as its own resolver requirement.
                Dependency::Scope(members) => {
                    for member in members {
                        reqs.push(self.gather_one(
                            key,
                            member,
                            base_dir,
                            path_git,
                            registry_queue,
                        )?);
                    }
                }
                _ => reqs.push(self.gather_one(key, dep, base_dir, path_git, registry_queue)?),
            }
        }
        Ok(reqs)
    }

    /// Gather one **leaf** dependency (never a scope) into the candidate graph, returning its
    /// `(identity, requirement)` for the resolver. Part of [`Walker::gather`].
    fn gather_one(
        &mut self,
        key: &str,
        dep: &Dependency,
        base_dir: &Path,
        path_git: &mut BTreeMap<String, PathGitCandidate>,
        registry_queue: &mut Vec<String>,
    ) -> Result<(String, VersionReq), PmError> {
        match dep {
            Dependency::Path { .. } | Dependency::Git { .. } => {
                let (dir, _source, _hash) = self.materialize(key, dep, base_dir)?;
                let child_manifest = read_manifest(&dir.join(crate::manifest::MANIFEST_NAME))
                    .map_err(|err| err.map_msg(|m| format!("dependency `{key}`: {m}")))?;
                let pkg = child_manifest.package().ok_or_else(|| {
                    PmError::Manifest(format!(
                        "dependency `{key}` at `{}` has no `[package]` table",
                        dir.display()
                    ))
                })?;
                let identity = format!("{}/{}", pkg.name.company, pkg.name.package);
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
                Ok((identity, exact_req(&pkg.version)))
            }
            Dependency::Registry { package, req } => {
                let package = package.as_ref().ok_or_else(|| {
                    PmError::Manifest(format!(
                        "dependency `{key}` is a registry dependency but names no package — add \
                         `package = \"company/pkg\"`"
                    ))
                })?;
                let identity = format!("{}/{}", package.company, package.package);
                // Supply-chain invariant (namespace-protection #2): a built-in scope
                // (`std`/`noeta`/`core`) is served by the compiler, never a registry — refuse it
                // here, before it can enter the candidate graph, so no registry can shadow core.
                if crate::reserved::is_builtin(&package.company) {
                    return Err(PmError::Trust(format!(
                        "dependency `{key}`: {}",
                        crate::reserved::builtin_registry_refusal(&package.company, &identity)
                    )));
                }
                registry_queue.push(identity.clone());
                Ok((identity, req.clone()))
            }
            Dependency::Scope(_) => Err(PmError::Manifest(format!(
                "internal error: scope dependency `{key}` reached `gather_one` unexpanded"
            ))),
        }
    }

    /// Drop registry candidates published within `[trust].publish_cooldown` (namespace-protection #1),
    /// so a too-fresh release is never *newly selected* — giving an advisory or a yank time to catch a
    /// compromised release before it auto-propagates. A release with no known publish time is kept
    /// (undateable — the local index and any pre-cooldown registry aren't subject to it). If **every**
    /// candidate of a package is within the window, that's a hard error naming the package: the control
    /// fails closed (silently allowing the just-published version would defeat its purpose), and the
    /// message points at the levers (wait, lower, or disable the cooldown).
    fn apply_cooldown(
        &self,
        identity: &str,
        releases: Vec<crate::registry::Release>,
        exact_pins: &std::collections::BTreeSet<(String, Version)>,
    ) -> Result<Vec<crate::registry::Release>, PmError> {
        let Some(cooldown_secs) = self.publish_cooldown else {
            return Ok(releases);
        };
        if cooldown_secs == 0 || releases.is_empty() {
            return Ok(releases);
        }
        // The versions of *this* package the root consumer exact-pinned — exempt from the window.
        let exempt: std::collections::BTreeSet<Version> = exact_pins
            .iter()
            .filter(|(id, _)| id == identity)
            .map(|(_, v)| v.clone())
            .collect();
        let had = releases.len();
        let kept = cooldown_kept(releases, cooldown_secs, now_unix_ms(), &exempt);
        if kept.is_empty() && had > 0 {
            return Err(PmError::Conflict(format!(
                "every published version of `{identity}` is within the {} publish cooldown \
                 (`[trust].publish_cooldown`) — wait for a version to age out, lower the cooldown, or \
                 remove the setting",
                human_secs(cooldown_secs)
            )));
        }
        Ok(kept)
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
        Some(crate::manifest::RegistrySource::GitForge(base)) => format!("forge:{base}"),
    }
}

/// Now as Unix epoch **milliseconds** (publish-cooldown). A clock before the epoch → 0 (treats every
/// release as old enough — cooldown fails safe toward availability if the clock is absurd).
fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Keep only registry candidates old enough to clear the cooldown: `published_at ≤ now − cooldown`.
/// A release is also kept if it is undateable (`published_at == None`) or its version is in `exempt`
/// (the root consumer exact-pinned it). Pure (clock passed in) so it is directly testable;
/// [`Walker::apply_cooldown`] wraps it with the fail-closed empty check.
fn cooldown_kept(
    releases: Vec<crate::registry::Release>,
    cooldown_secs: u64,
    now_ms: i64,
    exempt: &std::collections::BTreeSet<Version>,
) -> Vec<crate::registry::Release> {
    let cutoff_ms = now_ms.saturating_sub((cooldown_secs as i64).saturating_mul(1000));
    releases
        .into_iter()
        .filter(|r| match r.published_at {
            Some(ts) => ts <= cutoff_ms || exempt.contains(&r.version),
            None => true,
        })
        .collect()
}

/// The `(identity, version)` pairs a set of resolver requirements **exactly** pins — a fully specified
/// `=x.y.z`. Only these bypass the publish cooldown; a range (`^1`, `>=1, <2`) or a partial exact
/// (`=1.5`) is not a single deliberate version and does not.
fn root_exact_pins(reqs: &[(String, VersionReq)]) -> std::collections::BTreeSet<(String, Version)> {
    reqs.iter()
        .filter_map(|(id, req)| exact_version(req).map(|v| (id.clone(), v)))
        .collect()
}

/// If `req` is a fully specified exact pin (`=x.y.z`), the pinned [`Version`]; else `None`. A partial
/// exact (`=1.5`, `=1`) matches a range of patches, so it isn't a single-version pin.
fn exact_version(req: &VersionReq) -> Option<Version> {
    if req.comparators.len() != 1 {
        return None;
    }
    let c = &req.comparators[0];
    if c.op != semver::Op::Exact {
        return None;
    }
    Some(Version {
        major: c.major,
        minor: c.minor?,
        patch: c.patch?,
        pre: c.pre.clone(),
        build: semver::BuildMetadata::EMPTY,
    })
}

/// A compact rendering of a cooldown window (`86400` → `"1d"`), for the error message.
fn human_secs(s: u64) -> String {
    if s != 0 && s.is_multiple_of(86_400) {
        format!("{}d", s / 86_400)
    } else if s != 0 && s.is_multiple_of(3_600) {
        format!("{}h", s / 3_600)
    } else if s != 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
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
    root_edges: &BTreeMap<String, Vec<String>>,
    trusted_commands: &std::collections::BTreeSet<String>,
    scope_trust: BTreeMap<String, crate::lock::ScopeTrust>,
    root_edition: crate::edition::Edition,
    registry_identities: std::collections::BTreeSet<String>,
) -> ResolvedGraph {
    // Global segment per identity. Direct dependencies keep the consumer's key (so the entry's
    // `use <key>.…` needs no rewrite); transitive-only packages get a unique synthesized segment.
    let mut global: BTreeMap<String, String> = BTreeMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for (key, identities) in root_edges {
        // A direct dependency keeps the consumer's key; every member of a root **scope** dependency
        // shares that one key, so they all land under the scope root in the flat pool. First root key
        // wins if an identity is aliased under several keys.
        for identity in identities {
            global
                .entry(identity.clone())
                .or_insert_with(|| key.clone());
        }
        used.insert(key.clone());
    }
    // A **transitive** scope group (a dependency's own scope dependency resolving several members)
    // must likewise share one global segment, so its members co-locate under one scope root. Reuse a
    // segment a member already has (e.g. also reached from the root), else synthesize one shared
    // segment from the scope company (each scope member's `root_segment` is its company).
    for inst in instances.values() {
        for children in inst.edges.values() {
            if children.len() < 2 {
                continue;
            }
            let seg = children
                .iter()
                .find_map(|id| global.get(id).cloned())
                .unwrap_or_else(|| {
                    let base = instances
                        .get(&children[0])
                        .map(|i| i.root_segment.clone())
                        .unwrap_or_else(|| children[0].clone());
                    unique_segment(&base, &mut used)
                });
            for id in children {
                global.entry(id.clone()).or_insert_with(|| seg.clone());
            }
        }
    }
    // Deterministic assignment order for the remaining single transitive-only packages.
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
    // the trusted identities so the composer can tie command registration to exactly those packages'
    // extension units (Phase 4). Identity — never the root segment: a scope-keyed package's segment
    // (`db` for `para/db`) is not what its extensions report as root, and root-name matching would
    // over-trust every package sharing a scope root.
    let mut trusted_command_identities: Vec<String> = Vec::new();
    for (identity, inst) in &instances {
        let key = global[identity].clone();
        // A local dependency key re-roots to the global segment of the package it resolves to. A
        // scope key's members all share one global segment (assigned above), so any member is a
        // faithful representative — the leading segment `<local key>.…` maps to that one scope root.
        let dep_renames: BTreeMap<String, String> = inst
            .edges
            .iter()
            .map(|(local_key, children)| (local_key.clone(), global[&children[0]].clone()))
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
            // The package's own edition, carried to the loader (editions arc): each dependency's
            // modules lex/parse/check under it. Typed end to end — `noeta_pm::edition` and the
            // loader's `noeta_lexer::Edition` are the same `noeta-edition` type.
            edition: inst.edition,
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
                content_hash: inst.content_hash.clone(),
            });
            // Commands only exist inside a native package; grant its commands only if command-trusted.
            if trusted_commands.contains(identity) {
                trusted_command_identities.push(identity.clone());
            }
        }
    }
    // Sort by global segment so the loader's SourceId assignment and the startup-cache key are
    // deterministic regardless of walk order.
    packages.sort_by(|a, b| a.key.cmp(&b.key));
    trusted_command_identities.sort();
    trusted_command_identities.dedup();
    ResolvedGraph {
        packages,
        locked,
        native_crates,
        trusted_command_identities,
        scope_trust,
        root_edition,
        log_trust: None,
        registry_identities,
    }
}

/// Enforce a manifest's `package.toolchain` requirement against the **running binary's** version.
/// Checked at resolve time — for the root package and for every materialized dependency — so a
/// too-old binary fails with "upgrade noeta", not a Rust compile error deep inside a native
/// compose (or a checker error against language features the binary predates).
fn check_toolchain_req(pkg: &crate::manifest::PackageMeta, what: &str) -> Result<(), PmError> {
    let Some(req) = &pkg.toolchain else {
        return Ok(());
    };
    let running = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is always valid SemVer");
    if toolchain_req_satisfied(req, &running) {
        return Ok(());
    }
    Err(PmError::Conflict(format!(
        "{what} requires noeta {req} but this binary is {running} — run `noeta upgrade` (or \
         install a matching release) to use it"
    )))
}

/// The version-vs-requirement core of [`check_toolchain_req`], split out for direct testing (the
/// caller bakes in `CARGO_PKG_VERSION`). Pre-release/build metadata on the running version is
/// stripped before matching: SemVer comparators never match a pre-release of a *different* triple,
/// which would make a `0.3.0-rc.1` dev binary spuriously fail `toolchain = ">=0.2"`. For a
/// minimum-toolchain claim, an rc of 0.3.0 has 0.3.0's surface — treat it as such.
fn toolchain_req_satisfied(req: &semver::VersionReq, running: &semver::Version) -> bool {
    let released = semver::Version::new(running.major, running.minor, running.patch);
    req.matches(&released)
}

/// Check a declared native entry crate exists: `<package root>/<native>/Cargo.toml` must be a
/// file (Phase 3, N3.1). The manifest parser already rejected absolute/`..` values.
fn validate_native_crate(package_dir: &Path, native: &str) -> Result<(), PmError> {
    let crate_dir = package_dir.join(native);
    if !crate_dir.join("Cargo.toml").is_file() {
        return Err(PmError::NativeBuild(format!(
            "`package.native = \"{native}\"` names no Rust crate — expected `{}`",
            crate_dir.join("Cargo.toml").display()
        )));
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
fn read_manifest(path: &Path) -> Result<Manifest, PmError> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| PmError::Io(format!("cannot read `{}`: {err}", path.display())))?;
    Manifest::parse(&text)
        .map_err(|err| err.map_msg(|m| format!("invalid `{}`: {m}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny two-package fixture on disk: `app` with one path dependency `lib`.
    fn path_dep_fixture(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("noeta_graph_test_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("noeta.toml"),
            "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(lib.join("lib.noe"), "pub fn one(): int { return 1; }\n").unwrap();
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[dependencies]\nlib = { path = \"../lib\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1\n").unwrap();
        app
    }

    #[test]
    fn a_dependency_requiring_a_newer_toolchain_fails_with_an_upgrade_message() {
        let app = path_dep_fixture("toolchain_req_dep");
        std::fs::write(
            app.parent().unwrap().join("lib").join("noeta.toml"),
            "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\ntoolchain = \">=999.0\"\n",
        )
        .unwrap();
        let err = resolve_graph(&app.join("main.noe")).expect_err("a too-new dep is refused");
        let msg = err.message().to_string();
        assert!(msg.contains("acme/lib"), "names the package: {msg}");
        assert!(
            msg.contains("requires noeta >=999.0"),
            "states the requirement: {msg}"
        );
        assert!(msg.contains("noeta upgrade"), "points at the fix: {msg}");
    }

    #[test]
    fn the_root_packages_own_toolchain_requirement_is_enforced() {
        let app = path_dep_fixture("toolchain_req_root");
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\ntoolchain = \">=999.0\"\n\
             [dependencies]\nlib = { path = \"../lib\" }\n",
        )
        .unwrap();
        let err = resolve_graph(&app.join("main.noe")).expect_err("a too-new root is refused");
        assert!(err.message().contains("requires noeta >=999.0"), "{err}");
        // A satisfiable requirement resolves normally.
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\ntoolchain = \">=0.1\"\n\
             [dependencies]\nlib = { path = \"../lib\" }\n",
        )
        .unwrap();
        let graph = resolve_graph(&app.join("main.noe")).expect("a satisfied requirement passes");
        assert_eq!(graph.packages.len(), 1);
    }

    #[test]
    fn toolchain_matching_ignores_the_running_versions_prerelease() {
        // A `0.3.0-rc.1` dev binary has 0.3.0's surface; SemVer's "a plain comparator never
        // matches a pre-release" rule must not make it fail `>= 0.2`.
        let req = semver::VersionReq::parse(">=0.2").unwrap();
        let rc = semver::Version::parse("0.3.0-rc.1").unwrap();
        assert!(!req.matches(&rc), "raw SemVer matching would refuse the rc");
        assert!(
            toolchain_req_satisfied(&req, &rc),
            "the stripped match accepts it"
        );
        let old = semver::Version::parse("0.1.5").unwrap();
        assert!(
            !toolchain_req_satisfied(&req, &old),
            "a genuinely old binary still fails"
        );
    }

    #[test]
    fn a_query_resolve_never_writes_the_lockfile() {
        // The IDE (and `noeta fmt`) resolve the graph purely to SEE dependency modules — a query
        // must not mutate project state on disk. The build-command resolve refreshes the lock;
        // the query resolve leaves the directory untouched.
        let app = path_dep_fixture("query_no_lock");
        let entry = app.join("main.noe");
        let graph = resolve_graph_query(&entry).expect("query resolves");
        assert_eq!(graph.packages.len(), 1, "the path dep resolves");
        assert!(
            !app.join("noeta.lock").exists(),
            "a query resolve must not create noeta.lock"
        );
        let graph = resolve_graph(&entry).expect("build resolve");
        assert_eq!(graph.packages.len(), 1);
        assert!(
            app.join("noeta.lock").exists(),
            "the build-command resolve refreshes noeta.lock"
        );
    }

    fn dated_release(major: u64, published_at: Option<i64>) -> crate::registry::Release {
        crate::registry::Release {
            version: Version::new(major, 0, 0),
            coords: crate::registry::GitCoords {
                url: "u".into(),
                tag: format!("v{major}.0.0"),
                sha: "s".into(),
            },
            deps: Vec::new(),
            yanked: false,
            signature: None,
            bundle: None,
            published_at,
            license: None,
            keywords: Vec::new(),
            description: None,
        }
    }

    #[test]
    fn cooldown_drops_only_versions_inside_the_window() {
        let now = 1_000_000_000_000i64; // fixed "now" in ms
        let day = 86_400_000i64;
        let releases = vec![
            dated_release(1, Some(now - 10 * day)), // old → kept
            dated_release(2, Some(now - day / 2)),  // 12h old → within a 1d window → dropped
            dated_release(3, None),                 // undateable → kept
        ];
        let none = std::collections::BTreeSet::new();
        let kept = cooldown_kept(releases, 86_400, now, &none); // 1-day cooldown
        let versions: Vec<u64> = kept.iter().map(|r| r.version.major).collect();
        assert_eq!(versions, vec![1, 3]);
    }

    #[test]
    fn cooldown_keeps_everything_at_the_boundary_and_when_zero() {
        let now = 2_000_000_000_000i64;
        let none = std::collections::BTreeSet::new();
        // Published exactly at the cutoff is old enough (inclusive).
        let at_cutoff = dated_release(1, Some(now - 3_600_000));
        assert_eq!(cooldown_kept(vec![at_cutoff], 3_600, now, &none).len(), 1);
    }

    #[test]
    fn an_exact_pin_bypasses_the_cooldown() {
        let now = 3_000_000_000_000i64;
        // A brand-new version, within the window — normally dropped.
        let fresh = dated_release(2, Some(now - 60_000)); // 1 minute old
        let none = std::collections::BTreeSet::new();
        assert!(cooldown_kept(vec![fresh.clone()], 86_400, now, &none).is_empty());
        // But if the consumer exact-pinned exactly that version, it's exempt.
        let exempt = std::collections::BTreeSet::from([Version::new(2, 0, 0)]);
        assert_eq!(cooldown_kept(vec![fresh], 86_400, now, &exempt).len(), 1);
    }

    #[test]
    fn only_fully_specified_exact_reqs_pin_a_version() {
        assert_eq!(
            exact_version(&VersionReq::parse("=1.5.0").unwrap()),
            Some(Version::new(1, 5, 0))
        );
        // A range or partial exact is not a single deliberate version.
        assert_eq!(exact_version(&VersionReq::parse("=1.5").unwrap()), None);
        assert_eq!(exact_version(&VersionReq::parse("^1.5.0").unwrap()), None);
        assert_eq!(exact_version(&VersionReq::parse(">=1, <2").unwrap()), None);
        // root_exact_pins keys by identity.
        let reqs = vec![
            ("acme/a".to_string(), VersionReq::parse("=2.0.0").unwrap()),
            ("acme/b".to_string(), VersionReq::parse("^1").unwrap()),
        ];
        let pins = root_exact_pins(&reqs);
        assert!(pins.contains(&("acme/a".to_string(), Version::new(2, 0, 0))));
        assert_eq!(pins.len(), 1);
    }

    #[test]
    fn human_secs_renders_the_largest_whole_unit() {
        assert_eq!(human_secs(86_400), "1d");
        assert_eq!(human_secs(7 * 86_400), "7d");
        assert_eq!(human_secs(3_600), "1h");
        assert_eq!(human_secs(1_800), "30m");
        assert_eq!(human_secs(45), "45s");
        assert_eq!(human_secs(0), "0s");
    }

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
    fn a_scope_dependency_binds_several_packages_under_one_key() {
        // Two packages of the same scope `para` (`para/aether` + `para/db`) bound under one array
        // key: both resolve, and both get the scope key `para` as their global segment (so the app's
        // `use para.aether.…` and `use para.db.…` both reach the flat pool).
        let base = std::env::temp_dir().join("noeta_graph_test_scope_dep");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        for pkg in ["aether", "db"] {
            let d = base.join(format!("para-{pkg}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("noeta.toml"),
                format!("[package]\nname = \"para/{pkg}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(
                d.join(format!("{pkg}.noe")),
                format!("namespace para.{pkg}.m;\npub fn one(): int {{ return 1; }}\n"),
            )
            .unwrap();
        }
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\npara = [ { path = \"../para-aether\" }, { path = \"../para-db\" } ]\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();

        let graph = resolve_graph(&app.join("main.noe")).expect("resolves");
        let ids: Vec<String> = graph.locked.iter().map(|l| l.identity.clone()).collect();
        assert!(ids.contains(&"para/aether".to_string()));
        assert!(ids.contains(&"para/db".to_string()));
        // Both members share the scope key `para` as their global segment, and each re-roots from its
        // company (`para`) — an identity re-root here — so its literal `para.<pkg>.…` lands in the pool.
        for pkg in ["aether", "db"] {
            let p = graph
                .packages
                .iter()
                .find(|p| {
                    p.modules
                        .iter()
                        .any(|m| m.name.contains(&format!("para-{pkg}")))
                })
                .unwrap_or_else(|| panic!("package para/{pkg} missing from the link set"));
            assert_eq!(
                p.key, "para",
                "scope member para/{pkg} must key on the scope"
            );
            assert_eq!(
                p.root, "para",
                "scope member para/{pkg} re-roots from its scope"
            );
        }
    }

    #[test]
    fn a_scope_dependency_rejects_members_from_different_scopes() {
        let base = std::env::temp_dir().join("noeta_graph_test_scope_mixed");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        for (dir, ident) in [
            ("para-aether", "para/aether"),
            ("other-thing", "other/thing"),
        ] {
            let d = base.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("noeta.toml"),
                format!("[package]\nname = \"{ident}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(
                d.join("m.noe"),
                "namespace x.m;\npub fn one(): int { return 1; }\n",
            )
            .unwrap();
        }
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\npara = [ { path = \"../para-aether\" }, { path = \"../other-thing\" } ]\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        let err = resolve_graph(&app.join("main.noe")).expect_err("mixed scopes must be refused");
        assert!(
            err.message().contains("different scopes"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn registry_cache_key_is_distinct_per_source() {
        // private-registries S2: the per-scope index cache keys off the source, so two scopes on the
        // same registry share one index and different registries get distinct ones (the routing seam).
        use crate::manifest::RegistrySource;
        assert_eq!(registry_cache_key(None), "default");
        assert_eq!(
            registry_cache_key(Some(&RegistrySource::GitForge(
                "https://github.com/acme".into()
            ))),
            "forge:https://github.com/acme"
        );
        assert_ne!(
            registry_cache_key(Some(&RegistrySource::Hosted("https://a".into()))),
            registry_cache_key(Some(&RegistrySource::Hosted("https://b".into())))
        );
        // The forge and hosted namespaces don't collide.
        assert_ne!(
            registry_cache_key(Some(&RegistrySource::GitForge("x".into()))),
            registry_cache_key(Some(&RegistrySource::Hosted("x".into())))
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

        // The edition is also carried on the DepPackage that reaches the loader/compiler (editions
        // arc) — the root and the dependency each get *their own*, in canonical string form.
        let dep_pkg = graph
            .packages
            .iter()
            .find(|p| p.root == "dep")
            .expect("dep package");
        assert_eq!(dep_pkg.edition.as_str(), "2026");
    }

    #[test]
    fn an_unknown_dependency_edition_fails_resolution_actionably() {
        // A dependency pinned to an edition this toolchain doesn't understand must fail the resolve
        // (not be silently miscompiled under the root's edition) — naming the dependency and the fix.
        let base = std::env::temp_dir().join("noeta_graph_test_future_edition");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        let dep = base.join("dep");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nfuturedep = { path = \"../dep\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        std::fs::write(
            dep.join("noeta.toml"),
            "[package]\nname = \"acme/dep\"\nversion = \"1.0.0\"\nedition = \"2099\"\n",
        )
        .unwrap();
        let err = resolve_graph(&app.join("main.noe")).expect_err("future edition must fail");
        assert!(
            err.message().contains("futuredep"),
            "names the dependency: {err}"
        );
        assert!(
            err.message().contains("2099"),
            "names the offending edition: {err}"
        );
        assert!(
            err.message().contains("2026"),
            "enumerates the known editions: {err}"
        );
    }

    #[test]
    fn a_root_package_composes_its_own_native_crate() {
        // A package that declares `package.native` must compose that crate when it is itself the
        // root — so `noeta check`/`run` on a file inside it resolves a `use` of its own namespace.
        // Before this, the root was never walked as its own dependency, so its native never entered
        // `native_crates` and the package was checkable only as somebody else's dependency.
        let base = std::env::temp_dir().join("noeta_graph_test_root_native");
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("imgfx");
        std::fs::create_dir_all(pkg.join("native")).unwrap();
        // No `[trust].native` here on purpose: a package does not authorize its own native code,
        // the way cargo never asks you to trust your own `build.rs`.
        std::fs::write(
            pkg.join("noeta.toml"),
            "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("fx.noe"),
            "namespace imgfx.fx;\npub fn one(): int { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("native").join("Cargo.toml"),
            "[package]\nname = \"imgfx-native\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();

        let graph = resolve_graph(&pkg.join("fx.noe")).expect("root native resolves");
        assert_eq!(
            graph.native_crates.len(),
            1,
            "the root's own native composes"
        );
        let nc = &graph.native_crates[0];
        assert_eq!(nc.identity, "acme/imgfx");
        // Absolute even though nothing here materialized the root through the store — the
        // canonicalization is what lets the composer's shim resolve the path dependency.
        assert!(
            nc.crate_dir.is_absolute(),
            "root crate dir must be absolute"
        );
        assert!(nc.crate_dir.join("Cargo.toml").is_file());
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
        assert!(err.message().contains("acme/imgfx"), "{err}");
        assert!(err.message().contains("Cargo.toml"), "{err}");
    }

    #[test]
    fn command_trust_gates_which_native_packages_may_add_commands() {
        // A native dep trusted for native but NOT for commands contributes no trusted command
        // identity; adding it to `[trust].commands` surfaces its package identity for the composer.
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
                .trusted_command_identities
        };
        // Native-trusted but not command-trusted → the package composes, but no command identity.
        assert!(make(false).is_empty());
        // Command-trusted → its package IDENTITY (not its root segment) is surfaced, so the shim
        // ties command registration to exactly this package's extension units — a scope-keyed
        // package (`para/db`) must not be matched (or over-matched) by root-name strings.
        assert_eq!(make(true), vec!["acme/imgfx".to_string()]);
    }

    #[test]
    fn an_untrusted_native_dep_is_refused() {
        // Phase 4: a native-declaring dependency the app did not authorize in `[trust].native` is
        // refused — the mere presence of native code no longer runs arbitrary Rust.
        let entry = native_dep_project("native_untrusted", true, false);
        let err = resolve_graph(&entry).expect_err("must be refused");
        assert!(err.message().contains("acme/imgfx"), "{err}");
        assert!(
            err.message().contains("[trust].native"),
            "points at the fix: {err}"
        );
        assert!(err.message().contains("native code"), "{err}");
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
        assert!(err.message().contains("acme/imgfx"), "{err}");
        assert!(err.message().contains("[trust].native"), "{err}");
    }

    #[test]
    fn a_builtin_scope_registry_dependency_is_refused() {
        // namespace-protection #2: a registry dependency under a built-in scope (`std`/`noeta`/`core`)
        // is refused at resolve time — the compiler provides these, so a registry serving `std/…` is a
        // shadow-core supply-chain attack. Refusal happens in `solve`/`gather` before any index query,
        // so this needs no network and no configured registry.
        let base = std::env::temp_dir().join("noeta_graph_test_reserved_scope");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nextra = { version = \"^1\", package = \"std/extra\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
        let err = resolve_graph(&app.join("main.noe")).expect_err("built-in scope must be refused");
        assert!(err.message().contains("std/extra"), "{err}");
        assert!(
            err.message().contains("supply-chain"),
            "names the threat: {err}"
        );
        assert!(err.message().contains("built into"), "{err}");
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
