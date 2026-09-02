//! Git dependency fetch — the real-IO seam, CLI-only, **outside** the
//! differential oracle (a network operation, done before compilation; the build then runs over the
//! materialized on-disk tree).
//!
//! A git dependency names a repository URL and a [`GitRef`] — a **tag** (the release model), a
//! **branch**, or the default-branch **HEAD** (a tag-free in-dev/bundled package). Fetch shells out
//! to the **system `git`** — dependency-light, Go-like, no libgit2/gix — so a consumer needs `git` on
//! `PATH` only when it actually pulls a git dependency (a pure-path / pure-`std` program needs
//! nothing). The ref is resolved to a **commit SHA** at the remote, the commit is checked out into
//! the [`Store`], and the clone's `HEAD` is verified against the resolved SHA (integrity: a moved ref
//! or a tampered remote is rejected). The SHA + content hash are what the lockfile pins, so a
//! later build reproduces exactly regardless of the ref kind; `noeta update` re-resolves a moving
//! branch/HEAD ref to its new tip.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::PmError;
use crate::manifest::GitRef;
use crate::store::{Store, hash_tree};

/// A materialized git dependency.
#[derive(Debug)]
pub struct Fetched {
    /// The commit SHA the tag resolved to — the store key and the lockfile pin.
    #[allow(dead_code)]
    pub sha: String,
    /// The content hash of the checked-out tree — the lockfile's integrity value.
    #[allow(dead_code)]
    pub content_hash: String,
    /// The on-disk tree in the store.
    pub path: PathBuf,
}

/// Fetch `url` at `git_ref` into `store`, returning the materialized tree. The ref is resolved to a
/// commit SHA at the remote (`ls-remote`), then materialized. If that SHA is already stored, no clone
/// happens — only the cheap `ls-remote`. Use [`fetch_pinned`] when the lockfile already records the
/// SHA (it skips even the `ls-remote`).
pub fn fetch(url: &str, git_ref: &GitRef, store: &Store) -> Result<Fetched, PmError> {
    let sha = ls_remote_ref(url, git_ref).map_err(PmError::Network)?;
    materialize_sha(url, git_ref, sha, store)
}

/// Fetch `url` at `git_ref` **pinned to a known commit `sha`** — the lockfile
/// path. If the SHA is already stored, this touches the network **not at all** (offline,
/// reproducible). If it isn't, the ref is cloned and its `HEAD` verified against `sha`; a mismatch
/// means the ref moved since the lock was written — a reproducibility check the user resolves with
/// `noeta update` (expected for a `branch`/`HEAD` ref, which tracks a moving tip).
pub fn fetch_pinned(
    url: &str,
    git_ref: &GitRef,
    sha: &str,
    store: &Store,
) -> Result<Fetched, PmError> {
    materialize_sha(url, git_ref, sha.to_string(), store)
}

/// Materialize a known `url`@`git_ref`→`sha` into the store (shared by [`fetch`] and [`fetch_pinned`]):
/// reuse the stored tree if present, else clone the ref and verify its `HEAD` equals `sha`.
fn materialize_sha(
    url: &str,
    git_ref: &GitRef,
    sha: String,
    store: &Store,
) -> Result<Fetched, PmError> {
    let path = if store.contains(&sha) {
        store.path_for(&sha)
    } else {
        // The publish's build step IS the network clone, so a failure here is the fetch failing
        // (a filesystem error inside the store rides along under the same message).
        store
            .publish(&sha, |staging| clone_ref(url, git_ref, &sha, staging))
            .map_err(|err| {
                PmError::Network(format!("storing `{url}`@`{}`: {err}", git_ref.describe()))
            })?
    };
    let content_hash = hash_tree(&path)
        .map_err(|err| PmError::Io(format!("hashing `{url}`@`{}`: {err}", git_ref.describe())))?;
    Ok(Fetched {
        sha,
        content_hash,
        path,
    })
}

/// Resolve `url`@`tag` to the commit SHA it currently points at, without cloning — used by
/// `noeta publish` to pin the SHA into the registry index at publish time
/// (a published release is always a tag).
pub fn resolve_tag_sha(url: &str, tag: &str) -> Result<String, PmError> {
    ls_remote_ref(url, &GitRef::Tag(tag.to_string())).map_err(PmError::Network)
}

