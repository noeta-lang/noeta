//! Optional git credential injection. Private repos in a `github:<org>`
//! registry need authentication for **both** halves that shell out to git — version discovery
//! ([`crate::git_forge`]) and materialization ([`crate::git`]). Two mechanisms, in order:
//!
//! 1. **Ambient git credentials (the default).** With no configuration here, git uses whatever the
//!    user already has — a credential helper, `gh auth login`, `~/.git-credentials`, or SSH. A laptop
//!    that can already `git clone` a private repo needs nothing.
//! 2. **`NOETA_GITHUB_TOKEN` (the CI override).** When set, every git subprocess this crate spawns is
//!    given an HTTP `Authorization: Basic` header for github.com via a per-command `-c` config — so a
//!    CI job with only a token (no credential helper) resolves private repos too. The token is passed
//!    per-invocation (`git -c http.<host>.extraHeader=…`), never written to a repo config or a
//!    lockfile.
//!
//! Applied at the two git choke points ([`crate::git::run_git`] and [`crate::git_forge`]'s runner), so
//! it also authenticates a plain private `git = { url = "https://github.com/…" }` dependency.

/// The extra `-c <key>=<value>` args to prepend to a git invocation for token auth — empty when no
/// `NOETA_GITHUB_TOKEN` is set (git then falls back to ambient credentials). `NOETA_GITHUB_AUTH_HOST`
/// overrides the host the header is scoped to (default `https://github.com`).
pub fn git_auth_args() -> Vec<String> {
    let token = std::env::var("NOETA_GITHUB_TOKEN").ok();
    let host = std::env::var("NOETA_GITHUB_AUTH_HOST")
        .unwrap_or_else(|_| "https://github.com".to_string());
    auth_args_from(token.as_deref(), &host)
}

/// Pure core of [`git_auth_args`]: the `-c` args for `token` scoped to `host`, or empty for no/blank
/// token. The header is `Authorization: Basic base64("x-access-token:<token>")` — GitHub's documented
/// way to present a token over HTTPS — scoped so git only sends it to that host.
fn auth_args_from(token: Option<&str>, host: &str) -> Vec<String> {
    match token {
        Some(t) if !t.is_empty() => {
            let basic = base64(format!("x-access-token:{t}").as_bytes());
            vec![
                "-c".to_string(),
                format!("http.{host}.extraHeader=Authorization: Basic {basic}"),
            ]
        }
        _ => Vec::new(),
    }
}

/// Minimal standard base64 (no external dep) for the Basic-auth header.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Run `git` with the crate's auth config prepended — THE subprocess choke point (this module's
/// header calls out that credential injection must flow through every git invocation; one runner
/// keeps that true by construction, where two copies drifted on error detail).
pub(crate) fn run_git<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    let args: Vec<&str> = args.into_iter().collect();
    let auth = git_auth_args();
    let output = std::process::Command::new("git")
        .args(auth.iter().map(String::as_str))
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

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn no_token_means_no_args_so_git_uses_ambient_credentials() {
        assert!(auth_args_from(None, "https://github.com").is_empty());
        assert!(auth_args_from(Some(""), "https://github.com").is_empty());
    }

    #[test]
    fn a_token_becomes_a_scoped_basic_auth_header() {
        let args = auth_args_from(Some("ghp_secret"), "https://github.com");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "-c");
        // Scoped to the host, and the header is Basic base64("x-access-token:ghp_secret").
        let expected = base64(b"x-access-token:ghp_secret");
        assert_eq!(
            args[1],
            format!("http.https://github.com.extraHeader=Authorization: Basic {expected}")
        );
    }
}
