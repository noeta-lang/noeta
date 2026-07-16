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
    /// Publish time as Unix epoch **milliseconds** (publish-cooldown, namespace-protection #1), when
    /// the index knows it. Drives the consumer's `[trust].publish_cooldown` filter — a release younger
    /// than the window is not newly selected. `None` for sources without a timestamp (the local index,
    /// path/git), which are never subject to cooldown.
    pub published_at: Option<i64>,
    /// The declared license (SPDX expression, `[package] license`). Part of the immutable release
    /// record — the registry binds it into the release's transparency-log leaf — but
    /// publisher-asserted: the SHA-pinned source's LICENSE file is the ground truth. `None` when
    /// the release declared none.
    pub license: Option<String>,
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

    /// Store a release's **README markdown** (the package's `README.md`, rendered on the hosted
    /// registry's package page — the npm/crates.io model; the registry never fetches source, so a
    /// README is only ever what the publisher uploads). Same posture as docs: *advisory metadata*,
    /// unsigned and last-wins, never part of the immutable release record. Default: an error — an
    /// index that does not store READMEs says so, and `noeta publish` degrades to a warning.
    fn put_readme(&self, _name: &str, _version: &Version, _readme_md: &str) -> Result<(), String> {
        Err("this registry does not store READMEs".to_string())
    }

    /// The stored README markdown for `name@version`, if any. Default: `None`.
    fn readme(&self, _name: &str, _version: &Version) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// A **local git repository** already holding `name`'s commits, if this index maintains one, so the
    /// resolver can materialize a release's tree from it instead of a second network clone
    /// (private-registries arc). The default `None` means "fetch from the release's coordinates URL"
    /// (the hosted/local index don't hold a clone). A git-forge index returns its cached bare clone.
    fn local_repo(&self, _name: &str) -> Option<PathBuf> {
        None
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

    /// The file holding a release's README markdown (readme-on-package-page follow-up).
    fn readme_file(&self, name: &str, version: &Version) -> PathBuf {
        self.dir
            .join(format!("readme__{}__{version}.md", name.replace('/', "__")))
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
                    // The local (offline) index carries no publish time — never subject to cooldown.
                    published_at: None,
                    license: get("license").map(str::to_string),
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
        // The rewrite below regenerates the WHOLE file from what `releases()` parsed — and that
        // parse is deliberately lossy (an entry with a shape this toolchain half-understands is
        // skipped, so resolution degrades gracefully). Writing over a lossy parse would silently
        // DELETE the skipped entries; refuse instead (a publish must never destroy records it
        // didn't understand — likely a newer toolchain's entry, or hand-corruption).
        if let Ok(text) = std::fs::read_to_string(self.file_for(name)) {
            let on_disk = text
                .parse::<toml::Table>()
                .ok()
                .and_then(|t| t.get("version").and_then(|v| v.as_array()).map(|a| a.len()))
                .unwrap_or(0);
            if on_disk != releases.len() {
                return Err(format!(
                    "registry entry for `{name}` records {on_disk} version(s) but only {} parse                      with this toolchain — refusing to rewrite the entry (it would silently drop                      the rest). It may be written by a newer toolchain, or corrupted: fix or                      remove `{}` first",
                    releases.len(),
                    self.file_for(name).display()
                ));
            }
        }
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
            if let Some(license) = &r.license {
                text.push_str(&format!("license = {}\n", quote(license)));
            }
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

    fn put_readme(&self, name: &str, version: &Version, readme_md: &str) -> Result<(), String> {
        let path = self.readme_file(name, version);
        std::fs::write(&path, readme_md)
            .map_err(|err| format!("cannot write readme `{}`: {err}", path.display()))
    }

    fn readme(&self, name: &str, version: &Version) -> Result<Option<String>, String> {
        let path = self.readme_file(name, version);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("cannot read readme `{}`: {err}", path.display())),
        }
    }
}