/// Resolve `git_ref` to its commit SHA at the remote, without cloning (a lightweight network call).
/// A **tag** resolves via `refs/tags/<tag>` — for an *annotated* tag `ls-remote` prints both the tag
/// object and its peeled commit (`…^{}`), and the peeled commit (the one a checkout lands on) is
/// preferred. A **branch** resolves via `refs/heads/<branch>`, and **HEAD** via the symbolic `HEAD`
/// (the remote's default branch). A ref that resolves to nothing is an error.
fn ls_remote_ref(url: &str, git_ref: &GitRef) -> Result<String, String> {
    match git_ref {
        GitRef::Tag(tag) => {
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
            plain.ok_or_else(|| format!("`{url}` has no tag `{tag}`"))
        }
        GitRef::Branch(branch) => {
            let refspec = format!("refs/heads/{branch}");
            single_ref_sha(url, &refspec)?
                .ok_or_else(|| format!("`{url}` has no branch `{branch}`"))
        }
        GitRef::Head => single_ref_sha(url, "HEAD")?
            .ok_or_else(|| format!("`{url}` has no HEAD (is it an empty repository?)")),
    }
}

/// The single commit SHA `refspec` resolves to at `url`, or `None` if the remote lists no such ref.
fn single_ref_sha(url: &str, refspec: &str) -> Result<Option<String>, String> {
    let out = run_git(["ls-remote", url, refspec])?;
    Ok(out
        .lines()
        .find_map(|line| line.split_once('\t').map(|(sha, _)| sha.to_string())))
}

/// Shallow-clone `url` at `git_ref` into `staging`, verify the checked-out `HEAD` equals
/// `expected_sha`, then drop the `.git` metadata (the package *is* its working tree). Runs inside the
/// store's atomic publish, so any error here leaves no partial tree. A tag/branch clones with
/// `--branch <name>` (git accepts either for that flag); a bare `HEAD` clones the default branch.
fn clone_ref(url: &str, git_ref: &GitRef, expected_sha: &str, staging: &Path) -> io::Result<()> {
    let dir = staging
        .to_str()
        .ok_or_else(|| io::Error::other("store path is not valid UTF-8"))?;
    match git_ref {
        GitRef::Tag(name) | GitRef::Branch(name) => {
            run_git(["clone", "--depth", "1", "--branch", name, url, dir])
                .map_err(io::Error::other)?;
        }
        GitRef::Head => {
            run_git(["clone", "--depth", "1", url, dir]).map_err(io::Error::other)?;
        }
    }
    let head = run_git(["-C", dir, "rev-parse", "HEAD"]).map_err(io::Error::other)?;
    let head = head.trim();
    if head != expected_sha {
        return Err(io::Error::other(format!(
            "integrity check failed: `{url}`@`{}` resolved to {expected_sha} but the clone's \
             HEAD is {head} (the ref may have moved)",
            git_ref.describe()
        )));
    }
    let _ = std::fs::remove_dir_all(staging.join(".git"));
    Ok(())
}

/// The committers a release introduces (namespace-protection, committer signal) — a **soft** anomaly
/// signal (git author/committer fields are self-set and thus forgeable), meant to prompt a human to
/// look, not to gate. A release spans a *range* of commits, so this reports every committer new to the
/// repo across that whole range — not just the tip commit's author. Computed by [`authorship`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorship {
    /// Distinct committers (as `Name <email>`) who appear in the release's commit range but nowhere
    /// in the repo's history before it — the new maintainers to review. Empty when there are none, or
    /// when no baseline release exists to compare against.
    pub new_committers: Vec<String>,
}

impl Authorship {
    /// Whether this is worth warning about — the release brought in at least one new committer.
    pub fn is_noteworthy(&self) -> bool {
        !self.new_committers.is_empty()
    }
}

/// Analyze the committers a release (`sha`) introduces in `url`'s repo (namespace-protection, committer
/// signal). The release's commit *range* is `baseline..sha`, where `baseline` is `since` when given
/// (an upgrade: the previously-locked commit), else the **previous tag** reachable from `sha` (a fresh
/// add: the prior release). Does a **blobless** clone (`--filter=blob:none`, no working tree) — the
/// commit graph without file contents, a network op run only for `noeta update`/`add` on the deps that
/// changed, never on the resolve hot path. Best-effort: an `Err` means "couldn't tell", so the caller
/// stays quiet rather than failing the command.
pub fn authorship(url: &str, sha: &str, since: Option<&str>) -> Result<Authorship, PmError> {
    let short = &sha[..sha.len().min(12)];
    // Pid **and** a per-process counter: two concurrent `authorship` calls in one process would
    // otherwise share a scratch clone whenever they were asked about the same commit, and the
    // `remove_dir_all` below would delete the other's clone out from under it.
    static SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "noeta-authorship-{}-{n}-{short}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let dir_str = dir
        .to_str()
        .ok_or_else(|| PmError::Io("temp path is not valid UTF-8".to_string()))?
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
    result.map_err(PmError::Network)
}

