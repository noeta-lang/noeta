//! `noeta init` — scaffold a new Noeta project.
//!
//! Writes the manifest (with the std dev tiers wired into a `development` target and an
//! explicit `production` baseline), a `src/main.noe` that dogfoods the four tiers, the
//! editor surface (`.vscode/` run profiles + extension recommendation), `.gitignore`, and
//! the agent surface: `AGENTS.md` (how to drive the toolchain, CLI and MCP) plus a
//! generated `SYNTAX.md`. The syntax reference is **assembled from the embedded language
//! guide** (`noeta_ide::guide`, the same corpus `noeta lsp`/`noeta mcp` serve), so it
//! documents exactly the installed compiler instead of a hand-maintained copy that rots.
//!
//! Existing files are never overwritten — each is reported and skipped — so `init` is safe
//! to run in a non-empty directory, *including* one that is already a package. Re-running it
//! is additive: every missing scaffold file is created and every existing one (the manifest
//! included) is left byte-identical, which is how a stale generated `SYNTAX.md` is refreshed
//! after a toolchain upgrade — delete it and re-run. A run with nothing left to create says
//! so and succeeds.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noeta_pm::manifest::PackageName;

const MANIFEST: &str = include_str!("../../templates/init/noeta.toml");
const MAIN_NOE: &str = include_str!("../../templates/init/main.noe");
const GITIGNORE: &str = include_str!("../../templates/init/gitignore");
const LAUNCH_JSON: &str = include_str!("../../templates/init/launch.json");
const EXTENSIONS_JSON: &str = include_str!("../../templates/init/extensions.json");
const AGENTS_MD: &str = include_str!("../../templates/init/AGENTS.md");

/// The guide pages assembled into `SYNTAX.md`: enough to write correct Noeta without looking
/// anything up, and nothing more.
///
/// This used to be eighteen pages — the whole language half of the wiki, ~340 KB — because the
/// embedded guide had no CLI door, so a reader with no MCP server had one chance to get everything.
/// [`noeta docs`](super::docs) is that door now, so the file carries the three pages that answer
/// "how do I write this at all" and points at the rest: the tour (the whole surface, by example),
/// the tier model (where tests, benchmarks and docs live, since Noeta has no separate test
/// directory), and the naming conventions (which nothing lints, so nothing else will tell you).
const SYNTAX_PAGES: &[&str] = &["Language-Tour", "Dev-Tiers", "Conventions"];