/// Minimal TOML basic-string quoting for the values we emit.
fn quote(s: &str) -> String {
    crate::toml_quote(s)
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

/// Open the index for a `[registries]` source (private-registries arc): `None` = the environment
/// default ([`open_default`]); a hosted URL = an [`HttpIndex`] at that base; a GitHub org = a
/// git-forge index over that org. This is what lets a project route each scope to its own registry.
pub fn open_source(
    source: Option<&crate::manifest::RegistrySource>,
) -> Result<Box<dyn Index>, String> {
    match source {
        None => open_default(),
        Some(crate::manifest::RegistrySource::Hosted(url)) => open_hosted(url),
        Some(crate::manifest::RegistrySource::GitForge(base)) => open_git_forge(base),
    }
}

/// Open a hosted HTTP registry at an explicit base URL (a `[registries]` `https://…` source).
#[cfg(feature = "registry-http")]
fn open_hosted(url: &str) -> Result<Box<dyn Index>, String> {
    Ok(Box::new(HttpIndex::new(url.to_string())?))
}

#[cfg(not(feature = "registry-http"))]
fn open_hosted(_url: &str) -> Result<Box<dyn Index>, String> {
    Err(
        "a hosted `[registries]` source needs the `registry-http` build of the toolchain"
            .to_string(),
    )
}

/// Open a git forge as a registry (a `[registries]` `github:`/`gitlab:`/`git:` source, normalized to a
/// base URL): packages resolve from `<base>/<package>` by their semver tags (private-registries arc).
fn open_git_forge(base: &str) -> Result<Box<dyn Index>, String> {
    Ok(Box::new(crate::git_forge::GitForgeIndex::from_base(base)?))
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
    /// Publish time as Unix epoch milliseconds (publish-cooldown). Absent for a registry that predates
    /// the field or can't parse its own timestamp → treated as undateable (never in cooldown).
    #[serde(default)]
    published_at_unix: Option<i64>,
    /// The declared SPDX license expression. Absent for releases (or registries) that predate it.
    #[serde(default)]
    license: Option<String>,
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

    /// Claim `scope` for `token`, proving ownership with `proof` — a GitHub Actions OIDC token (CI) or
    /// a GitHub OAuth access token from the device flow (laptop) (namespace-protection #1): `POST
    /// /v1/scopes/claim`. Returns the registry's status message on success (`scope claimed` /
    /// `scope re-claimed`), or the server's error. This binds `token` as the scope's publish token —
    /// the same token `noeta publish` later presents.
    pub fn claim_scope(
        &self,
        scope: &str,
        token: &str,
        proof: &ClaimProof,
    ) -> Result<String, String> {
        let mut body = serde_json::json!({ "scope": scope, "token": token });
        match proof {
            ClaimProof::Oidc(jwt) => body["oidc"] = serde_json::json!(jwt),
            ClaimProof::GithubToken(gh) => body["github_token"] = serde_json::json!(gh),
            ClaimProof::Domain(domain) => body["domain"] = serde_json::json!(domain),
        }
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

    /// The transparency log's current **signed checkpoint** (namespace-protection #1): `GET
    /// /v1/log/checkpoint`.
    pub fn log_checkpoint(&self) -> Result<LogCheckpoint, String> {
        let resp = self
            .client
            .get(format!("{}/v1/log/checkpoint", self.base))
            .send()
            .map_err(|err| format!("fetching the transparency-log checkpoint failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the transparency-log checkpoint",
                resp.status()
            ));
        }
        resp.json()
            .map_err(|err| format!("malformed transparency-log checkpoint: {err}"))
    }

    /// The transparency log's **public key** (hex) to pin, or `None` if the registry serves none.
    pub fn log_public_key(&self) -> Result<Option<String>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/log/key", self.base))
            .send()
            .map_err(|err| format!("fetching the transparency-log key failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the transparency-log key",
                resp.status()
            ));
        }
        #[derive(serde::Deserialize)]
        struct KeyResponse {
            public_key: String,
        }
        Ok(Some(
            resp.json::<KeyResponse>()
                .map_err(|err| format!("malformed transparency-log key: {err}"))?
                .public_key,
        ))
    }

    /// The **inclusion proof** for `name`@`version`, or `None` if the release is not logged: `GET
    /// /v1/log/proof/{name}/{version}`.
    pub fn log_inclusion(&self, name: &str, version: &str) -> Result<Option<LogInclusion>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/log/proof/{name}/{version}", self.base))
            .send()
            .map_err(|err| format!("fetching the transparency-log proof failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the transparency-log proof of `{name}`@{version}",
                resp.status()
            ));
        }
        Ok(Some(resp.json().map_err(|err| {
            format!("malformed transparency-log proof: {err}")
        })?))
    }

    /// A **consistency proof** that the log at size `from` is a prefix of size `to`: `GET
    /// /v1/log/consistency?from&to` (namespace-protection #1, append-only across checkpoints).
    pub fn log_consistency(&self, from: u64, to: u64) -> Result<LogConsistency, String> {
        let resp = self
            .client
            .get(format!(
                "{}/v1/log/consistency?from={from}&to={to}",
                self.base
            ))
            .send()
            .map_err(|err| {
                format!("fetching the transparency-log consistency proof failed: {err}")
            })?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the transparency-log consistency proof",
                resp.status()
            ));
        }
        resp.json()
            .map_err(|err| format!("malformed transparency-log consistency proof: {err}"))
    }
}