/// Compute the new committers from an already-cloned `git_dir` (split out so the git-plumbing is
/// testable against a local clone without the network).
fn authorship_from(git_dir: &str, sha: &str, since: Option<&str>) -> Result<Authorship, String> {
    let gd = format!("--git-dir={git_dir}");
    // The baseline that defines the release range: the caller's `since` (an upgrade's previous commit),
    // else the previous tag reachable from `sha`'s parent (the prior release). No baseline — a repo's
    // very first release — means there is nothing to compare against, so nobody is "new".
    let baseline = match since {
        Some(since) => Some(since.to_string()),
        None => run_git([&gd, "describe", "--tags", "--abbrev=0", &format!("{sha}^")])
            .ok()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty()),
    };
    let Some(baseline) = baseline else {
        return Ok(Authorship {
            new_committers: Vec::new(),
        });
    };

    // Author emails already present before the baseline, and the authors across the release range.
    // Both best-effort: an unrelated baseline (force-push / wrong repo) yields an empty range, so we
    // simply report nobody rather than failing.
    let before: std::collections::BTreeSet<String> =
        run_git([&gd, "log", "--format=%ae", &baseline])
            .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_default();
    let range = run_git([
        &gd,
        "log",
        "--format=%an <%ae>%x1f%ae",
        &format!("{baseline}..{sha}"),
    ])
    .unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    let mut new_committers = Vec::new();
    for line in range.lines() {
        if let Some((display, email)) = line.split_once('\u{1f}') {
            let email = email.trim();
            if !before.contains(email) && seen.insert(email.to_string()) {
                new_committers.push(display.trim().to_string());
            }
        }
    }
    Ok(Authorship { new_committers })
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

/// A browsable link to the **tag** `tag` in `url`'s repo (`<repo>/releases/tag/<tag>`, the
/// GitHub/Gitea release-page shape), or `None` when the repo isn't web-browsable.
///
/// The tag twin of [`commit_web_url`], and it points at the *release page* rather than the ref
/// because that is what a publisher wants to look at after publishing: the notes, the assets, and
/// whether the tag is the one they meant. A publish prints this beside the commit link, so the two
/// questions a fresh release raises — what content went out, and what does it look like to a
/// consumer — each have an answer to click.
pub fn tag_web_url(url: &str, tag: &str) -> Option<String> {
    repo_web_url(url).map(|repo| format!("{repo}/releases/tag/{tag}"))
}

