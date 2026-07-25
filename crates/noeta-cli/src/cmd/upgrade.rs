//! `noeta upgrade` — self-update the installed toolchain binary from GitHub releases.
//!
//! Deliberately distinct from `noeta update`, which re-resolves a *project's dependencies*: this
//! verb replaces the running `noeta` executable itself (the `deno upgrade` / `rustup self update`
//! split). It consumes the release artifact contract that `.github/workflows/release.yml` produces
//! and the repo-root `install.sh` shares: per-tag assets `noeta-<tag>-<target>.tar.gz` (containing
//! a `noeta-<tag>-<target>/` directory with the binary) plus a `SHA256SUMS` file. Prereleases
//! (any `-` suffix in the tag, release.yml's own definition) are never installed: the GitHub
//! `releases/latest` endpoint already excludes them, and an explicit `--version` naming one is
//! refused outright.

use std::cmp::Ordering;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use semver::Version;
use sha2::{Digest, Sha256};

/// The GitHub repository the toolchain's releases are published under.
const REPO: &str = "noeta-lang/noeta";
/// Where an unsupported host is pointed instead.
const BUILD_FROM_SOURCE: &str = "https://github.com/noeta-lang/noeta#building-from-source";
/// The default GitHub API base (`releases/latest` resolution).
const DEFAULT_API_BASE: &str = "https://api.github.com";
/// The default release-asset download base (`<base>/<tag>/<file>`).
const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com/noeta-lang/noeta/releases/download";

// Test-only seams: the integration tests point these at a local fixture server so the whole flow
// (resolve → download → verify → replace) runs against controlled data. They are not user-facing
// configuration — the release artifact contract is GitHub's.
const API_BASE_ENV: &str = "NOETA_UPGRADE_API_BASE";
const DOWNLOAD_BASE_ENV: &str = "NOETA_UPGRADE_DOWNLOAD_BASE";

/// `noeta upgrade [--version vX.Y.Z] [--check]` — self-update the toolchain binary.
///
/// Exit codes follow the CLI convention: 0 success (or `--check` finding nothing newer), 1 a
/// failed run — and, documented for scripts, `--check` finding an upgrade available — 2 a
/// usage/config problem (prerelease or malformed `--version`, a cargo-installed binary,
/// an unsupported host, an unwritable install directory).
pub(crate) fn cmd_upgrade(version: Option<&str>, check: bool) -> ExitCode {
    let current = match Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("noeta: this binary carries an unparseable version: {err}");
            return ExitCode::from(1);
        }
    };
    // Resolve the tag to install: an explicit `--version` (validated locally, no network), or the
    // latest release from the GitHub API (which excludes prereleases by definition).
    let (tag, explicit) = match version {
        Some(requested) => {
            let tag = normalize_tag(requested);
            // Prereleases are never installable — same definition as release.yml: any `-` in the tag.
            if is_prerelease_tag(&tag) {
                eprintln!(
                    "noeta: prerelease builds are not installable via `noeta upgrade` (requested \
                     `{requested}`) — only proper releases (vX.Y.Z) can be installed"
                );
                return ExitCode::from(2);
            }
            if tag_version(&tag).is_none() {
                eprintln!(
                    "noeta: `--version` must be a release tag like `vX.Y.Z` (got `{requested}`)"
                );
                return ExitCode::from(2);
            }
            (tag, true)
        }
        None => match fetch_latest_tag() {
            Ok(tag) => (tag, false),
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        },
    };
    let Some(remote) = tag_version(&tag) else {
        eprintln!("noeta: the latest release reports an unparseable tag `{tag}`");
        return ExitCode::from(1);
    };
    // `--check`: report and change nothing. Exit 0 when current, 1 when an upgrade is available,
    // so scripts can gate on the code alone.
    if check {
        return if remote > current {
            println!("noeta v{current} → v{remote} is available (run `noeta upgrade`)");
            ExitCode::from(1)
        } else {
            println!("noeta v{current} is up to date (latest release: {tag})");
            ExitCode::SUCCESS
        };
    }
    // The default path never reinstalls or downgrades: at-or-above the latest release is a no-op.
    // An explicit `--version` is an instruction — install exactly that, downgrade included.
    if !explicit && remote <= current {
        println!("noeta v{current} is already the latest release");
        return ExitCode::SUCCESS;
    }
    let Some(target) = release_target() else {
        eprintln!(
            "noeta: no release binaries are published for this platform — build from source \
             instead: {BUILD_FROM_SOURCE}"
        );
        return ExitCode::from(2);
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("noeta: cannot locate the running executable: {err}");
            return ExitCode::from(1);
        }
    };
    // A cargo-installed binary belongs to cargo — replacing it behind cargo's back leaves its
    // registry state lying about what is on disk. Point at the cargo path instead.
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let resolved = exe.canonicalize().unwrap_or_else(|_| exe.clone());
    if cargo_installed(&resolved, cargo_home.as_deref(), home.as_deref()) {
        eprintln!(
            "noeta: this noeta was installed by cargo — upgrade it with `cargo install --path \
             crates/noeta-cli` (from a source checkout) or your original `cargo install` command"
        );
        return ExitCode::from(2);
    }
    let Some(dir) = exe.parent().map(Path::to_path_buf) else {
        eprintln!("noeta: the running executable has no parent directory");
        return ExitCode::from(1);
    };
    // Fail on an unwritable install dir before any network traffic — and never attempt privilege
    // escalation on its behalf.
    if let Err(err) = probe_writable(&dir) {
        eprintln!(
            "noeta: the install directory `{}` is not writable ({err}) — rerun with write access \
             to it",
            dir.display()
        );
        return ExitCode::from(2);
    }
    println!("downloading noeta {tag} for {target}");
    let binary = match download_verified_binary(&tag, target) {
        Ok(binary) => binary,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = replace_binary(&exe, &binary) {
        eprintln!("noeta: {err}");
        return ExitCode::from(1);
    }
    match remote.cmp(&current) {
        Ordering::Greater => println!("upgraded noeta v{current} → v{remote}"),
        Ordering::Less => println!("downgraded noeta v{current} → v{remote}"),
        Ordering::Equal => println!("reinstalled noeta v{remote}"),
    }
    ExitCode::SUCCESS
}

