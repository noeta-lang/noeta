//! `noeta doc` — extract `@doc` blocks, generate the package docs artifact, fetch a
//! published package's stored docs, or emit the registry-backed API reference.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_pm::registry;
use noeta_runner::resolve_providers;

use crate::context::{load_linked, provider_escape, target_gate};
use crate::output::plural;
use crate::{compose, docgen};

/// `noeta doc <FILE>` — extract the program's `@doc { … }` text blocks (object-model slice 6f) to
/// stdout, in source order. Each block's verbatim body is dedented (the common leading indentation
/// and the surrounding blank lines from sitting inside `@doc { … }` are stripped) and preceded by an
/// HTML-comment header noting its source location — valid markdown that renders to nothing. The
/// program is not type-checked or run; doc extraction works on a parse alone, so docs can be pulled
/// from work-in-progress code.
/// Fetch a published package's stored documentation artifact from the registry: `name` picks the
/// highest published version, `name@1.2.0` an exact one. Prints the `docs.json` to stdout, or —
/// with `--out` — writes it and renders the Markdown tree from it (no source needed).
pub(crate) fn cmd_doc_package(spec: &str, out: &Option<PathBuf>) -> ExitCode {
    let (name, version) = match spec.split_once('@') {
        Some((n, v)) => match semver::Version::parse(v) {
            Ok(version) => (n.to_string(), Some(version)),
            Err(err) => {
                eprintln!("noeta: `{v}` is not a version: {err}");
                return ExitCode::from(2);
            }
        },
        None => (spec.to_string(), None),
    };
    let index = match registry::open_default() {
        Ok(index) => index,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    let version = match version {
        Some(v) => v,
        None => {
            // Highest published version, matching how registry resolution selects.
            let mut releases = match index.releases(&name) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("noeta: {err}");
                    return ExitCode::from(1);
                }
            };
            releases.sort_by(|a, b| b.version.cmp(&a.version));
            match releases.first() {
                Some(r) => r.version.clone(),
                None => {
                    eprintln!("noeta: registry has no package `{name}`");
                    return ExitCode::from(1);
                }
            }
        }
    };
    let docs = match index.docs(&name, &version) {
        Ok(Some(docs)) => docs,
        Ok(None) => {
            eprintln!("noeta: no docs stored for `{name}@{version}`");
            return ExitCode::from(1);
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    // The hosted registry renders these same docs in a browser; point there when one is
    // configured. Always on **stderr** so it never pollutes the `docs.json` piped to stdout.
    if let Some(web) = registry_web_docs_url(&name, &version) {
        eprintln!("view online: {web}");
    }
    match out {
        Some(dir) => match docgen::render_json_to(dir, &docs) {
            Ok(done) => {
                println!(
                    "rendered `{name}@{version}` docs ({} module{}, {} declaration{}) → {}",
                    done.modules,
                    plural(done.modules),
                    done.decls,
                    plural(done.decls),
                    dir.display(),
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("noeta: {err}");
                ExitCode::from(1)
            }
        },
        None => {
            print!("{docs}");
            let _ = io::stdout().flush();
            ExitCode::SUCCESS
        }
    }
}

/// The hosted registry's browser URL for a release's rendered docs, when a hosted registry is
/// configured (`NOETA_REGISTRY_URL`). `None` for the file-backed local index (no web surface).
pub(crate) fn registry_web_docs_url(name: &str, version: &semver::Version) -> Option<String> {
    let base = std::env::var("NOETA_REGISTRY_URL").ok()?;
    Some(web_docs_url(&base, name, &version.to_string()))
}

/// Format the registry's browser docs URL: the web UI lives at the registry root (the JSON API is
/// under `/v1`), and `name` already carries the `company/package` slash, so the path is
/// `{base}/{name}/{version}/docs` with any trailing slash on `base` trimmed.
pub(crate) fn web_docs_url(base: &str, name: &str, version: &str) -> String {
    format!("{}/{name}/{version}/docs", base.trim_end_matches('/'))
}

/// `noeta doc --api`: generate the API reference from the intrinsic registry (stdlib + any composed
/// native modules). Prints the schema-1 `docs.json` to stdout, or — with `--out` — writes the
/// artifact and renders its Markdown tree (the same schema the hosted registry renders). `root`
/// scopes to one extension's namespace (a package documenting itself).
pub(crate) fn cmd_doc_api(out: &Option<PathBuf>, root: Option<&str>, lint: bool) -> ExitCode {
    // The publish namespace lint: a package's whole surface must sit under its own root (`--root`).
    // Report every offender and refuse (exit 2) before emitting anything — the publish gate.
    if lint && let Some(root) = root {
        let violations = noeta_ide::api::namespace_violations(root);
        if !violations.is_empty() {
            eprintln!(
                "noeta: package `{root}` registers surface outside its own namespace ({} \
                 violation{}):",
                violations.len(),
                plural(violations.len()),
            );
            for v in &violations {
                eprintln!("  - {v}");
            }
            return ExitCode::from(2);
        }
    }
    let (json, done) = docgen::registry_docs_json(None, root);
    match out {
        Some(out_dir) => match docgen::render_json_to(out_dir, &json) {
            Ok(_) => {
                println!(
                    "documented {} module{} ({} function{}) → {}",
                    done.modules,
                    plural(done.modules),
                    done.decls,
                    plural(done.decls),
                    out_dir.display(),
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("noeta: {err}");
                ExitCode::from(2)
            }
        },
        None => {
            print!("{json}");
            ExitCode::SUCCESS
        }
    }
}

pub(crate) fn cmd_doc(
    file: &Option<PathBuf>,
    package: &Option<String>,
    out: &Option<PathBuf>,
    target: &Option<String>,
    api: bool,
    root: Option<&str>,
    lint: bool,
) -> ExitCode {
    // `--api`: the registry-backed path — the intrinsic surface (stdlib + composed native modules)
    // as a schema-1 `docs.json`, organized by module. No local source involved.
    if api {
        return cmd_doc_api(out, root, lint);
    }
    // `--package`: the registry-fetch path — a published release's stored artifact, no local
    // source involved.
    if let Some(spec) = package {
        return cmd_doc_package(spec, out);
    }
    let Some(file) = file else {
        eprintln!(
            "noeta: `noeta doc` needs a `.noe` file (or `--package <NAME>` for published docs)"
        );
        return ExitCode::from(2);
    };
    let file = file.as_path();
    // The compose probe hands back the graph it resolved (default selection) for the load below
    // (audit-5 F2); the `--out` generator path never links, so it simply drops it.
    let resolved = match compose::maybe_delegate(file) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };
    if let Some(code) = target_gate(file, target, "doc") {
        return code;
    }
    // `--out`: the generator path — a registry-ready artifact from a bare parse, before (and
    // independent of) the extraction/provider machinery below.
    if let Some(out_dir) = out {
        return match docgen::generate(file, out_dir) {
            Ok(done) => {
                for skipped in &done.skipped {
                    eprintln!("noeta: skipped `{skipped}` (does not parse)");
                }
                println!(
                    "documented {} module{} ({} declaration{}) → {}",
                    done.modules,
                    plural(done.modules),
                    done.decls,
                    plural(done.decls),
                    out_dir.display(),
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("noeta: {err}");
                ExitCode::from(2)
            }
        };
    }
    let linked = match load_linked(file, resolved) {
        Ok(linked) => linked,
        Err(code) => return code,
    };

    // The target's provider selection (provider dispatch): `doc = "<pkg>"` hands documentation
    // to that package's `@tier(doc)` runner — activation stamps every adjacency-attached block as
    // `#[Doc]`, so the runner reads the documented symbols through `attributes_of::<Doc>()` (the
    // doc-site-generator seam). Default keeps the native extractor below.
    let providers = match resolve_providers(file, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    if providers.get("doc").is_some_and(|p| p != "std") {
        let activated = noeta_check::activate_tiers_with(&linked.program, &["doc"], &providers);
        // The shared declared-provider dispatch (context): a package's `@tier(doc)` runner owns
        // the invocation; an Extension resolution falls through to the native extractor below —
        // which reads the *unactivated* program, so the activation is discarded here.
        if let Err(code) = provider_escape("doc", &linked, activated, &providers) {
            return code;
        }
    }

    let docs = noeta_check::resolve_docs(&linked.program);
    if docs.is_empty() {
        eprintln!("noeta: no `@doc` blocks found");
        return ExitCode::SUCCESS;
    }

    let mut out = String::new();
    for (i, doc) in docs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let source = linked.sources.source(doc.span.source);
        let line = source.line_col(doc.span.start).line;
        // A declaration-attached block's header carries the documented symbol (adjacency-resolved);
        // an unattached block's header is the bare location, exactly as before.
        let header = match &doc.target {
            noeta_check::DocTarget::Decl { name, .. } => {
                format!("<!-- {}:{} · {} -->\n", source.name(), line, name)
            }
            _ => format!("<!-- {}:{} -->\n", source.name(), line),
        };
        out.push_str(&header);
        out.push_str(&noeta_check::dedent_doc(&doc.text));
        out.push('\n');
    }
    print!("{out}");
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_docs_url_joins_base_name_version() {
        // `name` keeps its `company/package` slash; a trailing slash on the base is trimmed.
        assert_eq!(
            web_docs_url("https://reg.noeta.dev", "acme/greeter", "0.3.0"),
            "https://reg.noeta.dev/acme/greeter/0.3.0/docs"
        );
        assert_eq!(
            web_docs_url("https://reg.noeta.dev/", "acme/greeter", "0.3.0"),
            "https://reg.noeta.dev/acme/greeter/0.3.0/docs"
        );
    }
}
