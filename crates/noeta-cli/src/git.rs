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
    /// The commit SHA the tag resolved to — the store key and the lockfile pin.
    pub sha: String,
    /// The content hash of the checked-out tree (the lockfile's integrity value).
    pub content_hash: String,
    /// The on-disk tree in the store.
    pub path: PathBuf,
}

/// Fetch `url`@`tag` into `store`, returning the materialized tree. If the tag's commit is already
/// stored (keyed by SHA), no network clone happens — only the cheap `ls-remote` to learn the SHA.
pub fn fetch(url: &str, tag: &str, store: &Store) -> Result<Fetched, String> {
    let sha = ls_remote_tag(url, tag)?;
    let path = if store.contains(&sha) {
        store.path_for(&sha)
    } else {
        store
            .publish(&sha, |staging| clone_tag(url, tag, &sha, staging))
            .map_err(|err| format!("storing `{url}`@`{tag}`: {err}"))?
    };
    let content_hash =
        hash_tree(&path).map_err(|err| format!("hashing `{url}`@`{tag}`: {err}"))?;
    Ok(Fetched {
        sha,
        content_hash,
        path,
    })
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
        std::fs::write(repo.join("lib.noe"), "namespace lib.core;\npub fn v(): int { return 1; }\n").unwrap();
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
}
