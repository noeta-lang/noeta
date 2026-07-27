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
//! to run in a non-empty directory; only a pre-existing `noeta.toml` aborts (the directory
//! is already a package).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noeta_pm::manifest::PackageName;

const MANIFEST: &str = include_str!("../../templates/init/noeta.toml");
const MAIN_NOE: &str = include_str!("../../templates/init/main.noe");
const GITIGNORE: &str = include_str!("../../templates/init/gitignore");
const LAUNCH_JSON: &str = include_str!("../../templates/init/launch.json");
const EXTENSIONS_JSON: &str = include_str!("../../templates/init/extensions.json");
const AGENTS_MD: &str = include_str!("../../templates/init/AGENTS.md");

/// The guide pages assembled into `SYNTAX.md`, in reading order: lexical foundations
/// first, then the tour, then each topic in the order the tour introduces it. Deliberately
/// excludes toolchain/internals pages (The-CLI, The-Virtual-Machine, …) — SYNTAX.md is the
/// *language* reference; AGENTS.md covers the tooling.
const SYNTAX_PAGES: &[&str] = &[
    "Syntax-Basics",
    "Language-Tour",
    "Type-System",
    "Structs-Classes-and-Enums",
    "Functions-and-Closures",
    "Control-Flow-and-Pattern-Matching",
    "Error-Handling",
    "Generics-and-Traits",
    "Derives",
    "Modules",
    "Fixed-Width-Integers",
    "Concurrency",
    "Dev-Tiers",
    "Documentation-and-Tiers",
    "Attributes-and-Reflection",
    "Testing",
    "Standard-Library",
];

pub(crate) fn cmd_init(path: &Path, name: &Option<String>, no_git: bool) -> ExitCode {
    // Resolve the package name first: `--name` verbatim, else `local/<dir>` — so a bad
    // name (or an unusable directory name) fails before anything touches the filesystem.
    let dir_label = dir_stem(path);
    let raw_name = match name {
        Some(n) => n.clone(),
        None => format!("local/{}", sanitize_identifier(&dir_label)),
    };
    let package_name = match PackageName::parse(&raw_name) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = std::fs::create_dir_all(path) {
        eprintln!("noeta: cannot create {}: {err}", path.display());
        return ExitCode::FAILURE;
    }
    if path.join("noeta.toml").exists() {
        eprintln!(
            "noeta: {} is already a Noeta package (noeta.toml exists)",
            path.display()
        );
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
    for (rel, contents) in files {
        match write_new(path, rel, contents) {
            Ok(true) => println!("  created {rel}"),
            Ok(false) => println!("  exists  {rel} (left unchanged)"),
            Err(err) => {
                eprintln!("noeta: cannot write {rel}: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if !no_git && !inside_git_worktree(path) {
        match git_init(path) {
            Ok(()) => println!("  created git repository"),
            // A missing/failing `git` shouldn't fail the scaffold — everything above is
            // already in place and useful without version control.
            Err(err) => eprintln!("noeta: skipped `git init`: {err}"),
        }
    }

    println!(
        "initialized Noeta package `{}/{}` in {}",
        package_name.company,
        package_name.package,
        path.display()
    );
    ExitCode::SUCCESS
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

/// Assemble `SYNTAX.md` from the embedded guide: a provenance header, a table of
/// contents, then each of [`SYNTAX_PAGES`] verbatim. Cross-page wiki links between
/// included pages are rewritten to in-document anchors so the file works standalone.
fn render_syntax_md() -> String {
    let pages: Vec<(&str, &'static str)> = SYNTAX_PAGES
        .iter()
        // Every slug is a repo `docs/*.md` page baked in at compile time, so a miss can
        // only mean a renamed page — skip it rather than scaffold a broken reference,
        // and let the CLI test that asserts every slug resolves catch the rename.
        .filter_map(|slug| noeta_ide::guide::get_page(slug).map(|body| (*slug, body)))
        .collect();

    let mut out = String::with_capacity(256 * 1024);
    out.push_str(&format!(
        "# The Noeta language reference\n\n\
         Generated by `noeta init` (noeta {}) from the toolchain's embedded language guide \
         — the same pages `noeta mcp`'s `docs_search` serves. It matches the installed \
         compiler; after upgrading the toolchain, delete this file and re-run `noeta init` \
         to refresh it.\n\n## Contents\n\n",
        env!("CARGO_PKG_VERSION")
    ));
    for (_, body) in &pages {
        let title = page_title(body);
        out.push_str(&format!("- [{title}](#{})\n", github_anchor(&title)));
    }
    for (_, body) in &pages {
        out.push_str("\n---\n\n");
        out.push_str(&rewrite_wiki_links(body, &pages));
        if !out.ends_with('\n') {
            out.push('\n');
        }
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

/// Rewrite guide-internal links (`[Language Tour](Language-Tour)`, with an optional
/// `#fragment`) between *included* pages into in-document anchors. Links to pages not in
/// the bundle are left as-is — inert but honest about where they point.
fn rewrite_wiki_links(body: &str, pages: &[(&str, &'static str)]) -> String {
    let mut out = body.to_string();
    for (slug, target_body) in pages {
        let anchor = format!("](#{})", github_anchor(&page_title(target_body)));
        // The fragment form first (`](Slug#section)` → `](#section)`: the fragment
        // already names a heading anchor in the merged document), then the bare form.
        out = out.replace(&format!("]({slug}#"), "](#");
        out = out.replace(&format!("]({slug})"), &anchor);
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
}
