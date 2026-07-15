//! Git-tag dependency fetch (package-manager P2.3b) — the real-IO seam, CLI-only, **outside** the
//! differential oracle (a network operation, done before compilation; the build then runs over the
//! materialized on-disk tree).
//!
//! Sources are **git + tagged releases only** (user decision): a dependency names a repository URL
//! and a tag. Fetch shells out to the **system `git`** — dependency-light, Go-like, no libgit2/gix —
//! so a consumer needs `git` on `PATH` only when it actually pulls a git dependency (a pure-path /
//! pure-`std` program needs nothing). The tag is resolved to a **commit SHA** at the remote, the
//! commit is checked out into the [`Store`], and the clone's `HEAD` is verified against the resolved
//! SHA (integrity: a moved tag or a tampered remote is rejected). The SHA + content hash are what the
//! lockfile pins (P2.4), so a later build reproduces exactly.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::store::{Store, hash_tree};

/// A materialized git dependency.
#[derive(Debug)]
pub struct Fetched {
    /// The commit SHA the tag resolved to — the store key and the lockfile pin (consumed in P2.4).
    #[allow(dead_code)]
    pub sha: String,
    /// The content hash of the checked-out tree (the lockfile's integrity value, P2.4).
    #[allow(dead_code)]
    pub content_hash: String,
    /// The on-disk tree in the store.
    pub path: PathBuf,
}

/// Fetch `url`@`tag` into `store`, returning the materialized tree. The tag is resolved to a commit
/// SHA at the remote (`ls-remote`), then materialized. If that SHA is already stored, no clone
/// happens — only the cheap `ls-remote`. Use [`fetch_pinned`] when the lockfile already records the
/// SHA (it skips even the `ls-remote`).
pub fn fetch(url: &str, tag: &str, store: &Store) -> Result<Fetched, String> {
    let sha = ls_remote_tag(url, tag)?;
    materialize_sha(url, tag, sha, store)
}

/// Fetch `url`@`tag` **pinned to a known commit `sha`** (package-manager P2.4) — the lockfile path.
/// If the SHA is already stored, this touches the network **not at all** (offline, reproducible). If
/// it isn't, the tag is cloned and its `HEAD` verified against `sha`; a mismatch means the tag moved
/// since the lock was written — a reproducibility violation the user resolves with `noeta update`.
pub fn fetch_pinned(url: &str, tag: &str, sha: &str, store: &Store) -> Result<Fetched, String> {
    materialize_sha(url, tag, sha.to_string(), store)
}

/// Materialize a known `url`@`tag`→`sha` into the store (shared by [`fetch`] and [`fetch_pinned`]):
/// reuse the stored tree if present, else clone the tag and verify its `HEAD` equals `sha`.
fn materialize_sha(url: &str, tag: &str, sha: String, store: &Store) -> Result<Fetched, String> {
    let path = if store.contains(&sha) {
        store.path_for(&sha)
    } else {
        store
            .publish(&sha, |staging| clone_tag(url, tag, &sha, staging))
            .map_err(|err| format!("storing `{url}`@`{tag}`: {err}"))?
    };
    let content_hash = hash_tree(&path).map_err(|err| format!("hashing `{url}`@`{tag}`: {err}"))?;
    Ok(Fetched {
        sha,
        content_hash,
        path,
    })
}

/// Resolve `url`@`tag` to the commit SHA it currently points at, without cloning (package-manager
/// Phase 4, S2) — used by `noeta publish` to pin the SHA into the registry index at publish time.
pub fn resolve_tag_sha(url: &str, tag: &str) -> Result<String, String> {
    ls_remote_tag(url, tag)
}

/// Resolve `tag` to its commit SHA at the remote, without cloning (a lightweight network call). For
/// an **annotated** tag `ls-remote` prints both the tag object and its peeled commit (`…^{}`); the
/// peeled commit is the one a checkout lands on, so it is preferred. A missing tag is an error.
fn ls_remote_tag(url: &str, tag: &str) -> Result<String, String> {
    let refspec = format!("refs/tags/{tag}");
    let out = run_git(["ls-remote", url, &refspec, &format!("{refspec}^{{}}")])?;
    let mut plain = None;
    for line in out.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if name == format!("{refspec}^{{}}") {
            return Ok(sha.to_string()); // peeled commit — definitive
        }
        if name == refspec {
            plain = Some(sha.to_string());
        }
    }
    plain.ok_or_else(|| format!("`{url}` has no tag `{tag}` (sources must be tagged releases)"))
}