/// Run `git` with `args`, returning trimmed stdout on success or a message built from stderr. A
/// failure to even spawn `git` (not installed) is reported as such.
fn run_git<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    // Delegates to the git_auth choke point (one runner, one credential-injection path).
    crate::git_auth::run_git(args)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::test_temp::TempDir;

    /// Whether `git` is available — the fetch tests are meaningless without it and skip gracefully
    /// (though this project is itself a git repo, so it's essentially always present).
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn git(args: &[&str], cwd: &Path) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        // The stderr is the whole diagnosis when setup fails (a bare "git [...] failed" once hid a
        // fixture-path collision for a long time), so carry it into the panic.
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Build a throwaway local git repo with a tagged commit in its own fixture directory, returning
    /// the guard (its path is usable as a `file` URL — git accepts local paths). Keep the guard
    /// alive for the whole test: dropping it deletes the repo.
    fn tagged_repo(name: &str, tag: &str) -> TempDir {
        let repo = TempDir::new(name);
        git(&["init", "-q"], repo.path());
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
        git(&["add", "."], repo.path());
        git(&["commit", "-q", "-m", "release"], repo.path());
        git(&["tag", tag], repo.path());
        repo
    }

    #[test]
    fn fetches_a_tag_into_the_store_and_verifies_head() {
        if !git_available() {
            return;
        }
        let repo = tagged_repo("fetch", "v1.0.0");
        let store_dir = TempDir::new("fetch-store");
        let store = Store::open_at(store_dir.path()).unwrap();

        let tag = GitRef::Tag("v1.0.0".to_string());
        let fetched = fetch(repo.path().to_str().unwrap(), &tag, &store).unwrap();
        assert_eq!(fetched.sha.len(), 40, "a full commit SHA");
        assert!(store.contains(&fetched.sha));
        // The working tree is materialized; `.git` is stripped.
        assert!(fetched.path.join("lib.noe").is_file());
        assert!(!fetched.path.join(".git").exists());
        assert!(!fetched.content_hash.is_empty());

        // A second fetch is served from the store (idempotent) and agrees on the SHA + hash.
        let again = fetch(repo.path().to_str().unwrap(), &tag, &store).unwrap();
        assert_eq!(again.sha, fetched.sha);
        assert_eq!(again.content_hash, fetched.content_hash);
    }

    #[test]
    fn a_missing_tag_is_an_error() {
        if !git_available() {
            return;
        }
        let repo = tagged_repo("missingtag", "v1.0.0");
        let store_dir = TempDir::new("missingtag-store");
        let store = Store::open_at(store_dir.path()).unwrap();
        let err = fetch(
            repo.path().to_str().unwrap(),
            &GitRef::Tag("v9.9.9".to_string()),
            &store,
        )
        .unwrap_err();
        assert!(err.message().contains("no tag"), "got: {err}");
    }

    /// Fetch an **untagged** repo by its default-branch HEAD, and by a named branch — the tag-free
    /// in-dev/bundled case. Both resolve to the same commit and materialize the working tree.
    #[test]
    fn fetches_head_and_a_branch_without_a_tag() {
        if !git_available() {
            return;
        }
        // A repo with a commit but no tag; force a known default branch name for the branch case.
        let repo = TempDir::new("head");
        git(&["init", "-q", "-b", "trunk"], repo.path());
        std::fs::write(
            repo.join("noeta.toml"),
            "[package]\nname = \"acme/lib\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        std::fs::write(repo.join("lib.noe"), "namespace lib.core;\n").unwrap();
        git(&["add", "."], repo.path());
        git(&["commit", "-q", "-m", "wip"], repo.path());
        let url = repo.path().to_str().unwrap();
        let expected = super::run_git(["-C", url, "rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let store_dir = TempDir::new("head-store");
        let store = Store::open_at(store_dir.path()).unwrap();
        let head = fetch(url, &GitRef::Head, &store).unwrap();
        assert_eq!(head.sha, expected, "HEAD resolves to the tip commit");
        assert!(head.path.join("lib.noe").is_file());

        let branch = fetch(url, &GitRef::Branch("trunk".to_string()), &store).unwrap();
        assert_eq!(
            branch.sha, expected,
            "the named branch resolves to the same tip"
        );

        // A branch that does not exist is a clean error.
        let err = fetch(url, &GitRef::Branch("nope".to_string()), &store).unwrap_err();
        assert!(err.message().contains("no branch"), "got: {err}");
    }
    /// Commit `file` (created with unique content) authored by `name <email>`.
    fn commit_as(repo: &Path, file: &str, name: &str, email: &str) {
        std::fs::write(repo.join(file), format!("// {file}\n")).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_COMMITTER_EMAIL", email)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["add", "."]);
        run(&["commit", "-q", "-m", file]);
    }

    #[test]
    fn authorship_reports_new_committers_across_a_release_range() {
        if !git_available() {
            return;
        }
        let fixture = TempDir::new("authorship");
        let repo = fixture.path();
        git(&["init", "-q"], repo);
        let url = repo.to_str().unwrap();
        let head = || {
            super::run_git(["-C", url, "rev-parse", "HEAD"])
                .unwrap()
                .trim()
                .to_string()
        };
        // History: Alice (v0.9.0), then two more commits — one by Alice, one by first-timer Bob —
        // making up the v1.0.0 release; the release *range* spans both, not just the tip.
        commit_as(repo, "a.noe", "Alice", "alice@example.com");
        let v0 = head();
        git(&["tag", "v0.9.0"], repo);
        commit_as(repo, "b.noe", "Alice", "alice@example.com");
        commit_as(repo, "c.noe", "Bob", "bob@example.com");
        let v1 = head();
        git(&["tag", "v1.0.0"], repo);

        // Explicit baseline (an upgrade from v0.9.0): Bob is new across v0..v1; Alice is established.
        let up = authorship(url, &v1, Some(&v0)).unwrap();
        assert_eq!(up.new_committers.len(), 1, "only Bob is new: {up:?}");
        assert!(up.new_committers[0].contains("Bob"), "{up:?}");
        assert!(up.is_noteworthy(), "{up:?}");

        // No baseline given (a fresh add): the previous tag v0.9.0 is discovered and used — same range,
        // same result. This is what proves we span the release, not just look at the tip commit.
        let added = authorship(url, &v1, None).unwrap();
        assert!(
            added.new_committers.iter().any(|a| a.contains("Bob")),
            "{added:?}"
        );
        assert!(added.is_noteworthy(), "{added:?}");

        // A release entirely by the established maintainer introduces nobody new.
        let est = authorship(url, &v0, Some(&v0)).unwrap();
        assert!(est.new_committers.is_empty(), "{est:?}");
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
        // The tag twin, including the `git@` form a publisher's remote is usually written as, and
        // a tag whose name carries the `v` prefix most release tags do.
        assert_eq!(
            tag_web_url("https://github.com/acme/http", "v1.2.0").as_deref(),
            Some("https://github.com/acme/http/releases/tag/v1.2.0")
        );
        assert_eq!(
            tag_web_url("git@github.com:acme/http.git", "v1.2.0").as_deref(),
            Some("https://github.com/acme/http/releases/tag/v1.2.0")
        );
        // Nothing to open for a source with no web form — the publish line simply omits the link.
        assert_eq!(tag_web_url("/tmp/x", "v1.2.0"), None);
    }
}
