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
    stdin: bool,
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> ExitCode {
    if stdin {
        return cmd_fmt_stdin(parens, semicolons);
    }
    if paths.is_empty() {
        eprintln!("noeta fmt: no files given (use `--stdin` to format standard input)");
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
    // Per-directory text-tier sets (text-tiers arc): a tier declared in a sibling file or a
    // dependency package must keep this file's `@<name> { … }` bodies verbatim.
    let mut tier_sets: std::collections::HashMap<PathBuf, noeta_lexer::TextTiers> =
        std::collections::HashMap::new();

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
    let mut formatter_sets: std::collections::HashMap<PathBuf, noeta_fmt::TierBodyFormatters> =
        std::collections::HashMap::new();

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
        let text_tiers = tier_sets
            .entry(dir.to_path_buf())
            .or_insert_with(|| fmt_text_tiers(dir, file))
            .clone();
        let tier_formatters = formatter_sets
            .entry(dir.to_path_buf())
            .or_insert_with(|| fmt_tier_formatters(dir, file, &lang_formatters))
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
                if check {
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

    if failed || (check && would_change) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The project-wide text-tier set for formatting files in `dir` (text-tiers arc): the union of
/// `@tier(…, text: "…")` declarations across the directory's sibling `.noe` files and — when the
/// entry's package graph resolves (a manifest with dependencies) — every dependency module.
/// Mirrors the loader's program-wide lex, so `noeta fmt` and `noeta run` agree on which bodies
/// are verbatim. A standalone file with no siblings or manifest gets the default set (same-file
/// declarations need no help — the lexer discovers those itself).
pub(crate) fn fmt_text_tiers(
    dir: &std::path::Path,
    entry: &std::path::Path,
) -> noeta_lexer::TextTiers {
    let mut names: Vec<String> = Vec::new();
    let mut scan = |name: &str, text: &str| {
        let source = Source::new(SourceId(0), name, text);
        names.extend(noeta_lexer::lex(&source).text_tier_decls);
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "noe")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                scan(&path.to_string_lossy(), &text);
            }
        }
    }
    // A query resolve: `noeta fmt` scanning dependency modules for text tiers must not refresh
    // `noeta.lock` as a side effect of formatting.
    if let Ok(graph) = graph::resolve_graph_query(entry) {
        for dep in &graph.packages {
            for module in &dep.modules {
                scan(&module.name, &module.text);
            }
        }
    }
    // Plus the installed extensions' verbatim-body tiers (no `.noe` file declares a native one).
    names.extend(
        noeta_stdlib::registry::ext_verbatim_tier_names()
            .into_iter()
            .map(str::to_string),
    );
    noeta_lexer::TextTiers::with(names)
}

/// The `tier name → body formatter` map for formatting files in `dir` (extension-driven tier-body
/// formatting). A tier — a program `@tier(name, …, text: "lang")` in the directory's siblings or a
/// dependency module, or an installed native `ExtTier { name, text }` — is mapped to the formatter
/// registered for its `text:` language, if any. So `@html` (a program tier declaring `text: "html"`)
/// gets a first-party HTML formatter, while its reactive handler stays in Noeta. Mirrors
/// [`fmt_text_tiers`]'s scan so the same tiers are seen.
pub(crate) fn fmt_tier_formatters(
    dir: &std::path::Path,
    entry: &std::path::Path,
    lang_formatters: &std::collections::HashMap<&'static str, noeta_fmt::TierBodyFormatter>,
) -> noeta_fmt::TierBodyFormatters {
    let mut map = noeta_fmt::TierBodyFormatters::new();
    let mut scan = |name: &str, text: &str| {
        let source = Source::new(SourceId(0), name, text);
        for (tier, lang) in noeta_lexer::declared_tier_languages(&source) {
            if let Some(&f) = lang_formatters.get(lang.as_str()) {
                map.insert(tier, f);
            }
        }
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "noe")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                scan(&path.to_string_lossy(), &text);
            }
        }
    }
    // A query resolve: `noeta fmt` scanning dependency modules for text tiers must not refresh
    // `noeta.lock` as a side effect of formatting.
    if let Ok(graph) = graph::resolve_graph_query(entry) {
        for dep in &graph.packages {
            for module in &dep.modules {
                scan(&module.name, &module.text);
            }
        }
    }
    // Native tiers: an installed `ExtTier { name, text }` whose language has a registered formatter.
    for t in noeta_stdlib::registry::ext_tiers() {
        if let Some(lang) = t.text
            && let Some(&f) = lang_formatters.get(lang)
        {
            map.insert(t.name.to_string(), f);
        }
    }
    map
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

/// Format stdin → stdout with the config discovered from the current directory.
pub(crate) fn cmd_fmt_stdin(
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