/// Shallow-clone `url` at `tag` into `staging`, verify the checked-out `HEAD` equals `expected_sha`,
/// then drop the `.git` metadata (the package *is* its working tree). Runs inside the store's atomic
/// publish, so any error here leaves no partial tree.
fn clone_tag(url: &str, tag: &str, expected_sha: &str, staging: &Path) -> io::Result<()> {
    let dir = staging
        .to_str()
        .ok_or_else(|| io::Error::other("store path is not valid UTF-8"))?;
    run_git(["clone", "--depth", "1", "--branch", tag, url, dir]).map_err(io::Error::other)?;
    let head = run_git(["-C", dir, "rev-parse", "HEAD"]).map_err(io::Error::other)?;
    let head = head.trim();
    if head != expected_sha {
        return Err(io::Error::other(format!(
            "integrity check failed: `{url}`@`{tag}` resolved to {expected_sha} but the clone's \
             HEAD is {head} (the tag may have moved)"
        )));
    }
    let _ = std::fs::remove_dir_all(staging.join(".git"));
    Ok(())
}

/// The authorship of a release commit and how it sits in the repo's history (namespace-protection,
/// committer signal) — a **soft** anomaly signal (git author/committer fields are self-set and thus
/// forgeable), meant to prompt a human to look, not to gate. Computed by [`authorship`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorship {
    /// The release commit's author, `Name <email>`.
    pub author: String,
    /// Whether the author has **no earlier commit** reachable from the release commit — this is their
    /// first contribution to the repo (the event-stream / new-maintainer pattern).
    pub author_first_seen: bool,
    /// How many *distinct other* authors appear earlier in the history. Zero means a brand-new or
    /// solo repo, where "first-time author" is trivially true and not worth flagging.
    pub prior_authors: usize,
    /// Authors introduced in `since..sha` who never appear before `since` — the committers an upgrade
    /// pulls in. Empty when `since` is `None`, unrelated to `sha`, or introduces nobody new.
    pub new_since: Vec<String>,
}

impl Authorship {
    /// Whether this is worth warning about: a first-time author *in a repo that already had other
    /// authors* (so a fresh solo project doesn't warn), or an upgrade that pulls in a new committer.
    pub fn is_noteworthy(&self) -> bool {
        (self.author_first_seen && self.prior_authors > 0) || !self.new_since.is_empty()
    }
}

