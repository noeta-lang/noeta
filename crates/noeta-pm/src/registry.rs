//! The package **registry index** (package-manager P2.5).
//!
//! A registry is an *index*, not a code store (a locked user decision): it maps a package identity +
//! version to the **git coordinates** (URL + tag) where that release's source lives. Resolving a
//! registry dependency (`webclient = { version = "^1.2", package = "guzzle/http" }`) means asking the
//! index for the published versions of `guzzle/http`, picking the highest that satisfies the
//! requirement, and then fetching its git coordinates through the same path a direct `git` dependency
//! takes. So the registry adds a *name→coordinates* lookup in front of the existing git machinery; it
//! never hosts or serves source.
//!
//! [`Index`] is the contract. The real registry is an HTTP service hosted separately (Cloudflare
//! Workers + KV/D1) and is out of scope here; this module ships [`LocalIndex`], a file-backed index
//! used offline and by tests, plus [`resolve_coords`], the pure selection step. The `noeta publish`
//! verb writes a release into the configured index via [`Index::publish`].
//!
//! **Scope note (surfaced, not hidden).** Version *selection* here is per-dependency: [`resolve_coords`]
//! picks the highest published version matching one requirement. Joint constraint solving across
//! several registry dependencies that share a transitive package — PubGrub backtracking over real
//! version *ranges* — is wired-ready through [`crate::resolve`] but not yet driven from the registry
//! path (git/path pins are exact, so the graph walk selects greedily and reports a conflict rather
//! than backtracking). That depth arrives with the hosted registry, which serves per-version
//! dependency metadata cheaply enough to feed the resolver the whole candidate set.

use std::path::PathBuf;

use semver::{Version, VersionReq};

/// The git coordinates a registry release resolves to (package-manager P2.5). The **commit SHA** the
/// tag resolved to at publish time is pinned here too (Phase 4, S2): the index — not just the
/// lockfile — is authoritative on "this version = this commit", so a *first* registry resolve fetches
/// by the pinned SHA rather than trusting whatever the tag currently points at, and a moved tag is
/// caught against the index. Immutability keys on the whole coordinate, so a tag that moves to a new
/// SHA can't silently replace a published version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCoords {
    pub url: String,
    pub tag: String,
    pub sha: String,
}

/// The registry index contract: look up a package's published versions (each with its git
/// coordinates), and record a new release. Implemented by [`LocalIndex`] here; the hosted service
/// implements the same shape over HTTP.
pub trait Index {
    /// Every published `(version, git coordinates)` of `name` (`company/package`). An unknown package
    /// yields an empty list; an `Err` is a real lookup failure (a corrupt/unreadable index).
    fn versions(&self, name: &str) -> Result<Vec<(Version, GitCoords)>, String>;

    /// Record `name`@`version` → `coords` (the `noeta publish` write path). Re-publishing the same
    /// version with identical coordinates is idempotent; a *different* coordinate for an existing
    /// version is rejected (a published release is immutable).
    fn publish(&self, name: &str, version: &Version, coords: &GitCoords) -> Result<(), String>;
}

/// Resolve a registry requirement to a concrete release: the **highest published version** of `name`
/// satisfying `req`. Errors when the package is unknown or no published version matches (with the
/// available versions listed, matching the project's diagnostic bar).
pub fn resolve_coords(
    index: &dyn Index,
    name: &str,
    req: &VersionReq,
) -> Result<(Version, GitCoords), String> {
    let mut versions = index.versions(name)?;
    if versions.is_empty() {
        return Err(format!("registry has no package `{name}`"));
    }
    // Highest first, so the first match is the selection.
    versions.sort_by(|a, b| b.0.cmp(&a.0));
    versions
        .iter()
        .find(|(v, _)| req.matches(v))
        .cloned()
        .ok_or_else(|| {
            let available: Vec<String> = versions.iter().map(|(v, _)| v.to_string()).collect();
            format!(
                "no version of `{name}` matches `{req}` (published: {})",
                available.join(", ")
            )
        })
}

/// A file-backed [`Index`] (package-manager P2.5): one TOML file per package under a directory, used
/// offline and in tests. Located at `NOETA_REGISTRY_DIR` if set, else `<cache>/registry`. The hosted
/// registry replaces this with an HTTP client of the same [`Index`] shape.
#[derive(Debug)]
pub struct LocalIndex {
    dir: PathBuf,
}

