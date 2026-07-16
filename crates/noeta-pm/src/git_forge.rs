//! A **git forge as a registry** (private-registries arc) — resolve packages directly from any git
//! host (GitHub, GitLab, Gitea/Forgejo, a bare git server) instead of the hosted index service. The
//! convention is Go-module-like: a package `company/pkg` routed to a forge base `<base>` lives at
//! `<base>/<pkg>`, its **published versions are the semver git tags** (`v1.2.3`), and each version's
//! dependencies come from the `noeta.toml` at that tag. Nothing is stored server-side: the "registry"
//! is just the forge's repos + tags. The `[registries]` shorthands `github:<owner>`, `gitlab:<group>`,
//! and `git:<url>` all resolve to a forge base URL (see [`crate::manifest::RegistrySource`]).
//!
//! Because it implements the same [`crate::registry::Index`] trait as the hosted index, the resolver
//! treats a git-forge package identically — the `GitCoords` it returns flow through the same fetch path
//! a direct `git` dependency takes. Publishing is `git tag && git push` (so [`Index::publish`] is
//! intentionally unsupported here).
//!
//! Private repos authenticate via git itself: ambient credentials (a helper / `gh auth` / SSH) by
//! default, or `NOETA_GITHUB_TOKEN` as a CI override — see [`crate::git_auth`]. This type holds no
//! credential; auth is applied per git-invocation.

use std::path::{Path, PathBuf};

use semver::Version;

use crate::error::PmError;
use crate::registry::{Dep, GitCoords, Index, Release};

/// A git-forge registry over one org/group **base URL** (any git host). Version discovery + per-tag
/// manifests come from a cached bare clone of each repo (so tags and `noeta.toml@tag` are read with
/// plain local git). Authentication for private repos is applied per git-invocation from the
/// environment (see [`crate::git_auth`]) — this type holds no credential.
#[derive(Debug)]
pub struct GitForgeIndex {
    /// The org/group prefix URL — e.g. `https://github.com/acme`, `https://gitlab.com/team/sub`, a
    /// self-hosted `https://git.example.com/org`, or (tests) a local path. `<base>/<package>` is a repo.
    base: String,
    /// Where bare clones are cached (one per repo).
    cache_dir: PathBuf,
}

impl GitForgeIndex {
    /// Open the git forge at `base` (an org/group prefix URL) as a registry. The cache dir comes from
    /// `NOETA_GIT_FORGE_CACHE` or the toolchain cache. Private-repo auth is separate (ambient git
    /// credentials, or `NOETA_GITHUB_TOKEN` — see [`crate::git_auth`]).
    pub fn from_base(base: &str) -> Result<GitForgeIndex, PmError> {
        let cache_dir = match std::env::var_os("NOETA_GIT_FORGE_CACHE") {
            Some(dir) => PathBuf::from(dir),
            None => noeta_cache::Cache::locate()
                .ok_or_else(|| {
                    PmError::Io(
                        "cannot locate a cache directory for git-forge registries (set HOME)"
                            .to_string(),
                    )
                })?
                .join("git-forge"),
        };
        Ok(GitForgeIndex::new(base, cache_dir))
    }

    /// Construct with an explicit cache dir (used by tests to point at a local repo host).
    pub fn new(base: impl Into<String>, cache_dir: impl Into<PathBuf>) -> GitForgeIndex {
        GitForgeIndex {
            base: base.into(),
            cache_dir: cache_dir.into(),
        }
    }

    /// The clone URL for a repo under this forge: `<base>/<package>`.
    fn repo_url(&self, package: &str) -> String {
        format!("{}/{}", self.base.trim_end_matches('/'), package)
    }

    /// The cached bare-clone path for a repo — under a filesystem-safe slug of the base URL, so forges
    /// on different hosts/orgs never collide in the cache.
    fn bare_path(&self, package: &str) -> PathBuf {
        self.cache_dir
            .join(slug(&self.base))
            .join(format!("{package}.git"))
    }

