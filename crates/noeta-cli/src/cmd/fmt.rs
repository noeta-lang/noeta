//! `noeta fmt` — the canonical formatter driver: file/directory expansion, per-directory
//! text-tier + tier-body-formatter discovery, stdin mode, and atomic in-place writes.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_pm::{graph, manifest};
use noeta_span::{Source, SourceId};

/// `noeta fmt` — format `.noe` source into the canonical style. Rewrites the given files (or every
/// `.noe` under the given directories) in place by default; `--check` writes nothing and lists any
/// file that is not already formatted (exit 1, for CI); `--stdin` reads stdin and writes the result
/// to stdout. Style comes from the nearest `noeta.toml` `[fmt]` table. Files that do not parse are
/// left untouched and reported; the safety gate inside `noeta_fmt::format_source` guarantees a
/// written file never changes meaning. In-place writes are atomic (temp file + rename) and skipped
/// when the content is already canonical.
/// Apply any `--parens` / `--semicolons` CLI overrides on top of a manifest-resolved `FmtConfig`.
pub(crate) fn apply_fmt_overrides(
    mut config: noeta_fmt::FmtConfig,
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> noeta_fmt::FmtConfig {
    if let Some(p) = parens {
        config.parens = p;
    }
    if let Some(s) = semicolons {
        config.semicolons = s;
    }
    config
}

pub(crate) fn cmd_fmt(
    paths: &[PathBuf],
    check: bool,
    diff: bool,
    stdin: bool,
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> ExitCode {
    if stdin {
        return cmd_fmt_stdin(diff, parens, semicolons);
    }
    if paths.is_empty() {
        eprintln!("noeta fmt: no files given (use `--stdin` to format standard input)");
        return ExitCode::FAILURE;
    }

    // Tier-body formatters are a dev-only capability a package brings via `package.dev-native`. If
    // this app's dependency graph carries any native crate (dev-only formatters INCLUDED, unlike a
    // `run`/`check` delegation), compose and re-exec the dev toolchain so those formatters are
    // present — otherwise `@html` bodies from a package that provides an HTML formatter would be
    // left verbatim by a stock binary. On a successful compose this `exec`s and never returns; a
    // compose failure is surfaced rather than silently formatting without the formatter. A composed
    // binary (the `NOETA_COMPOSED` guard) skips this and formats with its own linked-in extensions.
    if let Err(err) = crate::compose::maybe_delegate_fmt(&paths[0]) {
        eprintln!("noeta fmt: cannot compose the toolchain for this app's formatters:");
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }

    // Expand any directory argument into the `.noe` files beneath it.
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_noe_files(path, &mut files);
        } else {
            files.push(path.clone());
        }
    }

    let mut failed = false; // a parse or IO error on any file
    let mut would_change = false; // `--check`: some file is not already formatted

    // Extension-registered body formatters, keyed by body **language** (e.g. std's `"json"`). A tier
    // — native or program-declared — whose `text:` language is here has its body reflowed; every
    // other tier stays verbatim. The language set is fixed (the installed extensions); the tier →
    // language resolution is per project, so the tier → formatter map is cached per directory.
    // Built with explicit inserts (not `collect`): the formatter fn type is higher-ranked over
    // lifetimes, which `FromIterator` cannot infer through here.
    let mut lang_formatters: std::collections::HashMap<&'static str, noeta_fmt::TierBodyFormatter> =
        std::collections::HashMap::new();
    let mut sub_formatters = noeta_fmt::TierBodyFormatters::new();
    for (language, formatter) in noeta_stdlib::registry::ext_body_formatters() {
        lang_formatters.insert(language, formatter);
        sub_formatters.insert(language.to_string(), formatter);
    }
    // Per-directory tier discovery (one scan per directory, audit-4 F10): the text-tier set —
    // a tier declared in a sibling file or a dependency package must keep this file's
    // `@<name> { … }` bodies verbatim (text-tiers arc) — and the tier → body-formatter map,
    // produced together by `fmt_dir_tiers`.
    let mut dir_tiers: std::collections::HashMap<
        PathBuf,
        (noeta_lexer::TextTiers, noeta_fmt::TierBodyFormatters),
    > = std::collections::HashMap::new();

    for file in &files {
        let dir = file.parent().unwrap_or_else(|| std::path::Path::new("."));
        let config = match manifest::resolve_fmt_config(file) {
            Ok(config) => apply_fmt_overrides(config, parens, semicolons),
            Err(err) => {
                eprintln!("noeta fmt: {err}");
                return ExitCode::FAILURE;
            }
        };
        let original = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("noeta fmt: cannot read `{}`: {err}", file.display());
                failed = true;
                continue;
            }
        };
        let (text_tiers, tier_formatters) = dir_tiers
            .entry(dir.to_path_buf())
            .or_insert_with(|| fmt_dir_tiers(dir, file, &lang_formatters))
            .clone();

        match noeta_fmt::format_source_in_with_formatters(
            &file.to_string_lossy(),
            &original,
            &config,
            manifest::root_edition(file),
            &text_tiers,
            &tier_formatters,
            &sub_formatters,
        ) {
            Ok(formatted) => {
                if formatted == original {
                    continue; // already canonical — no write, no churn
                }
                if diff {
                    would_change = true;
                    print!(
                        "{}",
                        unified_diff(&file.display().to_string(), &original, &formatted)
                    );
                } else if check {
                    would_change = true;
                    println!("{}", file.display());
                } else if let Err(err) = atomic_write(file, &formatted) {
                    eprintln!("noeta fmt: cannot write `{}`: {err}", file.display());
                    failed = true;
                }
            }
            Err(err) => {
                report_fmt_error(&file.display().to_string(), &err);
                failed = true;
            }
        }
    }

    if failed || ((check || diff) && would_change) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// A `git`/`diff -u`-style unified diff of a pending reformat: a `--- a/<path>` / `+++ b/<path>`