impl LocalIndex {
    /// Open the configured local index, creating its directory. `NOETA_REGISTRY_DIR` overrides the
    /// default `<cache>/registry`. Errors when no location can be resolved or created.
    pub fn open() -> Result<LocalIndex, String> {
        let dir = match std::env::var_os("NOETA_REGISTRY_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => noeta_cache::Cache::locate()
                .ok_or_else(|| {
                    "cannot locate a registry directory (set NOETA_REGISTRY_DIR or HOME)"
                        .to_string()
                })?
                .join("registry"),
        };
        Self::open_at(dir)
    }

    /// Open an index at an explicit directory (tests / an override).
    pub fn open_at(dir: impl Into<PathBuf>) -> Result<LocalIndex, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("cannot create registry dir `{}`: {err}", dir.display()))?;
        Ok(LocalIndex { dir })
    }

    /// The index file for `name` — the `company/package` slash is flattened so it is one file, not a
    /// nested directory (`guzzle/http` → `guzzle__http.toml`).
    fn file_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.toml", name.replace('/', "__")))
    }
}

impl Index for LocalIndex {
    fn versions(&self, name: &str) -> Result<Vec<(Version, GitCoords)>, String> {
        let path = self.file_for(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new()); // unknown package
        };
        let table: toml::Table = text
            .parse()
            .map_err(|err| format!("corrupt registry entry `{}`: {err}", path.display()))?;
        let mut out = Vec::new();
        if let Some(entries) = table.get("version").and_then(|v| v.as_array()) {
            for entry in entries {
                let Some(t) = entry.as_table() else { continue };
                let get = |k: &str| t.get(k).and_then(|v| v.as_str());
                if let (Some(ver), Some(url), Some(tag), Some(sha)) =
                    (get("version"), get("url"), get("tag"), get("sha"))
                    && let Ok(version) = Version::parse(ver)
                {
                    out.push((
                        version,
                        GitCoords {
                            url: url.to_string(),
                            tag: tag.to_string(),
                            sha: sha.to_string(),
                        },
                    ));
                }
            }
        }
        Ok(out)
    }

    fn publish(&self, name: &str, version: &Version, coords: &GitCoords) -> Result<(), String> {
        let mut versions = self.versions(name)?;
        if let Some((_, existing)) = versions.iter().find(|(v, _)| v == version) {
            if existing == coords {
                return Ok(()); // idempotent re-publish
            }
            return Err(format!(
                "`{name}`@{version} is already published at {}#{} — a release is immutable",
                existing.url, existing.tag
            ));
        }
        versions.push((version.clone(), coords.clone()));
        versions.sort_by(|a, b| a.0.cmp(&b.0));

        let mut text = String::from("# noeta registry index entry (generated).\n");
        for (v, c) in &versions {
            text.push_str("\n[[version]]\n");
            text.push_str(&format!("version = {}\n", quote(&v.to_string())));
            text.push_str(&format!("url = {}\n", quote(&c.url)));
            text.push_str(&format!("tag = {}\n", quote(&c.tag)));
            text.push_str(&format!("sha = {}\n", quote(&c.sha)));
        }
        std::fs::write(self.file_for(name), text)
            .map_err(|err| format!("cannot write registry entry for `{name}`: {err}"))
    }
}

/// Minimal TOML basic-string quoting for the values we emit.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Open the registry index a resolve/publish should use (package-manager Phase 4, S4). With the
/// `registry-http` feature and `NOETA_REGISTRY_URL` set, this is the **networked** [`HttpIndex`]
/// (the hosted index); otherwise the file-backed [`LocalIndex`] (offline / tests). A future default
/// production URL flips the else-branch once the hosted registry is live.
pub fn open_default() -> Result<Box<dyn Index>, String> {
    #[cfg(feature = "registry-http")]
    if let Some(url) = std::env::var_os("NOETA_REGISTRY_URL") {
        let base = url
            .into_string()
            .map_err(|_| "NOETA_REGISTRY_URL is not valid UTF-8".to_string())?;
        return Ok(Box::new(HttpIndex::new(base)?));
    }
    Ok(Box::new(LocalIndex::open()?))
}

/// The networked registry index (package-manager Phase 4, S4): an HTTP client of the hosted index
/// (see the `noeta-registry` Worker + its `PROTOCOL.md`). Reads over `GET`, publishes over `POST`
/// with a bearer token (`NOETA_REGISTRY_TOKEN`). The registry serves only git *coordinates*, never
/// source, so a compromised index can at worst point at a different repo/tag — which the SHA pin and
/// the consumer's lockfile catch.
#[cfg(feature = "registry-http")]
pub struct HttpIndex {
    base: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
}