/// The release target triple this binary was compiled for — one of the four `release.yml` builds.
/// `None` for anything else (musl Linux, Windows, *BSD…), which builds from source instead.
/// Compile-time detection: the binary knows what it is; `uname` sniffing would misclassify e.g. a
/// musl build running on a glibc host.
fn release_target() -> Option<&'static str> {
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

/// `v1.2.3` (as release tags are) from user input that may omit the `v`.
fn normalize_tag(requested: &str) -> String {
    if requested.starts_with('v') {
        requested.to_string()
    } else {
        format!("v{requested}")
    }
}

/// Whether a tag names a prerelease — release.yml's own definition: any `-` suffix
/// (`v1.2.0-rc.1`, `v1.2.0-alpha`).
fn is_prerelease_tag(tag: &str) -> bool {
    tag.contains('-')
}

/// The SemVer version a release tag carries (`v1.2.3` → `1.2.3`); `None` when the tag doesn't
/// parse as one.
fn tag_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v')?).ok()
}

/// The release asset's distribution stem: the archive is `<stem>.tar.gz` and unpacks to a
/// `<stem>/` directory containing the `noeta` binary — the contract `release.yml` produces.
fn dist_stem(tag: &str, target: &str) -> String {
    format!("noeta-{tag}-{target}")
}

/// Whether the executable lives under cargo's bin directory (`$CARGO_HOME/bin`, or the default
/// `~/.cargo/bin`) — i.e. was `cargo install`ed and should be upgraded through cargo.
fn cargo_installed(exe: &Path, cargo_home: Option<&Path>, home: Option<&Path>) -> bool {
    let mut bins: Vec<PathBuf> = Vec::new();
    if let Some(cargo_home) = cargo_home {
        bins.push(cargo_home.join("bin"));
    }
    if let Some(home) = home {
        bins.push(home.join(".cargo").join("bin"));
    }
    bins.iter().any(|bin| exe.starts_with(bin))
}

/// The expected SHA-256 for `asset` out of a `SHA256SUMS` body: `<64-hex>  <name>` lines, with
/// the `*<name>` binary-mode variant some `sha256sum` invocations emit also accepted.
fn expected_checksum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, rest) = line
            .trim_end()
            .split_once(|c: char| c.is_ascii_whitespace())?;
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let name = rest.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// Lowercase hex of a byte string's SHA-256.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The blocking HTTP client all requests share: identified UA, bounded connect. No overall
/// timeout — the tarball download on a slow link may legitimately take a while; per-request
/// timeouts are set where bounded responses are expected.
fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("noeta/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| format!("cannot build the HTTP client: {err}"))
}

/// A `GITHUB_TOKEN`/`GH_TOKEN` from the environment, if present — attached to the API request to
/// lift its unauthenticated rate limit (CI). Never required.
fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|token| !token.is_empty()))
}

