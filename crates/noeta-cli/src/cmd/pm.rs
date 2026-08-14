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
        ScopeAction::Rotate { scope } => cmd_scope_rotate(scope),
    }
}

/// `noeta scope rotate <scope>` — replace the scope's publish token with a registry-minted one and
/// print it once.
///
/// The output is the point of the command, so it says outright that the token is not recoverable and
/// that the old one stopped working — a rotation whose result scrolls past unread is a scope whose
/// CI is about to fail with no obvious cause.
pub(crate) fn cmd_scope_rotate(scope: &str) -> ExitCode {
    // Same routing as the policy verb: the project's `[registries]` mapping for this scope, else
    // the environment default chain ending in the built-in hosted registry.
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: rotating a token needs the hosted registry, but `NOETA_REGISTRY_DIR` \
                 routes to the file-backed local index — unset it, set `NOETA_REGISTRY_URL`, or \
                 map the scope under `[registries]`"
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
    match index.rotate_scope_token(scope) {
        Ok((status, token)) => {
            println!("{status}: `{scope}`");
            println!();
            println!("  {token}");
            println!();
            println!(
                "This is the only time it is shown — the registry stores only its hash, so a lost \
                 token means another rotation, not a lookup."
            );
            println!(
                "The previous token no longer publishes: update `NOETA_REGISTRY_TOKEN` wherever \
                 it is set, CI secrets included."
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            ExitCode::from(1)
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
    // the environment default chain (ending in the built-in hosted registry) otherwise.
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: setting a scope policy needs the hosted registry, but \
                 `NOETA_REGISTRY_DIR` routes to the file-backed local index — unset it, set \
                 `NOETA_REGISTRY_URL`, or map the scope under `[registries]`"
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
    // routes this scope to (like resolve/publish), else the environment default chain ending in
    // the built-in hosted registry.
    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: `noeta claim` needs the hosted registry, but `NOETA_REGISTRY_DIR` routes \
                 to the file-backed local index — unset it, set `NOETA_REGISTRY_URL` to the \
                 registry you are claiming a scope on, or map the scope under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    let audience = claim_audience(
        audience,
        std::env::var("NOETA_REGISTRY_AUDIENCE").ok(),
        &base,
    );

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

/// The hosted registry's "Noeta Registry" GitHub OAuth app — the device-flow client id `noeta
/// claim` uses when `NOETA_GITHUB_CLIENT_ID` is unset. A client id is a public identifier, not a
/// secret: the device flow authenticates with the user's browser approval, never an app secret.
const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23liXGU5JL6IDlR8L6";

/// The OIDC audience `noeta claim` requests: `--audience` wins, then `NOETA_REGISTRY_AUDIENCE`,
/// else it is **derived from the host of the registry base URL** the claim talks to — the
/// production registry validates tokens whose audience is its own hostname
/// (`https://registry.noeta.dev` → `registry.noeta.dev`), so the derived default matches whatever
/// registry the claim was routed to (an explicit URL, a `[registries]` mapping, or the built-in
/// default). The env value is passed in rather than read here so the precedence is unit-testable
/// without mutating process env.
fn claim_audience(flag: Option<&str>, env: Option<String>, base: &str) -> String {
    flag.map(str::to_string)
        .or(env)
        .unwrap_or_else(|| host_of(base))
}

/// The host component of a base URL: scheme, userinfo, port, and path stripped. Falls back to the
/// trimmed input when there is no recognizable host (a malformed base fails later, at the HTTP
/// client, with a better error than anything we could say here).
fn host_of(base: &str) -> String {
    let rest = base.split_once("://").map_or(base, |(_, rest)| rest);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    // A bracketed IPv6 literal keeps its colons; otherwise a colon starts the port.
    let host = if let Some(v6) = rest.strip_prefix('[') {
        v6.split_once(']').map_or(rest, |(host, _)| host)
    } else {
        rest.split(':').next().unwrap_or(rest)
    };
    if host.is_empty() {
        base.trim().trim_end_matches('/').to_string()
    } else {
        host.to_string()
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
    // Laptop: the GitHub OAuth device flow. The client id is a PUBLIC identifier (the device flow
    // uses no secret): the built-in default is the hosted registry's "Noeta Registry" OAuth app;
    // `NOETA_GITHUB_CLIENT_ID` overrides it for a third-party registry's own app, and
    // `NOETA_GITHUB_OAUTH_URL` overrides the endpoint for testing.
    let client_id = std::env::var("NOETA_GITHUB_CLIENT_ID")
        .unwrap_or_else(|_| DEFAULT_GITHUB_CLIENT_ID.to_string());
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

/// `noeta add [key|company/pkg] [--path|--git+--tag|--version] [--package company/pkg]` — add a
/// dependency to the nearest `noeta.toml`, then resolve so `noeta.lock` reflects it
/// (package-manager P2.4d).
///
/// A source is **optional** for a registry dependency: given an identity and no `--path`/`--git`/
/// `--version`, `add` asks the registry for the package's current version and writes a caret
/// requirement for it — `noeta add para/cli` on a 0.2.0 package writes `{ version = "^0.2",
/// package = "para/cli" }`. That is what `cargo add` and `npm install` do, and it is what keeps a
/// tutorial from hard-coding a version that goes stale the moment the package ships a minor.
/// Auto-selection never lands on a prerelease or a yanked release (see
/// [`registry::latest_selectable`]); an explicit `--version` still says exactly what it says.
///
/// `--package` applies to **every** source form. A `--version` dependency resolves by it. A
/// `--path`/`--git` dependency is already selected by its source, so there it is written into the
/// entry as a *claim* and verified — against the target's own `[package] name` up front for a
/// `--path` (nothing is written if it disagrees), and at resolve time for a `--git`. That is the
/// spelling a scope-array member wants: `{ path = "../..", package = "para/ai" }` says which
/// package of the scope the member is, which the path alone does not.
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
    // A positional carrying a `/` is a **package identity**, not an import-root key: a key is an
    // identifier (it becomes `use <key>.…`), so a slash cannot occur in one — `noeta add para/cli`
    // is unambiguous, and today it fails with "must be an identifier". Reading it as `--package`
    // gives the registry form its shortest spelling, which is the one docs and READMEs want.
    let (key, package) = match key {
        Some(k) if k.contains('/') => match package {
            Some(p) if p != k => {
                eprintln!(
                    "noeta: `{k}` and `--package {p}` name different packages — a positional with a \
                     `/` IS the package identity, so give it once (`noeta add {p}`, or `noeta add \
                     <key> --package {p}` to bind it under a different import root)"
                );
                return ExitCode::from(2);
            }
            // The key is then derived from the identity, exactly as with a bare `--package`.
            _ => (None, Some(k)),
        },
        other => (other, package),
    };

    // `--package` names a `company/package` identity, and it is meaningful on every source form: it
    // is what a `--version` dependency *resolves by*, and on a `--path`/`--git` dependency it is a
    // claim about the tree the source points at, written into the entry and checked at resolve time.
    // The claim is what makes a scope-array member readable (`{ path = "../..", package = "para/ai" }`
    // says which package of the scope the member is), so `add` must be able to produce it.
    //
    // Parse it up front so a malformed identity fails before touching the manifest.
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

    // `, package = "company/pkg"` for the table forms, or empty when no `--package` was given.
    let package_field = match &package_name {
        Some(p) => format!(", package = {}", toml_string(&p.to_string())),
        None => String::new(),
    };

    // At most one source form — and none at all is the *latest-resolving* registry form, which
    // needs the project's `[registries]` routing and so cannot be spelled until the manifest is
    // located below.
    let value_toml = match (path, git, version) {
        (Some(p), None, None) => Some(format!(
            "{{ path = {}{package_field} }}",
            toml_string(&p.display().to_string())
        )),
        (None, Some(url), None) => {
            let Some(tag) = tag else {
                eprintln!(
                    "noeta: `--git` requires `--tag` (sources are git + tagged releases only)"
                );
                return ExitCode::from(2);
            };
            Some(format!(
                "{{ git = {}, tag = {}{package_field} }}",
                toml_string(url),
                toml_string(tag)
            ))
        }
        (None, None, Some(req)) => Some(match &package_name {
            // A registry dependency resolves only with its identity, so fold `--package` into the
            // table form; without it, keep the bare shorthand (it errors at resolve, pointing here).
            Some(_) => format!("{{ version = {}{package_field} }}", toml_string(req)),
            None => toml_string(req),
        }),
        // No source: a registry dependency whose version `add` looks up (filled in below). Only an
        // identity makes that possible — without one there is nothing to ask the registry about.
        (None, None, None) => {
            if package_name.is_none() {
                eprintln!(
                    "noeta: name a registry package (`noeta add company/pkg`) or give a source — \
                     `--path`, `--git` (+ `--tag`), or `--version`"
                );
                return ExitCode::from(2);
            }
            None
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

    // The latest-resolving form: no source was given, so look the package's current version up in
    // the registry that owns its scope and write a caret requirement for it. Done here, before the
    // manifest is edited, so a lookup failure leaves `noeta.toml` untouched.
    let value_toml = match value_toml {
        Some(toml) => toml,
        None => {
            let name = package_name
                .as_ref()
                .expect("the sourceless form is only reachable with a package identity");
            match latest_requirement(&manifest_path, name) {
                Ok(req) => {
                    println!("resolved `{name}` to {req} (the registry's current version)");
                    format!("{{ version = {}{package_field} }}", toml_string(&req))
                }
                Err(code) => return code,
            }
        }
    };

    // A `--path` dependency's own declared identity, read straight from the tree it points at. It
    // answers two questions at once: whether a `--package` claim about that tree is true, and what
    // import root/scope to derive when no key was given. `None` when there is no `--path`, or when
    // that tree has no readable `[package]` (the resolve below reports that, and reports it better).
    let path_identity: Option<String> = path.and_then(|rel| {
        manifest::current_package(&manifest_dir.join(rel).join(manifest::MANIFEST_NAME))
            .ok()
            .map(|(identity, _)| identity)
    });

    // `--package` on a `--path` source is a **claim** about that tree, not a selector — so check it
    // here, while the manifest is still untouched, rather than writing an entry that only fails when
    // the graph resolves. (A `--git` claim needs the repo fetched, so the resolve checks that one.)
    if let (Some(claimed), Some(actual)) = (&package_name, &path_identity)
        && claimed.to_string() != *actual
    {
        eprintln!(
            "noeta: `--package {claimed}` does not match the package at `{}`, which declares \
             `{actual}` — on a path dependency `--package` is a checked claim about that tree",
            path.expect("a path identity is only read for a --path source")
                .display()
        );
        return ExitCode::from(2);
    }

    // The package's declared **root segment**, computed cheaply where the identity is known without
    // fetching: `--package`'s `package` half, or a `--path` dep's `[package]` name. `None` for a
    // `--git` (or bare `--version`) source, whose identity isn't known until it is materialized.
    let derived_root: Option<String> = match &package_name {
        Some(p) => Some(p.package.clone()),
        None => identity_half(path_identity.as_deref(), 1),
    };

    // The package's **scope** (the `company` half), derived the same way and for the same reason as
    // the root above. Binding `para/aether` under the key `para` is not a rename — it is the
    // package's own scope, and the scope is a legitimate import root that several packages share
    // (`para.aether`, `para.api`). Without this, the documented spelling from the package guide
    // earns a warning telling the author their correct code is surprising.
    let derived_scope: Option<String> = match &package_name {
        Some(p) => Some(p.company.clone()),
        None => identity_half(path_identity.as_deref(), 0),
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
    // Whether the key was already bound: adding a second package under it widens the entry into a
    // scope array, which is worth saying out loud rather than doing silently.
    let widened_a_scope = manifest::load(&manifest_path)
        .map(|m| m.dependencies().contains_key(&binding_key))
        .unwrap_or(false);
    if let Err(err) = manifest::add_dependency(&manifest_path, &binding_key, &value_toml) {
        eprintln!("noeta: {err}");
        return ExitCode::from(1);
    }
    // Resolve so the new dependency is fetched and the lock is refreshed; a bad URL/tag/path fails
    // here (the manifest edit already succeeded — the entry stays so the user can fix it).
    match graph::resolve_graph(&manifest_path) {
        Ok(resolved) => {
            let bound: Vec<&noeta_loader::DepPackage> = resolved
                .packages
                .iter()
                .filter(|p| p.key() == binding_key)
                .collect();
            if widened_a_scope {
                println!(
                    "added `{binding_key}` to {} — `{binding_key}` is now a scope binding {n} \
                     packages under one import root",
                    manifest_path.display(),
                    n = bound.len()
                );
            } else {
                println!("added `{binding_key}` to {}", manifest_path.display());
            }
            // Now that the package is materialized, its *declared* root is authoritative (this also
            // covers `--git`, whose root wasn't known before). If the chosen key differs, the binding
            // is a deliberate rename — surface it so `use <key>.…` isn't a surprise. Two bindings are
            // NOT renames and stay quiet: a scope binding several packages under one root, and a
            // single package bound under its own scope (`para/aether` keyed `para`), which is the
            // spelling the package guide teaches and the one its own modules declare.
            if let [dep] = bound[..]
                && dep.root != binding_key
                && derived_scope.as_deref() != Some(binding_key.as_str())
            {
                eprintln!(
                    "warning: `{binding_key}` binds a package whose own module root is `{root}` — \
                     imports resolve as `{binding_key}.…`, not `{root}.…`",
                    root = dep.root
                );
            }
            print_import_paths(&bound);
            warn_new_committers(&old_lock, &resolved);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: added `{binding_key}`, but resolving it failed: {err}");
            ExitCode::from(1)
        }
    }
}

/// Print the `use` lines the freshly added packages actually answer to — the line a reader needs
/// next, and the one `noeta add` used to leave them to guess.
///
/// Knowing the import *root* is not knowing the import: a root of `para` says nothing about whether
/// the thing you want is `para.cli`, `para.api` or `para` itself, and the reader's next move is to
/// go looking. The modules are already in hand here — resolution just materialized them — so the
/// answer is printed rather than implied.
///
/// Shows the module paths, capped, because a large package would otherwise bury the message it is
/// part of; the cap says how many it did not print rather than trailing off. A **native** package's
/// modules are provided by its Rust extension and are invisible to the host loader, so there is
/// nothing to enumerate and the root is stated as what it is. A package whose modules derived no
/// path (nothing on disk to derive from) prints nothing at all rather than a guess.
fn print_import_paths(bound: &[&noeta_loader::DepPackage]) {
    /// Beyond this many, the list stops being an answer and starts being a directory listing.
    const SHOWN: usize = 6;
    for dep in bound {
        if dep.native {
            println!(
                "  import it as `use {key}.…` — its modules come from the package's native \
                 extension, so they resolve once the toolchain composes",
                key = dep.key()
            );
            continue;
        }
        let mut paths: Vec<String> = dep
            .modules
            .iter()
            .filter_map(|m| m.path.derived())
            .map(|segments| segments.join("."))
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            continue;
        }
        let extra = paths.len().saturating_sub(SHOWN);
        for path in paths.iter().take(SHOWN) {
            println!("  use {path}");
        }
        if extra > 0 {
            println!("  … and {extra} more module{}", plural(extra));
        }
    }
}

/// The requirement `noeta add company/pkg` writes when no source was given: ask the registry that
/// owns the package's scope for its current version, and spell a caret at that version's
/// compatibility boundary (`0.2.0` → `^0.2`). `Err` is the exit code to return — every failure is
/// reported here, before the manifest is touched, so a lookup that cannot answer leaves
/// `noeta.toml` exactly as it was.
///
/// The scope's registry is the same one a resolve would use: a `[registries]` mapping in the
/// project manifest if it routes this scope, else the environment default chain. Looking it up
/// through the project's own routing is what keeps a private scope's version lookup off the public
/// registry.
fn latest_requirement(
    manifest_path: &std::path::Path,
    name: &manifest::PackageName,
) -> Result<String, ExitCode> {
    let identity = name.to_string();
    // The root manifest's `[registries]` routing. A manifest we cannot parse is not fatal here —
    // `add_dependency` re-reads it and reports the parse failure properly — so fall back to the
    // environment default rather than inventing a second diagnostic for it.
    let registries = manifest::load(manifest_path)
        .map(|m| m.registries().clone())
        .unwrap_or_default();
    let source = registries.source_for(&name.company);
    let index = registry::open_source(source).map_err(|err| {
        eprintln!(
            "noeta: cannot open the registry to look up `{identity}`: {err}\n  \
             pass `--version <req>` to add it without a lookup"
        );
        ExitCode::from(1)
    })?;
    let releases = index.releases(&identity).map_err(|err| {
        eprintln!(
            "noeta: cannot reach the registry to resolve the latest `{identity}`: {err}\n  \
             check your connection, then retry — or pass `--version <req>` to add it without a \
             lookup"
        );
        ExitCode::from(1)
    })?;
    match registry::latest_selectable(&releases) {
        Ok(release) => Ok(registry::caret_requirement(&release.version)),
        Err(registry::NoLatest::UnknownPackage) => {
            eprintln!(
                "noeta: the registry has no package `{identity}` — check the spelling (a package \
                 is `company/package`), or map `{}` to the registry that serves it under \
                 `[registries]`",
                name.company
            );
            Err(ExitCode::from(1))
        }
        Err(registry::NoLatest::OnlyPrereleases(highest)) => {
            eprintln!(
                "noeta: `{identity}` has published only prereleases (the highest is {highest}), \
                 and `add` never selects one on its own — depend on it deliberately with \
                 `--version {highest}`"
            );
            Err(ExitCode::from(1))
        }
        Err(registry::NoLatest::AllYanked) => {
            eprintln!(
                "noeta: every published version of `{identity}` is yanked — a yanked release keeps \
                 an existing pin resolving but is never newly selected, so there is nothing to \
                 add. Wait for a fixed release, or pin a yanked one deliberately with \
                 `--version =<x.y.z>`"
            );
            Err(ExitCode::from(1))
        }
    }
}

/// One half of a `company/package` identity — `0` for the scope, `1` for the root segment.
fn identity_half(identity: Option<&str>, index: usize) -> Option<String> {
    identity
        .and_then(|id| id.split('/').nth(index))
        .map(str::to_string)
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
/// With `--docs-only`, skip the index entirely and only regenerate + re-upload the docs artifact
/// for an already-published version (remediation for a release whose stored docs are wrong).
#[allow(clippy::too_many_arguments)] // a straight fan-out of the clap variant's fields
pub(crate) fn cmd_publish(
    git: Option<&str>,
    tag: Option<&str>,
    force_key: bool,
    interactive: bool,
    oob: bool,
    no_docs: bool,
    no_readme: bool,
    docs_only: bool,
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
    // `--docs-only`: regenerate this release's docs artifact (the same pipeline a publish runs,
    // including the composed-toolchain build for a native package) and re-upload it for a version
    // ALREADY in the index — no new version, no provenance, no index write. The remediation tool
    // for a shelf release whose stored docs are wrong or empty.
    if docs_only {
        return cmd_publish_docs_only(&manifest, &manifest_path, pkg, &name, &version);
    }
    let Some(git) = git else {
        // clap enforces `--git` unless `--docs-only`; keep a real error for library callers.
        eprintln!("noeta: `noeta publish` needs `--git <URL>` (the release's source repository)");
        return ExitCode::from(2);
    };
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
    let native_docs: Option<String> =
        match native_release_docs(pkg, &manifest_path, &name, &version) {
            Ok(docs) => docs,
            Err(err) => {
                eprintln!("noeta: native package build failed — not publishing.\n{err}");
                return ExitCode::from(1);
            }
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
            // Link what was just pushed. A publish names a tag and a sha, and neither is something
            // a person can check by reading it — so the two questions a fresh release raises get an
            // answer to click: what does it look like to a consumer (the release page), and what
            // content actually went out (the commit). Absent for a source with no web form (a local
            // path, a `file:` URL), where there would be nothing to open.
            if let Some(url) = noeta_pm::tag_web_url(git, &tag) {
                println!("  {url}");
            }
            if let Some(url) = noeta_pm::commit_web_url(git, &sha) {
                println!("  {url}");
            }
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
                    // A native package's docs are pre-generated JSON (the quality-gated build
                    // above); a pure-Noeta package's are generated from source here.
                    Some(json) => Ok(json),
                    None => docgen::package_docs_json(&pkg_dir).map(|(json, _)| json),
                };
                match docs {
                    Ok(docs_json) => match index.put_docs(&name, &version, &docs_json) {
                        Ok(()) => {
                            let (modules, decls) = docs_json_counts(&docs_json);
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

/// Generate a NATIVE package's registry docs artifact: build the package's own crate into a
/// composed toolchain (the publish quality gate — an uncompilable crate is refused), emit its
/// **non-builtin** API surface (every extension the composition adds over the toolchain's builtin
/// units, so a `root()` diverging from the package segment still documents), and fold in any
/// `.noe` glue the package also ships. `Ok(None)` for a pure-source package (no `native =` entry).
fn native_release_docs(
    pkg: &manifest::PackageMeta,
    manifest_path: &std::path::Path,
    name: &str,
    version: &semver::Version,
) -> Result<Option<String>, String> {
    let Some(native_dir) = &pkg.native else {
        return Ok(None);
    };
    let pkg_dir = manifest_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let crate_dir = pkg_dir.join(native_dir);
    println!(
        "building native crate at `{}` (publish quality gate)…",
        crate_dir.display()
    );
    let api_json = compose::package_api_docs(name, &crate_dir)?;
    // Fold in any `.noe` glue the package also ships (advisory; the API surface wins).
    let noe_json = docgen::package_docs_json(pkg_dir).ok().map(|(j, _)| j);
    Ok(Some(docgen::finalize_native_docs(
        &api_json,
        noe_json.as_deref(),
        name,
        &version.to_string(),
    )))
}

/// `(modules, declarations)` counted from a finished `docs.json` artifact, for the summary line.
/// A declaration item carries `kind`; a free-floating `@doc` section does not — so this matches
/// the generator's own `decl_count` exactly.
fn docs_json_counts(json: &str) -> (usize, usize) {
    let Some(modules) = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|d| d.get("modules").and_then(|m| m.as_array()).cloned())
    else {
        return (0, 0);
    };
    let decls = modules
        .iter()
        .map(|m| {
            m.get("items")
                .and_then(|i| i.as_array())
                .map_or(0, |items| {
                    items.iter().filter(|i| i.get("kind").is_some()).count()
                })
        })
        .sum();
    (modules.len(), decls)
}

/// `noeta publish --docs-only` — regenerate the release's documentation artifact through the same
/// pipeline a publish runs (composed-toolchain API docs for a native package, source docs for a
/// pure one) and re-upload it for a version **already in the index**. Never touches the index:
/// no new version, no tag/SHA pinning, no provenance — the hosted registry's docs endpoint wants
/// only the scope's publish token (`NOETA_REGISTRY_TOKEN`), which the HTTP client supplies as on a
/// normal publish. Refuses when the manifest's version is not published (docs belong to a release;
/// uploading docs for a version that doesn't exist is a mistake, and the registry would 404 it).
fn cmd_publish_docs_only(
    manifest: &manifest::Manifest,
    manifest_path: &std::path::Path,
    pkg: &manifest::PackageMeta,
    name: &str,
    version: &semver::Version,
) -> ExitCode {
    // Route to the registry that owns this package's scope, exactly like a publish (a private
    // scope's docs must not leak to the public registry).
    let scope_source = manifest.registries().source_for(&pkg.name.company);
    if scope_source.is_some() {
        println!(
            "uploading docs for `{name}` via the `[registries]` source for `{}`",
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
    // Docs belong to a release — refuse an upload for a version the index has never published.
    let releases = match index.releases(name) {
        Ok(releases) => releases,
        Err(err) => {
            eprintln!("noeta: cannot check whether `{name}@{version}` is published: {err}");
            return ExitCode::from(1);
        }
    };
    if !releases.iter().any(|r| r.version == *version) {
        eprintln!(
            "noeta: `{name}@{version}` is not published — `--docs-only` re-uploads the docs \
             artifact for an EXISTING release and never creates one. Publish the release first \
             (`noeta publish --git …`), or fix `[package] version` to name the release whose docs \
             you are regenerating."
        );
        return ExitCode::from(1);
    }
    let pkg_dir = manifest_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let docs_json = match native_release_docs(pkg, manifest_path, name, version) {
        Ok(Some(json)) => json,
        Ok(None) => match docgen::package_docs_json(&pkg_dir) {
            Ok((json, _)) => json,
            Err(err) => {
                eprintln!("noeta: docs not generated: {err}");
                return ExitCode::from(1);
            }
        },
        Err(err) => {
            eprintln!("noeta: native package build failed — docs not generated.\n{err}");
            return ExitCode::from(1);
        }
    };
    // Unlike the ride-along upload of a full publish (advisory: a docs failure must not unpublish
    // a release that already succeeded), the upload IS the whole point here — fail loudly.
    match index.put_docs(name, version, &docs_json) {
        Ok(()) => {
            let (modules, decls) = docs_json_counts(&docs_json);
            println!(
                "docs re-uploaded for `{name}` {version} ({modules} module{}, {decls} \
                 declaration{}) — index untouched",
                plural(modules),
                plural(decls)
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: docs not uploaded: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta audit [path]` — report the dependency tree's trust footprint (package-manager Phase 4, S6):
/// every resolved dependency, its source, and the elevated authority (`native` / `commands`) the root
/// `[trust]` grants make active. Transparency/informed-consent: since an *unauthorized* native
/// dependency fails resolution, a successful audit lists exactly the elevated authority that is live.
///
/// Exit 0 means *checked and clean*. It is non-zero when a matched advisory's tier policy says `fail`,
/// and also when the registry's advisory data did not **verify** — an audit that could not read the
/// feed has not cleared the graph, and saying so only on stdout while exiting 0 made drift in the
/// signed formats indistinguishable from a clean run in CI (audit row 4a). An *unreachable* registry
/// stays a note: this section is best-effort, and offline is not evidence of anything.
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
        if trust.commands.values().any(|b| b.provider == pkg.identity) {
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
    println!("    commands : {}", render_binding_table(&trust.commands));
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
    // serve an unlogged forgery without detection. Best-effort and only when the default chain lands
    // on a hosted registry (`NOETA_REGISTRY_URL`, else — unless `NOETA_REGISTRY_DIR` routes to the
    // file-backed local index, which serves no log — the built-in hosted default); a
    // not-logged/unreachable result is a note (direct git deps aren't logged), not a failure. The
    // pinned key is carried across deps so a registry serving different keys is caught.
    if let Ok(Some(index)) = registry::open_http() {
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
    //
    // Two *failures* to distinguish here, because "could not verify" is not "verified clean" (audit
    // row 4a). A [`PmError::Network`]/`Io` is transient and environmental — an offline CI box, a 502 —
    // and degrades to a note, exactly as the IDE's resolve does (`noeta-project/src/workspace.rs`) and as
    // this whole section's best-effort contract says. A [`PmError::Trust`] is a signature that did not
    // verify, a head that does not attest to the served advisories, a log leaf that does not match, or
    // a 200 whose body is not the shape `test_data/wire` pins (a cross-repo protocol drift, which is
    // the *cause* the fixtures exist to catch — see `registry::shape_drift`): never routine, and every
    // one of them means *no dependency was checked against the feed at all*. That is reported
    // on stderr and exits non-zero, the same answer `noeta advisory watch` already gives to the same call
    // and the same answer resolution gives to a bad release signature. The `[trust.advisories]` policy
    // is deliberately NOT consulted: it selects which *intake tier's* hits fail a build, and an
    // unverifiable feed has no tier — nothing was read to have one.
    let mut advisory_hits = 0usize;
    let mut advisory_fails = 0usize;
    // What failed to verify, in the words of the exit line below — set at the failing site. At most one
    // of the two can fire: a feed that does not verify never reaches the log-binding check.
    let mut advisory_unverified: Option<&'static str> = None;
    if let Ok(Some(index)) = registry::open_http() {
        println!("\n  Security advisories:");
        let old = lock::Lock::read(&manifest_dir).advisory_trust().cloned();
        // The feed key: the lock's pin (trust-on-first-use) when there is one, else the key the
        // registry serves. A registry that serves *no* advisory key runs no feed at all (`None` here) —
        // a note, like the transparency-log section above, not a verification failure. Once a key is
        // pinned we never ask again, so a registry that stops serving one cannot quietly un-pin itself
        // into that note: it fails the head verification below instead.
        let fetched = match old.as_ref().map(|a| a.public_key.clone()) {
            Some(pinned) => Some(index.fetch_advisories(Some(&pinned))),
            None => match index.advisory_public_key() {
                Ok(Some(served)) => Some(index.fetch_advisories(Some(&served))),
                Ok(None) => None,
                // A failure fetching the key is not "this registry runs no feed": pass it down so the
                // match below classifies it — transient → note, drifted/unverifiable → ✗ and exit 1.
                Err(err) => Some(Err(err)),
            },
        };
        match fetched {
            None => {
                println!("    (this registry serves no advisory feed — nothing to check against)")
            }
            Some(Ok(feed)) => {
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
                    // Same split as the feed fetch: a checkpoint that does not verify, or a logged leaf
                    // that is not the advisory served, is evidence — not a note.
                    Err(err @ noeta_pm::PmError::Trust(_)) => {
                        advisory_unverified = Some(
                            "the advisories were read, but the registry's transparency-log evidence \
                             for them did not hold",
                        );
                        eprintln!("    ✗ advisory-log verification failed — {err}");
                    }
                    Err(err) => println!("    ⚠ advisory-log not checked — {err}"),
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
            // The feed did not VERIFY: a bad per-advisory signature, a head that does not attest to the
            // served advisories (a withheld advisory), or a response that is not the pinned wire shape
            // at all. Nothing was checked, so this is reported like any other trust refusal — on
            // stderr, with a non-zero exit.
            Some(Err(err @ noeta_pm::PmError::Trust(_))) => {
                advisory_unverified =
                    Some("the dependency graph was NOT checked against the advisory feed");
                eprintln!("    ✗ the advisory feed did not verify — {err}");
            }
            // Transient/environmental (offline, a 5xx, a refused connection): the section is
            // best-effort by design, so it stays a note and the audit still succeeds. A malformed
            // *body* is no longer in this bucket — see the classification note above.
            Some(Err(err)) => println!("    not checked — {err}"),
        }
    }

    // Advisory data that could not be VERIFIED is not a clean audit. Reported before the matched-advisory
    // tally because it subsumes it — a feed that never verified produced no matches to tally.
    if let Some(what) = advisory_unverified {
        eprintln!(
            "\nthe registry's advisory data did not verify (marked ✗ above) — {what}. Treat this build \
             as unaudited, not as clean."
        );
        return ExitCode::from(1);
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
                "noeta: publishing an advisory needs the hosted registry, but `NOETA_REGISTRY_DIR` \
                 routes to the file-backed local index — unset it, set `NOETA_REGISTRY_URL`, or \
                 map `{scope}` under `[registries]`"
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
                "noeta: filing a report needs the hosted registry, but `NOETA_REGISTRY_DIR` routes \
                 to the file-backed local index — unset it, set `NOETA_REGISTRY_URL`, or map \
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
/// URL comes from `--scope`'s `[registries]` routing (scope-owner path) or the environment default
/// chain ending in the built-in hosted registry (operator path — same precedence as everything
/// else). The bearer token is the scope publish token (`NOETA_REGISTRY_TOKEN`, the default) unless
/// `operator` is set, which swaps in the admin token (`NOETA_REGISTRY_ADMIN_TOKEN`).
fn report_index(scope: Option<&str>, operator: bool) -> Result<registry::HttpIndex, ExitCode> {
    let base = match scope {
        Some(s) => match scope_registry_base(s) {
            Ok(Some(base)) => base,
            Ok(None) => {
                eprintln!(
                    "noeta: this needs the hosted registry, but `NOETA_REGISTRY_DIR` routes to the \
                     file-backed local index — unset it, set `NOETA_REGISTRY_URL`, or map `{s}` \
                     under `[registries]`"
                );
                return Err(ExitCode::from(2));
            }
            Err(code) => return Err(code),
        },
        None => match registry::default_http_base() {
            Ok(Some(base)) => base,
            Ok(None) => {
                eprintln!(
                    "noeta: this needs the hosted registry, but `NOETA_REGISTRY_DIR` routes to the \
                     file-backed local index — unset it or set `NOETA_REGISTRY_URL`"
                );
                return Err(ExitCode::from(2));
            }
            Err(err) => {
                eprintln!("noeta: {err}");
                return Err(ExitCode::from(1));
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

/// Render a `[trust.commands]`/`[trust.directives]` binding table for `noeta audit` — each local
/// name with the package it resolves to, showing the exported name only when it was renamed
/// (`undo → para/db:rollback`), so the common no-rename case stays terse.
pub(crate) fn render_binding_table(
    table: &std::collections::BTreeMap<String, noeta_pm::manifest::Binding>,
) -> String {
    if table.is_empty() {
        "(none)".to_string()
    } else {
        table
            .iter()
            .map(|(local, b)| {
                if b.exported == *local {
                    format!("{local} → {}", b.provider)
                } else {
                    format!("{local} → {}:{}", b.provider, b.exported)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
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
/// scope to a hosted URL — the same routing resolution and publish follow — else the environment
/// default chain (`NOETA_REGISTRY_URL`, then `NOETA_REGISTRY_DIR` → `None`, the file-backed local
/// index has no claim/policy endpoints, then the built-in hosted default). A scope mapped to a git
/// forge is a hard error (a forge has no claim/policy endpoints), not a silent fall-through to the
/// wrong registry.
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
    registry::default_http_base().map_err(|err| {
        eprintln!("noeta: {err}");
        ExitCode::from(1)
    })
}

/// Quote a string as a TOML basic string for a manifest value we write (`noeta add`).
pub(crate) fn toml_string(s: &str) -> String {
    noeta_pm::toml_quote(s)
}

/// The pinned state `noeta advisory watch` carries between runs (advisory-intake arc, tier 6): the keys it
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

/// `noeta advisory watch [scope]` — the transparency-log suppression monitor (advisory-intake arc,
/// tier 6). Watches the scope named, or — the form a CI cron wants — **every scope this project's
/// `noeta.lock` pins**, since the set worth watching is the set you depend on and nobody should have
/// to keep that list current in a cron file by hand. Exits non-zero if any watched scope drifted.
///
/// Each scope is fetched separately even when several route to the same registry: the feed is verified
/// against *that scope's* pinned advisory key, and two scopes can legitimately sit on different sides
/// of a key rotation, so one shared fetch would have to be re-verified per scope anyway. The cost is a
/// few small requests per cron run, which is the right side of that trade.
pub(crate) fn cmd_advisory_watch(
    scope: Option<&str>,
    state_dir: Option<&std::path::Path>,
) -> ExitCode {
    let scopes: Vec<String> = match scope {
        Some(one) => vec![one.to_string()],
        None => match lockfile_scopes() {
            Ok(scopes) if scopes.is_empty() => {
                println!(
                    "noeta: nothing to watch — this project's `noeta.lock` pins no dependencies. \
                     Name a scope to watch it anyway: `noeta advisory watch <scope>`."
                );
                return ExitCode::SUCCESS;
            }
            Ok(scopes) => scopes,
            Err(code) => return code,
        },
    };
    let dir = match state_dir {
        Some(p) => p.to_path_buf(),
        None => match noeta_cache::Cache::locate() {
            Some(cache) => cache.join("watch"),
            None => {
                eprintln!("noeta: cannot locate a state directory — pass `--state <dir>`");
                return ExitCode::from(2);
            }
        },
    };

    if scope.is_none() {
        println!(
            "watching {} scope{} from `noeta.lock`: {}\n",
            scopes.len(),
            plural(scopes.len()),
            scopes.join(", ")
        );
    }

    // The exit code is the *worst* outcome across the set, not the last one: a misconfigured scope
    // (2) outranks a detected drift (1), because it means that scope was never actually checked.
    let mut drifted = 0usize;
    let mut unusable = 0usize;
    for scope in &scopes {
        // One state file per scope, so a set read from the lockfile grows and shrinks with the
        // dependency list without a scope's pinned baseline ever being confused for another's.
        match watch_one_scope(scope, &dir.join(format!("{scope}.toml"))) {
            code if code == ExitCode::SUCCESS => {}
            code if code == ExitCode::from(1) => drifted += 1,
            _ => unusable += 1,
        }
    }
    if scopes.len() > 1 && (drifted + unusable) > 0 {
        eprintln!(
            "\n{} of {} watched scopes reported a problem.",
            drifted + unusable,
            scopes.len()
        );
    }
    match (unusable, drifted) {
        (0, 0) => ExitCode::SUCCESS,
        (0, _) => ExitCode::from(1),
        _ => ExitCode::from(2),
    }
}

/// The distinct scopes of every dependency the nearest project's `noeta.lock` pins — what a bare
/// `noeta advisory watch` watches. Deliberately **not** filtered to registry-sourced pins: an advisory
/// is a statement about a package *name*, so one against `acme/http` applies whether you resolved it
/// from the registry or straight from git. That is exactly the set [`cmd_audit`] cross-references
/// against the feed, and the two verbs must not disagree about which packages a feed speaks for.
fn lockfile_scopes() -> Result<Vec<String>, ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let Some(manifest_path) = manifest::find(&cwd) else {
        eprintln!(
            "noeta: no `{}` found at or above `{}` — run `noeta advisory watch` inside a project, \
             or name the scope to watch",
            manifest::MANIFEST_NAME,
            cwd.display()
        );
        return Err(ExitCode::from(2));
    };
    let dir = manifest_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let lock = lock::Lock::read(&dir);
    let scopes: std::collections::BTreeSet<String> = lock
        .locked_versions()
        .filter_map(|(identity, _)| identity.split('/').next())
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect();
    Ok(scopes.into_iter().collect())
}

/// Watch one scope, diffing against `state_file` and rewriting it. Verifies, against the state pinned
/// by the previous run, that the registry's advisory log is an append-only extension (no history
/// rewrite) and that no advisory previously seen for the scope has disappeared from the feed (silent
/// suppression). A detected rewrite, key change, feed rollback, or disappearance exits non-zero; the
/// first run establishes the baseline.
fn watch_one_scope(scope: &str, state_file: &std::path::Path) -> ExitCode {
    use noeta_pm::transparency;

    let base = match scope_registry_base(scope) {
        Ok(Some(base)) => base,
        Ok(None) => {
            eprintln!(
                "noeta: watching `{scope}` needs the hosted registry, but `NOETA_REGISTRY_DIR` \
                 routes to the file-backed local index — unset it, set `NOETA_REGISTRY_URL`, or map \
                 `{scope}` under `[registries]`"
            );
            return ExitCode::from(2);
        }
        Err(code) => return code,
    };
    // Load prior state, but only if it is for this same base (a different registry is a fresh anchor).
    let prior: Option<WatchState> = std::fs::read_to_string(state_file)
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

    println!("watch `{scope}` — {base}");
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
            if let Err(err) = std::fs::write(state_file, text) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // `noeta claim`'s OIDC audience: `--audience` wins, then `NOETA_REGISTRY_AUDIENCE`, then the
    // host of the registry base the claim was routed to. These pass the would-be env value in
    // rather than mutating process env (tests run in parallel), mirroring the `default_route`
    // tests in `noeta_pm::registry`.
    #[test]
    fn claim_audience_defaults_to_the_registry_host() {
        assert_eq!(
            claim_audience(None, None, "https://registry.noeta.dev"),
            "registry.noeta.dev"
        );
        // The production default registry derives the production audience.
        assert_eq!(
            claim_audience(None, None, noeta_pm::registry::DEFAULT_REGISTRY_URL),
            "registry.noeta.dev"
        );
    }

    #[test]
    fn claim_audience_flag_and_env_win_over_the_derived_host() {
        assert_eq!(
            claim_audience(
                Some("custom-aud"),
                Some("env-aud".to_string()),
                "https://registry.noeta.dev"
            ),
            "custom-aud"
        );
        assert_eq!(
            claim_audience(
                None,
                Some("env-aud".to_string()),
                "https://registry.noeta.dev"
            ),
            "env-aud"
        );
    }

    #[test]
    fn host_of_strips_scheme_port_path_and_userinfo() {
        assert_eq!(host_of("https://registry.noeta.dev/"), "registry.noeta.dev");
        assert_eq!(
            host_of("https://registry.noeta.dev/v1/x?q=1"),
            "registry.noeta.dev"
        );
        assert_eq!(host_of("http://127.0.0.1:39041"), "127.0.0.1");
        assert_eq!(
            host_of("https://user:pw@reg.example.com:8443/path"),
            "reg.example.com"
        );
        assert_eq!(host_of("http://[::1]:8080/x"), "::1");
        assert_eq!(host_of("registry.example.com"), "registry.example.com");
    }
}