#[cfg(feature = "registry-http")]
impl std::fmt::Debug for HttpIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpIndex").field("base", &self.base).finish_non_exhaustive()
    }
}

#[cfg(feature = "registry-http")]
#[derive(serde::Deserialize)]
struct VersionsResponse {
    #[serde(default)]
    versions: Vec<WireVersion>,
}

#[cfg(feature = "registry-http")]
#[derive(serde::Deserialize)]
struct WireVersion {
    version: String,
    url: String,
    tag: String,
    sha: String,
    #[serde(default)]
    yanked: bool,
}

#[cfg(feature = "registry-http")]
impl HttpIndex {
    /// A client for the registry at `base` (e.g. `https://registry.noeta.dev`). The publish token,
    /// if any, comes from `NOETA_REGISTRY_TOKEN`.
    pub fn new(base: impl Into<String>) -> Result<HttpIndex, String> {
        let base = base.into().trim_end_matches('/').to_string();
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("noeta/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("cannot build the registry HTTP client: {err}"))?;
        Ok(HttpIndex {
            base,
            token: std::env::var("NOETA_REGISTRY_TOKEN").ok(),
            client,
        })
    }

    fn url_for(&self, name: &str) -> String {
        // `name` is `company/package`, which becomes the two path segments verbatim.
        format!("{}/v1/packages/{name}", self.base)
    }
}

#[cfg(feature = "registry-http")]
impl Index for HttpIndex {
    fn versions(&self, name: &str) -> Result<Vec<(Version, GitCoords)>, String> {
        let resp = self
            .client
            .get(self.url_for(name))
            .send()
            .map_err(|err| format!("registry request for `{name}` failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for `{name}`",
                resp.status()
            ));
        }
        let body: VersionsResponse = resp
            .json()
            .map_err(|err| format!("registry sent an unreadable response for `{name}`: {err}"))?;
        let mut out = Vec::new();
        for v in body.versions {
            // A yanked release is never *newly* selected (an existing lockfile pin bypasses the index
            // entirely, so it still resolves) — skip it from the candidate set.
            if v.yanked {
                continue;
            }
            let Ok(version) = Version::parse(&v.version) else {
                continue; // ignore an unparseable version rather than failing the whole resolve
            };
            out.push((
                version,
                GitCoords {
                    url: v.url,
                    tag: v.tag,
                    sha: v.sha,
                },
            ));
        }
        Ok(out)
    }