/// Resolve the latest release tag via the GitHub API (`releases/latest` excludes prereleases and
/// drafts by definition, which is exactly the "never install a prerelease" guarantee).
fn fetch_latest_tag() -> Result<String, String> {
    let base = std::env::var(API_BASE_ENV).unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
    let url = format!(
        "{}/repos/{REPO}/releases/latest",
        base.trim_end_matches('/')
    );
    let mut request = http_client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30));
    if let Some(token) = github_token() {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .map_err(|err| format!("cannot reach the release API at {url}: {err}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err("no releases found — has a release been published yet?".to_string());
    }
    if !status.is_success() {
        let hint = if status == reqwest::StatusCode::FORBIDDEN {
            " (rate-limited? set GITHUB_TOKEN)"
        } else {
            ""
        };
        return Err(format!("the release API returned {status}{hint}"));
    }
    let body = response
        .text()
        .map_err(|err| format!("cannot read the release API response: {err}"))?;
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = serde_json::from_str(&body)
        .map_err(|err| format!("unexpected release API response: {err}"))?;
    Ok(release.tag_name)
}

/// Download `tag`'s tarball for `target` plus the release's `SHA256SUMS`, verify the checksum,
/// and extract the `noeta` binary's bytes.
fn download_verified_binary(tag: &str, target: &str) -> Result<Vec<u8>, String> {
    let base =
        std::env::var(DOWNLOAD_BASE_ENV).unwrap_or_else(|_| DEFAULT_DOWNLOAD_BASE.to_string());
    let base = base.trim_end_matches('/');
    let stem = dist_stem(tag, target);
    let asset = format!("{stem}.tar.gz");
    let client = http_client()?;
    let tarball = fetch_bytes(&client, &format!("{base}/{tag}/{asset}"))?;
    let sums = fetch_bytes(&client, &format!("{base}/{tag}/SHA256SUMS"))?;
    let sums = String::from_utf8(sums).map_err(|_| "SHA256SUMS is not UTF-8".to_string())?;
    let expected = expected_checksum(&sums, &asset)
        .ok_or_else(|| format!("no checksum for {asset} in the release's SHA256SUMS"))?;
    let actual = sha256_hex(&tarball);
    if expected != actual {
        return Err(format!(
            "checksum mismatch for {asset} (expected {expected}, got {actual}) — refusing to \
             install"
        ));
    }
    extract_binary(&tarball, &stem)
}

/// GET a URL to bytes, treating any non-2xx as an error.
fn fetch_bytes(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|err| format!("download failed: {url}: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "download failed: {url} returned {status} (is this a released tag with binaries?)"
        ));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|err| format!("download failed: {url}: {err}"))
}

/// The `noeta` binary's bytes out of the release tarball (`<stem>/noeta` inside `<stem>.tar.gz`).
fn extract_binary(tar_gz: &[u8], stem: &str) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    let want: PathBuf = [stem, "noeta"].iter().collect();
    let entries = archive
        .entries()
        .map_err(|err| format!("cannot read the release tarball: {err}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|err| format!("cannot read the release tarball: {err}"))?;
        let path = entry
            .path()
            .map_err(|err| format!("cannot read the release tarball: {err}"))?;
        if path == want {
            let mut binary = Vec::new();
            entry
                .read_to_end(&mut binary)
                .map_err(|err| format!("cannot read the release tarball: {err}"))?;
            return Ok(binary);
        }
    }
    Err(format!(
        "unexpected archive layout: no {stem}/noeta inside the tarball"
    ))
}

/// Confirm `dir` is writable by creating and removing a probe file — the honest check (directory
/// permission bits under-report ACLs and read-only mounts).
fn probe_writable(dir: &Path) -> Result<(), std::io::Error> {
    let probe = dir.join(format!(".noeta-upgrade-probe-{}", std::process::id()));
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)
}