pub(crate) fn cmd_init(path: &Path, name: &Option<String>, no_git: bool) -> ExitCode {
    // An existing manifest makes this a gap-filling run, not a fresh scaffold: it is the one
    // file whose *content* encodes a decision the user already made, so it is never rewritten
    // and it, not `--name`, names the package from here on.
    let manifest_path = path.join("noeta.toml");
    let adopting = manifest_path.exists();
    if adopting && name.is_some() {
        eprintln!(
            "noeta: ignoring --name: {} already has a noeta.toml (edit it to rename the package)",
            path.display()
        );
    }

    // Resolve the package name first: `--name` verbatim, else `local/<dir>` — so a bad
    // name (or an unusable directory name) fails before anything touches the filesystem.
    let dir_label = dir_stem(path);
    let raw_name = match name {
        Some(n) if !adopting => n.clone(),
        _ => format!("local/{}", sanitize_identifier(&dir_label)),
    };
    let package_name = match PackageName::parse(&raw_name) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::FAILURE;
        }
    };
    // …but when a manifest is already there, the label we report is *its* name, read back
    // from disk, so the summary can never claim a package the directory doesn't hold.
    let label = if adopting {
        existing_package_label(&manifest_path).unwrap_or_else(|| path.display().to_string())
    } else {
        format!("`{}/{}`", package_name.company, package_name.package)
    };

    if let Err(err) = std::fs::create_dir_all(path) {
        eprintln!("noeta: cannot create {}: {err}", path.display());
        return ExitCode::FAILURE;
    }

    let manifest = MANIFEST.replace("@PACKAGE_NAME@", &raw_name);
    let files: &[(&str, String)] = &[
        ("noeta.toml", manifest),
        ("src/main.noe", MAIN_NOE.to_string()),
        (".gitignore", GITIGNORE.to_string()),
        (".vscode/launch.json", LAUNCH_JSON.to_string()),
        (".vscode/extensions.json", EXTENSIONS_JSON.to_string()),
        ("AGENTS.md", AGENTS_MD.to_string()),
        ("SYNTAX.md", render_syntax_md()),
    ];
    let mut created = 0usize;
    let mut kept = 0usize;
    for (rel, contents) in files {
        match write_new(path, rel, contents) {
            Ok(true) => {
                created += 1;
                println!("  created {rel}");
            }
            Ok(false) => {
                kept += 1;
                println!("  exists  {rel} (left unchanged)");
            }
            Err(err) => {
                eprintln!("noeta: cannot write {rel}: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if !no_git && !inside_git_worktree(path) {
        match git_init(path) {
            Ok(()) => {
                created += 1;
                println!("  created git repository");
            }
            // A missing/failing `git` shouldn't fail the scaffold — everything above is
            // already in place and useful without version control.
            Err(err) => eprintln!("noeta: skipped `git init`: {err}"),
        }
    }

    if created == 0 {
        println!(
            "nothing to do: {} is already fully scaffolded ({kept} files left unchanged)",
            path.display()
        );
    } else if adopting {
        println!(
            "updated Noeta package {label} in {}: {created} created, {kept} left unchanged",
            path.display()
        );
    } else {
        println!("initialized Noeta package {label} in {}", path.display());
    }
    ExitCode::SUCCESS
}

/// `` `company/package` `` read back from an existing manifest, or `None` when it cannot be
/// parsed — a manifest we can't read is still not one we may overwrite, so the caller falls
/// back to naming the directory rather than failing a run that only creates missing files.
fn existing_package_label(manifest_path: &Path) -> Option<String> {
    let manifest = noeta_pm::manifest::load(manifest_path).ok()?;
    let pkg = manifest.package()?;
    Some(format!("`{}/{}`", pkg.name.company, pkg.name.package))
}

/// Write `rel` under `root` unless it already exists. `Ok(true)` = written,
/// `Ok(false)` = pre-existing (left alone).
fn write_new(root: &Path, rel: &str, contents: &str) -> std::io::Result<bool> {
    let target = root.join(rel);
    if target.exists() {
        return Ok(false);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, contents)?;
    Ok(true)
}

/// The directory's own name, resolving `.`/relative paths against the cwd so
/// `noeta init` in `~/projects/webapp` names the package after `webapp`.
fn dir_stem(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    // components() normalizes away trailing `.` segments, so `init .` sees the cwd's name.
    absolute
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .next_back()
        .unwrap_or("app")
        .to_string()
}

/// Coerce a directory name into a manifest identifier (`[A-Za-z_][A-Za-z0-9_]*`):
/// lowercase, every other character folded to `_`, digit-led names prefixed. `my-webapp`
/// → `my_webapp`. Empty/degenerate names fall back to `app`.
fn sanitize_identifier(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else {
            out.push('_');
        }
    }
    while out.starts_with('_') && out.len() > 1 {
        out.remove(0);
    }
    if out.is_empty() || out.chars().all(|c| c == '_') {
        return "app".to_string();
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Whether `dir` is already inside a git worktree (so `init` doesn't nest a repo).
fn inside_git_worktree(dir: &Path) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_init(dir: &Path) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Assemble `SYNTAX.md` from the embedded guide: a provenance header naming the command that
/// fetches everything this file leaves out, each of [`SYNTAX_PAGES`] verbatim, then an index of
/// the whole guide. Cross-page wiki links between included pages are rewritten to in-document
/// anchors so the file works standalone; links to pages it does not carry become `noeta docs`
/// commands, so a reference the file cannot satisfy still says how to satisfy it.
fn render_syntax_md() -> String {
    let pages: Vec<(&str, &'static str)> = SYNTAX_PAGES
        .iter()
        // Every slug is a repo `docs/*.md` page baked in at compile time, so a miss can
        // only mean a renamed page — skip it rather than scaffold a broken reference,
        // and let the CLI test that asserts every slug resolves catch the rename.
        .filter_map(|slug| noeta_ide::guide::get_page(slug).map(|body| (*slug, body)))
        .collect();

    let mut out = String::with_capacity(64 * 1024);
    out.push_str(&format!(
        "# The Noeta language reference\n\n\
         Generated by `noeta init` (noeta {}) from the language guide embedded in the toolchain, \
         so it describes the compiler you have installed. Re-run `noeta init` after `noeta \
         upgrade` to refresh it (delete this file first — `init` never overwrites).\n\n\
         **This is the short version.** The full guide ships inside the `noeta` binary and is \
         searchable offline, with no network and no project:\n\n\
         ```console\n\
         $ noeta docs pattern matching        # rank the guide's sections against a query\n\
         $ noeta docs --page Error-Handling   # read one page\n\
         $ noeta docs --page Error-Handling#the-try-operator   # read one section\n\
         $ noeta docs --list                  # every page\n\
         ```\n\n\
         Reach for it rather than guessing: the signatures and rules there are the compiler's own. \
         Every page is listed at the end of this file.\n\n## Contents\n\n",
        env!("CARGO_PKG_VERSION")
    ));
    for (_, body) in &pages {
        let title = page_title(body);
        out.push_str(&format!("- [{title}](#{})\n", github_anchor(&title)));
    }
    out.push_str("- [The rest of the guide](#the-rest-of-the-guide)\n");
    for (_, body) in &pages {
        out.push_str("\n---\n\n");
        out.push_str(&rewrite_wiki_links(body, &pages));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&render_guide_index(&pages));
    out
}

/// The closing index: every guide page the toolchain carries, with the command that reads it.
/// Pages already included above are marked, so a reader can tell "it is above" from "fetch it".
fn render_guide_index(included: &[(&str, &'static str)]) -> String {
    let mut out = String::from(
        "\n---\n\n## The rest of the guide\n\n\
         Every page below is embedded in the `noeta` binary. Read one with \
         `noeta docs --page <slug>`, or search them all with `noeta docs <query>`.\n\n\
         | Page | Slug |\n|---|---|\n",
    );
    for (slug, title) in noeta_ide::guide::index() {
        let here = included.iter().any(|(s, _)| *s == slug);
        let note = if here { " *(above)*" } else { "" };
        out.push_str(&format!("| {title}{note} | `{slug}` |\n"));
    }
    out
}

/// The page's first `# ` heading (every guide page has one).
fn page_title(body: &str) -> String {
    body.lines()
        .find_map(|l| l.strip_prefix("# "))
        .unwrap_or("Untitled")
        .trim()
        .to_string()
}

/// GitHub's heading-anchor scheme: lowercase, spaces to `-`, punctuation dropped.
fn github_anchor(title: &str) -> String {
    title
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == ' ' || c == '-' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Where the guide is published, for links this file cannot resolve locally.
const DOCS_SITE: &str = "https://docs.noeta.dev";

/// Site routes the guide links to that are **generated**, not `docs/*.md` pages: the standard
/// library API reference and the diagnostics catalog (rendered from `noeta explain --all
/// --format json`). They resolve on the site but have no page in the embedded corpus, so they
/// need the same absolute rewrite the excluded pages get.
const GENERATED_ROUTES: &[&str] = &["Std", "Diagnostics"];

/// Rewrite the guide's wiki links (`[Language Tour](Language-Tour)`, with an optional
/// `#fragment`) so every one of them still goes somewhere from a standalone file.
///
/// A link between two *included* pages becomes an in-document anchor. A link to any other guide
/// page becomes its published URL — the closing index names the `noeta docs` command that reads
/// the same page offline. A link to something that is not a guide page at all (a generated
/// reference, an external site) is left exactly as it was.
fn rewrite_wiki_links(body: &str, pages: &[(&str, &'static str)]) -> String {
    let mut out = body.to_string();
    for (slug, target_body) in pages {
        let anchor = format!("](#{})", github_anchor(&page_title(target_body)));
        // The fragment form first (`](Slug#section)` → `](#section)`: the fragment
        // already names a heading anchor in the merged document), then the bare form.
        out = out.replace(&format!("]({slug}#"), "](#");
        out = out.replace(&format!("]({slug})"), &anchor);
    }
    let elsewhere = noeta_ide::guide::index()
        .into_iter()
        .map(|(slug, _)| slug)
        .filter(|slug| !pages.iter().any(|(s, _)| s == slug))
        .chain(GENERATED_ROUTES.iter().map(|s| s.to_string()));
    for slug in elsewhere {
        out = out.replace(&format!("]({slug}#"), &format!("]({DOCS_SITE}/{slug}#"));
        out = out.replace(&format!("]({slug})"), &format!("]({DOCS_SITE}/{slug})"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_folds_to_identifier() {
        assert_eq!(sanitize_identifier("my-webapp"), "my_webapp");
        assert_eq!(sanitize_identifier("My App 2"), "my_app_2");
        assert_eq!(sanitize_identifier("42things"), "_42things");
        assert_eq!(sanitize_identifier("---"), "app");
        assert_eq!(sanitize_identifier(""), "app");
    }

    #[test]
    fn every_syntax_page_resolves() {
        for slug in SYNTAX_PAGES {
            assert!(
                noeta_ide::guide::get_page(slug).is_some(),
                "SYNTAX.md source page `{slug}` missing from the embedded guide — renamed?"
            );
        }
    }

    #[test]
    fn syntax_md_toc_matches_pages() {
        let rendered = render_syntax_md();
        assert!(rendered.contains("# The Noeta language reference"));
        // Every bundled page's title must appear both in the TOC and as a heading.
        for slug in SYNTAX_PAGES {
            let title = page_title(noeta_ide::guide::get_page(slug).unwrap());
            assert!(
                rendered.contains(&format!("- [{title}](#")),
                "TOC entry missing for `{title}`"
            );
        }
    }

    /// The scaffold must point at the guide it does not carry — otherwise trimming the bundle
    /// just loses the material instead of relocating it.
    #[test]
    fn syntax_md_indexes_every_page_and_names_the_command_that_reads_them() {
        let rendered = render_syntax_md();
        assert!(
            rendered.contains("noeta docs --page"),
            "the file must name the command that fetches what it leaves out"
        );
        for (slug, _) in noeta_ide::guide::index() {
            assert!(
                rendered.contains(&format!("`{slug}`")),
                "the closing index is missing `{slug}`"
            );
        }
    }

    /// A wiki link out of an included page must not become a dead relative path once the file
    /// stands alone: either it resolves in-document, or it goes to the published site.
    #[test]
    fn syntax_md_leaves_no_dangling_wiki_links() {
        let rendered = render_syntax_md();
        let targets = noeta_ide::guide::index()
            .into_iter()
            .map(|(slug, _)| slug)
            .chain(GENERATED_ROUTES.iter().map(|s| s.to_string()));
        for slug in targets {
            if SYNTAX_PAGES.contains(&slug.as_str()) {
                continue;
            }
            // A bare `](Slug)` or `](Slug#…)` would be a relative link to a file that is not there.
            assert!(
                !rendered.contains(&format!("]({slug})"))
                    && !rendered.contains(&format!("]({slug}#")),
                "`{slug}` is still linked as a bare wiki path"
            );
        }
    }

    /// The point of the trim. The old bundle was eighteen pages; a scaffolded file that large is
    /// one an agent cannot afford to read, which is what made it useless.
    #[test]
    fn syntax_md_stays_small_enough_to_read() {
        let rendered = render_syntax_md();
        assert!(
            rendered.len() < 64 * 1024,
            "SYNTAX.md grew to {} bytes — it is meant to be the short version, with \
             `noeta docs` carrying the depth",
            rendered.len()
        );
    }
}