/// Analyze the authorship of `sha` in `url`'s repo, optionally relative to a previously-pinned commit
/// `since` (namespace-protection, committer signal). Does a **blobless** clone (`--filter=blob:none`,
/// no working tree) so it pulls the commit graph without file contents — a network op done only for
/// `noeta update`/`add` on the deps that changed, never on the resolve hot path. Best-effort: the
/// caller treats an `Err` as "couldn't tell" and stays quiet rather than failing the command.
pub fn authorship(url: &str, sha: &str, since: Option<&str>) -> Result<Authorship, String> {
    let short = &sha[..sha.len().min(12)];
    let dir = std::env::temp_dir().join(format!("noeta-authorship-{}-{short}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let dir_str = dir
        .to_str()
        .ok_or_else(|| "temp path is not valid UTF-8".to_string())?
        .to_string();
    // A blobless bare clone: full commit graph, no blobs, no checkout. On a local `file` transport the
    // filter is a no-op (still a full but cheap clone), so tests and real remotes both work.
    let clone = run_git([
        "clone",
        "--filter=blob:none",
        "--bare",
        "--quiet",
        url,
        &dir_str,
    ]);
    let result = clone.and_then(|_| authorship_from(&dir_str, sha, since));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Compute the authorship facts from an already-cloned `git_dir` (split out so the git-plumbing is
/// testable against a local clone without the network).
fn authorship_from(git_dir: &str, sha: &str, since: Option<&str>) -> Result<Authorship, String> {
    let gd = format!("--git-dir={git_dir}");
    // The tip author's display + email, and the full ancestor author-email list (newest first).
    let author = run_git([&gd, "show", "-s", "--format=%an <%ae>", sha])?
        .trim()
        .to_string();
    let emails = run_git([&gd, "log", "--format=%ae", sha])?;
    let mut lines = emails.lines();
    let tip_email = lines.next().unwrap_or("").trim().to_string();
    let ancestors: Vec<String> = lines.map(|l| l.trim().to_string()).collect();
    let author_first_seen = !ancestors.contains(&tip_email);
    let prior_authors = ancestors
        .iter()
        .filter(|e| **e != tip_email)
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    // New authors an upgrade pulls in (best-effort — a `since` unrelated to `sha` yields nobody new).
    let new_since = match since {
        None => Vec::new(),
        Some(since) => {
            let before: std::collections::BTreeSet<String> =
                run_git([&gd, "log", "--format=%ae", since])
                    .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
                    .unwrap_or_default();
            let range = run_git([
                &gd,
                "log",
                "--format=%an <%ae>%x1f%ae",
                &format!("{since}..{sha}"),
            ])
            .unwrap_or_default();
            let mut seen = std::collections::BTreeSet::new();
            let mut out = Vec::new();
            for line in range.lines() {
                if let Some((display, email)) = line.split_once('\u{1f}') {
                    let email = email.trim();
                    if !before.contains(email) && seen.insert(email.to_string()) {
                        out.push(display.trim().to_string());
                    }
                }
            }
            out
        }
    };

    Ok(Authorship {
        author,
        author_first_seen,
        prior_authors,
        new_since,
    })
}

/// Normalize a git remote `url` to a browsable **HTTPS repo URL** (best-effort): strip a trailing
/// `.git` and rewrite the `git@host:owner/repo` and `ssh://git@host/owner/repo` SSH forms. Returns
/// `None` for a local path / `file:` URL (no web home) or anything whose shape we don't recognize —
/// callers then show the raw `url`.
pub fn repo_web_url(url: &str) -> Option<String> {
    let strip_git = |s: &str| s.strip_suffix(".git").unwrap_or(s).to_string();
    if let Some(rest) = url.strip_prefix("https://") {
        return Some(format!("https://{}", strip_git(rest)));
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return Some(format!("https://{}", strip_git(rest)));
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        // ssh://git@host/owner/repo(.git)
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        return Some(format!("https://{}", strip_git(rest)));
    }
    if let Some(rest) = url.strip_prefix("git@") {
        // git@host:owner/repo(.git)
        let (host, path) = rest.split_once(':')?;
        return Some(format!("https://{host}/{}", strip_git(path)));
    }
    None
}

/// A browsable link to `sha` in `url`'s repo (`<repo>/commit/<sha>`, the GitHub/GitLab/Gitea shape),
/// or `None` when the repo isn't web-browsable (a local/`file` source).
pub fn commit_web_url(url: &str, sha: &str) -> Option<String> {
    repo_web_url(url).map(|repo| format!("{repo}/commit/{sha}"))
}

/// Run `git` with `args`, returning trimmed stdout on success or a message built from stderr. A
/// failure to even spawn `git` (not installed) is reported as such.
fn run_git<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    let args: Vec<&str> = args.into_iter().collect();
    let output = Command::new("git")
        .args(&args)
        .output()
        .map_err(|err| format!("cannot run `git` (is it installed and on PATH?): {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `git` is available — the fetch tests are meaningless without it and skip gracefully
    /// (though this project is itself a git repo, so it's essentially always present).
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "git {args:?} failed");
    }

    /// Build a throwaway local git repo with a tagged commit, returning its path (usable as a `file`
    /// URL — git accepts local paths).
    fn tagged_repo(name: &str, tag: &str) -> PathBuf {
        let repo = std::env::temp_dir().join(format!("noeta_git_test_{name}"));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        git(&["init", "-q"], &repo);
        std::fs::write(
            repo.join("noeta.toml"),
            "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("lib.noe"),
            "namespace lib.core;\npub fn v(): int { return 1; }\n",
        )
        .unwrap();
        git(&["add", "."], &repo);
        git(&["commit", "-q", "-m", "release"], &repo);
        git(&["tag", tag], &repo);
        repo
    }

    #[test]
    fn fetches_a_tag_into_the_store_and_verifies_head() {
        if !git_available() {
            return;
        }
        let repo = tagged_repo("fetch", "v1.0.0");
        let store_dir = std::env::temp_dir().join("noeta_git_test_store");
        let _ = std::fs::remove_dir_all(&store_dir);
        let store = Store::open_at(store_dir).unwrap();

        let fetched = fetch(repo.to_str().unwrap(), "v1.0.0", &store).unwrap();
        assert_eq!(fetched.sha.len(), 40, "a full commit SHA");
        assert!(store.contains(&fetched.sha));
        // The working tree is materialized; `.git` is stripped.
        assert!(fetched.path.join("lib.noe").is_file());
        assert!(!fetched.path.join(".git").exists());
        assert!(!fetched.content_hash.is_empty());

        // A second fetch is served from the store (idempotent) and agrees on the SHA + hash.
        let again = fetch(repo.to_str().unwrap(), "v1.0.0", &store).unwrap();
        assert_eq!(again.sha, fetched.sha);
        assert_eq!(again.content_hash, fetched.content_hash);
    }

    #[test]
    fn a_missing_tag_is_an_error() {
        if !git_available() {
            return;
        }
        let repo = tagged_repo("missingtag", "v1.0.0");
        let store = Store::open_at(std::env::temp_dir().join("noeta_git_test_store2")).unwrap();
        let err = fetch(repo.to_str().unwrap(), "v9.9.9", &store).unwrap_err();
        assert!(err.contains("no tag"), "got: {err}");
    }

    /// Commit `file` (created with unique content) authored by `name <email>`.
    fn commit_as(repo: &Path, file: &str, name: &str, email: &str) {
        std::fs::write(repo.join(file), format!("// {file}\n")).unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_COMMITTER_EMAIL", email)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["add", "."]);
        run(&["commit", "-q", "-m", file]);
    }

    #[test]
    fn authorship_flags_a_new_committer_but_not_an_established_one() {
        if !git_available() {
            return;
        }
        let repo = std::env::temp_dir().join(format!("noeta_authorship_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .unwrap();
        // History: alice, alice (tag v1), then bob (tag v2).
        commit_as(&repo, "a.noe", "Alice", "alice@example.com");
        commit_as(&repo, "b.noe", "Alice", "alice@example.com");
        let v1 = super::run_git(["-C", repo.to_str().unwrap(), "rev-parse", "HEAD"]).unwrap();
        let v1 = v1.trim();
        commit_as(&repo, "c.noe", "Bob", "bob@example.com");
        let v2 = super::run_git(["-C", repo.to_str().unwrap(), "rev-parse", "HEAD"]).unwrap();
        let v2 = v2.trim();
        let url = repo.to_str().unwrap();

        // The v2 release commit is authored by a first-time committer of an established repo.
        let up = authorship(url, v2, Some(v1)).unwrap();
        assert!(up.author.contains("Bob"), "{:?}", up);
        assert!(up.author_first_seen, "bob is new: {up:?}");
        assert_eq!(up.prior_authors, 1, "alice preceded him: {up:?}");
        assert!(up.new_since.iter().any(|a| a.contains("Bob")), "{up:?}");
        assert!(up.is_noteworthy(), "{up:?}");

        // v1's author (alice) is established — not noteworthy.
        let est = authorship(url, v1, None).unwrap();
        assert!(est.author.contains("Alice"));
        assert!(!est.author_first_seen, "alice recurs: {est:?}");
        assert!(!est.is_noteworthy(), "{est:?}");
    }

    #[test]
    fn repo_and_commit_links_normalize_the_common_url_forms() {
        assert_eq!(
            repo_web_url("https://github.com/acme/http.git").as_deref(),
            Some("https://github.com/acme/http")
        );
        assert_eq!(
            repo_web_url("git@github.com:acme/http.git").as_deref(),
            Some("https://github.com/acme/http")
        );
        assert_eq!(
            repo_web_url("ssh://git@gitlab.com/acme/http").as_deref(),
            Some("https://gitlab.com/acme/http")
        );
        assert_eq!(
            repo_web_url("http://x/acme/http").as_deref(),
            Some("https://x/acme/http")
        );
        // A local path / file URL has no web home.
        assert_eq!(repo_web_url("/tmp/acme/http"), None);
        assert_eq!(repo_web_url("file:///tmp/acme/http"), None);
        assert_eq!(
            commit_web_url("https://github.com/acme/http", "abc123").as_deref(),
            Some("https://github.com/acme/http/commit/abc123")
        );
        assert_eq!(commit_web_url("/tmp/x", "abc"), None);
    }
}