    fn publish(&self, name: &str, version: &Version, coords: &GitCoords) -> Result<(), String> {
        let token = self.token.as_ref().ok_or_else(|| {
            "publishing needs a token — set NOETA_REGISTRY_TOKEN to your registry publish token"
                .to_string()
        })?;
        let body = serde_json::json!({
            "version": version.to_string(),
            "url": coords.url,
            "tag": coords.tag,
            "sha": coords.sha,
        });
        let resp = self
            .client
            .post(self.url_for(name))
            .bearer_auth(token)
            .json(&body)
            .send()
            .map_err(|err| format!("publishing `{name}`@{version} failed: {err}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // Surface the server's error message when it sends one.
        let detail = resp
            .text()
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| status.to_string());
        Err(format!("registry rejected `{name}`@{version}: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(name: &str) -> LocalIndex {
        let dir = std::env::temp_dir().join(format!("noeta_registry_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        LocalIndex::open_at(dir).unwrap()
    }

    fn coords(tag: &str) -> GitCoords {
        GitCoords {
            url: "https://example.com/guzzle/http".to_string(),
            tag: tag.to_string(),
            sha: format!("{tag}-sha"),
        }
    }

    #[test]
    fn publish_then_resolve_picks_highest_match() {
        let index = mem("pick_highest");
        index
            .publish("guzzle/http", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        index
            .publish("guzzle/http", &Version::new(1, 4, 0), &coords("v1.4.0"))
            .unwrap();
        index
            .publish("guzzle/http", &Version::new(2, 0, 0), &coords("v2.0.0"))
            .unwrap();

        let (version, c) =
            resolve_coords(&index, "guzzle/http", &VersionReq::parse("^1.0").unwrap()).unwrap();
        assert_eq!(version, Version::new(1, 4, 0)); // highest in ^1
        assert_eq!(c.tag, "v1.4.0");
    }

    #[test]
    fn resolve_reports_no_match_and_unknown() {
        let index = mem("no_match");
        index
            .publish("guzzle/http", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        let err =
            resolve_coords(&index, "guzzle/http", &VersionReq::parse("^2").unwrap()).unwrap_err();
        assert!(err.contains("no version"), "{err}");
        let err = resolve_coords(&index, "who/dis", &VersionReq::parse("^1").unwrap()).unwrap_err();
        assert!(err.contains("no package"), "{err}");
    }

    #[test]
    fn republishing_a_version_with_new_coords_is_rejected() {
        let index = mem("republish");
        index
            .publish("a/b", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        // Same coords: idempotent.
        index
            .publish("a/b", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        // Different coords for a published version: rejected (immutable).
        let err = index
            .publish("a/b", &Version::new(1, 0, 0), &coords("v9.9.9"))
            .unwrap_err();
        assert!(err.contains("immutable"), "{err}");
    }
}

#[cfg(all(test, feature = "registry-http"))]
mod http_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A one-shot in-process HTTP/1.1 server: it handles connections on a background thread, calling
    /// `handler(method, path, body) -> (status, json)`. Returns the base URL. Hermetic — no network.
    fn mock_server(
        handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut content_length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    if header == "\r\n" || header == "\n" {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    reader.read_exact(&mut body).unwrap();
                }
                let (status, json) = handler(&method, &path, &String::from_utf8_lossy(&body));
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        ready_rx.recv().unwrap();
        format!("http://{addr}")
    }

    #[test]
    fn http_index_lists_versions_and_skips_yanked() {
        let base = mock_server(|method, path, _body| {
            assert_eq!(method, "GET");
            assert_eq!(path, "/v1/packages/acme/imgfx");
            (
                200,
                r#"{"name":"acme/imgfx","versions":[
                    {"version":"1.2.0","url":"https://x/acme/imgfx","tag":"v1.2.0","sha":"abc","yanked":false},
                    {"version":"2.0.0","url":"https://x/acme/imgfx","tag":"v2.0.0","sha":"def","yanked":true}
                ]}"#
                    .to_string(),
            )
        });
        let index = HttpIndex::new(base).unwrap();
        let versions = index.versions("acme/imgfx").unwrap();
        // 2.0.0 is yanked → not offered as a candidate; 1.2.0 carries its pinned SHA.
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].0, Version::new(1, 2, 0));
        assert_eq!(versions[0].1.sha, "abc");

        // resolve_coords picks it through the same trait the local index uses.
        let (v, c) = resolve_coords(&index, "acme/imgfx", &VersionReq::parse("^1.0").unwrap()).unwrap();
        assert_eq!(v, Version::new(1, 2, 0));
        assert_eq!(c.tag, "v1.2.0");
    }

    #[test]
    fn http_index_publishes_with_a_bearer_token() {
        let (tx, rx) = mpsc::channel();
        let base = mock_server(move |method, path, body| {
            tx.send((method.to_string(), path.to_string(), body.to_string())).unwrap();
            (201, r#"{"status":"published"}"#.to_string())
        });
        // Construct with an explicit token (avoids racing on a process-global env var).
        let index = HttpIndex {
            token: Some("secret-token".to_string()),
            ..HttpIndex::new(base).unwrap()
        };
        index
            .publish(
                "acme/imgfx",
                &Version::new(1, 0, 0),
                &GitCoords {
                    url: "https://x/acme/imgfx".to_string(),
                    tag: "v1.0.0".to_string(),
                    sha: "abc".to_string(),
                },
            )
            .unwrap();
        let (method, path, body) = rx.recv().unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/packages/acme/imgfx");
        assert!(body.contains("\"sha\":\"abc\""), "body: {body}");
        assert!(body.contains("\"version\":\"1.0.0\""), "body: {body}");
    }

    #[test]
    fn http_index_publish_without_token_errors() {
        let base = mock_server(|_, _, _| (201, "{}".to_string()));
        let index = HttpIndex {
            token: None,
            ..HttpIndex::new(base).unwrap()
        };
        let err = index
            .publish(
                "acme/imgfx",
                &Version::new(1, 0, 0),
                &GitCoords {
                    url: "u".to_string(),
                    tag: "t".to_string(),
                    sha: "s".to_string(),
                },
            )
            .unwrap_err();
        assert!(err.contains("NOETA_REGISTRY_TOKEN"), "{err}");
    }
}