/// header and standard `@@` hunks with three lines of context. Printed by `noeta fmt --diff` instead
/// of rewriting the file.
pub(crate) fn unified_diff(path: &str, original: &str, formatted: &str) -> String {
    similar::TextDiff::from_lines(original, formatted)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// The per-directory tier discovery for formatting, in **one scan** (audit-4 F10): each sibling
/// `.noe` file — and, when the entry's package graph resolves (a manifest with dependencies),
/// each dependency module — is read once, producing both artifacts together:
///
/// - the project-wide **text-tier set** (text-tiers arc): the union of `@tier(…, text:/expr:)`
///   declarations, whose `@<name> { … }` bodies must stay verbatim. Mirrors the loader's
///   program-wide lex, so `noeta fmt` and `noeta run` agree on which bodies are verbatim. A
///   standalone file with no siblings or manifest gets the default set (same-file declarations
///   need no help — the lexer discovers those itself).
/// - the **`tier name → body formatter` map** (extension-driven tier-body formatting): a tier —
///   a program `@tier(name, …, text: "lang")`, or an installed native `ExtTier { name, text }` —
///   mapped to the formatter registered for its `text:` language, if any. So `@html` (a program
///   tier declaring `text: "html"`) gets a first-party HTML formatter, while its reactive
///   handler stays in Noeta.
///
/// These were two mirrored scans, each re-reading the directory and each resolving the
/// dependency graph, held identical by a comment; one scan resolves the graph once and cannot
/// drift. Each file is lexed **once**: the same [`noeta_lexer::Lexed`] yields `text_tier_decls`
/// and feeds [`noeta_lexer::declared_tier_languages_in`].
fn fmt_dir_tiers(
    dir: &std::path::Path,
    entry: &std::path::Path,
    lang_formatters: &std::collections::HashMap<&'static str, noeta_fmt::TierBodyFormatter>,
) -> (noeta_lexer::TextTiers, noeta_fmt::TierBodyFormatters) {
    let mut names: Vec<String> = Vec::new();
    let mut map = noeta_fmt::TierBodyFormatters::new();
    // Returns the file's own `@tier(…, text:/expr:)` declarations as well as folding them in, so the
    // per-dependency index the renamed-tier resolution needs comes out of this one lex too.
    let mut scan = |name: &str, text: &str| -> Vec<String> {
        let source = Source::new(SourceId(0), name, text);
        let lexed = noeta_lexer::lex(&source);
        for (tier, lang) in noeta_lexer::declared_tier_languages_in(&source, &lexed.tokens) {
            if let Some(&f) = lang_formatters.get(lang.as_str()) {
                map.insert(tier, f);
            }
        }
        names.extend(lexed.text_tier_decls.iter().cloned());
        lexed.text_tier_decls
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "noe")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                let _ = scan(&path.to_string_lossy(), &text);
            }
        }
    }
    // A query resolve: `noeta fmt` scanning dependency modules for text tiers must not refresh
    // `noeta.lock` as a side effect of formatting.
    if let Ok(graph) = graph::resolve_graph_query(entry) {
        // Each dependency's own text-tier declarations, indexed by its link key — the input the
        // renamed-tier resolution below needs for a `.noe` provider, collected from the same lex.
        let mut declared: Vec<(String, Vec<String>)> = Vec::new();
        for dep in &graph.packages {
            let mut decls: Vec<String> = Vec::new();
            for module in &dep.modules {
                decls.extend(scan(&module.name, &module.text));
            }
            if !decls.is_empty() {
                declared.push((dep.key().to_string(), decls));
            }
        }
        // The **root package's** `[directives]` renames (per-package naming arc 3g): a manifest binding
        // `docs = "std:doc"` makes `@docs { … }` a verbatim body for this package, and the formatter
        // must know it or it tokenizes markdown as code and declares the file unparseable — on a
        // file `noeta run` and `noeta check` both accept. The same resolution the loader and the
        // editor use, over the graph this function already had in hand. Root only: fmt's set is one
        // flat project set, and a *dependency's* local spelling must not capture in these files.
        let renamed = noeta_loader::renamed_text_tier_locals(
            &graph.package_uses,
            declared,
            &noeta_loader::ExtTiers::from_process_registry(),
        );
        if let Some(locals) = renamed.get(&noeta_span::PackageOrigin::Root) {
            names.extend(locals.iter().cloned());
        }
    }
    // Plus the installed extensions' verbatim-body tiers (no `.noe` file declares a native one),
    // and any native `ExtTier { name, text }` whose language has a registered formatter.
    names.extend(
        noeta_stdlib::registry::ext_verbatim_tier_names()
            .into_iter()
            .map(str::to_string),
    );
    for t in noeta_stdlib::registry::ext_tiers() {
        if let Some(lang) = t.text
            && let Some(&f) = lang_formatters.get(lang)
        {
            map.insert(t.name.to_string(), f);
        }
    }
    (noeta_lexer::TextTiers::with(names), map)
}

