//! Dependency resolution (package-manager P2.2) — a PubGrub resolver over the package graph.
//!
//! The resolver is **pure**: it works on package identities (`company/package`) + SemVer, never
//! touching the network. A [`Registry`] abstracts *where* package metadata comes from — a synthetic
//! in-memory map in tests, the real git/registry index in P2.3+ — so the algorithm is unit-tested in
//! isolation. Resolution walks the reachable dependency graph into a [`OfflineDependencyProvider`],
//! then runs [`pubgrub::resolve`], which yields either a `name → version` solution or an
//! **explainable** conflict report (the reason the user's choice was PubGrub).
//!
//! The one bridge PubGrub needs is SemVer → its interval `Ranges`: a `semver::VersionReq` (a set of
//! comparators) becomes a [`Ranges<Version>`]. That conversion ([`req_to_ranges`]) is the subtle part
//! and is checked against `semver`'s own `VersionReq::matches` on a version grid (see tests).

use std::collections::{BTreeMap, HashSet, VecDeque};

use pubgrub::{DefaultStringReporter, OfflineDependencyProvider, Ranges, Reporter};
use semver::{Comparator, Op, Version, VersionReq};

/// PubGrub's version-set type over SemVer versions — a union of half-open intervals.
type Vs = Ranges<Version>;

/// A source of package metadata for resolution (package-manager P2.2). Abstracts the registry/git
/// index so the resolver stays pure: tests supply a synthetic map, P2.3+ supplies the real index.
pub trait Registry {
    /// Every published version of `package` (order irrelevant — PubGrub picks the highest in range).
    fn versions(&self, package: &str) -> Vec<Version>;
    /// The dependencies of `package`@`version`: each a `(package identity, requirement)`.
    fn dependencies(&self, package: &str, version: &Version) -> Vec<(String, VersionReq)>;
}

/// Resolve the root package's dependency graph to a concrete `name → version` map, or an explainable
/// error. `root` / `root_version` identify the consuming package (its own version is irrelevant to
/// the result — nothing depends on the root); `root_deps` are its declared `[dependencies]`. The
/// returned map excludes the synthetic root.
pub fn resolve(
    registry: &dyn Registry,
    root: &str,
    root_version: &Version,
    root_deps: &[(String, VersionReq)],
) -> Result<BTreeMap<String, Version>, String> {
    let mut provider = OfflineDependencyProvider::<String, Vs>::new();

    // The root, with its declared dependencies.
    provider.add_dependencies(
        root.to_string(),
        root_version.clone(),
        root_deps
            .iter()
            .map(|(name, req)| (name.clone(), req_to_ranges(req))),
    );

    // Walk the reachable dependency packages breadth-first, registering *every* version of each with
    // its own dependencies — PubGrub needs the whole candidate subgraph to choose a solution.
    let mut queue: VecDeque<String> = root_deps.iter().map(|(n, _)| n.clone()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(package) = queue.pop_front() {
        if !seen.insert(package.clone()) {
            continue;
        }
        for version in registry.versions(&package) {
            let deps = registry.dependencies(&package, &version);
            for (dep, _) in &deps {
                if !seen.contains(dep) {
                    queue.push_back(dep.clone());
                }
            }
            provider.add_dependencies(
                package.clone(),
                version,
                deps.into_iter().map(|(n, req)| (n, req_to_ranges(&req))),
            );
        }
    }

    match pubgrub::resolve(&provider, root.to_string(), root_version.clone()) {
        Ok(solution) => Ok(solution
            .into_iter()
            .filter(|(name, _)| name != root)
            .collect()),
        Err(pubgrub::PubGrubError::NoSolution(tree)) => Err(DefaultStringReporter::report(&tree)),
        Err(err) => Err(format!("{err}")),
    }
}

/// Convert a `semver::VersionReq` to PubGrub's interval [`Ranges`]. An empty requirement (`*`) is the
/// full range; otherwise every comparator is intersected (a `VersionReq` is a conjunction). The
/// per-comparator mapping mirrors Cargo/npm SemVer semantics, including the partial-version forms
/// (`^1.2`, `~1`, `1.*`, `=1`); it is validated against `semver`'s own `matches` in the tests.
pub fn req_to_ranges(req: &VersionReq) -> Vs {
    if req.comparators.is_empty() {
        return Ranges::full();
    }
    req.comparators
        .iter()
        .map(comparator_to_ranges)
        .fold(Ranges::full(), |acc, r| acc.intersection(&r))
}

/// A version with unspecified minor/patch zero-filled — the lower bound of a comparator.
fn low(c: &Comparator) -> Version {
    Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0))
}

