//! End-to-end tests for `noeta ide --vscode` (the VS Code extension installer). The same local
//! HTTP fixture pattern as the `upgrade` tests: a fake release serves the `.vsix` + `SHA256SUMS`
//! through the shared `NOETA_UPGRADE_DOWNLOAD_BASE` seam, and a fake editor script passed as
//! `--bin` records its argv — so the whole flow (resolve editor → download → verify → install →
//! clean up) runs offline against controlled data.

use crate::support::*;
use crate::upgrade::{serve_routes, sha256_hex};

/// The version this test build reports — `noeta ide` pins the `.vsix` to exactly this, so the
/// fixture routes are derived from it.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The fixture route set for the release matching this binary: the `.vsix` asset plus a
/// `SHA256SUMS` that also carries an unrelated tarball line (as the real release's does), with
/// the `.vsix` checksum optionally corrupted.
fn vsix_routes(vsix: &[u8], corrupt: bool) -> Vec<(String, Vec<u8>)> {
    let asset = format!("noeta-{CURRENT}.vsix");
    let checksum = if corrupt {
        "0".repeat(64)
    } else {
        sha256_hex(vsix)
    };
    let sums = format!(
        "{}  noeta-v{CURRENT}-x86_64-unknown-linux-gnu.tar.gz\n{checksum}  {asset}\n",
        "1".repeat(64)
    );
    vec![
        (format!("/dl/v{CURRENT}/{asset}"), vsix.to_vec()),
        (format!("/dl/v{CURRENT}/SHA256SUMS"), sums.into_bytes()),
    ]
}

/// A fake editor on disk: a shell script that writes its argv (one per line) to `<dir>/argv.txt`
/// and exits with `exit_code`. Returns `(script, argv_record)`.
#[cfg(unix)]
fn fake_editor(name: &str, exit_code: i32) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let record = dir.join("argv.txt");
    let script = dir.join("fake-code");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexit {exit_code}\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, record)
}

/// A `noeta ide --vscode` invocation against the fixture at `base`, with its own private cache
/// dir (the staged `.vsix` lands under `<cache>/ide/` — private so parallel tests can't collide
/// on the deterministic asset name). Returns the command and the staging path.
fn ide_cmd(name: &str, base: &str) -> (Command, PathBuf) {
    let cache = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}_cache"));
    let _ = std::fs::remove_dir_all(&cache);
    let staged = cache.join("ide").join(format!("noeta-{CURRENT}.vsix"));
    let mut cmd = lang();
    cmd.args(["ide", "--vscode"])
        .env("NOETA_CACHE_DIR", &cache)
        .env("NOETA_UPGRADE_DOWNLOAD_BASE", format!("{base}/dl"));
    (cmd, staged)
}

#[cfg(unix)]
#[test]
fn ide_vscode_downloads_verifies_and_installs() {
    let vsix = b"PK fake vsix bytes".to_vec();
    let base = serve_routes(vsix_routes(&vsix, false));
    let (editor, record) = fake_editor("ide_install", 0);
    let (mut cmd, staged) = ide_cmd("ide_install", &base);
    cmd.args(["--bin", editor.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "installed the Noeta VS Code extension v{CURRENT}"
        )));
    // The editor was invoked as `--install-extension <staged.vsix> --force`.
    let argv: Vec<String> = std::fs::read_to_string(&record)
        .expect("the fake editor ran")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        argv,
        vec![
            "--install-extension".to_string(),
            staged.display().to_string(),
            "--force".to_string(),
        ]
    );
    // The staged file held the verified bytes at install time and is cleaned up afterwards.
    assert!(
        !staged.exists(),
        "the staged .vsix must be removed after a successful install"
    );
}

#[cfg(unix)]
#[test]
fn ide_vscode_refuses_a_checksum_mismatch() {
    let vsix = b"evil vsix bytes".to_vec();
    let base = serve_routes(vsix_routes(&vsix, true));
    let (editor, record) = fake_editor("ide_mismatch", 0);
    let (mut cmd, staged) = ide_cmd("ide_mismatch", &base);
    cmd.args(["--bin", editor.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("checksum mismatch"));
    // Nothing was staged and the editor was never invoked.
    assert!(!staged.exists(), "unverified bytes must never be staged");
    assert!(!record.exists(), "the editor must not run on a mismatch");
}

#[cfg(unix)]
#[test]
fn ide_vscode_keeps_the_vsix_when_the_editor_fails() {
    let vsix = b"PK good vsix, bad editor".to_vec();
    let base = serve_routes(vsix_routes(&vsix, false));
    let (editor, _record) = fake_editor("ide_editor_fails", 3);
    let (mut cmd, staged) = ide_cmd("ide_editor_fails", &base);
    cmd.args(["--bin", editor.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("installing the extension failed")
                .and(predicate::str::contains(staged.display().to_string()))
                .and(predicate::str::contains("--install-extension")),
        );
    // The verified download is kept for a manual install.
    assert_eq!(std::fs::read(&staged).unwrap(), vsix);
}

#[test]
fn ide_without_a_flag_points_at_vscode() {
    lang()
        .arg("ide")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--vscode"));
}

#[test]
fn ide_errors_clearly_when_no_editor_is_found() {
    // An empty PATH: the resolver fires before any network traffic (dead-port seam proves it)
    // and names the candidates plus the override flag.
    let empty = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ide_empty_path");
    let _ = std::fs::create_dir_all(&empty);
    lang()
        .args(["ide", "--vscode"])
        .env("PATH", &empty)
        .env("NOETA_UPGRADE_DOWNLOAD_BASE", "http://127.0.0.1:1")
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("`code`")
                .and(predicate::str::contains("`codium`"))
                .and(predicate::str::contains("`code-insiders`"))
                .and(predicate::str::contains("--bin")),
        );
}

#[test]
fn ide_bin_requires_vscode() {
    // clap usage error: `--bin` only means something under an editor flag.
    lang().args(["ide", "--bin", "code"]).assert().code(2);
}