/// Atomically replace the running executable with `binary`: write `<exe>.new` beside it, set it
/// executable, and `rename` over the original — safe on unix while the process runs, since the
/// old inode stays alive for the running image and the path flips in one step.
fn replace_binary(exe: &Path, binary: &[u8]) -> Result<(), String> {
    let dir = exe
        .parent()
        .ok_or_else(|| "the running executable has no parent directory".to_string())?;
    let name = exe
        .file_name()
        .ok_or_else(|| "the running executable has no file name".to_string())?;
    let staging = dir.join(format!("{}.new", name.to_string_lossy()));
    std::fs::write(&staging, binary)
        .map_err(|err| format!("cannot write `{}`: {err}", staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("cannot mark `{}` executable: {err}", staging.display()))?;
    }
    std::fs::rename(&staging, exe).map_err(|err| {
        format!(
            "cannot replace `{}` with `{}`: {err}",
            exe.display(),
            staging.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_parsing_finds_the_asset() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  noeta-v1.0.0-x86_64-unknown-linux-gnu.tar.gz\n\
                    fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210  noeta-v1.0.0-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_checksum(sums, "noeta-v1.0.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
        );
        assert_eq!(expected_checksum(sums, "noeta-v1.0.0-other.tar.gz"), None);
    }

    #[test]
    fn sums_parsing_accepts_the_binary_mode_star_variant() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *noeta-v1.0.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            expected_checksum(sums, "noeta-v1.0.0-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn sums_parsing_rejects_malformed_lines() {
        // Too-short hash, non-hex hash, missing name: none should match.
        assert_eq!(expected_checksum("abc123  a.tar.gz\n", "a.tar.gz"), None);
        let non_hex = "z".repeat(64);
        assert_eq!(
            expected_checksum(&format!("{non_hex}  a.tar.gz\n"), "a.tar.gz"),
            None
        );
        assert_eq!(expected_checksum(&"a".repeat(64), "a.tar.gz"), None);
    }

    #[test]
    fn asset_naming_matches_the_release_contract() {
        assert_eq!(
            dist_stem("v0.2.0", "x86_64-unknown-linux-gnu"),
            "noeta-v0.2.0-x86_64-unknown-linux-gnu"
        );
        // The archive and the directory inside it share the stem.
        assert_eq!(
            format!("{}.tar.gz", dist_stem("v0.2.0", "aarch64-apple-darwin")),
            "noeta-v0.2.0-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn version_compare_orders_tags() {
        let current = Version::parse("0.2.0").unwrap();
        assert_eq!(
            tag_version("v0.2.0").unwrap().cmp(&current),
            Ordering::Equal
        );
        assert_eq!(
            tag_version("v0.3.0").unwrap().cmp(&current),
            Ordering::Greater
        );
        assert_eq!(tag_version("v0.1.9").unwrap().cmp(&current), Ordering::Less);
        // A prerelease of the same version orders BELOW the release, per SemVer.
        assert_eq!(
            tag_version("v0.2.0-rc.1").unwrap().cmp(&current),
            Ordering::Less
        );
        assert_eq!(tag_version("not-a-tag"), None);
        assert_eq!(tag_version("0.2.0"), None); // the `v` prefix is the tag contract
    }

    #[test]
    fn prerelease_tags_are_detected_like_release_yml() {
        // Same definition as release.yml: any `-` in the tag.
        assert!(is_prerelease_tag("v0.3.0-rc.1"));
        assert!(is_prerelease_tag("v1.2.0-alpha"));
        assert!(!is_prerelease_tag("v0.3.0"));
    }

    #[test]
    fn normalize_tag_accepts_both_forms() {
        assert_eq!(normalize_tag("v0.2.0"), "v0.2.0");
        assert_eq!(normalize_tag("0.2.0"), "v0.2.0");
    }

    #[test]
    fn cargo_install_paths_are_detected() {
        let cargo_home = PathBuf::from("/opt/cargo");
        let home = PathBuf::from("/home/dev");
        // Under $CARGO_HOME/bin.
        assert!(cargo_installed(
            Path::new("/opt/cargo/bin/noeta"),
            Some(&cargo_home),
            Some(&home)
        ));
        // Under ~/.cargo/bin even without CARGO_HOME set.
        assert!(cargo_installed(
            Path::new("/home/dev/.cargo/bin/noeta"),
            None,
            Some(&home)
        ));
        // An ordinary install location is fine.
        assert!(!cargo_installed(
            Path::new("/home/dev/.local/bin/noeta"),
            Some(&cargo_home),
            Some(&home)
        ));
        // Prefix matching is per-component: a sibling like `binx` must not match `bin`.
        assert!(!cargo_installed(
            Path::new("/opt/cargo/binx/noeta"),
            Some(&cargo_home),
            None
        ));
    }

    #[test]
    fn sha256_hex_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn extract_finds_the_binary_in_a_release_shaped_tarball() {
        let stem = "noeta-v9.9.9-x86_64-unknown-linux-gnu";
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let body = b"fake binary bytes";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{stem}/noeta"), &body[..])
            .unwrap();
        let tar_gz = builder.into_inner().unwrap().finish().unwrap();
        assert_eq!(extract_binary(&tar_gz, stem).unwrap(), body);
        // A tarball without the expected layout is an explicit error.
        let err = extract_binary(&tar_gz, "noeta-v0.0.0-other").unwrap_err();
        assert!(err.contains("unexpected archive layout"), "{err}");
    }
}
