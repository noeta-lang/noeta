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

/// One registry dependency edge of a published release (package-manager Phase 4, S5): the depended-on
/// **package identity** and the SemVer requirement. Carried in the index so the PubGrub resolver sees
/// a package's dependencies without cloning it — the crates.io-index model that makes range
/// backtracking practical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub package: String,
    pub req: VersionReq,
}

/// A published release (package-manager Phase 4, S5): a version, its git [`GitCoords`], the registry
/// dependencies that version declares, and — when signed — the release's provenance. The `deps` let
/// the resolver backtrack over ranges; the provenance lets a consumer verify "version → commit"
/// independently of trusting the registry, under one of two trust roots (Phase 4 #2 / Phase 5):
/// a key-based `signature` **or** a keyless `bundle` — at most one (enforced at publish).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub coords: GitCoords,
    pub deps: Vec<Dep>,
    /// Hex Ed25519 signature over the attestation (key trust root), or `None`.
    pub signature: Option<String>,
    /// JSON Sigstore bundle over the same attestation (keyless trust root, Phase 5): a DSSE
    /// envelope + Fulcio certificate + Rekor inclusion proof, verified offline. Or `None`.
    pub bundle: Option<String>,
}

impl Release {
    /// A release carries **at most one** trust root: `signature` (key) or `bundle` (keyless), never
    /// both — two roots would make "which one did the consumer verify?" ambiguous and give a
    /// downgrade attack a second surface. Both `None` = unsigned (allowed, unverified).
    pub fn check_provenance_shape(&self) -> Result<(), String> {
        if self.signature.is_some() && self.bundle.is_some() {
            return Err(
                "a release carries either a key signature or a keyless bundle, not both"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// The registry index contract: look up a package's published releases (each with its git coordinates
/// **and dependencies**), record a new release, and serve a **scope's public key** (provenance,
/// Phase 4 #2). Implemented by [`LocalIndex`] here; the hosted service implements the same shape.
pub trait Index {
    /// Every published [`Release`] of `name` (`company/package`). An unknown package yields an empty
    /// list; an `Err` is a real lookup failure (a corrupt/unreadable index).
    fn releases(&self, name: &str) -> Result<Vec<Release>, String>;

    /// Record a `name` release (the `noeta publish` write path). Re-publishing the same version with
    /// identical coordinates + deps is idempotent; a *different* coordinate for an existing version is
    /// rejected (a published release is immutable).
    fn publish(&self, name: &str, release: &Release) -> Result<(), String>;

    /// The registered Ed25519 **public key** (hex) of `scope` (a `company`), for verifying that
    /// scope's release signatures. `None` if the scope registered no key. Default: `None` (an index
    /// that doesn't serve keys yet — a consumer then treats releases as unverified).
    fn scope_key(&self, _scope: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// Register a scope's public key. Default: a **no-op** — the hosted registry registers keys via
    /// its admin endpoint (scope ownership), so `noeta publish` doesn't self-register there. The
    /// local index overrides this to record the key, so an offline publish/verify round-trips.
    fn set_scope_key(&self, _scope: &str, _public_hex: &str) -> Result<(), String> {
        Ok(())
    }

    /// Store a release's **documentation artifact** — the `docs.json` the generator produces
    /// (`noeta doc --out`, docs-ingestion follow-up). Docs are *advisory metadata*, not
    /// provenance: they are unsigned, last-wins on re-put (a regenerated artifact for the same
    /// immutable release is fine), and a hosted registry may choose to regenerate them from
    /// source itself (the docs.rs model) rather than trust the upload. Default: an error — an
    /// index that does not store docs says so, and `noeta publish` degrades to a warning.
    fn put_docs(&self, _name: &str, _version: &Version, _docs_json: &str) -> Result<(), String> {
        Err("this registry does not store documentation".to_string())
    }

    /// The stored `docs.json` for `name@version`, if any. Default: `None`.
    fn docs(&self, _name: &str, _version: &Version) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// Resolve a registry requirement to a concrete release's coordinates: the **highest published
/// version** of `name` satisfying `req`. Errors when the package is unknown or no published version
/// matches. Used where a single dependency is materialized directly; the graph's PubGrub pass
/// (Phase 4, S5b) instead reads whole candidate sets through [`Index::releases`].
pub fn resolve_coords(
    index: &dyn Index,
    name: &str,
    req: &VersionReq,
) -> Result<(Version, GitCoords), String> {
    let mut releases = index.releases(name)?;
    if releases.is_empty() {
        return Err(format!("registry has no package `{name}`"));
    }
    // Highest first, so the first match is the selection.
    releases.sort_by(|a, b| b.version.cmp(&a.version));
    releases
        .iter()
        .find(|r| req.matches(&r.version))
        .map(|r| (r.version.clone(), r.coords.clone()))
        .ok_or_else(|| {
            let available: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
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

    /// The file holding a scope's registered public key (provenance, Phase 4 #2).
    fn scope_key_file(&self, scope: &str) -> PathBuf {
        self.dir.join(format!("scope__{scope}.pub"))
    }

    /// The file holding a release's documentation artifact (docs-ingestion follow-up).
    fn docs_file(&self, name: &str, version: &Version) -> PathBuf {
        self.dir
            .join(format!("docs__{}__{version}.json", name.replace('/', "__")))
    }
}

impl Index for LocalIndex {
    fn releases(&self, name: &str) -> Result<Vec<Release>, String> {
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
                let (Some(ver), Some(url), Some(tag), Some(sha)) =
                    (get("version"), get("url"), get("tag"), get("sha"))
                else {
                    continue;
                };
                let Ok(version) = Version::parse(ver) else {
                    continue;
                };
                let mut deps = Vec::new();
                if let Some(dep_arr) = t.get("deps").and_then(|v| v.as_array()) {
                    for d in dep_arr {
                        let Some(dt) = d.as_table() else { continue };
                        if let (Some(package), Some(req)) = (
                            dt.get("package").and_then(|v| v.as_str()),
                            dt.get("req").and_then(|v| v.as_str()),
                        ) && let Ok(req) = VersionReq::parse(req)
                        {
                            deps.push(Dep {
                                package: package.to_string(),
                                req,
                            });
                        }
                    }
                }
                out.push(Release {
                    version,
                    coords: GitCoords {
                        url: url.to_string(),
                        tag: tag.to_string(),
                        sha: sha.to_string(),
                    },
                    deps,
                    signature: get("sig").map(str::to_string),
                    bundle: get("bundle").map(str::to_string),
                });
            }
        }
        Ok(out)
    }

    fn scope_key(&self, scope: &str) -> Result<Option<String>, String> {
        match std::fs::read_to_string(self.scope_key_file(scope)) {
            Ok(text) => Ok(Some(text.trim().to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("cannot read scope key for `{scope}`: {err}")),
        }
    }

    fn set_scope_key(&self, scope: &str, public_hex: &str) -> Result<(), String> {
        std::fs::write(self.scope_key_file(scope), format!("{public_hex}\n"))
            .map_err(|err| format!("cannot write scope key for `{scope}`: {err}"))
    }

    fn publish(&self, name: &str, release: &Release) -> Result<(), String> {
        release.check_provenance_shape()?;
        let mut releases = self.releases(name)?;
        if let Some(existing) = releases.iter().find(|r| r.version == release.version) {
            if existing == release {
                return Ok(()); // idempotent re-publish
            }
            return Err(format!(
                "`{name}`@{} is already published at {}#{} — a release is immutable",
                release.version, existing.coords.url, existing.coords.tag
            ));
        }
        releases.push(release.clone());
        releases.sort_by(|a, b| a.version.cmp(&b.version));

        let mut text = String::from("# noeta registry index entry (generated).\n");
        for r in &releases {
            text.push_str("\n[[version]]\n");
            text.push_str(&format!("version = {}\n", quote(&r.version.to_string())));
            text.push_str(&format!("url = {}\n", quote(&r.coords.url)));
            text.push_str(&format!("tag = {}\n", quote(&r.coords.tag)));
            text.push_str(&format!("sha = {}\n", quote(&r.coords.sha)));
            if let Some(sig) = &r.signature {
                text.push_str(&format!("sig = {}\n", quote(sig)));
            }
            if let Some(bundle) = &r.bundle {
                text.push_str(&format!("bundle = {}\n", quote(bundle)));
            }
            for dep in &r.deps {
                text.push_str("\n[[version.deps]]\n");
                text.push_str(&format!("package = {}\n", quote(&dep.package)));
                text.push_str(&format!("req = {}\n", quote(&dep.req.to_string())));
            }
        }
        std::fs::write(self.file_for(name), text)
            .map_err(|err| format!("cannot write registry entry for `{name}`: {err}"))
    }
    fn put_docs(&self, name: &str, version: &Version, docs_json: &str) -> Result<(), String> {
        let path = self.docs_file(name, version);
        std::fs::write(&path, docs_json)
            .map_err(|err| format!("cannot write docs `{}`: {err}", path.display()))
    }

    fn docs(&self, name: &str, version: &Version) -> Result<Option<String>, String> {
        let path = self.docs_file(name, version);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("cannot read docs `{}`: {err}", path.display())),
        }
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
        f.debug_struct("HttpIndex")
            .field("base", &self.base)
            .finish_non_exhaustive()
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
    #[serde(default)]
    deps: Vec<WireDep>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    bundle: Option<String>,
}

#[cfg(feature = "registry-http")]
#[derive(serde::Deserialize)]
struct WireDep {
    package: String,
    req: String,
}

#[cfg(feature = "registry-http")]
#[derive(serde::Deserialize)]
struct ScopeResponse {
    #[serde(default)]
    public_key: Option<String>,
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

    /// Claim `scope` for `token`, proving ownership with a GitHub OIDC `oidc` JWT
    /// (namespace-protection #1): `POST /v1/scopes/claim`. Returns the registry's status message on
    /// success (`scope claimed` / `scope re-claimed`), or the server's error. This binds `token` as
    /// the scope's publish token — the same token `noeta publish` later presents.
    pub fn claim_scope(&self, scope: &str, token: &str, oidc: &str) -> Result<String, String> {
        let body = serde_json::json!({ "scope": scope, "token": token, "oidc": oidc });
        let resp = self
            .client
            .post(format!("{}/v1/scopes/claim", self.base))
            .json(&body)
            .send()
            .map_err(|err| format!("claiming scope `{scope}` failed: {err}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.is_success() {
            // Surface the human-readable status the Worker returns (`{ "status": … }`).
            let msg = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("scope `{scope}` claimed"));
            return Ok(msg);
        }
        // Prefer the server's `{ "error": … }` message when present.
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| status.to_string());
        Err(format!("registry refused the claim of `{scope}`: {detail}"))
    }

    /// Set a scope's **publishing policy** (namespace-protection #1, require-provenance): `POST
    /// /v1/scopes/{scope}/policy`, owner-authenticated with the scope's publish token
    /// (`NOETA_REGISTRY_TOKEN`). `require_provenance` turns the requirement on/off; `root` narrows
    /// which trust root is demanded (`key`/`keyless`), or `None` = either satisfies it. Returns the
    /// registry's status message.
    pub fn set_scope_policy(
        &self,
        scope: &str,
        require_provenance: bool,
        root: Option<&str>,
    ) -> Result<String, String> {
        let token = self.token.as_ref().ok_or_else(|| {
            "setting a scope policy needs a token — set NOETA_REGISTRY_TOKEN to the scope's publish \
             token"
                .to_string()
        })?;
        let mut body = serde_json::json!({ "require_provenance": require_provenance });
        if let Some(root) = root {
            body["root"] = serde_json::json!(root);
        }
        let resp = self
            .client
            .post(format!("{}/v1/scopes/{scope}/policy", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .map_err(|err| format!("setting the policy for scope `{scope}` failed: {err}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.is_success() {
            let msg = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("policy updated for `{scope}`"));
            return Ok(msg);
        }
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| status.to_string());
        Err(format!(
            "registry rejected the policy for `{scope}`: {detail}"
        ))
    }
}

/// Mint a fresh 256-bit publish token (hex) from OS entropy — what `noeta claim` binds to a scope
/// when the user doesn't supply one (namespace-protection #1).
#[cfg(feature = "registry-http")]
pub fn generate_publish_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| format!("cannot read OS entropy: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Fetch a GitHub Actions **OIDC token** for `audience`, or `Ok(None)` when not running under GitHub
/// Actions (namespace-protection #1). GitHub exposes the request URL + a bearer via the
/// `ACTIONS_ID_TOKEN_REQUEST_URL` / `ACTIONS_ID_TOKEN_REQUEST_TOKEN` env vars (present only when the
/// workflow grants `id-token: write`); the response is `{ "value": "<jwt>" }`. The `audience` must
/// match the registry's configured `OIDC_AUDIENCE`.
#[cfg(feature = "registry-http")]
pub fn fetch_github_oidc(audience: &str) -> Result<Option<String>, String> {
    let (Ok(req_url), Ok(req_token)) = (
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL"),
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
    ) else {
        return Ok(None);
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("noeta/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| format!("cannot build the OIDC HTTP client: {err}"))?;
    let sep = if req_url.contains('?') { '&' } else { '?' };
    let resp = client
        .get(format!("{req_url}{sep}audience={audience}"))
        .bearer_auth(req_token)
        .send()
        .map_err(|err| format!("requesting a GitHub OIDC token failed: {err}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "GitHub OIDC token endpoint returned {} (does the workflow grant `id-token: write`?)",
            resp.status()
        ));
    }
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        value: String,
    }
    let token: TokenResponse = resp
        .json()
        .map_err(|err| format!("GitHub OIDC token response was not the expected JSON: {err}"))?;
    Ok(Some(token.value))
}

#[cfg(feature = "registry-http")]
impl Index for HttpIndex {
    fn releases(&self, name: &str) -> Result<Vec<Release>, String> {
        let resp = self
            .client
            .get(self.url_for(name))
            .send()
            .map_err(|err| format!("registry request for `{name}` failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!("registry returned {} for `{name}`", resp.status()));
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
            let deps = v
                .deps
                .into_iter()
                .filter_map(|d| {
                    VersionReq::parse(&d.req).ok().map(|req| Dep {
                        package: d.package,
                        req,
                    })
                })
                .collect();
            out.push(Release {
                version,
                coords: GitCoords {
                    url: v.url,
                    tag: v.tag,
                    sha: v.sha,
                },
                deps,
                signature: v.signature,
                bundle: v.bundle,
            });
        }
        Ok(out)
    }

    fn scope_key(&self, scope: &str) -> Result<Option<String>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/scopes/{scope}", self.base))
            .send()
            .map_err(|err| format!("registry scope-key request for `{scope}` failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for scope `{scope}`",
                resp.status()
            ));
        }
        let body: ScopeResponse = resp.json().map_err(|err| {
            format!("registry sent an unreadable scope response for `{scope}`: {err}")
        })?;
        Ok(body.public_key)
    }

    fn publish(&self, name: &str, release: &Release) -> Result<(), String> {
        release.check_provenance_shape()?;
        let token = self.token.as_ref().ok_or_else(|| {
            "publishing needs a token — set NOETA_REGISTRY_TOKEN to your registry publish token"
                .to_string()
        })?;
        let deps: Vec<_> = release
            .deps
            .iter()
            .map(|d| serde_json::json!({ "package": d.package, "req": d.req.to_string() }))
            .collect();
        let body = serde_json::json!({
            "version": release.version.to_string(),
            "url": release.coords.url,
            "tag": release.coords.tag,
            "sha": release.coords.sha,
            "deps": deps,
            "signature": release.signature,
            "bundle": release.bundle,
        });
        let resp = self
            .client
            .post(self.url_for(name))
            .bearer_auth(token)
            .json(&body)
            .send()
            .map_err(|err| format!("publishing `{name}`@{} failed: {err}", release.version))?;
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
        Err(format!(
            "registry rejected `{name}`@{}: {detail}",
            release.version
        ))
    }
    fn put_docs(&self, name: &str, version: &Version, docs_json: &str) -> Result<(), String> {
        let token = self.token.as_ref().ok_or_else(|| {
            "uploading docs needs a token — set NOETA_REGISTRY_TOKEN to your registry publish token"
                .to_string()
        })?;
        let resp = self
            .client
            .put(format!("{}/docs/{version}", self.url_for(name)))
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(docs_json.to_string())
            .send()
            .map_err(|err| format!("registry docs upload for `{name}` failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} uploading docs for `{name}@{version}`",
                resp.status()
            ));
        }
        Ok(())
    }

    fn docs(&self, name: &str, version: &Version) -> Result<Option<String>, String> {
        let resp = self
            .client
            .get(format!("{}/docs/{version}", self.url_for(name)))
            .send()
            .map_err(|err| format!("registry docs request for `{name}` failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for `{name}@{version}` docs",
                resp.status()
            ));
        }
        resp.text()
            .map(Some)
            .map_err(|err| format!("registry sent unreadable docs for `{name}`: {err}"))
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

    /// A release with the given version + tag and no dependencies.
    fn release(major: u64, minor: u64, patch: u64, tag: &str) -> Release {
        Release {
            version: Version::new(major, minor, patch),
            coords: coords(tag),
            deps: Vec::new(),
            signature: None,
            bundle: None,
        }
    }

    #[test]
    fn publish_then_resolve_picks_highest_match() {
        let index = mem("pick_highest");
        index
            .publish("guzzle/http", &release(1, 0, 0, "v1.0.0"))
            .unwrap();
        index
            .publish("guzzle/http", &release(1, 4, 0, "v1.4.0"))
            .unwrap();
        index
            .publish("guzzle/http", &release(2, 0, 0, "v2.0.0"))
            .unwrap();

        let (version, c) =
            resolve_coords(&index, "guzzle/http", &VersionReq::parse("^1.0").unwrap()).unwrap();
        assert_eq!(version, Version::new(1, 4, 0)); // highest in ^1
        assert_eq!(c.tag, "v1.4.0");
    }

    #[test]
    fn signature_and_scope_key_round_trip_through_the_local_index() {
        let index = mem("signature_round_trip");
        let mut rel = release(1, 0, 0, "v1.0.0");
        rel.signature = Some("a".repeat(128));
        index.publish("acme/foo", &rel).unwrap();
        assert_eq!(
            index.releases("acme/foo").unwrap()[0].signature,
            Some("a".repeat(128))
        );

        // Scope keys register + serve; an unregistered scope is `None`.
        assert_eq!(index.scope_key("acme").unwrap(), None);
        index.set_scope_key("acme", "deadbeef").unwrap();
        assert_eq!(
            index.scope_key("acme").unwrap(),
            Some("deadbeef".to_string())
        );
    }

    /// Docs are per-release artifacts: put/get round-trips, an unknown release has none, and a
    /// re-put overwrites (advisory metadata, last-wins — unlike the immutable release itself).
    #[test]
    fn docs_round_trip_through_the_local_index() {
        let index = mem("docs_round_trip");
        let v = Version::parse("1.2.0").unwrap();
        assert_eq!(index.docs("acme/pkg", &v).unwrap(), None);
        index
            .put_docs("acme/pkg", &v, "{\"schema\":1}")
            .expect("put");
        assert_eq!(
            index.docs("acme/pkg", &v).unwrap().as_deref(),
            Some("{\"schema\":1}")
        );
        index
            .put_docs("acme/pkg", &v, "{\"schema\":1,\"modules\":[]}")
            .expect("re-put");
        assert_eq!(
            index.docs("acme/pkg", &v).unwrap().as_deref(),
            Some("{\"schema\":1,\"modules\":[]}")
        );
        // Another version's docs are independent.
        let v2 = Version::parse("2.0.0").unwrap();
        assert_eq!(index.docs("acme/pkg", &v2).unwrap(), None);
    }

    #[test]
    fn bundle_round_trips_through_the_local_index() {
        let index = mem("bundle_round_trip");
        let mut rel = release(1, 0, 0, "v1.0.0");
        // A realistic bundle is a large JSON document with quotes and escapes — the TOML
        // string round-trip must preserve it byte-for-byte.
        rel.bundle = Some(r#"{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","dsseEnvelope":{"payload":"e30=","payloadType":"application/vnd.in-toto+json"}}"#.to_string());
        index.publish("acme/foo", &rel).unwrap();
        assert_eq!(index.releases("acme/foo").unwrap()[0].bundle, rel.bundle);
    }

    #[test]
    fn a_release_with_both_trust_roots_is_rejected_at_publish() {
        let index = mem("both_roots");
        let mut rel = release(1, 0, 0, "v1.0.0");
        rel.signature = Some("a".repeat(128));
        rel.bundle = Some("{}".to_string());
        let err = index.publish("acme/foo", &rel).unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn deps_round_trip_through_the_local_index() {
        let index = mem("deps_round_trip");
        let mut rel = release(1, 0, 0, "v1.0.0");
        rel.deps = vec![
            Dep {
                package: "acme/bar".to_string(),
                req: VersionReq::parse("^1.2").unwrap(),
            },
            Dep {
                package: "acme/baz".to_string(),
                req: VersionReq::parse(">=2, <3").unwrap(),
            },
        ];
        index.publish("acme/foo", &rel).unwrap();
        let got = index.releases("acme/foo").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].deps, rel.deps);
    }

    #[test]
    fn resolve_reports_no_match_and_unknown() {
        let index = mem("no_match");
        index
            .publish("guzzle/http", &release(1, 0, 0, "v1.0.0"))
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
        index.publish("a/b", &release(1, 0, 0, "v1.0.0")).unwrap();
        // Same coords: idempotent.
        index.publish("a/b", &release(1, 0, 0, "v1.0.0")).unwrap();
        // Different coords for a published version: rejected (immutable).
        let err = index
            .publish("a/b", &release(1, 0, 0, "v9.9.9"))
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
    fn mock_server(handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static) -> String {
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
        let releases = index.releases("acme/imgfx").unwrap();
        // 2.0.0 is yanked → not offered as a candidate; 1.2.0 carries its pinned SHA.
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].version, Version::new(1, 2, 0));
        assert_eq!(releases[0].coords.sha, "abc");

        // resolve_coords picks it through the same trait the local index uses.
        let (v, c) =
            resolve_coords(&index, "acme/imgfx", &VersionReq::parse("^1.0").unwrap()).unwrap();
        assert_eq!(v, Version::new(1, 2, 0));
        assert_eq!(c.tag, "v1.2.0");
    }

    #[test]
    fn http_index_publishes_with_a_bearer_token() {
        let (tx, rx) = mpsc::channel();
        let base = mock_server(move |method, path, body| {
            tx.send((method.to_string(), path.to_string(), body.to_string()))
                .unwrap();
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
                &Release {
                    version: Version::new(1, 0, 0),
                    coords: GitCoords {
                        url: "https://x/acme/imgfx".to_string(),
                        tag: "v1.0.0".to_string(),
                        sha: "abc".to_string(),
                    },
                    deps: vec![Dep {
                        package: "acme/bar".to_string(),
                        req: VersionReq::parse("^1.0").unwrap(),
                    }],
                    signature: Some("deadbeef".to_string()),
                    bundle: None,
                },
            )
            .unwrap();
        let (method, path, body) = rx.recv().unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/packages/acme/imgfx");
        assert!(body.contains("\"sha\":\"abc\""), "body: {body}");
        assert!(body.contains("\"version\":\"1.0.0\""), "body: {body}");
        // The dependency metadata is sent so the index can serve it to the resolver (S5).
        assert!(
            body.contains("\"package\":\"acme/bar\""),
            "deps in body: {body}"
        );
        // The provenance signature rides along (Phase 4 #2).
        assert!(
            body.contains("\"signature\":\"deadbeef\""),
            "signature in body: {body}"
        );
    }

    #[test]
    fn http_index_round_trips_a_bundle() {
        // Serving: a version row carries its Sigstore bundle verbatim.
        let base = mock_server(|_, _, _| {
            (
                200,
                r#"{"versions":[{"version":"1.0.0","url":"https://x/a/b","tag":"v1.0.0","sha":"abc","bundle":"{\"mediaType\":\"application/vnd.dev.sigstore.bundle.v0.3+json\"}"}]}"#
                    .to_string(),
            )
        });
        let index = HttpIndex::new(base).unwrap();
        let releases = index.releases("a/b").unwrap();
        assert_eq!(
            releases[0].bundle.as_deref(),
            Some(r#"{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json"}"#)
        );

        // Publishing: the bundle rides the POST body (the Worker stores it next to the release).
        let (tx, rx) = mpsc::channel();
        let base = mock_server(move |_, _, body| {
            tx.send(body.to_string()).unwrap();
            (201, "{}".to_string())
        });
        let index = HttpIndex {
            token: Some("secret-token".to_string()),
            ..HttpIndex::new(base).unwrap()
        };
        let mut rel = Release {
            version: Version::new(1, 0, 0),
            coords: GitCoords {
                url: "https://x/a/b".to_string(),
                tag: "v1.0.0".to_string(),
                sha: "abc".to_string(),
            },
            deps: Vec::new(),
            signature: None,
            bundle: Some(r#"{"mediaType":"m"}"#.to_string()),
        };
        index.publish("a/b", &rel).unwrap();
        let body = rx.recv().unwrap();
        assert!(
            body.contains(r#""bundle":"{\"mediaType\":\"m\"}""#),
            "{body}"
        );

        // Both trust roots on one release never reach the wire.
        rel.signature = Some("deadbeef".to_string());
        let err = index.publish("a/b", &rel).unwrap_err();
        assert!(err.contains("not both"), "{err}");
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
                &Release {
                    version: Version::new(1, 0, 0),
                    coords: GitCoords {
                        url: "u".to_string(),
                        tag: "t".to_string(),
                        sha: "s".to_string(),
                    },
                    deps: Vec::new(),
                    signature: None,
                    bundle: None,
                },
            )
            .unwrap_err();
        assert!(err.contains("NOETA_REGISTRY_TOKEN"), "{err}");
    }

    #[test]
    fn http_index_claims_a_scope_and_surfaces_the_status() {
        // namespace-protection #1: `claim_scope` POSTs { scope, token, oidc } to /v1/scopes/claim and
        // returns the Worker's status message; a non-2xx surfaces the server's error verbatim.
        let (tx, rx) = mpsc::channel();
        let base = mock_server(move |method, path, body| {
            tx.send((method.to_string(), path.to_string(), body.to_string()))
                .unwrap();
            (
                201,
                r#"{"status":"scope claimed","scope":"widgetco","owner":"widgetco"}"#.to_string(),
            )
        });
        let index = HttpIndex::new(base).unwrap();
        let msg = index
            .claim_scope("widgetco", "publish-token-abc123", "eyJ.header.sig")
            .unwrap();
        assert_eq!(msg, "scope claimed");
        let (method, path, body) = rx.recv().unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/scopes/claim");
        assert!(body.contains("\"scope\":\"widgetco\""), "body: {body}");
        assert!(body.contains("\"oidc\":\"eyJ.header.sig\""), "body: {body}");
    }

    #[test]
    fn http_index_claim_surfaces_a_rejection() {
        let base = mock_server(|_, _, _| {
            (
                403,
                r#"{"error":"your GitHub identity `attacker` cannot claim scope `stripe`"}"#
                    .to_string(),
            )
        });
        let index = HttpIndex::new(base).unwrap();
        let err = index
            .claim_scope("stripe", "publish-token-abc123", "eyJ.header.sig")
            .unwrap_err();
        assert!(err.contains("cannot claim scope"), "{err}");
    }

    #[test]
    fn http_index_sets_a_scope_policy_with_the_owner_token() {
        // namespace-protection #1 Phase 1: `set_scope_policy` POSTs the require-provenance policy to
        // /v1/scopes/{scope}/policy under the scope's publish token, and surfaces the status.
        let (tx, rx) = mpsc::channel();
        let base = mock_server(move |method, path, body| {
            tx.send((method.to_string(), path.to_string(), body.to_string()))
                .unwrap();
            (200, r#"{"status":"policy updated","scope":"para","require_provenance":true,"root":"keyless"}"#.to_string())
        });
        let index = HttpIndex {
            token: Some("owner-token".to_string()),
            ..HttpIndex::new(base).unwrap()
        };
        let msg = index
            .set_scope_policy("para", true, Some("keyless"))
            .unwrap();
        assert_eq!(msg, "policy updated");
        let (method, path, body) = rx.recv().unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/scopes/para/policy");
        assert!(body.contains("\"require_provenance\":true"), "body: {body}");
        assert!(body.contains("\"root\":\"keyless\""), "body: {body}");
    }

    #[test]
    fn set_scope_policy_needs_a_token() {
        let base = mock_server(|_, _, _| (200, "{}".to_string()));
        let err = HttpIndex::new(base)
            .unwrap()
            .set_scope_policy("para", true, None)
            .unwrap_err();
        assert!(err.contains("NOETA_REGISTRY_TOKEN"), "{err}");
    }

    #[test]
    fn github_oidc_is_none_outside_ci() {
        // Absent the GitHub Actions token-request env, there is no ambient OIDC token — `Ok(None)`, so
        // `noeta claim` can print actionable guidance rather than error opaquely. (These vars are only
        // set inside a GitHub Actions job with `id-token: write`.) We don't mutate the environment —
        // the crate forbids `unsafe`, and `remove_var` is unsafe — so assert only when it is already
        // absent (the normal case, including ordinary CI test jobs).
        if std::env::var_os("ACTIONS_ID_TOKEN_REQUEST_URL").is_none()
            && std::env::var_os("ACTIONS_ID_TOKEN_REQUEST_TOKEN").is_none()
        {
            assert_eq!(fetch_github_oidc("noeta-registry").unwrap(), None);
        }
    }

    #[test]
    fn generated_publish_tokens_are_long_and_unique() {
        let a = generate_publish_token().unwrap();
        let b = generate_publish_token().unwrap();
        assert_eq!(a.len(), 64, "256 bits as hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must not repeat");
    }
}
