//! End-to-end tests for `noeta upgrade` (toolchain self-update). A minimal local HTTP fixture
//! server plays GitHub: it serves a fake `releases/latest` JSON plus release-shaped assets
//! (`noeta-<tag>-<target>.tar.gz` + `SHA256SUMS`), and the `NOETA_UPGRADE_API_BASE` /
//! `NOETA_UPGRADE_DOWNLOAD_BASE` test seams point the verb at it. The replace path always runs
//! against a **copy** of the built `noeta` binary in a temp dir (so `current_exe()` is the copy),
//! never the real test binary.

use std::io::{BufRead as _, BufReader, Write as _};

use crate::support::*;

/// The version this test build reports (`noeta-cli`'s own version — the tests compile in the same
/// crate, so it matches the `noeta` binary's `CARGO_PKG_VERSION` exactly).
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The release target triple of the machine running the tests — the same compile-time detection
/// the verb uses. `None` skips the download-path tests (no release artifact shape to fake).
fn host_target() -> Option<&'static str> {
    if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

/// Spawn a tiny HTTP/1.1 fixture server serving the given `(path, body)` routes (anything else
/// 404s) and return its base URL. The thread serves until the test process exits. Shared with the
/// `ide` tests — `noeta ide` consumes the same release-asset contract through the same seams.
pub(crate) fn serve_routes(routes: Vec<(String, Vec<u8>)>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            // Drain the headers (requests here are bodyless GETs).
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
            }
            let response = match routes.iter().find(|(route, _)| *route == path) {
                Some((_, body)) => {
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(body);
                    response
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

/// A release-shaped `.tar.gz` holding `<stem>/noeta` with the given bytes.
fn release_tarball(stem: &str, binary: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, format!("{stem}/noeta"), binary)
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The full fixture route set for one release: `releases/latest` JSON + tarball + SHA256SUMS
/// (checksum optionally corrupted for the mismatch test).
fn release_routes(tag: &str, target: &str, binary: &[u8], corrupt: bool) -> Vec<(String, Vec<u8>)> {
    let stem = format!("noeta-{tag}-{target}");
    let tarball = release_tarball(&stem, binary);
    let checksum = if corrupt {
        "0".repeat(64)
    } else {
        sha256_hex(&tarball)
    };
    let sums = format!("{checksum}  {stem}.tar.gz\n");
    vec![
        (
            "/api/repos/noeta-lang/noeta/releases/latest".to_string(),
            format!("{{\"tag_name\": \"{tag}\"}}").into_bytes(),
        ),
        (format!("/dl/{tag}/{stem}.tar.gz"), tarball),
        (format!("/dl/{tag}/SHA256SUMS"), sums.into_bytes()),
    ]
}

/// A `noeta upgrade` invocation against the fixture at `base`, run from the binary at `bin`
/// (defaults to the built test binary when `None`).
fn upgrade_cmd(bin: Option<&std::path::Path>, base: &str) -> Command {
    let mut cmd = match bin {
        Some(bin) => Command::new(bin),
        None => lang(),
    };
    cmd.arg("upgrade")
        .env("NOETA_UPGRADE_API_BASE", format!("{base}/api"))
        .env("NOETA_UPGRADE_DOWNLOAD_BASE", format!("{base}/dl"));
    cmd
}

/// Copy the built `noeta` into its own temp dir so the upgrade replaces the copy, never the
/// binary the rest of the suite runs.
fn temp_noeta_copy(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = assert_cmd::cargo::cargo_bin("noeta");
    let copy = dir.join("noeta");
    std::fs::copy(&source, &copy).expect("copy the noeta binary");
    wait_until_executable(&copy);
    copy
}

/// A just-written executable can be transiently **`ETXTBSY`**, and the cause is not this thread.
/// `fs::copy` holds the destination open for writing; a sibling test spawning a process in that
/// window forks, the child inherits the descriptor, and the file counts as open-for-write until
/// that child reaches its `exec`. The descriptor is `CLOEXEC`, so the window is short and closes
/// on its own — but `exec` during it fails, and under a loaded runner with tests in parallel it
/// does. Wait the window out rather than failing a run over it.
fn wait_until_executable(path: &std::path::Path) {
    /// `ETXTBSY` on both Linux and macOS. `io::ErrorKind::ExecutableFileBusy` is still unstable.
    const ETXTBSY: i32 = 26;
    for _ in 0..100 {
        match std::process::Command::new(path).arg("--version").output() {
            Ok(_) => return,
            Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("probing the copied binary at {}: {e}", path.display()),
        }
    }
    panic!(
        "{} stayed ETXTBSY for two seconds — that is not the fork window",
        path.display()
    );
}

#[test]
fn upgrade_is_a_noop_when_already_latest() {
    let base = serve_routes(vec![(
        "/api/repos/noeta-lang/noeta/releases/latest".to_string(),
        format!("{{\"tag_name\": \"v{CURRENT}\"}}").into_bytes(),
    )]);
    upgrade_cmd(None, &base)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "noeta v{CURRENT} is already the latest release"
        )));
}

