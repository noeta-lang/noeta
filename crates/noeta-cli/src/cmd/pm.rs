//! The package-manager verbs: `add`/`update`/`publish`/`audit`/`key`/`claim`/`scope`,
//! plus their trust/provenance/registry helpers.

use std::path::PathBuf;
use std::process::ExitCode;

use noeta_pm::{authorship, github, graph, lock, manifest, registry, repo_web_url, reserved};

use crate::output::plural;
use crate::{KeyAction, ScopeAction, compose, docgen};

/// `noeta scope <action>` — manage a registry scope you own (namespace-protection #1).
pub(crate) fn cmd_scope(action: &ScopeAction) -> ExitCode {
    match action {
        ScopeAction::RequireProvenance { scope, root, off } => {
            cmd_scope_require_provenance(scope, root.as_deref(), *off)
        }
    }
}

/// `noeta scope require-provenance <scope> [--root key|keyless] [--off]` — set (or lift) the scope's
/// require-provenance policy via the registry's policy endpoint, authenticated with its publish token.
pub(crate) fn cmd_scope_require_provenance(scope: &str, root: Option<&str>, off: bool) -> ExitCode {
    if let Some(root) = root
        && root != "key"
        && root != "keyless"
    {
        eprintln!("noeta: `--root` must be `key` or `keyless`");
        return ExitCode::from(2);
    }
    if off && root.is_some() {
        eprintln!("noeta: `--root` doesn't apply with `--off` (you're lifting the requirement)");
        return ExitCode::from(2);
    }
    // Route through the project's `[registries]` mapping for this scope (like resolve/publish);
    // env default otherwise.
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: setting a scope policy needs the hosted registry — set `NOETA_REGISTRY_URL` \
                 or map the scope under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    let index = match registry::HttpIndex::new(base) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    // `--off` lifts the requirement; the root only narrows an *on* requirement.
    let require = !off;
    match index.set_scope_policy(scope, require, if off { None } else { root }) {
        Ok(status) => {
            if require {
                let which = root.unwrap_or("any signed");
                println!("{status}: `{scope}` now requires {which} provenance to publish");
            } else {
                println!("{status}: `{scope}` no longer requires provenance");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta claim <scope> [--token T] [--audience A]` — claim a registry scope by proving control of
/// the GitHub org/user of the same name via a GitHub Actions OIDC token (namespace-protection #1).
pub(crate) fn cmd_claim(
    scope: &str,
    token: Option<&str>,
    audience: Option<&str>,
    domain: Option<&str>,
) -> ExitCode {
    // Claiming talks to the hosted registry over HTTP — the one the project's `[registries]`
    // routes this scope to (like resolve/publish), else the environment default.
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: `noeta claim` needs the hosted registry — set `NOETA_REGISTRY_URL` to the \
                 registry you are claiming a scope on, or map the scope under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    let audience = audience
        .map(str::to_string)
        .or_else(|| std::env::var("NOETA_REGISTRY_AUDIENCE").ok())
        .unwrap_or_else(|| "noeta-registry".to_string());

    // Prove ownership. `--domain` claims by proving control of a domain (the registry fetches its
    // well-known file); otherwise prefer an ambient GitHub Actions OIDC token (CI) and fall back to the
    // GitHub OAuth device flow (laptop) — both resolve to the same GitHub identity server-side.
    let proof = match domain {
        Some(domain) => registry::ClaimProof::Domain(domain.to_string()),
        None => match acquire_claim_proof(&audience) {
            Ok(proof) => proof,
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        },
    };

    // The publish token to bind: the one given, or a freshly minted one we print on success.
    let (token, generated) = match token {
        Some(t) => (t.to_string(), false),
        None => match registry::generate_publish_token() {
            Ok(t) => (t, true),
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        },
    };

    let index = match registry::HttpIndex::new(base) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    match index.claim_scope(scope, &token, &proof) {
        Ok(status) => {
            println!("{status}: `{scope}`");
            if generated {
                // The token is a secret the user must keep — print it once, for them to store.
                println!(
                    "\nBound a new publish token to `{scope}`. Save it — `noeta publish` reads it \
                     from `NOETA_REGISTRY_TOKEN`:\n\n    export NOETA_REGISTRY_TOKEN={token}\n"
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// Acquire a proof of GitHub ownership for `noeta claim` (namespace-protection #1): an ambient GitHub
/// Actions OIDC token when running in CI, else the GitHub OAuth **device flow** on a laptop — printing
/// the URL + code and blocking until the user authorizes in their browser.
pub(crate) fn acquire_claim_proof(audience: &str) -> Result<registry::ClaimProof, String> {
    // CI: a GitHub Actions OIDC token for the registry's audience.
    if let Some(jwt) = registry::fetch_github_oidc(audience)? {
        return Ok(registry::ClaimProof::Oidc(jwt));
    }
    // Laptop: the GitHub OAuth device flow. Needs the registry's GitHub OAuth app client id (public —
    // the device flow uses no secret); `NOETA_GITHUB_OAUTH_URL` overrides the endpoint for testing.
    let client_id = std::env::var("NOETA_GITHUB_CLIENT_ID").map_err(|_| {
        "not running in GitHub Actions, and the GitHub OAuth device flow isn't configured — set \
         NOETA_GITHUB_CLIENT_ID to the registry's GitHub OAuth app client id (the registry operator \
         provides it), or run `noeta claim` from a GitHub Actions workflow granting `id-token: write`."
            .to_string()
    })?;
    let oauth_base = std::env::var("NOETA_GITHUB_OAUTH_URL")
        .unwrap_or_else(|_| "https://github.com".to_string());
    let device = github::request_device_code(&oauth_base, &client_id, "read:org")?;
    println!(
        "To authorize this device, open {} and enter the code:\n\n    {}\n\nWaiting for authorization…",
        device.verification_uri, device.user_code
    );
    let token = github::poll_for_token(&oauth_base, &client_id, &device)?;
    Ok(registry::ClaimProof::GithubToken(token))
}

/// `noeta add [key] --path/--git+--tag/--version [--package company/pkg]` — add a dependency to the
/// nearest `noeta.toml`, then resolve so `noeta.lock` reflects it (package-manager P2.4d).
///
/// The import-root `key` may be omitted: it is then **derived** from the package's own root segment
/// (the `package` half of `--package`, or a `--path` dep's `[package]` name). When a key *is* given
/// but differs from the package's declared root, `add` warns — that binding is legitimate (like
/// Cargo's rename) but means `use <key>.…`, not `use <root>.…`. A key that would capture a built-in
/// import root (`std`/`noeta`/`core`) is refused: it would shadow the compiler's own namespace.
pub(crate) fn cmd_add(
    key: Option<&str>,
    path: Option<&std::path::Path>,
    git: Option<&str>,
    tag: Option<&str>,
    version: Option<&str>,
    package: Option<&str>,
) -> ExitCode {
    // `--package` names a registry identity, so it applies only to a `--version` dependency.
    if package.is_some() && version.is_none() {
        eprintln!(
            "noeta: `--package` names a registry identity (`company/package`) — it applies only to a \
             `--version` dependency"
        );
        return ExitCode::from(2);
    }
    // Parse `--package` up front so a malformed identity fails before touching the manifest.
    let package_name = match package {
        Some(s) => match manifest::PackageName::parse(s) {
            Ok(p) => Some(p),
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    // Exactly one source form.
    let value_toml = match (path, git, version) {
        (Some(p), None, None) => format!("{{ path = {} }}", toml_string(&p.display().to_string())),
        (None, Some(url), None) => {
            let Some(tag) = tag else {
                eprintln!(
                    "noeta: `--git` requires `--tag` (sources are git + tagged releases only)"
                );
                return ExitCode::from(2);
            };
            format!(
                "{{ git = {}, tag = {} }}",
                toml_string(url),
                toml_string(tag)
            )
        }
        (None, None, Some(req)) => match &package_name {
            // A registry dependency resolves only with its identity, so fold `--package` into the
            // table form; without it, keep the bare shorthand (it errors at resolve, pointing here).
            Some(p) => format!(
                "{{ version = {}, package = {} }}",
                toml_string(req),
                toml_string(&format!("{}/{}", p.company, p.package))
            ),
            None => toml_string(req),
        },
        (None, None, None) => {
            eprintln!("noeta: give a source — `--path`, `--git` (+ `--tag`), or `--version`");
            return ExitCode::from(2);
        }
        _ => {
            eprintln!("noeta: give exactly one source — `--path`, `--git`, or `--version`");
            return ExitCode::from(2);
        }
    };
    if git.is_none() && tag.is_some() {
        eprintln!("noeta: `--tag` only applies to a `--git` dependency");
        return ExitCode::from(2);
    }

    let manifest_path = match locate_manifest() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    // The package's declared **root segment**, computed cheaply where the identity is known without
    // fetching: `--package`'s `package` half, or a `--path` dep's `[package]` name. `None` for a
    // `--git` (or bare `--version`) source, whose identity isn't known until it is materialized.
    let derived_root: Option<String> = if let Some(p) = &package_name {
        Some(p.package.clone())
    } else if let Some(rel) = path {
        let dep_manifest = manifest_dir.join(rel).join(manifest::MANIFEST_NAME);
        manifest::current_package(&dep_manifest)
            .ok()
            .and_then(|(identity, _)| identity.split('/').nth(1).map(str::to_string))
    } else {
        None
    };

    // The import-root key: the one given, else the derived root, else an error (a `--git` source with
    // no explicit key can't derive one).
    let binding_key = match key {
        Some(k) => k.to_string(),
        None => match &derived_root {
            Some(root) => {
                println!("using import root `{root}` (derived from the package name)");
                root.clone()
            }
            None => {
                eprintln!(
                    "noeta: give an import-root key — `noeta add <key> …` (it can't be derived for \
                     this source; pass `--package company/pkg` for a registry dependency, or name \
                     the key explicitly)"
                );
                return ExitCode::from(2);
            }
        },
    };

    // A key that captures a built-in import root would shadow the compiler's own namespace — refuse
    // it with a direct message (the manifest parser enforces the same invariant defensively).
    if reserved::is_builtin(&binding_key) {
        eprintln!(
            "noeta: `{binding_key}` is a built-in import root (the compiler's own `{binding_key}` \
             namespace) and cannot be bound to a dependency — choose another key"
        );
        return ExitCode::from(2);
    }

    // The pins before this add — so we can flag a newly-pulled dependency authored by a first-time
    // committer (the committer signal). `add_dependency` only edits the manifest, not the lock.
    let old_lock = lock::Lock::read(manifest_dir);
    if let Err(err) = manifest::add_dependency(&manifest_path, &binding_key, &value_toml) {
        eprintln!("noeta: {err}");
        return ExitCode::from(1);
    }
    // Resolve so the new dependency is fetched and the lock is refreshed; a bad URL/tag/path fails
    // here (the manifest edit already succeeded — the entry stays so the user can fix it).
    match graph::resolve_graph(&manifest_path) {
        Ok(resolved) => {
            println!("added `{binding_key}` to {}", manifest_path.display());
            // Now that the package is materialized, its *declared* root is authoritative (this also
            // covers `--git`, whose root wasn't known before). If the chosen key differs, the binding
            // is a deliberate rename — surface it so `use <key>.…` isn't a surprise.
            if let Some(dep) = resolved.packages.iter().find(|p| p.key == binding_key)
                && dep.root != binding_key
            {
                eprintln!(
                    "warning: `{binding_key}` binds a package whose own module root is `{root}` — \
                     imports resolve as `{binding_key}.…`, not `{root}.…`",
                    root = dep.root
                );
            }
            warn_new_committers(&old_lock, &resolved);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: added `{binding_key}`, but resolving it failed: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta update` — discard the current pins and re-resolve, rewriting `noeta.lock` (P2.4d). Removing
/// the lock forces the graph walk to re-`ls-remote` each git tag and re-pin its current commit SHA.
pub(crate) fn cmd_update() -> ExitCode {
    let manifest_path = match locate_manifest() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let dir = manifest_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    // Capture the previous pins *before* discarding the lock, so we can point out which upgrades pull
    // in a new committer (the committer signal) once the graph re-resolves.
    let old_lock = lock::Lock::read(dir);
    let _ = std::fs::remove_file(dir.join(lock::LOCK_NAME));
    match graph::resolve_graph(&manifest_path) {
        Ok(graph) => {
            if graph.locked.is_empty() {
                println!("no dependencies to update");
            } else {
                println!(
                    "updated {} ({} package(s))",
                    lock::LOCK_NAME,
                    graph.locked.len()
                );
            }
            warn_new_committers(&old_lock, &graph);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// Surface the **committer signal** (namespace-protection): after resolving, for each git-sourced
/// dependency whose pinned commit is new or changed relative to `old_lock`, look at the *range* of
/// commits the release introduced and warn — on stderr, best-effort — when it brought in a committer
/// new to that repo (the event-stream / new-maintainer pattern). A release spans several commits, so
/// this reports the whole set of new committers, and links once to the repo (linking each commit would
/// be noise). This is a *soft* signal (git author fields are self-set and forgeable): a prompt to
/// look, never a gate, and never a failure.
pub(crate) fn warn_new_committers(old_lock: &lock::Lock, graph: &graph::ResolvedGraph) {
    for pkg in &graph.locked {
        let graph::ResolvedSource::Git { url, sha, .. } = &pkg.source else {
            continue; // path deps have no upstream history
        };
        let old = old_lock.git_sha(&pkg.identity);
        // Unchanged since last lock → nothing new to review.
        if old == Some(sha.as_str()) {
            continue;
        }
        // `since` is the previously-locked commit for an upgrade; for a fresh add it's absent and
        // `authorship` falls back to the previous release tag to define the range.
        let since = old.filter(|old| *old != sha);
        let Ok(facts) = authorship(url, sha, since) else {
            continue; // best-effort: an unreachable remote / missing git just stays quiet
        };
        if !facts.is_noteworthy() {
            continue;
        }
        eprintln!(
            "⚠ {} {}: this release introduces commits from committer(s) new to this repo:",
            pkg.identity, pkg.version
        );
        for who in &facts.new_committers {
            eprintln!("      {who}");
        }
        let link = repo_web_url(url).unwrap_or_else(|| url.to_string());
        eprintln!("    review before trusting it: {link}");
    }
}

/// `noeta publish --git <url> [--tag <tag>]` — record this package's identity + version → git
/// coordinates in the registry index (package-manager P2.5, client stub). The tag defaults to
/// `v<version>`. Writes to the local/offline index; the hosted registry is operated separately.
pub(crate) fn cmd_publish(
    git: &str,
    tag: Option<&str>,
    force_key: bool,
    interactive: bool,
    oob: bool,
    no_docs: bool,
    no_readme: bool,
) -> ExitCode {
    let manifest_path = match locate_manifest() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let manifest = match manifest::load(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    let Some(pkg) = manifest.package() else {
        eprintln!(
            "noeta: `{}` has no `[package]` table — only a package (with a name + version) can be \
             published",
            manifest_path.display()
        );
        return ExitCode::from(1);
    };
    // A `[patch]` override is dev-only state: it re-points package identities at local trees no
    // consumer has, so a release carrying one could never resolve the same way twice. Refuse up
    // front (before touching git), like the path-dependency lint below.
    if !manifest.patch().is_empty() {
        let ids: Vec<&str> = manifest.patch().keys().map(String::as_str).collect();
        eprintln!(
            "noeta: this manifest has a non-empty `[patch]` table ({}) — a patch is a local \
             dev-time override and must not travel with a release. Remove the `[patch]` table, \
             then publish. Publish aborted.",
            ids.join(", ")
        );
        return ExitCode::from(1);
    }
    let name = format!("{}/{}", pkg.name.company, pkg.name.package);
    let version = pkg.version.clone();
    // The declared license travels with the release into the registry's immutable record (and its
    // transparency-log leaf). Optional — but nudge, since consumers can't legally use an unlicensed
    // package.
    let license = pkg.license.clone();
    if license.is_none() {
        eprintln!(
            "noeta: note: `[package]` declares no `license` — consider `license = \"MIT OR Apache-2.0\"` \
             (an SPDX expression) so consumers know the terms"
        );
    }
    // Discovery keywords ride along into the same record. Unlike the license there is no nudge for
    // an empty set: an untagged package is merely harder to stumble across, not unusable.
    let keywords = pkg.keywords.clone();
    // The one-line search blurb, likewise.
    let description = pkg.description.clone();

    // A published package must depend **only via the registry** (Phase 4, follow-up #3): a path
    // dependency can't travel to a consumer, and a git dependency isn't expressible in the index's
    // (identity, req) shape — so a consumer resolving this release from the index would silently miss
    // it and fail to build. Reject up front (before touching git), naming the offending dependency.
    let mut deps: Vec<registry::Dep> = Vec::new();
    for (key, dep) in manifest.dependencies() {
        // A scope dependency (`para = [ … ]`) publishes as its member packages — each member is
        // subject to the same registry-only rule, so flatten and validate each leaf.
        let leaves: Vec<&manifest::Dependency> = match dep {
            manifest::Dependency::Scope(members) => members.iter().collect(),
            other => vec![other],
        };
        for dep in leaves {
            match dep {
                manifest::Dependency::Registry {
                    package: Some(pkg),
                    req,
                } => deps.push(registry::Dep {
                    package: format!("{}/{}", pkg.company, pkg.package),
                    req: req.clone(),
                }),
                manifest::Dependency::Registry { package: None, .. } => {
                    eprintln!(
                        "noeta: dependency `{key}` is a registry dependency but names no `package = \
                         \"company/pkg\"` — a published package's dependencies must each name their \
                         registry identity."
                    );
                    return ExitCode::from(1);
                }
                manifest::Dependency::Path { .. } | manifest::Dependency::Git { .. } => {
                    eprintln!(
                        "noeta: dependency `{key}` is a path/git dependency — a published package must \
                         depend only via the registry (`{key} = {{ version = \"…\", package = \
                         \"company/pkg\" }}`), so consumers can resolve it. Publish aborted."
                    );
                    return ExitCode::from(1);
                }
                manifest::Dependency::Scope(_) => unreachable!("scopes were flattened above"),
            }
        }
    }

    // Native packages: build the package's own native crate **on this machine** (a composed
    // toolchain, cached) and generate its registry-derived API docs. This doubles as a **publish
    // quality gate** — a native crate that won't compile can't be composed by any consumer, so we
    // refuse to publish it (fail fast, before pinning a SHA / attesting / touching the index). The
    // registry never compiles anything: only the finished `docs.json` is later uploaded.
    let native_docs: Option<String> = match &pkg.native {
        Some(native_dir) => {
            let pkg_dir = manifest_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let crate_dir = pkg_dir.join(native_dir);
            println!(
                "building native crate at `{}` (publish quality gate)…",
                crate_dir.display()
            );
            match compose::package_api_docs(&name, &crate_dir, &pkg.name.package) {
                Ok(api_json) => {
                    // Fold in any `.noe` glue the package also ships (advisory; the API surface wins).
                    let noe_json = docgen::package_docs_json(pkg_dir).ok().map(|(j, _)| j);
                    Some(docgen::finalize_native_docs(
                        &api_json,
                        noe_json.as_deref(),
                        &name,
                        &version.to_string(),
                    ))
                }
                Err(err) => {
                    eprintln!("noeta: native package build failed — not publishing.\n{err}");
                    return ExitCode::from(1);
                }
            }
        }
        None => None,
    };

    let tag = tag
        .map(str::to_string)
        .unwrap_or_else(|| format!("v{version}"));
    // Publish to the registry that OWNS this package's scope: route through the manifest's
    // `[registries]` map exactly like resolution does (private-registries arc), falling back to
    // the environment default only for an unmapped scope. Without this, a project that resolves
    // `acme/*` from a private registry would publish `acme/pkg` to whatever NOETA_REGISTRY_URL
    // points at — leaking a private package to the public registry. A `github:` forge source gets
    // the forge's intentional "publish = push a tag" error instead of a silent mis-publish.
    let scope_source = manifest.registries().source_for(&pkg.name.company);
    if scope_source.is_some() {
        println!(
            "publishing `{name}` via the `[registries]` source for `{}`",
            pkg.name.company
        );
    }
    let index = match registry::open_source(scope_source) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    // Pin the commit SHA at publish time (Phase 4, S2): the index — not just a consumer's lockfile —
    // records "this version = this commit", so a first registry resolve fetches the exact commit and
    // a later tag move is caught. A tag that doesn't resolve is a publish error (nothing to pin).
    let sha = match noeta_pm::resolve_tag_sha(git, &tag) {
        Ok(sha) => sha,
        Err(err) => {
            eprintln!("noeta: cannot resolve `{git}`@`{tag}` to a commit to pin: {err}");
            return ExitCode::from(1);
        }
    };
    let coords = registry::GitCoords {
        url: git.to_string(),
        tag: tag.clone(),
        sha: sha.clone(),
    };
    // Attest the release (Phase 4 #2 / Phase 5) under one of two trust roots:
    //  - **keyless** (preferred when available): an ambient OIDC identity (CI) signs via
    //    Sigstore — ephemeral key, Fulcio cert, transparency log; nothing to steal afterwards.
    //  - **key**: the Ed25519 file from NOETA_SIGNING_KEY / `noeta-signing.key` (`--key` forces
    //    this even in CI).
    // Neither available → publish *unsigned* (a warning — the release resolves, unverified).
    let attestation = noeta_pm::provenance::Attestation {
        name: &name,
        version: &version,
        sha: &sha,
    };
    let ambient = if force_key {
        None
    } else if interactive {
        // Interactive browser login (K6): Sigstore's OAuth signs you in with GitHub/Google/
        // Microsoft and the certified identity is the account email. `--oob` prints the URL
        // and prompts for the code instead of opening a browser.
        match noeta_pm::keyless::interactive_identity(oob) {
            Ok(identity) => Some(identity),
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        }
    } else {
        match noeta_pm::keyless::ambient_identity() {
            Ok(identity) => identity,
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        }
    };
    let (signature, bundle, provenance_tag) = if let Some(identity) = ambient {
        let who = identity.identity().to_string();
        let statement = noeta_pm::keyless::publish_statement(&attestation, &coords);
        let bundle = match noeta_pm::keyless::publish_bundle(statement.as_bytes(), identity) {
            Ok(bundle) => bundle,
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        };
        // Verify our own bundle before uploading it: a publisher should never ship provenance
        // consumers will reject (a broken CA response, a log that didn't include us, …).
        let digest = noeta_pm::keyless::attested_digest(&attestation);
        if let Err(err) = noeta_pm::keyless::verify_bundle(&bundle, &digest, None) {
            eprintln!("noeta: the freshly signed bundle does not verify — not publishing: {err}");
            return ExitCode::from(1);
        }
        (None, Some(bundle), format!("keyless: {who}"))
    } else {
        match provenance_sign(&name, &version, &sha, index.as_ref()) {
            Ok(Some(sig)) => (Some(sig), None, "signed".to_string()),
            Ok(None) => (None, None, "UNSIGNED".to_string()),
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        }
    };
    let release = registry::Release {
        version: version.clone(),
        coords,
        deps,
        // A freshly published release is by definition not yanked.
        yanked: false,
        signature,
        bundle,
        // The registry stamps the publish time server-side; the client doesn't supply it.
        published_at: None,
        license: license.clone(),
        keywords: keywords.clone(),
        description: description.clone(),
    };
    match index.publish(&name, &release) {
        Ok(()) => {
            println!("published `{name}` {version} → {git}#{tag} ({sha}) [{provenance_tag}]");
            // Docs ride along: store the artifact with the release. For a native package it is the
            // already-generated, build-gated API docs (`native_docs`); for a pure-Noeta package it
            // is generated now from the `.noe` source. Advisory metadata — an upload failure warns,
            // never unpublishes a release that already succeeded.
            let pkg_dir = manifest_path
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            if !no_docs {
                let docs = match native_docs {
                    // A native package's docs are pre-generated JSON; count declarations from its
                    // module items (the pure-Noeta path gets an exact count from docgen).
                    Some(json) => {
                        let decls = serde_json::from_str::<serde_json::Value>(&json)
                            .ok()
                            .and_then(|d| d.get("modules").and_then(|m| m.as_array()).cloned())
                            .map_or(0, |mods| {
                                mods.iter()
                                    .map(|m| {
                                        m.get("items")
                                            .and_then(|i| i.as_array())
                                            .map_or(0, Vec::len)
                                    })
                                    .sum()
                            });
                        Ok((json, decls))
                    }
                    None => docgen::package_docs_json(&pkg_dir).map(|(json, g)| (json, g.decls)),
                };
                match docs {
                    Ok((docs_json, decls)) => match index.put_docs(&name, &version, &docs_json) {
                        Ok(()) => {
                            let modules = serde_json::from_str::<serde_json::Value>(&docs_json)
                                .ok()
                                .and_then(|d| {
                                    d.get("modules").and_then(|m| m.as_array()).map(|a| a.len())
                                })
                                .unwrap_or(0);
                            println!(
                                "docs uploaded ({modules} module{}, {decls} declaration{})",
                                plural(modules),
                                plural(decls)
                            );
                        }
                        Err(err) => eprintln!("noeta: warning: docs not uploaded: {err}"),
                    },
                    Err(err) => eprintln!("noeta: warning: docs not generated: {err}"),
                }
            }
            // The README rides along too (rendered on the registry's package page — the registry
            // never fetches source, so the page shows only what we upload). Same posture as docs:
            // advisory, an upload failure warns, and a package without a README publishes silently.
            if !no_readme {
                match std::fs::read_to_string(pkg_dir.join("README.md")) {
                    Ok(readme) if !readme.trim().is_empty() => {
                        match index.put_readme(&name, &version, &readme) {
                            Ok(()) => println!("README uploaded"),
                            Err(err) => eprintln!("noeta: warning: README not uploaded: {err}"),
                        }
                    }
                    _ => {} // no README.md (or an empty one) — nothing to upload
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta audit [path]` — report the dependency tree's trust footprint (package-manager Phase 4, S6):
/// every resolved dependency, its source, and the elevated authority (`native` / `commands`) the root
/// `[trust]` grants make active. Transparency/informed-consent: since an *unauthorized* native
/// dependency fails resolution, a successful audit lists exactly the elevated authority that is live.
pub(crate) fn cmd_audit(path: &std::path::Path) -> ExitCode {
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf()
    };
    let Some(manifest_path) = manifest::find(&start) else {
        eprintln!(
            "noeta: no `{}` found at or above `{}`",
            manifest::MANIFEST_NAME,
            start.display()
        );
        return ExitCode::from(1);
    };
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    // A synthetic entry so `resolve_graph` discovers the manifest from its directory.
    let graph = match graph::resolve_graph(&manifest_dir.join("_.noe")) {
        Ok(graph) => graph,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    let manifest = match manifest::load(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    let trust = manifest.trust();

    println!("Trust audit — {}\n", manifest_path.display());
    if graph.locked.is_empty() {
        println!("  No dependencies.");
        return ExitCode::SUCCESS;
    }

    let mut deps = graph.locked.clone();
    deps.sort_by(|a, b| a.identity.cmp(&b.identity));
    let mut native_count = 0usize;
    let mut command_count = 0usize;
    println!("  Dependencies ({}):", deps.len());
    for pkg in &deps {
        let source = match &pkg.source {
            noeta_pm::graph::ResolvedSource::Path { path } => format!("path {}", path.display()),
            noeta_pm::graph::ResolvedSource::Git { url, git_ref, sha } => {
                format!(
                    "git {url}#{} ({})",
                    git_ref.describe(),
                    &sha[..sha.len().min(9)]
                )
            }
        };
        let mut flags = Vec::new();
        if pkg.native.is_some() {
            native_count += 1;
            flags.push("native");
        }
        if trust.commands.contains(&pkg.identity) {
            command_count += 1;
            flags.push("commands");
        }
        let tail = if flags.is_empty() {
            String::new()
        } else {
            format!("  ⚠ {}", flags.join(" + "))
        };
        println!("    {} {}  [{}]{}", pkg.identity, pkg.version, source, tail);
    }

    println!("\n  Elevated authority (granted in [trust]):");
    println!("    native   : {}", render_trust_list(&trust.native));
    println!("    commands : {}", render_trust_list(&trust.commands));
    println!(
        "\n  {native_count} package(s) run native code, {command_count} may add CLI commands — all \
         authorized (an unauthorized native dependency would have failed resolution)."
    );

    // Provenance (Phase 4 #2 / Phase 5): each scope's pinned trust root. Resolution *enforces*
    // verification (a bad signature/bundle, a changed key/identity, or a downgraded root fails the
    // resolve), so a successful audit means every signed release verified against its pinned root.
    println!("\n  Provenance (pinned trust roots):");
    if graph.scope_trust.is_empty() {
        println!("    (none — no registry dependency carried provenance)");
    } else {
        for (scope, trust) in &graph.scope_trust {
            match trust {
                noeta_pm::lock::ScopeTrust::Key(key) => {
                    println!("    {scope}: key {}…", &key[..key.len().min(16)]);
                }
                noeta_pm::lock::ScopeTrust::Keyless { issuer, identity } => {
                    println!("    {scope}: keyless {identity} (via {issuer})");
                }
            }
        }
        println!(
            "    releases from these scopes verified during resolution; a changed key or identity, \
             a downgraded trust root, or a bad signature/bundle aborts the build."
        );
    }

    // Transparency log (namespace-protection #1): verify each dependency is publicly **included** in
    // the registry's append-only log under a **signed** checkpoint — so a compromised registry can't
    // serve an unlogged forgery without detection. Best-effort and only when a hosted registry is
    // configured; a not-logged/unreachable result is a note (direct git deps aren't logged), not a
    // failure. The pinned key is carried across deps so a registry serving different keys is caught.
    if let Some(base) = std::env::var_os("NOETA_REGISTRY_URL")
        && let Ok(index) = registry::HttpIndex::new(base.to_string_lossy().into_owned())
    {
        println!("\n  Transparency log:");
        let mut pinned: Option<String> = None;
        let mut verified = 0usize;
        for pkg in &deps {
            let noeta_pm::graph::ResolvedSource::Git { url, git_ref, sha } = &pkg.source else {
                continue;
            };
            // Registry releases are tags; the transparency log is keyed by the tag (a non-tag git
            // source isn't registry-logged). `git_ref` generalized `tag` in the private-registries arc.
            let noeta_pm::manifest::GitRef::Tag(tag) = git_ref else {
                continue;
            };
            let version = pkg.version.to_string();
            match index.verify_release_logged(
                &noeta_pm::registry::ReleaseCoords {
                    name: &pkg.identity,
                    version: &version,
                    url,
                    tag,
                    sha,
                    // Resolved deps don't carry a license claim to cross-check — coordinates only.
                    license: None,
                },
                pinned.as_deref(),
            ) {
                Ok(v) => {
                    pinned.get_or_insert(v.public_key);
                    println!(
                        "    {} {}: ✓ included (log size {})",
                        pkg.identity, pkg.version, v.tree_size
                    );
                    verified += 1;
                }
                Err(err) => println!("    {} {}: not verified — {err}", pkg.identity, pkg.version),
            }
        }
        if verified == 0 {
            println!("    (no dependencies are recorded in this registry's transparency log)");
        } else {
            println!(
                "    included releases were verified against the log's signed checkpoint — the \
                 registry cannot serve an unlogged release without detection."
            );
        }
    }

    // Security advisories (namespace-protection #1, advisory feed): cross-reference every resolved
    // dependency against the registry's signed advisory database, flagging any pinned to a version with
    // a known vulnerability or a known-malicious release. Best-effort (needs a hosted registry); a
    // matched *active* advisory makes the audit exit non-zero so CI catches it. The advisory key + feed
    // head are pinned trust-on-first-use in the lock, so a later feed whose count shrank is surfaced as
    // a possible rollback (a withheld advisory).
    let mut advisory_hits = 0usize;
    let mut advisory_fails = 0usize;
    if let Ok(Some(index)) = registry::open_http() {
        println!("\n  Security advisories:");
        let old = lock::Lock::read(&manifest_dir).advisory_trust().cloned();
        match index.fetch_advisories(old.as_ref().map(|a| a.public_key.as_str())) {
            Ok(feed) => {
                if let Some(prev) = &old
                    && feed.count < prev.count as usize
                {
                    println!(
                        "    ⚠ the advisory feed shrank ({} → {}) since the last audit — an advisory \
                         may have been withdrawn upstream, or the registry may be rolling the feed back.",
                        prev.count, feed.count
                    );
                }
                for pkg in &deps {
                    for a in &feed.advisories {
                        if a.is_active() && a.package == pkg.identity && a.affects(&pkg.version) {
                            // Per-tier policy (advisory-intake arc, tier 5): `off` skips entirely,
                            // `warn` prints, `fail` prints and fails the audit.
                            let action = trust.advisories.action_for(a.tier.as_str());
                            if action == noeta_pm::manifest::AdvisoryAction::Off {
                                continue;
                            }
                            let fails = action == noeta_pm::manifest::AdvisoryAction::Fail;
                            advisory_hits += 1;
                            if fails {
                                advisory_fails += 1;
                            }
                            let url = if a.url.is_empty() {
                                String::new()
                            } else {
                                format!("  <{}>", a.url)
                            };
                            // Publisher-tier advisories carry a keyless bundle attributing them to the
                            // scope owner's OIDC identity — verify it offline against the scope's pinned
                            // identity (from resolve-time `scope_trust`) so a compromised registry can't
                            // fabricate an owner-issued advisory. Operator/imported tiers carry no bundle.
                            let prov = advisory_provenance_note(a, &graph.scope_trust);
                            let severity = advisory_severity_display(a);
                            let marker = if fails { "✗" } else { "⚠" };
                            println!(
                                "    {marker} {} {}: [{}/{}] {} ({}){}{}",
                                pkg.identity,
                                pkg.version,
                                a.tier,
                                severity,
                                a.summary,
                                a.id,
                                url,
                                prov
                            );
                        }
                    }
                }
                if advisory_hits == 0 {
                    println!(
                        "    no advisories affect the {} resolved dependencies (checked {} signed \
                         advisor{} across the operator/publisher/imported tiers against the pinned \
                         feed key).",
                        deps.len(),
                        feed.count,
                        if feed.count == 1 { "y" } else { "ies" }
                    );
                }
                // Advisory-log binding (namespace-protection #1): verify each served advisory is
                // **included** in the registry's transparency log at its signed checkpoint — so an
                // advisory is provably in the public, append-only log, not fabricated (or a real one
                // suppressed) for this consumer. Uses the log key pinned at resolve time when present.
                let pinned_log = graph.log_trust.as_ref().map(|l| l.public_key.as_str());
                match index.verify_advisories_logged(&feed.advisories, pinned_log) {
                    Ok(Some((n, unlogged))) if !unlogged.is_empty() => println!(
                        "    ⚠ {n} advisor{} publicly logged, but {} not in the transparency log: {}",
                        if n == 1 { "y" } else { "ies" },
                        unlogged.len(),
                        unlogged.join(", ")
                    ),
                    Ok(Some((n, _))) if n > 0 => println!(
                        "    {n} advisor{} verified as included in the transparency log (the registry \
                         can't fabricate or silently drop a logged advisory).",
                        if n == 1 { "y" } else { "ies" }
                    ),
                    Ok(Some(_)) => {}
                    Ok(None) => {} // this registry runs no transparency log
                    Err(err) => println!("    ⚠ advisory-log verification failed — {err}"),
                }
                // Pin (or refresh) the verified advisory-feed head, trust-on-first-use.
                let pin = lock::AdvisoryTrust {
                    public_key: feed.public_key,
                    count: feed.count as u64,
                    digest: feed.digest,
                };
                let _ = lock::write(
                    &manifest_dir,
                    &graph.locked,
                    &graph.scope_trust,
                    graph.log_trust.as_ref(),
                    Some(&pin),
                );
            }
            Err(err) => println!("    not checked — {err}"),
        }
    }

    // A matched advisory fails the audit only when its tier's `[trust.advisories]` policy says `fail`
    // (default: every tier warns). Warnings are printed above but never break the build.
    if advisory_fails > 0 {
        eprintln!(
            "\n{advisory_fails} known-vulnerable or known-malicious dependenc{} in the graph at a \
             fail-level advisory tier — see the advisories marked ✗ above.",
            if advisory_fails == 1 { "y" } else { "ies" }
        );
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// The severity cell for a matched advisory in the audit report. When the advisory carries a CVSS
/// vector (imported tier, residual b), the base score is re-derived from that vector *client-side* and
/// shown alongside the band — `high (CVSS 7.8)` — so the number behind the band is visible and honestly
/// recomputed, not taken on the registry's word. A band that disagrees with the vector's own band is
/// flagged, since the signed band is the trusted decision and a mismatch is worth seeing. No vector →
/// just the band.
fn advisory_severity_display(advisory: &noeta_pm::advisory::Advisory) -> String {
    let Some(vector) = advisory.cvss.as_deref() else {
        return advisory.severity.clone();
    };
    match noeta_pm::cvss::score_vector(vector) {
        Some((score, band)) => {
            if band.as_str() == advisory.severity {
                format!("{} (CVSS {score:.1})", advisory.severity)
            } else {
                // The signed band and the vector's computed band differ — show both.
                format!(
                    "{} (CVSS {score:.1} → {})",
                    advisory.severity,
                    band.as_str()
                )
            }
        }
        None => advisory.severity.clone(),
    }
}

/// The provenance note appended to a matched advisory in the audit report (advisory-intake arc). For a
/// **publisher**-tier advisory, verify its keyless bundle offline against the scope's pinned identity
/// (from resolve-time `scope_trust`): a verified owner attestation is the strong signal that the scope
/// owner really issued it. Operator/imported tiers carry no bundle — an empty note. A verification
/// failure is surfaced inline (never silently trusted).
fn advisory_provenance_note(
    advisory: &noeta_pm::advisory::Advisory,
    scope_trust: &std::collections::BTreeMap<String, noeta_pm::lock::ScopeTrust>,
) -> String {
    use noeta_pm::advisory::AdvisoryTier;
    match advisory.tier {
        AdvisoryTier::Imported => {
            // The upstream link is the provenance for an imported advisory.
            match &advisory.upstream_id {
                Some(id) => format!("  [imported from {id}]"),
                None => String::new(),
            }
        }
        AdvisoryTier::Operator => String::new(),
        AdvisoryTier::Publisher => {
            let Some(bundle) = &advisory.bundle else {
                return "  [publisher advisory carries no bundle — unverifiable]".to_string();
            };
            let digest = noeta_pm::keyless::advisory_attested_digest(&advisory.canonical_bytes());
            // Pin against the scope's keyless identity when we have one; otherwise verify the bundle
            // stands on its own (identity reported, trust-on-first-use).
            let scope = advisory
                .package
                .split('/')
                .next()
                .unwrap_or(&advisory.package);
            let policy = match scope_trust.get(scope) {
                Some(noeta_pm::lock::ScopeTrust::Keyless { issuer, identity }) => {
                    Some(noeta_pm::keyless::IdentityPolicy {
                        issuer: issuer.clone(),
                        identity: identity.clone(),
                    })
                }
                _ => None,
            };
            match noeta_pm::keyless::verify_bundle(bundle, &digest, policy.as_ref()) {
                Ok(id) => format!("  [publisher-verified: {}]", id.identity),
                Err(err) => format!(
                    "  [publisher bundle FAILED verification: {}]",
                    err.message()
                ),
            }
        }
    }
}

/// `noeta advisory publish <id> <package> <ranges> <severity> <summary>` — issue (or update) a
/// **publisher**-tier advisory for a package in a scope you own (advisory-intake arc, tier 2). The
/// advisory is keyless-signed with your OIDC identity (ambient CI, or `--interactive`), so consumers
/// verify it offline against your scope's pinned identity; it is sent authenticated with the scope's
/// publish token (`NOETA_REGISTRY_TOKEN`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_advisory_publish(
    id: &str,
    package: &str,
    ranges: &str,
    severity: &str,
    summary: &str,
    details: Option<&str>,
    url: Option<&str>,
    patched: Option<&str>,
    withdraw: bool,
    interactive: bool,
    oob: bool,
) -> ExitCode {
    if !matches!(severity, "low" | "medium" | "high" | "critical") {
        eprintln!("noeta: `severity` must be one of low, medium, high, critical");
        return ExitCode::from(2);
    }
    let Some((scope, _)) = package.split_once('/') else {
        eprintln!("noeta: `package` must be `company/package`");
        return ExitCode::from(2);
    };
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: publishing an advisory needs the hosted registry — set `NOETA_REGISTRY_URL` \
                 or map `{scope}` under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };

    // The advisory content, tier=publisher. Its canonical bytes are what the keyless bundle attests and
    // what the registry re-signs and logs.
    let advisory = noeta_pm::advisory::Advisory {
        id: id.to_string(),
        package: package.to_string(),
        ranges: ranges.to_string(),
        patched: patched.map(str::to_string),
        severity: severity.to_string(),
        summary: summary.to_string(),
        details: details.unwrap_or("").to_string(),
        url: url.unwrap_or("").to_string(),
        withdrawn: withdraw,
        seq: 0,
        signature: String::new(),
        log_index: None,
        tier: noeta_pm::advisory::AdvisoryTier::Publisher,
        bundle: None,
        upstream_id: None,
        upstream_url: None,
        cvss: None,
    };
    let (bundle, who) = match sign_advisory_keyless(&advisory, interactive, oob) {
        Ok(pair) => pair,
        Err(code) => return code,
    };

    let index = match registry::HttpIndex::new(base) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    match index.publish_scope_advisory(scope, &advisory, &bundle) {
        Ok(status) => {
            let verb = if withdraw { "withdrew" } else { "published" };
            println!("{status}: {verb} publisher advisory `{id}` for `{package}` (keyless: {who})");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta advisory report <package> <summary>` — file a **public report** against a package
/// (advisory-intake arc, tier 4). Unauthenticated + rate-limited; a report is not an advisory — it is
/// queued for an operator or the scope owner to triage.
pub(crate) fn cmd_advisory_report(
    package: &str,
    summary: &str,
    ranges: Option<&str>,
    details: Option<&str>,
    url: Option<&str>,
    reporter: Option<&str>,
) -> ExitCode {
    let Some((scope, _)) = package.split_once('/') else {
        eprintln!("noeta: `package` must be `company/package`");
        return ExitCode::from(2);
    };
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: filing a report needs the hosted registry — set `NOETA_REGISTRY_URL` or map \
                 `{scope}` under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    let index = match registry::HttpIndex::new(base) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    match index.file_report(package, summary, ranges, details, url, reporter) {
        Ok(id) => {
            println!("report filed against `{package}` (id {id})");
            println!(
                "  a report is not an advisory — it is queued for triage; an operator or the scope \
                 owner may promote it."
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// Keyless-sign an advisory's canonical bytes (the shared publisher-tier flow behind `advisory publish`
/// and the scope-owner path of `advisory promote`): acquire the OIDC identity (ambient CI, or an
/// interactive browser login), attest over the advisory's canonical bytes, and verify the bundle locally
/// before it leaves the machine — never ship an advisory a consumer would reject. Returns `(bundle,
/// signing-identity)`. `advisory.tier` MUST already be `Publisher` so the attested canonical bytes match
/// what the registry re-signs.
fn sign_advisory_keyless(
    advisory: &noeta_pm::advisory::Advisory,
    interactive: bool,
    oob: bool,
) -> Result<(String, String), ExitCode> {
    let canonical = advisory.canonical_bytes();
    let identity = if interactive {
        noeta_pm::keyless::interactive_identity(oob).map_err(|err| {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        })?
    } else {
        match noeta_pm::keyless::ambient_identity() {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                eprintln!(
                    "noeta: no ambient OIDC identity found — run under CI with `id-token: write`, or \
                     use `--interactive` to sign in via the browser"
                );
                return Err(ExitCode::from(1));
            }
            Err(err) => {
                eprintln!("noeta: {err}");
                return Err(ExitCode::from(1));
            }
        }
    };
    let who = identity.identity().to_string();
    let statement =
        noeta_pm::keyless::advisory_statement(&advisory.id, &advisory.package, &canonical);
    let bundle =
        noeta_pm::keyless::publish_bundle(statement.as_bytes(), identity).map_err(|err| {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        })?;
    let digest = noeta_pm::keyless::advisory_attested_digest(&canonical);
    if let Err(err) = noeta_pm::keyless::verify_bundle(&bundle, &digest, None) {
        eprintln!(
            "noeta: the freshly signed advisory bundle does not verify — not publishing: {err}"
        );
        return Err(ExitCode::from(1));
    }
    Ok((bundle, who))
}

/// Resolve the registry client for a report triage/promote verb (advisory-intake residual a). The base
/// URL comes from `--scope`'s `[registries]` routing (scope-owner path) or `NOETA_REGISTRY_URL` (operator
/// path). The bearer token is the scope publish token (`NOETA_REGISTRY_TOKEN`, the default) unless
/// `operator` is set, which swaps in the admin token (`NOETA_REGISTRY_ADMIN_TOKEN`).
fn report_index(scope: Option<&str>, operator: bool) -> Result<registry::HttpIndex, ExitCode> {
    let base = match scope {
        Some(s) => match scope_registry_base(s) {
            Ok(Some(base)) => base,
            Ok(None) => {
                eprintln!(
                    "noeta: this needs the hosted registry — set `NOETA_REGISTRY_URL` or map `{s}` \
                     under `[registries]`"
                );
                return Err(ExitCode::from(2));
            }
            Err(code) => return Err(code),
        },
        None => match std::env::var("NOETA_REGISTRY_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("noeta: this needs the hosted registry — set `NOETA_REGISTRY_URL`");
                return Err(ExitCode::from(2));
            }
        },
    };
    let index = registry::HttpIndex::new(base).map_err(|err| {
        eprintln!("noeta: {err}");
        ExitCode::from(1)
    })?;
    if operator {
        match std::env::var("NOETA_REGISTRY_ADMIN_TOKEN") {
            Ok(token) if !token.is_empty() => Ok(index.with_token(Some(token))),
            _ => {
                eprintln!(
                    "noeta: an operator action needs the admin token — set `NOETA_REGISTRY_ADMIN_TOKEN`"
                );
                Err(ExitCode::from(2))
            }
        }
    } else {
        Ok(index)
    }
}

/// `noeta advisory reports [--scope S]` — list the reports queued for triage (advisory-intake residual
/// a). Without `--scope`, the operator queue (admin token); with it, the scope owner's own queue. Shows
/// the promotable (`pending`) reports by default.
pub(crate) fn cmd_advisory_reports(scope: Option<&str>, status: Option<&str>) -> ExitCode {
    // A scope owner authenticates with their scope token (the default); the operator queue needs admin.
    let index = match report_index(scope, scope.is_none()) {
        Ok(index) => index,
        Err(code) => return code,
    };
    let reports = match index.list_reports(scope, status) {
        Ok(reports) => reports,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    let scope_note = scope
        .map(|s| format!(" for scope `{s}`"))
        .unwrap_or_default();
    let status_note = status.map(|s| format!(" ({s})")).unwrap_or_default();
    if reports.is_empty() {
        println!("no reports{scope_note}{status_note}");
        return ExitCode::SUCCESS;
    }
    println!("reports{scope_note}{status_note}:");
    for r in &reports {
        let ranges = r.ranges.as_deref().filter(|s| !s.is_empty());
        let range_note = ranges.map(|s| format!(" [{s}]")).unwrap_or_default();
        let advisory_note = r
            .advisory_id
            .as_deref()
            .map(|a| format!(" → advisory `{a}`"))
            .unwrap_or_default();
        println!(
            "  {} {} {}{}: {}{}",
            r.id, r.status, r.package, range_note, r.summary, advisory_note
        );
    }
    println!(
        "\npromote one with `noeta advisory promote <report-id> --id <advisory-id> --severity <sev>`."
    );
    ExitCode::SUCCESS
}

/// `noeta advisory promote <report-id>` — promote a queued report into a signed advisory (advisory-intake
/// residual a). The advisory is prefilled from the report and finalised with the triaged `--id` and
/// `--severity`. As an operator (`--operator`, admin token) it is an `operator`-tier advisory; otherwise
/// the report package's scope owner promotes it into a keyless-signed `publisher`-tier advisory — the
/// exact same keyless flow a fresh `advisory publish` runs, prefilled from the report.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_advisory_promote(
    report_id: &str,
    id: &str,
    severity: &str,
    ranges: Option<&str>,
    summary: Option<&str>,
    details: Option<&str>,
    url: Option<&str>,
    patched: Option<&str>,
    operator: bool,
    interactive: bool,
    oob: bool,
) -> ExitCode {
    if !matches!(severity, "low" | "medium" | "high" | "critical") {
        eprintln!("noeta: `severity` must be one of low, medium, high, critical");
        return ExitCode::from(2);
    }
    // The promote base URL comes from `NOETA_REGISTRY_URL` (we don't yet know the report's scope), and
    // the token is the admin token for `--operator`, else the scope publish token.
    let index = match report_index(None, operator) {
        Ok(index) => index,
        Err(code) => return code,
    };

    // Fetch the report to prefill the advisory from it.
    let report = match index.get_report(report_id) {
        Ok(Some(report)) => report,
        Ok(None) => {
            eprintln!("noeta: report `{report_id}` not found (or you can't triage it)");
            return ExitCode::from(1);
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };

    let ranges = ranges
        .map(str::to_string)
        .or_else(|| report.ranges.clone().filter(|s| !s.is_empty()));
    let Some(ranges) = ranges.filter(|s| !s.is_empty()) else {
        eprintln!(
            "noeta: the report carries no affected range — supply one with `--ranges` (an advisory \
             needs a non-empty SemVer requirement)"
        );
        return ExitCode::from(2);
    };
    let summary = summary
        .map(str::to_string)
        .or_else(|| Some(report.summary.clone()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| report.package.clone());
    let details = details
        .map(str::to_string)
        .or_else(|| report.details.clone())
        .unwrap_or_default();
    let url = url
        .map(str::to_string)
        .or_else(|| report.url.clone())
        .unwrap_or_default();

    // The prepared advisory. The tier decides the canonical bytes the scope-owner bundle attests, so set
    // it before signing: operator → operator-tier (no bundle); scope owner → publisher-tier (keyless).
    let advisory = noeta_pm::advisory::Advisory {
        id: id.to_string(),
        package: report.package.clone(),
        ranges,
        patched: patched.map(str::to_string),
        severity: severity.to_string(),
        summary,
        details,
        url,
        withdrawn: false,
        seq: 0,
        signature: String::new(),
        log_index: None,
        tier: if operator {
            noeta_pm::advisory::AdvisoryTier::Operator
        } else {
            noeta_pm::advisory::AdvisoryTier::Publisher
        },
        bundle: None,
        upstream_id: None,
        upstream_url: None,
        cvss: None,
    };

    // The operator path sends no bundle (an operator advisory); the scope-owner path keyless-signs the
    // advisory (prefilled from the report) exactly as `advisory publish` would.
    let (bundle, who) = if operator {
        (None, None)
    } else {
        match sign_advisory_keyless(&advisory, interactive, oob) {
            Ok((bundle, who)) => (Some(bundle), Some(who)),
            Err(code) => return code,
        }
    };

    match index.promote_report(report_id, &advisory, bundle.as_deref()) {
        Ok(status) => {
            let tier = if operator { "operator" } else { "publisher" };
            match who {
                Some(who) => println!(
                    "{status}: promoted report `{report_id}` into {tier} advisory `{id}` for \
                     `{}` (keyless: {who})",
                    advisory.package
                ),
                None => println!(
                    "{status}: promoted report `{report_id}` into {tier} advisory `{id}` for `{}`",
                    advisory.package
                ),
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
        }
    }
}

/// Render a `[trust]` identity set for the audit report (`(none)` when empty).
pub(crate) fn render_trust_list(set: &std::collections::BTreeSet<String>) -> String {
    if set.is_empty() {
        "(none)".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// Sign a release attestation for `noeta publish` (Phase 4, #2), returning the hex signature — or
/// `None` (with a warning) when no signing key is configured, so publishing stays possible while
/// provenance is adopted gradually. Reads the private key from `NOETA_SIGNING_KEY` (a path) or
/// `noeta-signing.key`. When it signs, it also registers the scope's public key with the index (a
/// no-op for the hosted registry, which registers keys via its admin endpoint).
pub(crate) fn provenance_sign(
    name: &str,
    version: &semver::Version,
    sha: &str,
    index: &dyn registry::Index,
) -> Result<Option<String>, String> {
    let key_path = std::env::var_os("NOETA_SIGNING_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("noeta-signing.key"));
    if !key_path.is_file() {
        eprintln!(
            "noeta: no signing key at `{}` — publishing UNSIGNED (consumers can't verify provenance). \
             Sign keyless with `noeta publish --interactive` (browser login), or run \
             `noeta key new` and set NOETA_SIGNING_KEY.",
            key_path.display()
        );
        return Ok(None);
    }
    let private_hex = std::fs::read_to_string(&key_path)
        .map_err(|err| format!("cannot read signing key `{}`: {err}", key_path.display()))?
        .trim()
        .to_string();

    let attestation = noeta_pm::provenance::Attestation { name, version, sha };
    let signature = noeta_pm::provenance::sign(&attestation, &private_hex)?;
    // Register this scope's public key so a consumer can verify (local index writes it; the hosted
    // registry no-ops — it registers keys via admin, enforcing scope ownership).
    let scope = name.split('/').next().unwrap_or(name);
    let public_hex = noeta_pm::provenance::public_key_hex(&private_hex)?;
    index.set_scope_key(scope, &public_hex)?;
    Ok(Some(signature))
}

/// `noeta key new` — generate an Ed25519 signing keypair (package-manager Phase 4, #2). The private
/// key is written to a file (kept secret); the public key is printed to register with the registry
/// scope. `noeta publish` signs releases with the private key; consumers verify with the public one.
pub(crate) fn cmd_key(action: &KeyAction) -> ExitCode {
    let KeyAction::New { out } = action;
    let keypair = match noeta_pm::provenance::generate_keypair() {
        Ok(keypair) => keypair,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = write_private_key(out, &keypair.private_hex) {
        eprintln!("noeta: cannot write `{}`: {err}", out.display());
        return ExitCode::from(1);
    }
    println!(
        "wrote private signing key to {} (keep it secret)",
        out.display()
    );
    println!("public key — register this with your registry scope:");
    println!("  {}", keypair.public_hex);
    println!(
        "`noeta publish` reads the private key from NOETA_SIGNING_KEY (a path) or `{}`.",
        out.display()
    );
    ExitCode::SUCCESS
}

/// Write a private key to `path`, `0600` on unix (a signing key must not be world-readable).
pub(crate) fn write_private_key(path: &std::path::Path, private_hex: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{private_hex}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Find the nearest `noeta.toml` from the current directory, or print a diagnostic and return a
/// non-zero exit code (shared by `noeta add`/`update`).
pub(crate) fn locate_manifest() -> Result<PathBuf, ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    manifest::find(&cwd).ok_or_else(|| {
        eprintln!(
            "noeta: no `{}` found at or above `{}`",
            manifest::MANIFEST_NAME,
            cwd.display()
        );
        ExitCode::from(1)
    })
}

/// The hosted-registry base URL a scope-management command (`claim`, `scope require-provenance`)
/// should talk to for `scope`: the enclosing project's `[registries]` mapping when it routes the
/// scope to a hosted URL — the same routing resolution and publish follow — else the
/// `NOETA_REGISTRY_URL` environment default. A scope mapped to a git forge is a hard error (a
/// forge has no claim/policy endpoints), not a silent fall-through to the wrong registry.
pub(crate) fn scope_registry_base(scope: &str) -> Result<Option<String>, ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(path) = manifest::find(&cwd)
        && let Ok(manifest) = manifest::load(&path)
    {
        match manifest.registries().source_for(scope) {
            Some(manifest::RegistrySource::Hosted(url)) => return Ok(Some(url.clone())),
            Some(manifest::RegistrySource::GitForge(base)) => {
                eprintln!(
                    "noeta: `{}` routes `{scope}` to the git forge `{base}` — a forge scope is \
                     claimed by owning the org/user there, and has no registry policy endpoint",
                    path.display()
                );
                return Err(ExitCode::from(2));
            }
            None => {}
        }
    }
    Ok(std::env::var_os("NOETA_REGISTRY_URL").map(|v| v.to_string_lossy().into_owned()))
}

/// Quote a string as a TOML basic string for a manifest value we write (`noeta add`).
pub(crate) fn toml_string(s: &str) -> String {
    noeta_pm::toml_quote(s)
}

/// The pinned state `noeta watch-scope` carries between runs (advisory-intake arc, tier 6): the keys it
/// trusts (advisory feed + transparency log), the last checkpoint it saw, and the set of advisory ids
/// it has ever seen for the scope. Persisted as TOML so a later run can prove the log only grew
/// (append-only) and no previously-seen advisory silently vanished.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct WatchState {
    /// The registry base this state is for — a different base resets the watch (a fresh trust anchor).
    base: String,
    advisory_key: Option<String>,
    log_key: Option<String>,
    log_tree_size: Option<u64>,
    log_root: Option<String>,
    feed_count: Option<u64>,
    /// Advisory ids ever seen for this scope, so a disappearance is a suppression (a withdrawn advisory
    /// stays in the feed with `withdrawn=true`, so it never counts as disappeared).
    #[serde(default)]
    seen: Vec<String>,
}

/// `noeta watch-scope <scope>` — the transparency-log suppression monitor (advisory-intake arc,
/// tier 6). Verifies, against the state pinned by the previous run, that the registry's advisory log is
/// an append-only extension (no history rewrite) and that no advisory previously seen for the scope has
/// disappeared from the feed (silent suppression). A detected rewrite, key change, feed rollback, or
/// disappearance exits non-zero; the first run establishes the baseline. Ideal as a CI cron.
pub(crate) fn cmd_watch_scope(scope: &str, state_path: Option<&std::path::Path>) -> ExitCode {
    use noeta_pm::transparency;

    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: watch-scope needs the hosted registry — set `NOETA_REGISTRY_URL` or map \
                 `{scope}` under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    let state_file = match state_path {
        Some(p) => p.to_path_buf(),
        None => match noeta_cache::Cache::locate() {
            Some(dir) => dir.join("watch").join(format!("{scope}.toml")),
            None => {
                eprintln!("noeta: cannot locate a state directory — pass `--state <path>`");
                return ExitCode::from(2);
            }
        },
    };
    // Load prior state, but only if it is for this same base (a different registry is a fresh anchor).
    let prior: Option<WatchState> = std::fs::read_to_string(&state_file)
        .ok()
        .and_then(|t| toml::from_str::<WatchState>(&t).ok())
        .filter(|s| s.base == base);

    let index = match registry::HttpIndex::new(base.clone()) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };

    println!("watch-scope `{scope}` — {base}");
    let mut drift = 0usize;

    // 1) The advisory feed, verified against the pinned advisory key (TOFU). This checks every
    //    signature and the signed head against exactly the served advisories.
    let feed = match index.fetch_advisories(prior.as_ref().and_then(|p| p.advisory_key.as_deref()))
    {
        Ok(feed) => feed,
        Err(err) => {
            eprintln!("noeta: advisory feed did not verify — {err}");
            return ExitCode::from(1);
        }
    };
    let scope_prefix = format!("{scope}/");
    let current_ids: std::collections::BTreeSet<String> = feed
        .advisories
        .iter()
        .filter(|a| a.package.starts_with(&scope_prefix))
        .map(|a| a.id.clone())
        .collect();

    // 2) Suppression: an advisory previously seen for the scope that is no longer in the feed.
    if let Some(prev) = &prior {
        for id in &prev.seen {
            if !current_ids.contains(id) {
                drift += 1;
                println!(
                    "    ✗ advisory `{id}` (previously seen for `{scope}`) has DISAPPEARED from the feed"
                );
            }
        }
        if let Some(prev_count) = prev.feed_count
            && (feed.count as u64) < prev_count
        {
            drift += 1;
            println!(
                "    ✗ the advisory feed shrank ({prev_count} → {}) — a possible rollback",
                feed.count
            );
        }
    }

    // 3) The transparency log: verify the checkpoint signature (against the pinned key, TOFU) and that
    //    the log is an append-only extension of the previously pinned checkpoint.
    let mut new_log_key = prior.as_ref().and_then(|p| p.log_key.clone());
    let mut new_tree_size = prior.as_ref().and_then(|p| p.log_tree_size);
    let mut new_root = prior.as_ref().and_then(|p| p.log_root.clone());
    match index.log_public_key() {
        Ok(Some(served_key)) => {
            if let Some(pinned) = prior.as_ref().and_then(|p| p.log_key.as_deref())
                && pinned != served_key
            {
                drift += 1;
                println!(
                    "    ✗ the transparency-log signing key changed since the last run — possible equivocation"
                );
            }
            let log_key = prior
                .as_ref()
                .and_then(|p| p.log_key.clone())
                .unwrap_or(served_key);
            match index.log_checkpoint() {
                Ok(cp) => {
                    match transparency::verify_checkpoint(
                        &log_key,
                        cp.tree_size,
                        &cp.root_hash,
                        &cp.signature,
                    ) {
                        Ok(true) => {
                            // Append-only check against the pinned checkpoint.
                            if let (Some(prev_size), Some(prev_root)) = (
                                prior.as_ref().and_then(|p| p.log_tree_size),
                                prior.as_ref().and_then(|p| p.log_root.clone()),
                            ) {
                                if cp.tree_size < prev_size {
                                    drift += 1;
                                    println!(
                                        "    ✗ the transparency log SHRANK ({prev_size} → {}) — history was rewritten",
                                        cp.tree_size
                                    );
                                } else if cp.tree_size > prev_size {
                                    match verify_log_extension(
                                        &index,
                                        prev_size,
                                        cp.tree_size,
                                        &prev_root,
                                        &cp.root_hash,
                                    ) {
                                        Ok(true) => println!(
                                            "    ✓ transparency log extended append-only ({prev_size} → {})",
                                            cp.tree_size
                                        ),
                                        Ok(false) => {
                                            drift += 1;
                                            println!(
                                                "    ✗ the transparency log is NOT an append-only extension of the pinned checkpoint — history was rewritten"
                                            );
                                        }
                                        Err(err) => println!(
                                            "    ⚠ could not verify log consistency — {err}"
                                        ),
                                    }
                                }
                            } else {
                                println!(
                                    "    ✓ transparency-log checkpoint verified (baseline pinned at size {})",
                                    cp.tree_size
                                );
                            }
                            new_log_key = Some(log_key);
                            new_tree_size = Some(cp.tree_size);
                            new_root = Some(cp.root_hash);
                        }
                        Ok(false) => {
                            drift += 1;
                            println!(
                                "    ✗ the transparency-log checkpoint signature does not verify against the pinned key"
                            );
                        }
                        Err(err) => println!("    ⚠ checkpoint verification error — {err}"),
                    }
                }
                Err(err) => {
                    println!("    ⚠ could not fetch the transparency-log checkpoint — {err}")
                }
            }
        }
        Ok(None) => println!("    (this registry runs no transparency log)"),
        Err(err) => println!("    ⚠ could not fetch the transparency-log key — {err}"),
    }

    // 4) Inclusion: every advisory the feed serves for this scope is provably in the log.
    let scope_advisories: Vec<_> = feed
        .advisories
        .iter()
        .filter(|a| a.package.starts_with(&scope_prefix))
        .cloned()
        .collect();
    match index.verify_advisories_logged(&scope_advisories, new_log_key.as_deref()) {
        Ok(Some((n, unlogged))) if !unlogged.is_empty() => {
            drift += 1;
            println!(
                "    ✗ {} of `{scope}`'s advisories are NOT in the transparency log: {}",
                unlogged.len(),
                unlogged.join(", ")
            );
            let _ = n;
        }
        Ok(Some((n, _))) if n > 0 => {
            println!(
                "    ✓ {n} of `{scope}`'s advisories verified as included in the transparency log"
            )
        }
        _ => {}
    }

    // Persist the refreshed state.
    let next = WatchState {
        base,
        advisory_key: Some(feed.public_key.clone()),
        log_key: new_log_key,
        log_tree_size: new_tree_size,
        log_root: new_root,
        feed_count: Some(feed.count as u64),
        seen: current_ids.into_iter().collect(),
    };
    if let Some(parent) = state_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match toml::to_string_pretty(&next) {
        Ok(text) => {
            if let Err(err) = std::fs::write(&state_file, text) {
                eprintln!(
                    "noeta: could not write watch state `{}`: {err}",
                    state_file.display()
                );
            }
        }
        Err(err) => eprintln!("noeta: could not serialize watch state: {err}"),
    }

    if drift > 0 {
        eprintln!(
            "\n{drift} advisory-log integrity problem(s) detected for `{scope}` — see the ✗ lines above."
        );
        return ExitCode::from(1);
    }
    println!("  no suppression or rewrite detected for `{scope}`.");
    ExitCode::SUCCESS
}

/// Verify the transparency log at `to_size`/`to_root` is an append-only extension of the pinned
/// `from_size`/`from_root` (advisory-intake arc, tier 6). Fetches the registry's consistency proof and
/// checks it reconstructs both roots.
fn verify_log_extension(
    index: &registry::HttpIndex,
    from_size: u64,
    to_size: u64,
    from_root: &str,
    to_root: &str,
) -> Result<bool, noeta_pm::PmError> {
    use noeta_pm::transparency;
    let cons = index.log_consistency(from_size, to_size)?;
    let (Some(root_from), Some(root_to)) = (
        transparency::hex_to_array::<32>(from_root),
        transparency::hex_to_array::<32>(to_root),
    ) else {
        return Err(noeta_pm::PmError::Trust(
            "malformed pinned/served root hash".to_string(),
        ));
    };
    let proof: Option<Vec<[u8; 32]>> = cons
        .proof
        .iter()
        .map(|h| transparency::hex_to_array::<32>(h))
        .collect();
    let Some(proof) = proof else {
        return Err(noeta_pm::PmError::Trust(
            "malformed consistency-proof hash".to_string(),
        ));
    };
    Ok(transparency::verify_consistency(
        from_size as usize,
        to_size as usize,
        &proof,
        &root_from,
        &root_to,
    ))
}