    /// Ensure an up-to-date bare clone of the repo exists in the cache; return its path. A first call
    /// clones; a later call refreshes tags. A clone failure (the repo doesn't exist, or is private and
    /// we're unauthenticated) surfaces as the error — which is exactly "no such package here".
    fn ensure_clone(&self, package: &str) -> Result<PathBuf, PmError> {
        let bare = self.bare_path(package);
        let url = self.repo_url(package);
        if bare.join("HEAD").exists() {
            // Refresh from the URL directly (not a named remote), so it works regardless of how the
            // bare clone configured `origin`.
            git(&[
                "-C",
                path_str(&bare)?,
                "fetch",
                "--tags",
                "--force",
                &url,
                "refs/tags/*:refs/tags/*",
            ])
            .map_err(|err| PmError::Network(format!("refreshing `{url}`: {err}")))?;
        } else {
            if let Some(parent) = bare.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    PmError::Io(format!("cannot create the git-forge cache: {err}"))
                })?;
            }
            git(&["clone", "--bare", "--quiet", &url, path_str(&bare)?]).map_err(|err| {
                PmError::Network(format!(
                    "cannot access `{url}` — the repo may not exist, or be private and require \
                     authentication: {err}"
                ))
            })?;
        }
        Ok(bare)
    }
}

/// A filesystem-safe slug of a base URL for the cache directory (`https://github.com/acme` →
/// `https_github.com_acme`). Distinct bases map to distinct slugs (non-`[A-Za-z0-9.-]` → `_`).
fn slug(base: &str) -> String {
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Index for GitForgeIndex {
    fn releases(&self, name: &str) -> Result<Vec<Release>, PmError> {
        let package = name
            .split('/')
            .nth(1)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                PmError::Manifest(format!("`{name}` is not a `company/package` identity"))
            })?;
        let bare = self.ensure_clone(package)?;
        let bare_str = path_str(&bare)?;

        let tag_list = git(&["-C", bare_str, "tag", "--list", "v*"]).map_err(PmError::Network)?;
        let mut releases = Vec::new();
        for tag in tag_list.lines().map(str::trim).filter(|t| !t.is_empty()) {
            // A version tag is `v<semver>`; anything else (a non-release tag) is skipped.
            let Some(version) = tag.strip_prefix('v').and_then(|v| Version::parse(v).ok()) else {
                continue;
            };
            // The commit the tag resolves to (peeling an annotated tag to its commit).
            let sha = git(&["-C", bare_str, "rev-list", "-n", "1", tag])
                .map_err(PmError::Network)?
                .trim()
                .to_string();
            if sha.is_empty() {
                continue;
            }
            // The version's dependency edges come from its `noeta.toml`. A tag with no manifest (or an
            // unparseable one) isn't a valid package release — skip it rather than fail the listing.
            let Ok(manifest_text) = git(&["-C", bare_str, "show", &format!("{tag}:noeta.toml")])
            else {
                continue;
            };
            let Ok(deps) = registry_deps(&manifest_text) else {
                continue;
            };
            releases.push(Release {
                version,
                coords: GitCoords {
                    url: self.repo_url(package),
                    tag: tag.to_string(),
                    sha,
                },
                deps,
                signature: None,
                bundle: None,
                // A git-forge release carries no publish timestamp, so it is never subject to the
                // consumer's publish cooldown (namespace-protection #1) — same as any git/path source.
                published_at: None,
                // No registry record to declare one in — the tag's own manifest/LICENSE is right there.
                license: None,
            });
        }
        Ok(releases)
    }

    fn publish(&self, _name: &str, _release: &Release) -> Result<(), PmError> {
        Err(PmError::Network(
            "a git-forge registry has no publish endpoint — publish by pushing a semver tag \
             (`git tag v1.2.3 && git push --tags`)"
                .to_string(),
        ))
    }

    /// The bare clone this index already fetched for `name` (populated by [`Self::releases`]), so the
    /// resolver materializes the tree from it — no second network clone. `None` if it isn't on disk.
    fn local_repo(&self, name: &str) -> Option<PathBuf> {
        let package = name.split('/').nth(1).filter(|p| !p.is_empty())?;
        let bare = self.bare_path(package);
        bare.join("HEAD").exists().then_some(bare)
    }

    // scope_key defaults to None: a git-forge registry serves no provenance keys (its trust model is
    // the host's access control + signed commits/tags, not a registry-registered key).
}