/// The exclusive upper bound implied by a caret at `(major, minor, patch)` precision: increment the
/// left-most non-zero component (SemVer "compatible" range), respecting how many components the user
/// actually wrote. `^0` → `<1.0.0`, `^0.2` → `<0.3.0`, `^0.0.3` → `<0.0.4`, `^1.2` → `<2.0.0`.
fn caret_upper(c: &Comparator) -> Version {
    match (c.major, c.minor, c.patch) {
        (0, Some(0), Some(p)) => Version::new(0, 0, p + 1),
        (0, Some(m), _) => Version::new(0, m + 1, 0),
        (0, None, _) => Version::new(1, 0, 0),
        (major, _, _) => Version::new(major + 1, 0, 0),
    }
}

/// The exclusive upper bound of an `=`/`~`/wildcard range at the precision the user wrote: a missing
/// component widens the bound one level up. `=1` / `~1` / `1.*` → `<2.0.0`; `=1.2` / `~1.2` / `1.2.*`
/// → `<1.3.0`; a fully-specified `=1.2.3` has no widening (handled as a singleton by the caller).
fn precision_upper(c: &Comparator) -> Version {
    match (c.minor, c.patch) {
        (Some(m), Some(_)) => Version::new(c.major, m, 0), // caller handles exact patch
        (Some(m), None) => Version::new(c.major, m + 1, 0),
        (None, _) => Version::new(c.major + 1, 0, 0),
    }
}