/// A transparency-log signed checkpoint (RFC 6962 signed tree head) as served by the registry.
#[cfg(feature = "registry-http")]
#[derive(Debug, serde::Deserialize)]
pub struct LogCheckpoint {
    pub tree_size: u64,
    pub root_hash: String,
    pub signature: String,
}

/// A transparency-log inclusion proof for one release.
#[cfg(feature = "registry-http")]
#[derive(Debug, serde::Deserialize)]
pub struct LogInclusion {
    pub index: u64,
    pub tree_size: u64,
    pub root_hash: String,
    /// The canonical leaf record — the client recomputes the leaf from it.
    pub record: String,
    /// The audit path (hex hashes).
    pub proof: Vec<String>,
}

/// A transparency-log consistency proof between two tree sizes (the audit path; the caller already
/// knows the two roots it is proving append-only between).
#[cfg(feature = "registry-http")]
#[derive(Debug, serde::Deserialize)]
pub struct LogConsistency {
    pub proof: Vec<String>,
}

/// Open the **hosted** registry as a concrete [`HttpIndex`] when `NOETA_REGISTRY_URL` is set (needed
/// for transparency-log verification, which uses HttpIndex-only endpoints), else `None`.
#[cfg(feature = "registry-http")]
pub fn open_http() -> Result<Option<HttpIndex>, String> {
    match std::env::var_os("NOETA_REGISTRY_URL") {
        Some(url) => {
            let base = url
                .into_string()
                .map_err(|_| "NOETA_REGISTRY_URL is not valid UTF-8".to_string())?;
            Ok(Some(HttpIndex::new(base)?))
        }
        None => Ok(None),
    }
}

/// A transparency-log checkpoint the client verified and can pin (namespace-protection #1): the log
/// key that signed it, plus the tree size + root it attests to (trust-on-first-use / anti-equivocation).
#[cfg(all(feature = "registry-http", feature = "provenance"))]
#[derive(Debug, Clone)]
pub struct VerifiedLog {
    pub tree_size: u64,
    pub root_hex: String,
    pub public_key: String,
}

#[cfg(all(feature = "registry-http", feature = "provenance"))]
impl HttpIndex {
    /// Verify a resolved release is **included** in the transparency log at a **signed** checkpoint
    /// (namespace-protection #1). `pinned_key` is the log public key the caller already trusts; `None`
    /// adopts the served key (trust-on-first-use). The release is identified by its coordinates
    /// (`name`/`version`/`url`/`tag`/`sha`), which must match the logged record. Returns the verified
    /// checkpoint to pin. This proves, without trusting the registry, that the release we're about to
    /// use is publicly logged under a key the log operator controls — a compromised registry can't
    /// quietly serve an unlogged forgery.
    pub fn verify_release_logged(
        &self,
        name: &str,
        version: &str,
        url: &str,
        tag: &str,
        sha: &str,
        license: Option<&str>,
        pinned_key: Option<&str>,
    ) -> Result<VerifiedLog, String> {
        use crate::transparency;
        let key = match pinned_key {
            Some(k) => k.to_string(),
            None => self
                .log_public_key()?
                .ok_or("the registry serves no transparency-log public key")?,
        };
        let cp = self.log_checkpoint()?;
        if !transparency::verify_checkpoint(&key, cp.tree_size, &cp.root_hash, &cp.signature)? {
            return Err(
                "the transparency-log checkpoint signature does not verify against the log key \
                 — the registry may be equivocating, or the log key changed"
                    .to_string(),
            );
        }
        self.verify_inclusion_at(name, version, url, tag, sha, license, &cp)?;
        Ok(VerifiedLog {
            tree_size: cp.tree_size,
            root_hex: cp.root_hash,
            public_key: key,
        })
    }

