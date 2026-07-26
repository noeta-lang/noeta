//! `noeta ide` — install matching editor tooling for this toolchain.
//!
//! `--vscode` is the stop-gap (and VSCodium/offline) install path for the Noeta VS Code extension
//! while the Visual Studio Marketplace listing is pending: every GitHub release already ships the
//! extension as a `noeta-<version>.vsix` asset (release.yml's `package-extension` job), so this
//! verb downloads it, verifies it against the release's `SHA256SUMS`, and hands it to the editor's
//! own `--install-extension`.
//!
//! **Version pinning is the point**: the extension is installed at the running binary's own
//! version (`v<CARGO_PKG_VERSION>` — the same tag/version lockstep `noeta upgrade` and release.yml
//! enforce), so the grammar and language-server integration always match the toolchain that serves
//! them. After a `noeta upgrade`, re-running `noeta ide --vscode` moves the extension in step.
//!
//! The download plumbing (host, checksum verification, test seams) is `noeta upgrade`'s — one
//! release artifact contract, one consumer implementation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::upgrade::{
    DEFAULT_DOWNLOAD_BASE, DOWNLOAD_BASE_ENV, cargo_installed, expected_checksum, fetch_bytes,
    http_client, sha256_hex,
};

/// The editor binaries probed on PATH, in order, when `--bin` is not given.
const EDITOR_CANDIDATES: [&str; 3] = ["code", "codium", "code-insiders"];
/// Where a non-release binary is pointed for a from-source extension install.
const EXTENSION_TREE: &str = "https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta";

