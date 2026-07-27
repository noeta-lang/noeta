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
            // Auto-picking is a *new* selection — never land on a yanked release.
            releases.retain(|r| !r.yanked);
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

/// The hosted registry's browser URL for a release's rendered docs, when the default chain lands
/// on a hosted registry (`NOETA_REGISTRY_URL`, else the built-in default — the same routing
/// `open_default` follows). `None` when `NOETA_REGISTRY_DIR` routes to the file-backed local index
/// (no web surface).
pub(crate) fn registry_web_docs_url(name: &str, version: &semver::Version) -> Option<String> {
    let base = registry::default_http_base().ok()??;
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
/// scopes to one extension's namespace (an explicit user filter); `non_builtin` scopes to every
/// registered non-builtin extension — the publish docs path, which must not guess a root because an
/// extension's `root()` may deliberately diverge from its package's manifest segment.
pub(crate) fn cmd_doc_api(
    out: &Option<PathBuf>,
    root: Option<&str>,
    non_builtin: bool,
    lint: bool,
) -> ExitCode {
    // The publish namespace lint: the scoped extensions' whole surface must sit under their own
    // roots (and, for `--non-builtin`, never claim a toolchain-owned root). Report every offender
    // and refuse (exit 2) before emitting anything — the publish gate.
    if lint {
        let violations = if non_builtin {
            noeta_ide::api::namespace_violations_excluding(
                &docgen::builtin_extension_names(),
                &docgen::toolchain_roots(),
            )
        } else if let Some(root) = root {
            noeta_ide::api::namespace_violations(root)
        } else {
            Vec::new()
        };
        if !violations.is_empty() {
            eprintln!(
                "noeta: this package registers surface outside its own namespace ({} \
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
    let scope = if non_builtin {
        docgen::ApiScope::NonBuiltin
    } else {
        root.map_or(docgen::ApiScope::All, docgen::ApiScope::Root)
    };
    let (json, done) = docgen::registry_docs_json(None, scope);
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

#[allow(clippy::too_many_arguments)] // a straight fan-out of the clap variant's fields
pub(crate) fn cmd_doc(
    file: &Option<PathBuf>,
    package: &Option<String>,
    out: &Option<PathBuf>,
    target: &Option<String>,
    api: bool,
    root: Option<&str>,
    non_builtin: bool,
    lint: bool,
) -> ExitCode {
    // `--api`: the registry-backed path — the intrinsic surface (stdlib + composed native modules)
    // as a schema-1 `docs.json`, organized by module. No local source involved.
    if api {
        return cmd_doc_api(out, root, non_builtin, lint);
    }
    // `--package`: the registry-fetch path — a published release's stored artifact, no local
    // source involved.
    if let Some(spec) = package {
        return cmd_doc_package(spec, out);
    }
    // No path and no `--package`/`--api`: document the current directory, as `check`/`test` do.
    let here = PathBuf::from(".");
    let file = file.as_deref().unwrap_or(&here);
    // The compose probe hands back the graph it resolved (default selection) for the load below
    // (audit-5 F2); the `--out` generator path never links, so it simply drops it.
    let resolved = match compose::maybe_delegate(file) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };
    if let Some(code) = target_gate(file, target, "doc") {
        return code;
    }
    // `--out` is checked BEFORE the directory branch below: the artifact is a *package's* docs,
    // keyed by the `[package]` identity beside its entry, so it needs that entry named. Silently
    // extracting to stdout instead would drop the flag the user actually asked for.
    if out.is_some() && file.is_dir() {
        eprintln!(
            "noeta: `--out` documents a package — name its entry `.noe` file, not a directory"
        );
        return ExitCode::from(2);
    }
    // A **directory**: extract from every `.noe` beneath it. There is no entry to link, so the
    // provider dispatch and the whole-program load below have nothing to act on — extraction works
    // on a parse alone, which is exactly what makes a directory meaningful here.
    if file.is_dir() {
        let rendered = render_docs(&doc_sources(file));
        if rendered.is_empty() {
            eprintln!("noeta: no `@doc` blocks found");
            return ExitCode::SUCCESS;
        }
        print!("{rendered}");
        let _ = io::stdout().flush();
        return ExitCode::SUCCESS;
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

    // Extract per file, over every module in the workspace — not just the entry's linked closure.
    // A `@doc` block is adjacency-resolved against the file it sits in, and linking merges a
    // module's *declarations* without the doc blocks beside them, so extracting from the linked
    // program silently dropped the documentation of every imported symbol: a two-module project
    // printed the entry's docs and nothing else, even for the module functions it calls. This is
    // also what `--out` has always done (`docgen::generate` documents every module it reads), so
    // the two halves of `noeta doc` now agree on what "the docs" means.
    let out = render_docs(&doc_sources(file));
    if out.is_empty() {
        eprintln!("noeta: no `@doc` blocks found");
        return ExitCode::SUCCESS;
    }
    print!("{out}");
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

/// The sources `noeta doc` extracts from: every `.noe` beneath a directory, or — for a file — the
/// entry together with its sibling modules, the same workspace `docgen::generate` reads.
fn doc_sources(path: &std::path::Path) -> Vec<noeta_span::Source> {
    if path.is_dir() {
        return crate::cmd::check::noe_files(path)
            .iter()
            .filter_map(|p| {
                std::fs::read_to_string(p).ok().map(|text| {
                    noeta_span::Source::new(
                        noeta_span::SourceId::FIRST,
                        p.display().to_string(),
                        text,
                    )
                })
            })
            .collect();
    }
    match noeta_loader::read_workspace(path) {
        Ok(workspace) => std::iter::once(workspace.entry)
            .chain(workspace.modules)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Extract and render every `@doc` block in `sources`, in file then source order.
///
/// Each file is parsed on its own and re-keyed to `SourceId::FIRST`, because doc adjacency is a
/// per-file fact and never crosses a file — the same reason `docgen::module_docs` does it. A file
/// that does not parse contributes nothing rather than failing the run: extraction works on a
/// parse alone, so it is expected to run over work-in-progress code.
fn render_docs(sources: &[noeta_span::Source]) -> String {
    let mut out = String::new();
    for source in sources {
        let local = noeta_span::Source::new(
            noeta_span::SourceId::FIRST,
            source.name(),
            source.text().to_string(),
        );
        let lexed = noeta_lexer::lex(&local);
        let parsed = noeta_parser::parse(&local, &lexed.tokens);
        if !lexed.diagnostics.is_empty() || !parsed.diagnostics.is_empty() {
            continue;
        }
        for doc in noeta_check::resolve_docs(&parsed.program) {
            if !out.is_empty() {
                out.push('\n');
            }
            let line = local.line_col(doc.span.start).line;
            // A declaration-attached block's header carries the documented symbol
            // (adjacency-resolved); an unattached block's header is the bare location.
            let header = match &doc.target {
                noeta_check::DocTarget::Decl { name, .. } => {
                    format!("<!-- {}:{} · {} -->\n", local.name(), line, name)
                }
                _ => format!("<!-- {}:{} -->\n", local.name(), line),
            };
            out.push_str(&header);
            out.push_str(&noeta_check::dedent_doc(&doc.text));
            out.push('\n');
        }
    }
    out
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