/// Extract a published package's **registry** dependency edges from its `noeta.toml` — the only edges a
/// resolver needs from the index. Path/git edges in a published package are ignored (a published
/// package depends on other packages by registry).
fn registry_deps(manifest_text: &str) -> Result<Vec<Dep>, PmError> {
    let manifest = crate::manifest::Manifest::parse(manifest_text)?;
    let mut deps = Vec::new();
    for dep in manifest.dependencies().values() {
        if let crate::manifest::Dependency::Registry {
            package: Some(pkg),
            req,
        } = dep
        {
            deps.push(Dep {
                package: format!("{}/{}", pkg.company, pkg.package),
                req: req.clone(),
            });
        }
    }
    Ok(deps)
}

fn path_str(p: &Path) -> Result<&str, PmError> {
    p.to_str()
        .ok_or_else(|| PmError::Io(format!("path `{}` is not valid UTF-8", p.display())))
}

/// Run `git` with `args`, returning trimmed stdout or an error built from stderr. Token auth
/// (private-registries S5) is prepended so private-repo version discovery authenticates; empty when no
/// `NOETA_GITHUB_TOKEN`, so git uses ambient credentials.
fn git(args: &[&str]) -> Result<String, String> {
    // Delegates to the git_auth choke point (one runner, one credential-injection path); the
    // richer error ("is it installed?", the failing argv) replaced this copy's bare stderr.
    crate::git_auth::run_git(args.iter().copied())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Run git in `dir`, panicking on failure (test setup).
    fn setup_git(dir: &Path, args: &[&str]) {
        let mut a = vec!["-C", dir.to_str().unwrap()];
        a.extend_from_slice(args);
        let out = Command::new("git").args(&a).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Lay out a repo `<host>/<org>/<package>` with a manifest + a couple of tagged releases; the
    /// second version declares a registry dependency (to prove dep extraction).
    fn make_repo(host: &Path, org: &str, package: &str) {
        let repo = host.join(org).join(package);
        std::fs::create_dir_all(&repo).unwrap();
        setup_git(&repo, &["init", "-q", "-b", "main"]);
        setup_git(&repo, &["config", "user.email", "t@t.test"]);
        setup_git(&repo, &["config", "user.name", "T"]);
        std::fs::write(
            repo.join("lib.noe"),
            "namespace thing.core;\npub fn go(): int { return 1; }\n",
        )
        .unwrap();

        std::fs::write(
            repo.join("noeta.toml"),
            format!("[package]\nname = \"{org}/{package}\"\nversion = \"1.0.0\"\n"),
        )
        .unwrap();
        setup_git(&repo, &["add", "."]);
        setup_git(&repo, &["commit", "-q", "-m", "v1"]);
        setup_git(&repo, &["tag", "v1.0.0"]);

        std::fs::write(
            repo.join("noeta.toml"),
            format!(
                "[package]\nname = \"{org}/{package}\"\nversion = \"1.1.0\"\n\
                 [dependencies]\nother = {{ version = \"^2.0\", package = \"{org}/other\" }}\n"
            ),
        )
        .unwrap();
        setup_git(&repo, &["commit", "-q", "-am", "v1.1"]);
        setup_git(&repo, &["tag", "v1.1.0"]);
        // A non-semver tag must be ignored.
        setup_git(&repo, &["tag", "nightly"]);
    }

    #[test]
    fn resolves_versions_coords_and_deps_from_tags() {
        if !git_available() {
            return;
        }
        let tmp = std::env::temp_dir().join("noeta_git_forge_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let host = tmp.join("host");
        let cache = tmp.join("cache");
        make_repo(&host, "acme", "thing");

        // The forge base is the org prefix (`<host>/acme`); the package's repo is `<base>/thing`.
        let base = host.join("acme");
        let idx = GitForgeIndex::new(base.to_str().unwrap(), cache.clone());
        // Called twice to exercise both the initial clone and the refresh path.
        let _ = idx.releases("acme/thing").unwrap();
        let mut releases = idx.releases("acme/thing").unwrap();
        releases.sort_by(|a, b| a.version.cmp(&b.version));

        let versions: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, vec!["1.0.0", "1.1.0"]); // `nightly` skipped

        let v11 = releases
            .iter()
            .find(|r| r.version == Version::new(1, 1, 0))
            .unwrap();
        assert_eq!(v11.coords.tag, "v1.1.0");
        assert_eq!(v11.coords.url, format!("{}/thing", base.to_str().unwrap()));
        assert_eq!(v11.coords.sha.len(), 40);
        // The registry dependency declared at v1.1.0 is surfaced for the resolver.
        assert_eq!(v11.deps.len(), 1);
        assert_eq!(v11.deps[0].package, "acme/other");
        // v1.0.0 declared no dependencies.
        let v10 = releases
            .iter()
            .find(|r| r.version == Version::new(1, 0, 0))
            .unwrap();
        assert!(v10.deps.is_empty());
    }

    #[test]
    fn a_missing_repo_is_a_clear_error() {
        if !git_available() {
            return;
        }
        let tmp = std::env::temp_dir().join("noeta_git_forge_missing");
        let _ = std::fs::remove_dir_all(&tmp);
        let idx = GitForgeIndex::new(
            tmp.join("host").join("acme").to_str().unwrap(),
            tmp.join("cache"),
        );
        let err = idx.releases("acme/nope").unwrap_err();
        assert!(
            err.message().contains("may not exist") || err.message().contains("private"),
            "{err}"
        );
    }

    #[test]
    fn publish_is_unsupported() {
        let idx = GitForgeIndex::new("https://github.com/acme", std::env::temp_dir());
        let r = Release {
            version: Version::new(1, 0, 0),
            coords: GitCoords {
                url: "u".into(),
                tag: "v1.0.0".into(),
                sha: "s".into(),
            },
            deps: Vec::new(),
            signature: None,
            bundle: None,
            published_at: None,
            license: None,
        };
        assert!(
            idx.publish("acme/thing", &r)
                .unwrap_err()
                .message()
                .contains("pushing a semver tag")
        );
    }

    #[test]
    fn materializes_a_tree_from_the_local_clone() {
        // Unified clone (private-registries): the resolver materializes a release's tree from the
        // index's already-fetched bare clone (`local_repo`), not a second network clone. Prove it by
        // fetching straight from that clone — the origin is never consulted here.
        if !git_available() {
            return;
        }
        let tmp = std::env::temp_dir().join("noeta_git_forge_local_mat");
        let _ = std::fs::remove_dir_all(&tmp);
        let host = tmp.join("host");
        make_repo(&host, "acme", "thing");

        let idx = GitForgeIndex::new(host.join("acme").to_str().unwrap(), tmp.join("cache"));
        let releases = idx.releases("acme/thing").unwrap();
        let sha = releases
            .iter()
            .find(|r| r.version == Version::new(1, 1, 0))
            .unwrap()
            .coords
            .sha
            .clone();

        // The index exposes the local clone it fetched…
        let bare = idx
            .local_repo("acme/thing")
            .expect("bare clone present after releases()");
        // …and the store materializes the pinned tree from it, with no reference to the origin.
        let store = crate::store::Store::open_at(tmp.join("store")).unwrap();
        let git_ref = crate::manifest::GitRef::Tag("v1.1.0".to_string());
        let fetched =
            crate::git::fetch_pinned(bare.to_str().unwrap(), &git_ref, &sha, &store).unwrap();
        assert!(fetched.path.join("lib.noe").exists());
        assert!(fetched.path.join("noeta.toml").exists());
        assert_eq!(fetched.sha, sha);
    }
}