    /// Verify a release is included at an **already-verified** checkpoint `cp` (namespace-protection
    /// #1): fetch its inclusion proof, require it be against `cp`'s signed tree, confirm the logged
    /// record's coordinates match the release, and verify the audit path. Shared by
    /// [`Self::verify_release_logged`] and the resolve-time enforcement (which verifies one checkpoint
    /// then checks every release against it).
    pub fn verify_inclusion_at(
        &self,
        name: &str,
        version: &str,
        url: &str,
        tag: &str,
        sha: &str,
        license: Option<&str>,
        cp: &LogCheckpoint,
    ) -> Result<(), String> {
        use crate::transparency;
        let incl = self
            .log_inclusion(name, version)?
            .ok_or_else(|| format!("`{name}`@{version} is not in the transparency log"))?;
        // The inclusion proof must be against the *signed* checkpoint's tree, else a registry could
        // prove inclusion in some other (unsigned) tree.
        if incl.root_hash != cp.root_hash || incl.tree_size != cp.tree_size {
            return Err(format!(
                "the transparency inclusion proof for `{name}`@{version} is not against the signed \
                 checkpoint (a concurrent publish can cause this — retry)"
            ));
        }
        // The logged record must be for exactly the release we resolved: identity, version, and git
        // coordinates. The record's provenance field rides along, authenticated by inclusion. We check
        // the *served* record so the client needn't recompute the provenance digest.
        let fields: Vec<&str> = incl.record.split('\n').collect();
        let matches = fields.len() >= 6
            && fields[0] == "noeta-transparency-log-v1"
            && fields[1] == name
            && fields[2] == version
            && fields[3] == url
            && fields[4] == tag
            && fields[5] == sha;
        if !matches {
            return Err(format!(
                "the transparency-log record for `{name}`@{version} does not match the resolved \
                 release (coordinates differ)"
            ));
        }
        // The license field was appended after the original six + provenance (fields are only ever
        // appended; the record ends in `\n`, so a post-license record splits into ≥ 9 parts, a
        // pre-license one into 8 with `fields[7]` being the trailing empty). When the caller knows
        // the release's license (`Some`; "" = declared none) and the record binds one, they must
        // match — otherwise the registry told this resolver something different from what it logged.
        // `None` skips the check (e.g. lockfile-driven verification, where the lock carries no
        // license); a pre-license record binds nothing, so nothing is checked against it.
        if let Some(expected) = license
            && fields.len() >= 9
            && fields[7] != expected
        {
            return Err(format!(
                "the transparency-log record for `{name}`@{version} binds license `{}` but the \
                 index serves `{expected}` — the registry may be equivocating",
                fields[7],
            ));
        }
        let root = transparency::hex_to_array::<32>(&cp.root_hash)
            .ok_or("malformed checkpoint root hash")?;
        let proof = incl
            .proof
            .iter()
            .map(|h| transparency::hex_to_array::<32>(h))
            .collect::<Option<Vec<_>>>()
            .ok_or("malformed inclusion-proof hash")?;
        let leaf = transparency::leaf_hash(incl.record.as_bytes());
        if !transparency::verify_inclusion(
            leaf,
            incl.index as usize,
            incl.tree_size as usize,
            &proof,
            &root,
        ) {
            return Err(format!(
                "the transparency inclusion proof for `{name}`@{version} does not verify"
            ));
        }
        Ok(())
    }
}

/// The registry's advisory-feed signed head (namespace-protection #1): the total advisory count and a
/// digest over every advisory's canonical bytes, signed with the feed's Ed25519 key.
#[cfg(all(feature = "registry-http", feature = "provenance"))]
#[derive(Debug, serde::Deserialize)]
pub struct AdvisoryCheckpoint {
    pub count: usize,
    pub digest: String,
    pub signature: String,
}

/// The verified advisory feed a client fetched (namespace-protection #1): the pinned advisory key, the
/// signed head it attests to (`count`/`digest`, for trust-on-first-use rollback detection), and the
/// signature-verified advisories.
#[cfg(all(feature = "registry-http", feature = "provenance"))]
#[derive(Debug, Clone)]
pub struct VerifiedAdvisories {
    pub public_key: String,
    pub count: usize,
    pub digest: String,
    pub advisories: Vec<crate::advisory::Advisory>,
}