#[test]
fn upgrade_check_exits_one_when_newer_and_zero_when_current() {
    // A newer release: report it and exit 1 (documented — scripts gate on the code).
    let newer = serve_routes(vec![(
        "/api/repos/noeta-lang/noeta/releases/latest".to_string(),
        b"{\"tag_name\": \"v99.9.9\"}".to_vec(),
    )]);
    upgrade_cmd(None, &newer)
        .arg("--check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains(format!(
            "noeta v{CURRENT} \u{2192} v99.9.9 is available"
        )));
    // Already current: exit 0, nothing to do.
    let current = serve_routes(vec![(
        "/api/repos/noeta-lang/noeta/releases/latest".to_string(),
        format!("{{\"tag_name\": \"v{CURRENT}\"}}").into_bytes(),
    )]);
    upgrade_cmd(None, &current)
        .arg("--check")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "noeta v{CURRENT} is up to date"
        )));
}

#[test]
fn upgrade_replaces_the_binary_and_verifies_the_checksum() {
    let Some(target) = host_target() else {
        eprintln!("skipping: no release target for this host");
        return;
    };
    let dummy = b"#!/bin/sh\necho fake-new-noeta\n".to_vec();
    let base = serve_routes(release_routes("v99.9.9", target, &dummy, false));
    let copy = temp_noeta_copy("upgrade_replace");
    upgrade_cmd(Some(&copy), &base)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "upgraded noeta v{CURRENT} \u{2192} v99.9.9"
        )));
    // The copy now holds the release's bytes, mode 755, with no staging file left behind.
    assert_eq!(std::fs::read(&copy).unwrap(), dummy);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&copy).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the replaced binary must be executable"
        );
    }
    assert!(
        !copy.with_file_name("noeta.new").exists(),
        "the staging file must be renamed away"
    );
}

#[test]
fn upgrade_refuses_a_checksum_mismatch() {
    let Some(target) = host_target() else {
        eprintln!("skipping: no release target for this host");
        return;
    };
    let dummy = b"evil bytes".to_vec();
    let base = serve_routes(release_routes("v99.9.9", target, &dummy, true));
    let copy = temp_noeta_copy("upgrade_checksum_mismatch");
    let before = std::fs::read(&copy).unwrap();
    upgrade_cmd(Some(&copy), &base)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("checksum mismatch"));
    // The binary is untouched.
    assert_eq!(std::fs::read(&copy).unwrap(), before);
}

#[test]
fn upgrade_installs_an_exact_older_version_as_a_downgrade() {
    let Some(target) = host_target() else {
        eprintln!("skipping: no release target for this host");
        return;
    };
    let dummy = b"old release bytes".to_vec();
    // `--version` skips the latest-release API entirely — only download routes are served.
    let mut routes = release_routes("v0.0.1", target, &dummy, false);
    routes.retain(|(path, _)| path.starts_with("/dl/"));
    let base = serve_routes(routes);
    let copy = temp_noeta_copy("upgrade_downgrade");
    upgrade_cmd(Some(&copy), &base)
        .args(["--version", "v0.0.1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "downgraded noeta v{CURRENT} \u{2192} v0.0.1"
        )));
    assert_eq!(std::fs::read(&copy).unwrap(), dummy);
}

#[test]
fn upgrade_refuses_a_prerelease_version() {
    // The guard fires before any network traffic: point both seams at a dead port to prove it.
    lang()
        .args(["upgrade", "--version", "v0.3.0-rc.1"])
        .env("NOETA_UPGRADE_API_BASE", "http://127.0.0.1:1")
        .env("NOETA_UPGRADE_DOWNLOAD_BASE", "http://127.0.0.1:1")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "prerelease builds are not installable via `noeta upgrade`",
        ));
}

#[test]
fn upgrade_rejects_a_malformed_version() {
    lang()
        .args(["upgrade", "--version", "banana"])
        .env("NOETA_UPGRADE_API_BASE", "http://127.0.0.1:1")
        .env("NOETA_UPGRADE_DOWNLOAD_BASE", "http://127.0.0.1:1")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "must be a release tag like `vX.Y.Z`",
        ));
}

#[test]
fn upgrade_check_conflicts_with_version() {
    // clap usage error: the two flags are mutually exclusive.
    lang()
        .args(["upgrade", "--check", "--version", "v1.0.0"])
        .assert()
        .code(2);
}