/// `noeta ide [--vscode] [--bin <NAME|PATH>]` — install editor tooling matching this binary.
///
/// Exit codes follow the CLI convention: 0 success, 1 a failed run (download, checksum, or the
/// editor's install invocation), 2 a usage/config problem (no editor flag, no editor binary
/// found, a binary with no matching release).
pub(crate) fn cmd_ide(vscode: bool, bin: Option<&str>) -> ExitCode {
    if !vscode {
        // No editor selected: a short pointer, not an error dump. Structured as a flag per editor
        // so future editors (--zed, --neovim…) slot in beside --vscode.
        eprintln!(
            "noeta ide installs editor tooling matching this toolchain.\n\n\
             Usage: noeta ide --vscode [--bin <NAME|PATH>]\n\n\
             --vscode installs the Noeta VS Code extension at this binary's version (v{version})\n\
             from its GitHub release, into VS Code or VSCodium. More editors may follow.",
            version = env!("CARGO_PKG_VERSION")
        );
        return ExitCode::from(2);
    }
    let version = env!("CARGO_PKG_VERSION");
    // Only released binaries have a matching `v<version>` release to download the .vsix from. A
    // source build carries whatever version the workspace happens to be at (or a placeholder
    // 0.0.0), which no release asset corresponds to — same reasoning as `noeta upgrade`'s refusal
    // to touch what it didn't install.
    if version == "0.0.0" {
        eprintln!(
            "noeta: this noeta is a source build (version 0.0.0) with no matching GitHub release \
             — install the extension from the source tree instead: {EXTENSION_TREE}"
        );
        return ExitCode::from(2);
    }
    // A cargo-installed binary was built from source by cargo, not downloaded from a release —
    // same detection as `noeta upgrade` (the executable lives under cargo's bin directory).
    let exe = std::env::current_exe().unwrap_or_default();
    let resolved = exe.canonicalize().unwrap_or(exe);
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cargo_installed(&resolved, cargo_home.as_deref(), home.as_deref()) {
        eprintln!(
            "noeta: this noeta was installed by cargo, so there is no matching GitHub release to \
             download the extension from — install it from the source tree instead: \
             {EXTENSION_TREE}"
        );
        return ExitCode::from(2);
    }
    // Resolve the editor before any network traffic — a missing editor should fail fast.
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let editor = match resolve_editor(bin, &path_var) {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    let tag = release_tag(version);
    let asset = vsix_asset(version);
    println!("downloading the Noeta VS Code extension {tag} ({asset})");
    let vsix = match download_verified_vsix(&tag, &asset) {
        Ok(vsix) => vsix,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    // Stage the verified bytes where the editor can read them: the noeta cache when it resolves,
    // the system temp dir otherwise. Removed after a successful install; kept (and pointed at) on
    // an install failure so the user can run `--install-extension` by hand.
    let staging = staging_dir().join(&asset);
    if let Some(parent) = staging.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("noeta: cannot create `{}`: {err}", parent.display());
        return ExitCode::from(1);
    }
    if let Err(err) = std::fs::write(&staging, &vsix) {
        eprintln!("noeta: cannot write `{}`: {err}", staging.display());
        return ExitCode::from(1);
    }
    // `--force` makes re-running an in-place update (downgrade included) — the verb's contract is
    // "the installed extension matches this binary", not "only ever moves forward".
    let status = std::process::Command::new(&editor)
        .arg("--install-extension")
        .arg(&staging)
        .arg("--force")
        .status();
    let failure = match status {
        Ok(status) if status.success() => {
            let _ = std::fs::remove_file(&staging);
            println!(
                "installed the Noeta VS Code extension v{version} via `{}` — after a `noeta \
                 upgrade`, run `noeta ide --vscode` again to keep them in step",
                editor.display()
            );
            return ExitCode::SUCCESS;
        }
        Ok(status) => format!("`{}` exited with {status}", editor.display()),
        Err(err) => format!("cannot run `{}`: {err}", editor.display()),
    };
    eprintln!(
        "noeta: installing the extension failed: {failure}\nthe downloaded extension was kept at \
         `{path}` — install it manually with `{editor} --install-extension {path}`",
        path = staging.display(),
        editor = editor.display()
    );
    ExitCode::from(1)
}

/// The release tag this binary's extension ships under: `v<version>`, the same tag/version
/// lockstep release.yml guards for the binary itself.
fn release_tag(version: &str) -> String {
    format!("v{version}")
}

/// The extension asset's release name: `noeta-<X.Y.Z>.vsix` (no `v` — release.yml stamps
/// `${GITHUB_REF_NAME#v}` into the package version and names the file after it).
fn vsix_asset(version: &str) -> String {
    format!("noeta-{version}.vsix")
}

/// The editor binary to install with: an explicit `--bin` (a path, or a name looked up on PATH),
/// else the first of [`EDITOR_CANDIDATES`] found on PATH.
fn resolve_editor(bin: Option<&str>, path_var: &OsStr) -> Result<PathBuf, String> {
    match bin {
        Some(bin) => {
            // Anything with a separator is a path — taken as given, existence-checked. A bare
            // name goes through the same PATH lookup as the defaults.
            let given = Path::new(bin);
            if given.components().count() > 1 {
                if given.is_file() {
                    Ok(given.to_path_buf())
                } else {
                    Err(format!("`--bin {bin}` does not exist or is not a file"))
                }
            } else {
                find_on_path(bin, path_var).ok_or_else(|| format!("`--bin {bin}` is not on PATH"))
            }
        }
        None => EDITOR_CANDIDATES
            .iter()
            .find_map(|name| find_on_path(name, path_var))
            .ok_or_else(|| {
                format!(
                    "no VS Code-family editor found on PATH (looked for `{}`) — pass the editor \
                     binary explicitly with `--bin <name-or-path>`",
                    EDITOR_CANDIDATES.join("`, `")
                )
            }),
    }
}

/// The first directory on `path_var` holding an executable file named `name`.
fn find_on_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is a file this process could execute (on unix: any execute bit; elsewhere,
/// existence — the PATH entry is the executability claim).
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// Download `tag`'s `.vsix` plus the release's `SHA256SUMS` and verify the checksum — the same
/// hosts, seams, and verification as `noeta upgrade`'s binary download.
fn download_verified_vsix(tag: &str, asset: &str) -> Result<Vec<u8>, String> {
    let base =
        std::env::var(DOWNLOAD_BASE_ENV).unwrap_or_else(|_| DEFAULT_DOWNLOAD_BASE.to_string());
    let base = base.trim_end_matches('/');
    let client = http_client()?;
    let vsix = fetch_bytes(&client, &format!("{base}/{tag}/{asset}"))?;
    let sums = fetch_bytes(&client, &format!("{base}/{tag}/SHA256SUMS"))?;
    let sums = String::from_utf8(sums).map_err(|_| "SHA256SUMS is not UTF-8".to_string())?;
    let expected = expected_checksum(&sums, asset)
        .ok_or_else(|| format!("no checksum for {asset} in the release's SHA256SUMS"))?;
    let actual = sha256_hex(&vsix);
    if expected != actual {
        return Err(format!(
            "checksum mismatch for {asset} (expected {expected}, got {actual}) — refusing to \
             install"
        ));
    }
    Ok(vsix)
}

/// Where the downloaded `.vsix` is staged: `<cache>/ide/` when the noeta cache resolves, the
/// system temp dir otherwise. Both are fine to leave a file in on failure — the path is printed.
fn staging_dir() -> PathBuf {
    match noeta_cache::Cache::locate() {
        Some(root) => root.join("ide"),
        None => std::env::temp_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_naming_matches_the_release_contract() {
        // release.yml packages `noeta-${GITHUB_REF_NAME#v}.vsix` under tag `v<version>`.
        assert_eq!(release_tag("0.2.1"), "v0.2.1");
        assert_eq!(vsix_asset("0.2.1"), "noeta-0.2.1.vsix");
        // Prerelease versions keep their suffix in both (the extension is packaged for every tag).
        assert_eq!(release_tag("0.3.0-rc.1"), "v0.3.0-rc.1");
        assert_eq!(vsix_asset("0.3.0-rc.1"), "noeta-0.3.0-rc.1.vsix");
    }

    /// A temp dir holding the given file names, executably. Returns the dir.
    #[cfg(unix)]
    fn dir_with_executables(name: &str, files: &[&str]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("noeta_ide_unit_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            let path = dir.join(file);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[cfg(unix)]
    #[test]
    fn editor_resolution_walks_the_candidates_in_order() {
        // `codium` in the first dir, `code` in the second: the *candidate* order wins (`code`
        // first), not the PATH order of whichever happens to exist.
        let first = dir_with_executables("cand_first", &["codium"]);
        let second = dir_with_executables("cand_second", &["code"]);
        let path_var = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            resolve_editor(None, &path_var).unwrap(),
            second.join("code")
        );
        // Without `code`, the next candidate is found.
        let path_var = std::env::join_paths([&first]).unwrap();
        assert_eq!(
            resolve_editor(None, &path_var).unwrap(),
            first.join("codium")
        );
    }

    #[cfg(unix)]
    #[test]
    fn editor_resolution_errors_name_the_candidates_and_the_flag() {
        let empty = dir_with_executables("cand_empty", &[]);
        let path_var = std::env::join_paths([&empty]).unwrap();
        let err = resolve_editor(None, &path_var).unwrap_err();
        for expected in ["`code`", "`codium`", "`code-insiders`", "--bin"] {
            assert!(err.contains(expected), "missing {expected} in: {err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn editor_resolution_bin_override_takes_a_name_or_a_path() {
        let dir = dir_with_executables("bin_override", &["my-editor"]);
        let path_var = std::env::join_paths([&dir]).unwrap();
        // A bare name goes through PATH lookup.
        assert_eq!(
            resolve_editor(Some("my-editor"), &path_var).unwrap(),
            dir.join("my-editor")
        );
        assert!(
            resolve_editor(Some("absent-editor"), &path_var)
                .unwrap_err()
                .contains("is not on PATH")
        );
        // A path is taken as given (PATH not consulted), and must exist.
        let explicit = dir.join("my-editor");
        let empty_path = std::ffi::OsString::new();
        assert_eq!(
            resolve_editor(Some(explicit.to_str().unwrap()), &empty_path).unwrap(),
            explicit
        );
        assert!(
            resolve_editor(Some("/nonexistent/editor"), &empty_path)
                .unwrap_err()
                .contains("does not exist")
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_requires_the_execute_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = dir_with_executables("exec_bit", &["tool"]);
        let path_var = std::env::join_paths([&dir]).unwrap();
        assert!(find_on_path("tool", &path_var).is_some());
        std::fs::set_permissions(dir.join("tool"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(find_on_path("tool", &path_var).is_none());
    }
}