#[cfg(all(feature = "registry-http", feature = "provenance"))]
impl HttpIndex {
    /// The advisory feed's **public key** (hex) to pin, or `None` if the registry serves none.
    pub fn advisory_public_key(&self) -> Result<Option<String>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/advisories/key", self.base))
            .send()
            .map_err(|err| format!("fetching the advisory-feed key failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the advisory-feed key",
                resp.status()
            ));
        }
        #[derive(serde::Deserialize)]
        struct KeyResponse {
            public_key: String,
        }
        Ok(Some(
            resp.json::<KeyResponse>()
                .map_err(|err| format!("malformed advisory-feed key: {err}"))?
                .public_key,
        ))
    }

    /// The advisory feed's current **signed head**: `GET /v1/advisories/checkpoint`.
    pub fn advisory_checkpoint(&self) -> Result<AdvisoryCheckpoint, String> {
        let resp = self
            .client
            .get(format!("{}/v1/advisories/checkpoint", self.base))
            .send()
            .map_err(|err| format!("fetching the advisory-feed checkpoint failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the advisory-feed checkpoint",
                resp.status()
            ));
        }
        resp.json()
            .map_err(|err| format!("malformed advisory-feed checkpoint: {err}"))
    }

    /// The raw advisory feed: `GET /v1/advisories`.
    pub fn list_advisories(&self) -> Result<Vec<crate::advisory::Advisory>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/advisories", self.base))
            .send()
            .map_err(|err| format!("fetching the advisory feed failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the advisory feed",
                resp.status()
            ));
        }
        #[derive(serde::Deserialize)]
        struct FeedResponse {
            advisories: Vec<crate::advisory::Advisory>,
        }
        Ok(resp
            .json::<FeedResponse>()
            .map_err(|err| format!("malformed advisory feed: {err}"))?
            .advisories)
    }

    /// Fetch and **verify** the whole advisory feed (namespace-protection #1). `pinned_key` is the
    /// advisory key the caller already trusts; `None` adopts the served key (trust-on-first-use). Every
    /// advisory's signature is checked against that key, the signed head's signature is checked, and the
    /// head's digest is required to equal the digest recomputed from the served advisories — so a
    /// registry can't withhold an advisory (the digest would diverge) or serve a tampered one. Returns
    /// the verified feed and its head, for the caller to pin and to match against resolved versions.
    pub fn fetch_advisories(&self, pinned_key: Option<&str>) -> Result<VerifiedAdvisories, String> {
        use crate::advisory;
        let key = match pinned_key {
            Some(k) => k.to_string(),
            None => self
                .advisory_public_key()?
                .ok_or("the registry serves no advisory-feed public key")?,
        };
        let advisories = self.list_advisories()?;
        for a in &advisories {
            if !a.verify(&key)? {
                return Err(format!(
                    "advisory `{}` does not verify against the advisory-feed key — the feed may be \
                     tampered, or the key changed",
                    a.id
                ));
            }
        }
        let cp = self.advisory_checkpoint()?;
        if !advisory::verify_feed_head(&key, cp.count, &cp.digest, &cp.signature)? {
            return Err(
                "the advisory-feed head signature does not verify against the feed key".to_string(),
            );
        }
        // The signed head must attest to exactly the advisories served (else one was withheld).
        let recomputed = advisory::feed_digest(&advisories);
        if cp.count != advisories.len() || cp.digest != recomputed {
            return Err(
                "the advisory-feed head does not match the served advisories — the registry may be \
                 withholding an advisory"
                    .to_string(),
            );
        }
        Ok(VerifiedAdvisories {
            public_key: key,
            count: cp.count,
            digest: cp.digest,
            advisories,
        })
    }

    /// The inclusion proof for an advisory's current transparency-log leaf (`GET
    /// /v1/log/advisory/{id}`), or `None` if the advisory is not logged.
    pub fn advisory_inclusion(&self, id: &str) -> Result<Option<LogInclusion>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/log/advisory/{id}", self.base))
            .send()
            .map_err(|err| format!("fetching the advisory inclusion proof failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for the advisory inclusion proof of `{id}`",
                resp.status()
            ));
        }
        Ok(Some(resp.json().map_err(|err| {
            format!("malformed advisory inclusion proof: {err}")
        })?))
    }

    /// Verify that every advisory in `advisories` is **included** in the transparency log at the
    /// registry's current **signed** checkpoint (advisory-log binding, namespace-protection #1) — so an
    /// advisory the registry serves is provably in the public, append-only log, not fabricated for one
    /// consumer. `pinned_log_key` is the log key the caller already trusts (TOFU); `None` adopts the
    /// served one. Returns `(verified count, unlogged ids)`: an advisory the registry serves *without* a
    /// log index, or whose logged leaf doesn't match, is surfaced. `Ok(None)` if the registry runs no
    /// transparency log at all (nothing to verify against).
    pub fn verify_advisories_logged(
        &self,
        advisories: &[crate::advisory::Advisory],
        pinned_log_key: Option<&str>,
    ) -> Result<Option<(usize, Vec<String>)>, String> {
        use crate::transparency;
        let log_key = match pinned_log_key {
            Some(k) => k.to_string(),
            None => match self.log_public_key()? {
                Some(k) => k,
                None => return Ok(None), // no transparency log configured
            },
        };
        let cp = self.log_checkpoint()?;
        if !transparency::verify_checkpoint(&log_key, cp.tree_size, &cp.root_hash, &cp.signature)? {
            return Err(
                "the transparency-log checkpoint signature does not verify against the log key"
                    .to_string(),
            );
        }
        let mut verified = 0usize;
        let mut unlogged = Vec::new();
        for a in advisories {
            let Some(_idx) = a.log_index else {
                unlogged.push(a.id.clone());
                continue;
            };
            let incl = match self.advisory_inclusion(&a.id)? {
                Some(incl) => incl,
                None => {
                    unlogged.push(a.id.clone());
                    continue;
                }
            };
            // The proof must be against the *signed* checkpoint, the logged record must be exactly this
            // advisory's canonical bytes, and the audit path must verify.
            let canonical = String::from_utf8(a.canonical_bytes())
                .map_err(|_| "advisory canonical bytes are not UTF-8".to_string())?;
            if incl.root_hash != cp.root_hash
                || incl.tree_size != cp.tree_size
                || incl.record != canonical
            {
                return Err(format!(
                    "advisory `{}` in the feed does not match its transparency-log leaf",
                    a.id
                ));
            }
            let root = transparency::hex_to_array::<32>(&cp.root_hash)
                .ok_or("malformed checkpoint root hash")?;
            let proof = incl
                .proof
                .iter()
                .map(|h| transparency::hex_to_array::<32>(h))
                .collect::<Option<Vec<_>>>()
                .ok_or("malformed advisory inclusion-proof hash")?;
            let leaf = transparency::leaf_hash(canonical.as_bytes());
            if !transparency::verify_inclusion(
                leaf,
                incl.index as usize,
                incl.tree_size as usize,
                &proof,
                &root,
            ) {
                return Err(format!(
                    "the transparency inclusion proof for advisory `{}` does not verify",
                    a.id
                ));
            }
            verified += 1;
        }
        Ok(Some((verified, unlogged)))
    }
}

