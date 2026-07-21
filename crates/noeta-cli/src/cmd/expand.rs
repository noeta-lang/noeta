//! `noeta expand` — print the Noeta source a program's compile-time `@`-directive expansions
//! produced.
//!
//! An expansion generates code the author never wrote, and until now the only way to see any of it
//! was to make it fail: a diagnostic inside generated code renders the line it landed on and
//! nothing else. This prints the whole thing, so a hook can be debugged against its real output and
//! a spec change shows up in CI as a reviewable delta rather than as a silent change of what the
//! program means.
//!
//! It is the load half of [`cmd_check`](super::check::cmd_check) and nothing more: same path
//! resolution, same per-directory parse, same [`ParsedDir::link_entry`](noeta_loader::ParsedDir)
//! link, same diagnostic rendering — then it prints the sources that link already produced instead
//! of type-checking the program. There is deliberately no second expansion path here; asking the
//! linker what it expanded is the only way this command can agree with the compiler.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_diagnostics::render_mapped;
use noeta_pm::{graph, manifest};

use crate::compose;

/// `noeta expand [PATH]` — link `PATH` and print the source of every declaration a compile-time
/// directive expanded, in Noeta syntax, on stdout.
///
/// `PATH` (default `.`) resolves exactly as `noeta check`'s does: a directory is walked recursively
/// and every `.noe` file under it is linked as its own entry; a file is linked with its
/// directory-sibling modules, as `run` does. Linking every file as an entry is what makes a
/// directive in a library module visible at all — no single entry need import it.
///
/// Exit codes follow `check`: **0** on success (a program with no expansions is a success, and says
/// so on stderr), **1** if any load diagnostic was reported — a hook that returned `Err`, generated
/// code that does not parse, or an ordinary parse error in the sources — and **2** for an
/// operational failure that stopped a file from being looked at at all.
pub(crate) fn cmd_expand(path: &std::path::Path) -> ExitCode {
    // A directive with an `expand` hook is contributed by a *native* extension, which the stock
    // binary does not carry: without composing from the app's manifest first, an app whose whole
    // reason to run this command is its expanding directive would report no expansions at all.
    // The probe hands back the graph it resolved, reused below for the directory it resolved
    // against (audit-5 F2).
    let mut resolved = match compose::maybe_delegate(path) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };

    let entries: Vec<PathBuf> = if path.is_dir() {
        super::check::noe_files(path)
    } else {
        vec![path.to_path_buf()]
    };
    if entries.is_empty() {
        eprintln!("noeta: no `.noe` files found under `{}`", path.display());
        return ExitCode::from(2);
    }

    // Every expansion, keyed by the name the loader gave it and its text. The name already *is* the
    // expansion's identity — `Api ⟨@openapi "petstore.yaml"⟩` names the declaration that grew the
    // members and the directive that grew them — so keying on it collapses the duplicate an
    // imported declaration would otherwise produce: a decorated module-level type expands once
    // under its own file's link and again under every entry that imports it, and printing the same
    // generated code N times would misrepresent how much code the program actually generates. The
    // text is part of the key so two genuinely different expansions that happen to share a name are
    // both shown rather than one silently winning. Ordered, so the output is diffable.
    let mut expansions: std::collections::BTreeMap<(String, String), ()> =
        std::collections::BTreeMap::new();
    // Diagnostics dedup exactly as `check`'s do — by the file, span, and code they render against,
    // never by `SourceId` (ids restart at 0 per directory) — so a module shared by several entries
    // reports its fault once.
    type MapDiag = (
        std::rc::Rc<noeta_span::SourceMap>,
        noeta_diagnostics::Diagnostic,
    );
    let mut diags: std::collections::BTreeMap<(String, u32, u32, &'static str), MapDiag> =
        std::collections::BTreeMap::new();
    let mut unreadable = false;

    // Group by directory, as `check` does: an entry's workspace is its directory's `.noe` files, so
    // each directory is read, resolved, lexed, and parsed once and every entry links against that
    // shared pool.
    let mut by_dir: std::collections::BTreeMap<PathBuf, Vec<&PathBuf>> =
        std::collections::BTreeMap::new();
    for entry in &entries {
        let dir = entry
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .to_path_buf();
        by_dir.entry(dir).or_default().push(entry);
    }
    // The directory the compose probe's graph belongs to — only that group may reuse it, since
    // another directory could resolve a different (nested) manifest.
    let probe_dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .to_path_buf()
    };

    for (dir, dir_entries) in &by_dir {
        let reusable = if *dir == probe_dir {
            resolved.take()
        } else {
            None
        };
        let deps = match reusable {
            Some(graph) => graph.packages,
            None => match graph::resolve_graph(dir_entries[0]) {
                Ok(graph) => graph.packages,
                Err(err) => {
                    for entry in dir_entries {
                        eprintln!("noeta: {}: {err}", entry.display());
                    }
                    unreadable = true;
                    continue;
                }
            },
        };
        let parsed = noeta_loader::parse_dir(
            noeta_loader::read_dir_modules(dir),
            manifest::root_edition(dir_entries[0]),
            &deps,
        );
        let sources = std::rc::Rc::new(parsed.source_map());

        let mut expand_entry = |parsed: &noeta_loader::ParsedDir,
                                shared: &std::rc::Rc<noeta_span::SourceMap>,
                                index: usize| {
            match parsed.link_entry(index) {
                // A failed expansion arrives here and nowhere else: the hook's `Err` and a parse
                // error in its output are both load diagnostics, blamed on the directive that was
                // written. Rendered by the same path a syntax error takes.
                Err(load_diagnostics) => {
                    for ld in &load_diagnostics {
                        let d = &ld.diagnostic;
                        let key = (
                            shared.source(d.span.source).name().to_string(),
                            d.span.start,
                            d.span.end,
                            d.code.code(),
                        );
                        diags
                            .entry(key)
                            .or_insert_with(|| (std::rc::Rc::clone(shared), d.clone()));
                    }
                }
                // `EntryLink::expansions` is the link's own answer to "what did this program
                // generate" — the sources it appended, told apart from hand-written ones by the
                // loader rather than by guessing at a source's name here.
                Ok(linked) => {
                    for source in &linked.expansions {
                        expansions
                            .insert((source.name().to_string(), source.text().to_string()), ());
                    }
                }
            }
        };

        for entry in dir_entries {
            let name = entry.display().to_string();
            match parsed.module_index(&name) {
                Some(index) => expand_entry(&parsed, &sources, index),
                // An entry the directory scan didn't yield: report it if the file itself is
                // unreadable, else link it alone — the same degradation `check` makes.
                None => match std::fs::read_to_string(entry) {
                    Err(err) => {
                        eprintln!("noeta: cannot read {}: {err}", entry.display());
                        unreadable = true;
                    }
                    Ok(text) => {
                        let lone = noeta_loader::parse_dir(
                            vec![noeta_loader::RawModule { name, text }],
                            manifest::root_edition(entry),
                            &deps,
                        );
                        let lone_sources = std::rc::Rc::new(lone.source_map());
                        expand_entry(&lone, &lone_sources, 0);
                    }
                },
            }
        }
    }

    let mut stderr = io::stderr();
    for (sources, diag) in diags.values() {
        let _ = stderr.write_all(render_mapped(sources, std::iter::once(diag)).as_bytes());
    }

    // The generated code goes to stdout on its own, so `noeta expand > expanded.noe` is a file a
    // reviewer diffs; every header is a Noeta comment, so what lands there is still Noeta source.
    // The header is the loader's own name for the expansion — target, directive, and arguments —
    // because that is what someone who did not write the generator needs in order to find it.
    let mut stdout = io::stdout();
    for (name, text) in expansions.keys() {
        let _ = writeln!(stdout, "// {name}");
        let _ = stdout.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = writeln!(stdout);
        }
        let _ = writeln!(stdout);
    }
    let _ = stdout.flush();

    // The summary goes to stderr, as `check`'s does, so it never contaminates the source on stdout.
    // "No expansions" is stated rather than left as silence: an empty stdout is also what a broken
    // invocation produces, and the difference matters to whoever is debugging a hook.
    let errors = diags.len();
    if expansions.is_empty() {
        eprintln!("no directive expansions");
    } else {
        let n = expansions.len();
        let decls = if n == 1 {
            "declaration"
        } else {
            "declarations"
        };
        eprintln!("expanded {n} {decls}");
    }

    if errors > 0 {
        ExitCode::from(1)
    } else if unreadable {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