fn comparator_to_ranges(c: &Comparator) -> Vs {
    match c.op {
        // `=1.2.3` is a singleton; `=1.2` / `=1` widen to the precision range.
        Op::Exact | Op::Wildcard => {
            if c.patch.is_some() && c.minor.is_some() {
                Ranges::singleton(low(c))
            } else {
                Ranges::between(low(c), precision_upper(c))
            }
        }
        Op::Greater => Ranges::strictly_higher_than(low(c)),
        Op::GreaterEq => Ranges::higher_than(low(c)),
        Op::Less => Ranges::strictly_lower_than(low(c)),
        // `<=1.2.3` includes 1.2.3; a partial `<=1.2` includes all of 1.2.x.
        Op::LessEq => {
            if c.patch.is_some() && c.minor.is_some() {
                Ranges::lower_than(low(c))
            } else {
                Ranges::strictly_lower_than(precision_upper(c))
            }
        }
        // `~1.2.3`/`~1.2` → `<1.3.0`; `~1` → `<2.0.0`.
        Op::Tilde => {
            let upper = match c.minor {
                Some(m) => Version::new(c.major, m + 1, 0),
                None => Version::new(c.major + 1, 0, 0),
            };
            Ranges::between(low(c), upper)
        }
        Op::Caret => Ranges::between(low(c), caret_upper(c)),
        // `semver::Op` is non-exhaustive; an unknown operator resolves permissively.
        _ => Ranges::full(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    fn req(s: &str) -> VersionReq {
        VersionReq::parse(s).unwrap()
    }

    /// The core correctness property: the interval conversion must agree with `semver`'s own
    /// `VersionReq::matches` for every version on a dense grid. This catches any comparator mapping
    /// bug directly, across caret/tilde/wildcard/exact/inequality and their partial forms.
    #[test]
    fn ranges_agree_with_semver_matches() {
        let reqs = [
            "*",
            "^1.2.3",
            "^1.2",
            "^1",
            "^0.2.3",
            "^0.2",
            "^0.0.3",
            "^0.0",
            "^0",
            "~1.2.3",
            "~1.2",
            "~1",
            "=1.2.3",
            "=1.2",
            "=1",
            "1.*",
            "1.2.*",
            ">1.2.3",
            ">=1.2.3",
            "<1.2.3",
            "<=1.2.3",
            ">=1.2.0, <1.5.0",
            ">1.0.0, <=2.0.0",
        ];
        let mut versions = Vec::new();
        for major in 0..3 {
            for minor in 0..4 {
                for patch in 0..4 {
                    versions.push(Version::new(major, minor, patch));
                }
            }
        }
        for r in reqs {
            let parsed = req(r);
            let ranges = req_to_ranges(&parsed);
            for ver in &versions {
                assert_eq!(
                    ranges.contains(ver),
                    parsed.matches(ver),
                    "mismatch for req `{r}` at version {ver}",
                );
            }
        }
    }

    /// A tiny in-memory registry for the resolution tests.
    #[derive(Default)]
    struct MemRegistry {
        // package → version → deps
        packages: HashMap<String, HashMap<Version, Vec<(String, VersionReq)>>>,
    }
    impl MemRegistry {
        fn add(&mut self, pkg: &str, version: &str, deps: &[(&str, &str)]) {
            self.packages.entry(pkg.to_string()).or_default().insert(
                v(version),
                deps.iter().map(|(n, r)| (n.to_string(), req(r))).collect(),
            );
        }
    }
    impl Registry for MemRegistry {
        fn versions(&self, package: &str) -> Vec<Version> {
            self.packages
                .get(package)
                .map(|vs| vs.keys().cloned().collect())
                .unwrap_or_default()
        }
        fn dependencies(&self, package: &str, version: &Version) -> Vec<(String, VersionReq)> {
            self.packages
                .get(package)
                .and_then(|vs| vs.get(version))
                .cloned()
                .unwrap_or_default()
        }
    }

    #[test]
    fn resolves_a_simple_chain_to_highest_compatible() {
        let mut reg = MemRegistry::default();
        reg.add("acme/http", "1.0.0", &[("acme/bytes", "^1.0")]);
        reg.add("acme/http", "1.2.0", &[("acme/bytes", "^1.0")]);
        reg.add("acme/bytes", "1.0.0", &[]);
        reg.add("acme/bytes", "1.4.0", &[]);
        let sln = resolve(
            &reg,
            "root",
            &v("0.0.0"),
            &[("acme/http".to_string(), req("^1.0"))],
        )
        .expect("resolves");
        assert_eq!(sln["acme/http"], v("1.2.0"));
        assert_eq!(sln["acme/bytes"], v("1.4.0"));
    }

    #[test]
    fn resolves_a_shared_transitive_to_one_version() {
        // Both deps need `acme/bytes` in overlapping ranges — one version must satisfy both.
        let mut reg = MemRegistry::default();
        reg.add("acme/http", "1.0.0", &[("acme/bytes", ">=1.0, <1.5")]);
        reg.add("acme/json", "2.0.0", &[("acme/bytes", "^1.2")]);
        reg.add("acme/bytes", "1.2.0", &[]);
        reg.add("acme/bytes", "1.4.0", &[]);
        reg.add("acme/bytes", "1.6.0", &[]);
        let sln = resolve(
            &reg,
            "root",
            &v("0.0.0"),
            &[
                ("acme/http".to_string(), req("^1.0")),
                ("acme/json".to_string(), req("^2.0")),
            ],
        )
        .expect("resolves");
        // 1.6.0 is out for http (<1.5); 1.4.0 satisfies both.
        assert_eq!(sln["acme/bytes"], v("1.4.0"));
    }

    #[test]
    fn backtracks_past_a_greedy_dead_end() {
        // The case a greedy resolver gets *wrong* (Phase 4, S5b): picking the highest `foo` forces an
        // incompatible `bar`, but a solution exists at a lower `foo`. PubGrub backtracks to find it.
        //   root → foo ^1, baz ^1
        //   foo 1.1 → bar ^2 ;  foo 1.0 → bar ^1 ;  baz 1.0 → bar ^1 ;  bar ∈ {1.0, 2.0}
        let mut reg = MemRegistry::default();
        reg.add("acme/foo", "1.1.0", &[("acme/bar", "^2.0")]);
        reg.add("acme/foo", "1.0.0", &[("acme/bar", "^1.0")]);
        reg.add("acme/baz", "1.0.0", &[("acme/bar", "^1.0")]);
        reg.add("acme/bar", "1.0.0", &[]);
        reg.add("acme/bar", "2.0.0", &[]);
        let sln = resolve(
            &reg,
            "root",
            &v("0.0.0"),
            &[
                ("acme/foo".to_string(), req("^1.0")),
                ("acme/baz".to_string(), req("^1.0")),
            ],
        )
        .expect("a solution exists — the resolver must backtrack to find it");
        // Greedy would pick foo 1.1 → bar 2.0 → clash with baz's bar ^1 and report "no solution".
        // Backtracking selects foo 1.0 + bar 1.0, satisfying everyone.
        assert_eq!(
            sln["acme/foo"],
            v("1.0.0"),
            "backtracked to the compatible foo"
        );
        assert_eq!(sln["acme/bar"], v("1.0.0"));
        assert_eq!(sln["acme/baz"], v("1.0.0"));
    }

    #[test]
    fn an_unsatisfiable_constraint_reports_a_conflict() {
        let mut reg = MemRegistry::default();
        reg.add("acme/http", "1.0.0", &[("acme/bytes", "^2.0")]);
        reg.add("acme/bytes", "1.0.0", &[]);
        let err = resolve(
            &reg,
            "root",
            &v("0.0.0"),
            &[("acme/http".to_string(), req("^1.0"))],
        )
        .expect_err("no solution");
        // The explainable report names the unsatisfiable package.
        assert!(err.contains("acme/bytes"), "report was: {err}");
    }
}