/// A proof of scope ownership presented to `POST /v1/scopes/claim` (namespace-protection #1): a GitHub
/// Actions OIDC token (CI), a GitHub OAuth access token from the device flow (laptop), or a **domain**
/// whose control the registry verifies via a well-known file (namespace-protection follow-on). The two
/// GitHub proofs resolve server-side to one owner identity (interchangeable); a domain proof is its own
/// `domain` owner kind.
#[cfg(feature = "registry-http")]
pub enum ClaimProof {
    /// A GitHub Actions OIDC JWT (the CI path).
    Oidc(String),
    /// A GitHub OAuth access token (the laptop device-flow path).
    GithubToken(String),
    /// A domain the claimant controls — the registry fetches its `/.well-known/noeta-registry.txt`.
    /// The domain is public (not a secret), so Debug shows it.
    Domain(String),
}

// The GitHub proofs carry a bearer secret, so Debug redacts them — the value must never leak into a log
// or panic message. A domain is public, so it's shown.
#[cfg(feature = "registry-http")]
impl std::fmt::Debug for ClaimProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimProof::Oidc(_) => write!(f, "ClaimProof::Oidc(<redacted>)"),
            ClaimProof::GithubToken(_) => write!(f, "ClaimProof::GithubToken(<redacted>)"),
            ClaimProof::Domain(domain) => write!(f, "ClaimProof::Domain({domain:?})"),
        }
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
                published_at: v.published_at_unix,
                license: v.license,
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
            "license": release.license,
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

    fn put_readme(&self, name: &str, version: &Version, readme_md: &str) -> Result<(), String> {
        let token = self.token.as_ref().ok_or_else(|| {
            "uploading a README needs a token — set NOETA_REGISTRY_TOKEN to your registry publish token"
                .to_string()
        })?;
        let resp = self
            .client
            .put(format!("{}/readme/{version}", self.url_for(name)))
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "text/markdown")
            .body(readme_md.to_string())
            .send()
            .map_err(|err| format!("registry README upload for `{name}` failed: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} uploading the README for `{name}@{version}`",
                resp.status()
            ));
        }
        Ok(())
    }

    fn readme(&self, name: &str, version: &Version) -> Result<Option<String>, String> {
        let resp = self
            .client
            .get(format!("{}/readme/{version}", self.url_for(name)))
            .send()
            .map_err(|err| format!("registry README request for `{name}` failed: {err}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!(
                "registry returned {} for `{name}@{version}` README",
                resp.status()
            ));
        }
        resp.text()
            .map(Some)
            .map_err(|err| format!("registry sent unreadable README for `{name}`: {err}"))
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

    #[test]
    fn publish_refuses_to_rewrite_over_entries_it_cannot_parse() {
        // The local index's read is lossy by design (an unknown shape degrades resolution
        // gracefully) — but publish's read-modify-REWRITE must never silently delete what the
        // parse skipped (e.g. an entry written by a newer toolchain).
        let dir = std::env::temp_dir().join("noeta_registry_lossy_rewrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let index = LocalIndex::open_at(&dir).unwrap();
        index
            .publish("acme/lib", &release(1, 0, 0, "v1.0.0"))
            .expect("first publish");
        // Corrupt-append an entry this toolchain can't fully parse (no `sha`).
        let file = index.file_for("acme/lib");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("\n[[version]]\nversion = \"9.0.0\"\nurl = \"u\"\ntag = \"v9.0.0\"\nfuture_shape = \"x\"\n");
        std::fs::write(&file, &text).unwrap();
        let err = index
            .publish("acme/lib", &release(2, 0, 0, "v2.0.0"))
            .expect_err("must refuse the lossy rewrite");
        assert!(err.contains("refusing to rewrite"), "{err}");
        // The half-understood entry is still on disk, untouched.
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("future_shape")
        );
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
            published_at: None,
            license: None,
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
    fn license_round_trips_through_the_local_index() {
        let index = mem("license_round_trip");
        let mut rel = release(1, 0, 0, "v1.0.0");
        rel.license = Some("MIT OR Apache-2.0".to_string());
        index.publish("acme/pkg", &rel).expect("publish");
        let got = index.releases("acme/pkg").unwrap();
        assert_eq!(got[0].license.as_deref(), Some("MIT OR Apache-2.0"));
        // A license-less release stays None (absent from the TOML entirely).
        index
            .publish("acme/pkg", &release(2, 0, 0, "v2.0.0"))
            .expect("publish");
        let got = index.releases("acme/pkg").unwrap();
        assert_eq!(got[1].license, None);
    }

    #[test]
    fn readme_round_trips_through_the_local_index() {
        let index = mem("readme_round_trip");
        let v = Version::parse("1.2.0").unwrap();
        assert_eq!(index.readme("acme/pkg", &v).unwrap(), None);
        index
            .put_readme("acme/pkg", &v, "# pkg\n\nHello.")
            .expect("put");
        assert_eq!(
            index.readme("acme/pkg", &v).unwrap().as_deref(),
            Some("# pkg\n\nHello.")
        );
        // Last-wins, like docs — a corrected README overwrites.
        index
            .put_readme("acme/pkg", &v, "# pkg v2")
            .expect("re-put");
        assert_eq!(
            index.readme("acme/pkg", &v).unwrap().as_deref(),
            Some("# pkg v2")
        );
        // Another version's README is independent.
        let v2 = Version::parse("2.0.0").unwrap();
        assert_eq!(index.readme("acme/pkg", &v2).unwrap(), None);
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
                    {"version":"1.2.0","url":"https://x/acme/imgfx","tag":"v1.2.0","sha":"abc","yanked":false,"published_at_unix":1700000000000},
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
        // The publish timestamp flows through as epoch-millis for the cooldown filter.
        assert_eq!(releases[0].published_at, Some(1_700_000_000_000));

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
                    published_at: None,
                    license: Some("MIT".to_string()),
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
        // The declared license rides along into the immutable release record.
        assert!(
            body.contains("\"license\":\"MIT\""),
            "license in body: {body}"
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
            published_at: None,
            license: None,
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
                    published_at: None,
                    license: None,
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
            .claim_scope(
                "widgetco",
                "publish-token-abc123",
                &ClaimProof::Oidc("eyJ.header.sig".into()),
            )
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
            .claim_scope(
                "stripe",
                "publish-token-abc123",
                &ClaimProof::Oidc("eyJ.header.sig".into()),
            )
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

    #[test]
    fn log_consistency_parses_the_audit_path() {
        let (tx, rx) = mpsc::channel();
        let base = mock_server(move |method, path, _body| {
            tx.send((method.to_string(), path.to_string())).unwrap();
            (
                200,
                r#"{"from":2,"to":3,"root_from":"aa","root_to":"bb","proof":["11","22"]}"#
                    .to_string(),
            )
        });
        let index = HttpIndex::new(base).unwrap();
        let cons = index.log_consistency(2, 3).unwrap();
        assert_eq!(cons.proof, vec!["11".to_string(), "22".to_string()]);
        let (method, path) = rx.recv().unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/log/consistency?from=2&to=3");
    }

    #[cfg(feature = "provenance")]
    #[test]
    fn verify_release_logged_checks_the_signed_checkpoint_and_inclusion() {
        // namespace-protection #1 TLog 3: end-to-end client verification against a mock log. A size-1
        // tree keeps the fixture simple (empty audit path); the multi-size proof math is exhaustively
        // covered in `transparency`. What this proves is the *wiring* — fetch key/checkpoint/proof,
        // verify the signature, match the record, verify inclusion — plus that a wrong pinned key is
        // rejected.
        use crate::transparency;
        use ed25519_dalek::{Signer, SigningKey};
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();

        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let pub_hex = hex(&sk.verifying_key().to_bytes());
        let record =
            transparency::log_record("acme/imgfx", "1.0.0", "u", "t", "abc", "unsigned", "MIT");
        let root_hex = hex(&transparency::leaf_hash(record.as_bytes())); // size-1 tree: root == leaf
        let sig_hex = hex(&sk
            .sign(format!("noeta-log-checkpoint-v1\n1\n{root_hex}\n").as_bytes())
            .to_bytes());

        let (pk, rk, sg, rec) = (pub_hex.clone(), root_hex.clone(), sig_hex, record.clone());
        let base = mock_server(move |_method, path, _body| match path {
            "/v1/log/key" => (200, format!("{{\"public_key\":\"{pk}\"}}")),
            "/v1/log/checkpoint" => (
                200,
                format!("{{\"tree_size\":1,\"root_hash\":\"{rk}\",\"signature\":\"{sg}\"}}"),
            ),
            "/v1/log/proof/acme/imgfx/1.0.0" => (
                200,
                format!(
                    "{{\"index\":0,\"tree_size\":1,\"root_hash\":\"{rk}\",\"record\":{},\"proof\":[]}}",
                    serde_json::to_string(&rec).unwrap()
                ),
            ),
            _ => (404, "{}".to_string()),
        });
        let index = HttpIndex::new(base).unwrap();

        // First use (no pinned key) adopts the served key and verifies the whole chain.
        let verified = index
            .verify_release_logged("acme/imgfx", "1.0.0", "u", "t", "abc", Some("MIT"), None)
            .unwrap();
        assert_eq!(verified.tree_size, 1);
        assert_eq!(verified.public_key, pub_hex);
        assert_eq!(verified.root_hex, root_hex);
        // A caller that doesn't know the license (lockfile-driven verification) skips that check.
        index
            .verify_release_logged("acme/imgfx", "1.0.0", "u", "t", "abc", None, None)
            .unwrap();

        // The record binds the license: an index serving a *different* license than it logged is
        // caught as equivocation.
        let err = index
            .verify_release_logged(
                "acme/imgfx",
                "1.0.0",
                "u",
                "t",
                "abc",
                Some("GPL-3.0-only"),
                None,
            )
            .unwrap_err();
        assert!(err.contains("license"), "{err}");

        // A wrong pinned log key is rejected — the checkpoint signature won't verify against it.
        let err = index
            .verify_release_logged(
                "acme/imgfx",
                "1.0.0",
                "u",
                "t",
                "abc",
                Some("MIT"),
                Some(&"00".repeat(32)),
            )
            .unwrap_err();
        assert!(err.contains("checkpoint signature"), "{err}");
    }
}