/// Recursively collect every `.noe` file under `dir` (skipping dot-directories like `.git`).
pub(crate) fn collect_noe_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("noeta fmt: cannot read directory `{}`", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if !hidden {
                collect_noe_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

/// Write `contents` to `path` atomically: write a sibling temp file, then rename over the target, so
/// a crash mid-write never leaves a truncated source file.
pub(crate) fn atomic_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dir.join(format!(".{name}.fmt-tmp"));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Format stdin → stdout with the config discovered from the current directory. With `diff`, print a
/// unified diff (input vs. formatted) instead of the formatted text, exiting non-zero if it differs.
pub(crate) fn cmd_fmt_stdin(
    diff: bool,
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> ExitCode {
    let text = match io::read_to_string(io::stdin()) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("noeta fmt: cannot read stdin: {err}");
            return ExitCode::FAILURE;
        }
    };
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Stdin has no path; use a representative `*.noe` name in cwd so `.editorconfig` globs apply.
    let config = match manifest::resolve_fmt_config(&dir.join("stdin.noe")) {
        Ok(config) => apply_fmt_overrides(config, parens, semicolons),
        Err(err) => {
            eprintln!("noeta fmt: {err}");
            return ExitCode::FAILURE;
        }
    };
    match noeta_fmt::format_source("<stdin>", &text, &config) {
        Ok(formatted) => {
            if diff {
                if formatted != text {
                    print!("{}", unified_diff("<stdin>", &text, &formatted));
                    return ExitCode::FAILURE;
                }
                return ExitCode::SUCCESS;
            }
            print!("{formatted}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            report_fmt_error("<stdin>", &err);
            ExitCode::FAILURE
        }
    }
}

/// Print a one-line reason a file could not be formatted (leaving it untouched).
pub(crate) fn report_fmt_error(name: &str, err: &noeta_fmt::FmtError) {
    use noeta_fmt::FmtError;
    match err {
        FmtError::Parse(diags) => {
            eprintln!(
                "{name}: not formatted — source does not parse ({} diagnostic(s))",
                diags.len()
            );
        }
        FmtError::Safety(why) => {
            eprintln!("{name}: not formatted — internal safety check failed: {why}");
        }
    }
}
