//! Multi-file module loading and linking (M1.9).
//!
//! A program is rooted at an *entry* `.noe` file. Sibling `.noe` files in the entry's
//! directory are candidate *modules*, each declaring its identity with `namespace App.Models;`.
//! The entry's `use App.Models.{User}` declarations resolve against those modules' declared
//! namespaces; each resolved name's real declaration is **merged into one [`Program`]** ahead of
//! the entry's own statements, so both backends run the linked program unchanged and the
//! differential oracle is preserved by construction.
//!
//! Linking is *additive and backward-compatible*: a `use` that no loaded module provides is left
//! in place, so the runtime falls back to its M0 opaque-stub behavior — a single file with no
//! sibling modules links to exactly itself. Real module loading therefore lights up only when
//! sibling modules actually provide the imported names.
//!
//! Diagnostics produced while loading carry the [`Source`] they should render against (a sibling
//! module's parse error renders against that module, not the entry). *Check/runtime* diagnostics
//! that land on a declaration merged in from a sibling resolve to the right source too: every span
//! is tagged at parse time with its [`SourceId`], so the caller resolves it through the [`Linked`]
//! [`SourceMap`] rather than against the entry (slice F4).

use std::collections::HashSet;
use std::io;
use std::path::Path;

use noeta_ast::{Program, Stmt, UseName};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId, SourceMap};

/// A loaded, linked program ready to type-check and run.
#[derive(Debug)]
pub struct Linked {
    /// The merged program: each resolved imported declaration, in resolution order, followed by
    /// the entry's own statements (with resolved names removed from their `use` lists).
    pub program: Program,
    /// The entry source — kept for the entry's name and the single-source rendering path.
    pub entry: Source,
    /// Every source the program is built from (entry + sibling modules), indexed by `SourceId`.
    /// A diagnostic may land on a declaration merged in from a sibling module; its span carries
    /// that module's `SourceId`, so it resolves to the right file through this map rather than
    /// always rendering against the entry.
    pub sources: SourceMap,
}

/// A diagnostic produced while loading, paired with the source it renders against.
#[derive(Debug)]
pub struct LoadDiagnostic {
    pub source: Source,
    pub diagnostic: Diagnostic,
}

/// Load and link the program rooted at `entry_path`. Returns the linked program, or the
/// load-time (lex/parse) diagnostics of the entry, each paired with its source. An `io::Error`
/// is only for a failure to read the entry file itself.
pub fn load(entry_path: &Path) -> io::Result<Result<Linked, Vec<LoadDiagnostic>>> {
    let text = std::fs::read_to_string(entry_path)?;
    let name = entry_path.display().to_string();
    let siblings = read_siblings(entry_path);
    Ok(link(&name, &text, &siblings))
}

/// One sibling module's identity (display name + source text), before parsing. Public so the
/// linker can be driven from in-memory sources in tests.
#[derive(Debug)]
pub struct RawModule {
    pub name: String,
    pub text: String,
}

/// Load and link the program rooted at `entry_path` **with its dependency packages** — the
/// dependency-aware twin of [`load`] (package-manager P2.1). The entry's siblings resolve as before;
/// each [`DepPackage`]'s modules are additionally re-rooted and linked in. An `io::Error` is only for
/// a failure to read the entry file itself.
pub fn load_with_deps(
    entry_path: &Path,
    deps: &[DepPackage],
) -> io::Result<Result<Linked, Vec<LoadDiagnostic>>> {
    let text = std::fs::read_to_string(entry_path)?;
    let name = entry_path.display().to_string();
    let siblings = read_siblings(entry_path);
    Ok(link_with_deps(&name, &text, &siblings, deps))
}

/// Read every `.noe` file **under `dir` recursively** as a [`RawModule`], in sorted order (so
/// SourceId assignment stays deterministic). A dependency package is a directory *tree*, not the
/// single flat directory the entry's siblings live in, so this walks subdirectories. Names are the
/// files' display paths (for diagnostics). Unreadable files are skipped.
pub fn read_package_sources(dir: &Path) -> io::Result<Vec<RawModule>> {
    let mut paths = Vec::new();
    collect_noe_files(dir, &mut paths)?;
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(RawModule {
                name: p.display().to_string(),
                text,
            })
        })
        .collect())
}

/// Recursively gather `.noe` file paths under `dir` into `out`. A subdirectory that can't be read is
/// skipped (best-effort), matching the sibling scan's tolerance.
fn collect_noe_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let _ = collect_noe_files(&path, out);
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "noe") {
            out.push(path);
        }
    }
    Ok(())
}

/// A dependency package's sources, to be linked into the entry under the consumer's import root
/// (package-manager P2.1, model R1). `root` is the package's own namespace root segment (the
/// `package` half of its `[package] name`); `key` is the consumer's dependency-table key. The loader
/// **re-roots** the package's modules from `root` to `key` — rewriting the leading segment of each
/// module's `namespace` and its intra-package `use`s — so the consumer addresses the package as
/// `use <key>.<sub>.Name` while the package's own imports (written against `root`) keep resolving.
///
/// Unlike a sibling, a dependency module's own `use`s **drive** imports (a package is a closed unit:
/// its internal cross-references must resolve), whereas same-app siblings stay pure decl-sources — so
/// wiring dependencies never changes single-package linking.
#[derive(Debug)]
pub struct DepPackage {
    pub key: String,
    pub root: String,
    pub modules: Vec<RawModule>,
}

/// Re-root a namespace/use path in place: if its leading segment is the package's own `root`, replace
/// it with the consumer's `key` (package-manager P2.1). A path that doesn't start with `root` (a
/// reference to `std`, or a malformed package path) is left untouched.
fn reroot_path(path: &mut [String], root: &str, key: &str) {
    if path.first().map(String::as_str) == Some(root) {
        path[0] = key.to_string();
    }
}

/// Re-root a dependency module's `namespace` (its match key) and `use` paths (its import drivers)
/// from the package root to the consumer's key. Touches only those two statement kinds — both are
/// consumed *during* linking (matching / import-driving) and never appear in the merged declaration
/// output — so re-rooting cannot alter what a package actually contributes, only how it's addressed.
fn reroot_program(program: &mut Program, root: &str, key: &str) {
    for stmt in &mut program.stmts {
        match stmt {
            Stmt::Namespace { path, .. } => reroot_path(path, root, key),
            Stmt::Use { path, .. } => reroot_path(path, root, key),
            _ => {}
        }
    }
}

/// The raw [`Source`]s of a workspace: the entry plus its sibling module files, each with its own
/// [`SourceId`] (entry = 0, siblings 1..) assigned identically to [`link`]. Lexing/parsing happen
/// downstream, so this only reads and labels files — it feeds the salsa module graph (`noeta-db`,
/// M1.9.3), which derives one memoized `SourceProgram` input per source.
#[derive(Debug)]
pub struct RawWorkspace {
    pub entry: Source,
    pub modules: Vec<Source>,
}

/// Read the entry file and its sibling `.noe` modules into labeled [`Source`]s, without lexing,
/// parsing, or linking — the file-system front of the salsa module graph. An `io::Error` is only
/// for a failure to read the entry itself; unreadable siblings are skipped (as in [`read_siblings`]).
pub fn read_workspace(entry_path: &Path) -> io::Result<RawWorkspace> {
    let text = std::fs::read_to_string(entry_path)?;
    let entry = Source::new(SourceId(0), entry_path.display().to_string(), text);
    let modules = read_siblings(entry_path)
        .into_iter()
        .enumerate()
        .map(|(i, raw)| Source::new(SourceId((i + 1) as u32), raw.name, raw.text))
        .collect();
    Ok(RawWorkspace { entry, modules })
}

/// Gather the `.noe` files in the entry's directory other than the entry itself, in sorted
/// order (so SourceId assignment and resolution are deterministic). A read failure yields no
/// siblings — a lone file simply links to itself.
fn read_siblings(entry_path: &Path) -> Vec<RawModule> {
    let Some(dir) = entry_path.parent() else {
        return Vec::new();
    };
    let entry_name = entry_path.file_name();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "noe"))
        .filter(|p| p.file_name() != entry_name)
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(RawModule {
                name: p.display().to_string(),
                text,
            })
        })
        .collect()
}

/// The pure core of [`load`], split out so it is testable from in-memory sources: link the entry
/// (`entry_name`/`entry_text`) against the given sibling modules.
pub fn link(
    entry_name: &str,
    entry_text: &str,
    siblings: &[RawModule],
) -> Result<Linked, Vec<LoadDiagnostic>> {
    // The entry is always SourceId 0; siblings follow. Each module keeps its own source so its
    // spans stay valid and its diagnostics render against it.
    let entry = Source::new(SourceId(0), entry_name, entry_text);
    let entry_lexed = lex(&entry);
    let entry_parsed = parse(&entry, &entry_lexed.tokens);
    let entry_diags: Vec<Diagnostic> = entry_lexed
        .diagnostics
        .iter()
        .chain(entry_parsed.diagnostics.iter())
        .cloned()
        .collect();
    if !entry_diags.is_empty() {
        return Err(entry_diags
            .into_iter()
            .map(|diagnostic| LoadDiagnostic {
                source: entry.clone(),
                diagnostic,
            })
            .collect());
    }

    // Parse each sibling. Only cleanly-parsed modules contribute (a module that fails to lex/parse
    // cannot be resolved against and is skipped — surfacing a *referenced-but-broken* module's
    // errors is the deferred SourceMap work). Keep the parsed programs alive for [`link_parsed`].
    // Every sibling's `Source` is retained (whether or not it parsed) so the `SourceMap` indices
    // line up with the `SourceId`s the parser stamped onto spans (entry = 0, sibling i = i + 1).
    let mut module_programs: Vec<Program> = Vec::new();
    let mut sources: Vec<Source> = vec![entry.clone()];
    for (i, raw) in siblings.iter().enumerate() {
        let source = Source::new(
            SourceId((i + 1) as u32),
            raw.name.as_str(),
            raw.text.as_str(),
        );
        let lexed = lex(&source);
        let parsed = (lexed.diagnostics.is_empty())
            .then(|| parse(&source, &lexed.tokens))
            .filter(|parsed| parsed.diagnostics.is_empty());
        sources.push(source);
        if let Some(parsed) = parsed {
            module_programs.push(parsed.program);
        }
    }

    let refs: Vec<&Program> = module_programs.iter().collect();
    let program = link_parsed(&entry, &entry_parsed.program, &refs)?;
    Ok(Linked {
        program,
        entry,
        sources: SourceMap::new(sources),
    })
}

/// Link the entry against its sibling modules **and its dependency packages** (package-manager P2.1).
/// Each [`DepPackage`]'s modules are parsed, re-rooted from the package's own root segment to the
/// consumer's dependency key ([`reroot_program`]), and linked as a closed unit (their own `use`s
/// drive imports). SourceIds continue past the siblings (entry = 0, siblings `1..=S`, dependency
/// modules after), so every declaration's spans and diagnostics still render against their own file.
/// A dependency module that fails to lex/parse is skipped (its `Source` is still retained so the
/// `SourceMap` indices line up), mirroring the sibling policy.
pub fn link_with_deps(
    entry_name: &str,
    entry_text: &str,
    siblings: &[RawModule],
    deps: &[DepPackage],
) -> Result<Linked, Vec<LoadDiagnostic>> {
    let entry = Source::new(SourceId(0), entry_name, entry_text);
    let entry_lexed = lex(&entry);
    let entry_parsed = parse(&entry, &entry_lexed.tokens);
    let entry_diags: Vec<Diagnostic> = entry_lexed
        .diagnostics
        .iter()
        .chain(entry_parsed.diagnostics.iter())
        .cloned()
        .collect();
    if !entry_diags.is_empty() {
        return Err(entry_diags
            .into_iter()
            .map(|diagnostic| LoadDiagnostic {
                source: entry.clone(),
                diagnostic,
            })
            .collect());
    }

    let mut next_id: u32 = 1;
    let mut sources: Vec<Source> = vec![entry.clone()];

    // Parse the siblings (pure decl-sources), assigning SourceIds `1..=S`.
    let mut sibling_programs: Vec<Program> = Vec::new();
    for raw in siblings {
        let source = Source::new(SourceId(next_id), raw.name.as_str(), raw.text.as_str());
        next_id += 1;
        let parsed = parse_clean(&source);
        sources.push(source);
        if let Some(program) = parsed {
            sibling_programs.push(program);
        }
    }

    // Parse + re-root each dependency package's modules, continuing the SourceId sequence.
    let mut dep_programs: Vec<Program> = Vec::new();
    for dep in deps {
        for raw in &dep.modules {
            let source = Source::new(SourceId(next_id), raw.name.as_str(), raw.text.as_str());
            next_id += 1;
            let parsed = parse_clean(&source);
            sources.push(source);
            if let Some(mut program) = parsed {
                reroot_program(&mut program, &dep.root, &dep.key);
                dep_programs.push(program);
            }
        }
    }

    let sibling_refs: Vec<&Program> = sibling_programs.iter().collect();
    let dep_refs: Vec<&Program> = dep_programs.iter().collect();
    let program =
        link_parsed_with_deps(&entry, &entry_parsed.program, &sibling_refs, &dep_refs)?;
    Ok(Linked {
        program,
        entry,
        sources: SourceMap::new(sources),
    })
}

/// Lex + parse a source, yielding its [`Program`] only if both are clean (a module that fails to
/// parse cannot be resolved against and is skipped). The shared helper behind [`link`]'s sibling
/// loop and [`link_with_deps`].
fn parse_clean(source: &Source) -> Option<Program> {
    let lexed = lex(source);
    (lexed.diagnostics.is_empty())
        .then(|| parse(source, &lexed.tokens))
        .filter(|parsed| parsed.diagnostics.is_empty())
        .map(|parsed| parsed.program)
}

/// Resolve and merge an *already-parsed* entry against already-parsed candidate modules — the pure
/// linking core shared by [`link`] (which lexes/parses first) and the salsa module-graph query
/// (`noeta-db`, M1.9.3), which feeds it programs straight from the memoized `ast` queries. `entry`
/// is the entry's [`Source`] (so import errors render against it); `modules` are the cleanly-parsed
/// candidate module programs (each declaring its `namespace`). Returns the merged [`Program`] — each
/// resolved import's declaration ahead of the entry's own statements — or the `use`-resolution
/// diagnostics (E0019 private/missing export, E0020 name collision).
pub fn link_parsed(
    entry: &Source,
    entry_program: &Program,
    modules: &[&Program],
) -> Result<Program, Vec<LoadDiagnostic>> {
    link_core(entry, entry_program, modules, &[])
}

/// The cross-package variant (package-manager P2.1): like [`link_parsed`], but `dep_modules` are the
/// re-rooted source modules of the entry's dependency packages. They are **both** resolution
/// candidates *and* import drivers — a package is a closed unit, so its own `use`s (already re-rooted
/// to the consumer's key) resolve its internal cross-references, unlike same-app siblings which stay
/// pure decl-sources. `dep_modules` must already be re-rooted (see [`reroot_program`]); the caller
/// ([`link_with_deps`]) does that. Every std import inside a dependency (`use std.…`) resolves against
/// no module here and is retained (deduped) so the compiler binds it downstream, exactly as an
/// entry's std imports are.
pub fn link_parsed_with_deps(
    entry: &Source,
    entry_program: &Program,
    siblings: &[&Program],
    dep_modules: &[&Program],
) -> Result<Program, Vec<LoadDiagnostic>> {
    // Dependency modules join the resolution pool; only they (not siblings) also drive imports.
    let pool: Vec<&Program> = siblings.iter().chain(dep_modules).copied().collect();
    link_core(entry, entry_program, &pool, dep_modules)
}

/// Where a merged top-level name came from — its local declaration, or the namespace an import
/// pulled it from. Lets two `use`s that name the **same** declaration (the entry and a dependency
/// module both importing `webclient.client.Client`) merge it once, while a genuine clash (same name,
/// different namespace, or an import shadowing a local) is still an E0020 collision.
enum Origin {
    Local,
    Import(Vec<String>),
}

/// The shared linking core: resolve the entry's imports (and any `dep_drivers`' imports) against the
/// `pool`, merging each resolved declaration once and retaining every unresolved `use` (deduped) for
/// the compiler's downstream binding. `dep_drivers` is empty for single-package linking, so that path
/// is unchanged bar one refinement: a duplicate *identical* import (same namespace + name) is now
/// skipped rather than flagged, which a closed dependency unit needs and no well-formed program relied
/// on erroring.
fn link_core(
    entry: &Source,
    entry_program: &Program,
    pool: &[&Program],
    dep_drivers: &[&Program],
) -> Result<Program, Vec<LoadDiagnostic>> {
    // A module contributes only if it declares a namespace to resolve against.
    let module_views: Vec<ModuleView> = pool
        .iter()
        .filter_map(|prog| {
            module_namespace(prog).map(|namespace| ModuleView {
                namespace,
                stmts: &prog.stmts,
            })
        })
        .collect();

    // Every merged top-level name → its origin. Seeded with the entry's own declarations (each
    // `Local`); an import that would shadow a local, or clash with a differently-sourced import, is a
    // collision. An import that re-names an already-merged declaration from the *same* namespace is a
    // no-op (the closed-unit dedup).
    let mut origins: std::collections::HashMap<String, Origin> = entry_program
        .stmts
        .iter()
        .filter_map(decl_name)
        .map(|n| (n.to_string(), Origin::Local))
        .collect();

    let mut imported: Vec<Stmt> = Vec::new();
    let mut errors: Vec<LoadDiagnostic> = Vec::new();
    // Retained (unresolved) imports — std imports and opaque-stub fallbacks — deduped by (path, name)
    // across the entry and every dependency so a shared `use std.…` isn't bound twice.
    let mut seen_retained: HashSet<(Vec<String>, String)> = HashSet::new();
    let mut dep_retained: Vec<Stmt> = Vec::new();
    let mut entry_stmts: Vec<Stmt> = Vec::with_capacity(entry_program.stmts.len());

    // Resolve one `use`'s names against the pool. Resolved names merge their declaration (deduped by
    // origin); unresolved names are collected for retention. Returns the still-unresolved names.
    let mut drive_use = |path: &[String],
                         names: &[UseName],
                         imported: &mut Vec<Stmt>,
                         errors: &mut Vec<LoadDiagnostic>|
     -> Vec<UseName> {
        let mut unresolved = Vec::new();
        for name in names {
            match resolve(&module_views, path, &name.name) {
                Resolution::Resolved(decl) => match origins.get(&name.name) {
                    None => {
                        origins.insert(name.name.clone(), Origin::Import(path.to_vec()));
                        imported.push(*decl);
                    }
                    // Same declaration re-imported (same namespace) — merge once, ignore the rest.
                    Some(Origin::Import(p)) if p.as_slice() == path => {}
                    // A different declaration under the same name — ambiguous.
                    Some(_) => errors.push(collision_error(entry, path, name)),
                },
                Resolution::NoModule => unresolved.push(name.clone()),
                Resolution::Private => {
                    errors.push(import_error(entry, path, name, Visibility::Private))
                }
                Resolution::Missing => {
                    errors.push(import_error(entry, path, name, Visibility::Missing))
                }
            }
        }
        unresolved
    };

    // The entry: its `use`s drive imports; its other statements are the program's tail (in order).
    for stmt in &entry_program.stmts {
        match stmt {
            Stmt::Use { path, names, span } => {
                let unresolved = drive_use(path, names, &mut imported, &mut errors);
                let fresh = retain_fresh(&mut seen_retained, path, unresolved);
                if !fresh.is_empty() {
                    entry_stmts.push(Stmt::Use {
                        path: path.clone(),
                        names: fresh,
                        span: *span,
                    });
                }
            }
            other => entry_stmts.push(other.clone()),
        }
    }

    // Each dependency module's `use`s also drive imports (closed unit); their unresolved remainder
    // (std imports) is retained up front, ahead of the entry's statements.
    for driver in dep_drivers {
        for stmt in &driver.stmts {
            if let Stmt::Use { path, names, span } = stmt {
                let unresolved = drive_use(path, names, &mut imported, &mut errors);
                let fresh = retain_fresh(&mut seen_retained, path, unresolved);
                if !fresh.is_empty() {
                    dep_retained.push(Stmt::Use {
                        path: path.clone(),
                        names: fresh,
                        span: *span,
                    });
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Merged declarations, then dependency std-imports, then the entry's own statements.
    let mut stmts = imported;
    stmts.append(&mut dep_retained);
    stmts.append(&mut entry_stmts);
    Ok(Program {
        stmts,
        span: entry_program.span,
    })
}

/// Filter `names` down to those not yet retained under `path`, recording the fresh ones — so a
/// `use std.…` shared by the entry and several dependencies is retained exactly once.
fn retain_fresh(
    seen: &mut HashSet<(Vec<String>, String)>,
    path: &[String],
    names: Vec<UseName>,
) -> Vec<UseName> {
    names
        .into_iter()
        .filter(|n| seen.insert((path.to_vec(), n.name.clone())))
        .collect()
}

/// A candidate module viewed for resolution: its declared namespace path and a borrow of its
/// statements (so resolution clones only the declarations it actually imports).
struct ModuleView<'a> {
    namespace: Vec<String>,
    stmts: &'a [Stmt],
}

/// The namespace a module declares (`namespace App.Models;`), if any.
fn module_namespace(program: &Program) -> Option<Vec<String>> {
    program.stmts.iter().find_map(|stmt| match stmt {
        Stmt::Namespace { path, .. } => Some(path.clone()),
        _ => None,
    })
}

/// The outcome of resolving one imported name against the loaded modules.
enum Resolution {
    /// The namespace exists and exports the name (a `pub` declaration): merge this clone. Boxed
    /// because the declaration statement is far larger than the unit variants.
    Resolved(Box<Stmt>),
    /// The namespace exists and declares the name, but it is not `pub`.
    Private,
    /// The namespace exists but declares no such name.
    Missing,
    /// No loaded module declares the namespace — fall back to the opaque stub (M0 behavior).
    NoModule,
}

/// Resolve the name `name` of namespace `path` against the loaded modules, honoring `pub`
/// visibility: only a `pub` declaration is importable.
fn resolve(modules: &[ModuleView], path: &[String], name: &str) -> Resolution {
    let Some(module) = modules.iter().find(|m| m.namespace == path) else {
        return Resolution::NoModule;
    };
    match module
        .stmts
        .iter()
        .find(|stmt| decl_name(stmt) == Some(name))
    {
        Some(decl) if decl_is_public(decl) => Resolution::Resolved(Box::new(decl.clone())),
        Some(_) => Resolution::Private,
        None => Resolution::Missing,
    }
}

/// Which `E0019` an unresolved import is.
enum Visibility {
    Private,
    Missing,
}

/// Build the `E0019` diagnostic for an import the resolved module does not export, pointed at the
/// imported name in the entry's `use`.
fn import_error(
    entry: &Source,
    path: &[String],
    name: &UseName,
    kind: Visibility,
) -> LoadDiagnostic {
    let namespace = path.join(".");
    let message = match kind {
        Visibility::Private => {
            format!("`{}` is private to module `{namespace}`", name.name)
        }
        Visibility::Missing => format!("module `{namespace}` has no export `{}`", name.name),
    };
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic: Diagnostic::error(DiagnosticCode::UnresolvedImport, name.span, message),
    }
}

/// Build the `E0020` diagnostic for an import whose name collides with another top-level name in
/// the entry (a second import of it, or a local declaration), pointed at the imported name.
fn collision_error(entry: &Source, path: &[String], name: &UseName) -> LoadDiagnostic {
    let namespace = path.join(".");
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic: Diagnostic::error(
            DiagnosticCode::NameCollision,
            name.span,
            format!(
                "`{}` imported from `{namespace}` collides with another top-level name",
                name.name
            ),
        )
        .with_help("rename or remove the conflicting import or declaration"),
    }
}

/// Whether a top-level declaration is `pub` (importable). Statements that declare no name are
/// never importable.
fn decl_is_public(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Class(d) => d.is_public,
        Stmt::Struct(d) => d.is_public,
        Stmt::Enum(d) => d.is_public,
        Stmt::Fn(d) => d.is_public,
        _ => false,
    }
}

/// The name a top-level declaration introduces (a class, struct, enum, or function); `None` for
/// statements that declare no importable name.
fn decl_name(stmt: &Stmt) -> Option<&str> {
    match stmt {
        Stmt::Class(decl) => Some(&decl.name),
        Stmt::Struct(decl) => Some(&decl.name),
        Stmt::Enum(decl) => Some(&decl.name),
        Stmt::Fn(decl) => Some(&decl.name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, text: &str) -> RawModule {
        RawModule {
            name: name.to_string(),
            text: text.to_string(),
        }
    }

    // --- cross-package linking (package-manager P2.1) -----------------------------------------

    fn has_class(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Class(c) if c.name == name))
    }

    #[test]
    fn a_dependency_package_is_imported_under_the_consumer_key() {
        // Package `guzzle/http` (root segment `http`) exposes `http.client.Client`; the consumer
        // keys it `webclient` and imports `use webclient.client.Client` — the loader re-roots
        // `http.*` → `webclient.*`.
        let dep = DepPackage {
            key: "webclient".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "client.noe",
                "namespace http.client;\npub class Client {\n  base: string\n}\n",
            )],
        };
        let entry = "use webclient.client.Client;\nc = Client { base: \"x\" };\n";
        let linked = link_with_deps("main.noe", entry, &[], std::slice::from_ref(&dep)).unwrap();
        assert!(has_class(&linked, "Client"));
        // The consumer's `use` resolved — no opaque stub remains for it.
        assert!(
            !linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { path, .. } if path == &["webclient".to_string(), "client".to_string()]))
        );
    }

    #[test]
    fn a_package_internal_import_resolves_after_reroot() {
        // The dependency's own module imports a sibling of the same package (`use http.models.Body`);
        // re-rooting rewrites it to `use webclient.models.Body`, and — because a dependency module's
        // `use`s drive imports — `Body` is pulled in even though the consumer never named it.
        let dep = DepPackage {
            key: "webclient".to_string(),
            root: "http".to_string(),
            modules: vec![
                module(
                    "client.noe",
                    "namespace http.client;\nuse http.models.Body;\npub class Client {\n  body: Body\n}\n",
                ),
                module(
                    "models.noe",
                    "namespace http.models;\npub class Body {\n  text: string\n}\n",
                ),
            ],
        };
        let entry = "use webclient.client.Client;\nc = Client { body: Body { text: \"hi\" } };\n";
        let linked = link_with_deps("main.noe", entry, &[], std::slice::from_ref(&dep)).unwrap();
        assert!(has_class(&linked, "Client"));
        assert!(has_class(&linked, "Body"), "the package-internal Body must be linked in");
    }

    #[test]
    fn two_packages_sharing_a_root_coexist_under_distinct_keys() {
        // Both packages have root segment `http` (e.g. `a/http` and `b/http`); distinct keys keep
        // them apart — the collision the dep-key decoupling exists to prevent.
        let a = DepPackage {
            key: "alpha".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "a.noe",
                "namespace http.core;\npub class Ping {\n  n: int\n}\n",
            )],
        };
        let b = DepPackage {
            key: "beta".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "b.noe",
                "namespace http.core;\npub class Pong {\n  n: int\n}\n",
            )],
        };
        let entry = "use alpha.core.Ping;\nuse beta.core.Pong;\np = Ping { n: 1 };\nq = Pong { n: 2 };\n";
        let linked = link_with_deps("main.noe", entry, &[], &[a, b]).unwrap();
        assert!(has_class(&linked, "Ping"));
        assert!(has_class(&linked, "Pong"));
    }

    #[test]
    fn a_dependency_std_import_is_retained_for_the_compiler() {
        // A package that `use`s `std.*` internally: the loader can't resolve std (not a module), so
        // the `use std.math.sqrt` is retained in the merged program for the compiler to bind.
        let dep = DepPackage {
            key: "geo".to_string(),
            root: "shapes".to_string(),
            modules: vec![module(
                "circle.noe",
                "namespace shapes.circle;\nuse std.math.sqrt;\npub fn area(r: float): float { return 3.14 * r * r; }\n",
            )],
        };
        let entry = "use geo.circle.area;\necho area(2.0);\n";
        let linked = link_with_deps("main.noe", entry, &[], std::slice::from_ref(&dep)).unwrap();
        // `use std.math.sqrt` survives (retained) so the native-module resolver still sees it.
        assert!(
            linked.program.stmts.iter().any(|s| matches!(
                s,
                Stmt::Use { path, .. } if path == &["std".to_string(), "math".to_string()]
            )),
            "the dependency's std import must be retained"
        );
    }

    #[test]
    fn links_a_used_module_declaration() {
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User {\n  name: string\n  id: int\n  fn new(name: string, id: int): User { return User { name: name, id: id }; }\n}\n",
        );
        let entry =
            "namespace App.Main;\nuse App.Models.User;\nu = User.new(\"Ada\", 7);\necho u.name;\n";
        let linked = link("main.noe", entry, std::slice::from_ref(&models)).unwrap();
        // The real `User` class is merged in; its `use` is dropped (no opaque stub for it).
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Class(c) if c.name == "User"))
        );
        assert!(
            !linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { .. }))
        );
    }

    #[test]
    fn unresolved_use_falls_back_to_opaque_stub() {
        // No sibling provides `App.Models.User`, so the `use` is kept for the opaque-stub fallback.
        let entry = "use App.Models.User;\nu = User { name: \"Ada\" };\n";
        let linked = link("main.noe", entry, &[]).unwrap();
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { .. }))
        );
    }

    #[test]
    fn importing_a_private_declaration_is_e0019() {
        // The namespace exists and declares `User`, but it is not `pub` → a hard error.
        let models = module(
            "models.noe",
            "namespace App.Models;\nclass User { id: int }\n",
        );
        let entry = "use App.Models.User;\n";
        let errs = link("main.noe", entry, std::slice::from_ref(&models)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::UnresolvedImport);
    }

    #[test]
    fn importing_a_missing_export_is_e0019() {
        // The namespace exists but declares no `Ghost`.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let entry = "use App.Models.Ghost;\n";
        let errs = link("main.noe", entry, std::slice::from_ref(&models)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::UnresolvedImport);
    }

    #[test]
    fn an_import_colliding_with_a_local_declaration_is_e0020() {
        // The entry imports `User` but also declares its own `User` — the reference is ambiguous.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let entry = "use App.Models.User;\nclass User { name: string }\n";
        let errs = link("main.noe", entry, std::slice::from_ref(&models)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::NameCollision);
    }

    #[test]
    fn two_imports_of_the_same_name_collide() {
        // Two modules each export `User`; importing both leaves an ambiguous reference.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let people = module(
            "people.noe",
            "namespace App.People;\npub class User { name: string }\n",
        );
        let entry = "use App.Models.User;\nuse App.People.User;\n";
        let errs = link("main.noe", entry, &[models, people]).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::NameCollision);
    }

    #[test]
    fn entry_parse_error_is_reported_against_the_entry() {
        let errs = link("main.noe", "echo $;", &[]).unwrap_err();
        assert!(!errs.is_empty());
        assert!(errs.iter().all(|e| e.source.name() == "main.noe"));
    }

    #[test]
    fn merged_declaration_spans_carry_the_sibling_source_id() {
        // The `User` class is declared in the sibling (SourceId 1) and merged into the entry
        // program. Its spans — including those deep in a method body — must stay tagged with the
        // sibling's id so a diagnostic on them renders against `models.noe`, not the entry.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User {\n  id: int\n  fn bad(): int { return 1 / 0; }\n}\n",
        );
        let entry = "use App.Models.User;\nu = User { id: 1 };\n";
        let linked = link("main.noe", entry, std::slice::from_ref(&models)).unwrap();

        // The merged class statement and everything under it belong to source 1 (the sibling).
        let class = linked
            .program
            .stmts
            .iter()
            .find(|s| matches!(s, Stmt::Class(c) if c.name == "User"))
            .expect("merged User class");
        assert_eq!(class.span().source, SourceId(1));

        // The entry's own statements stay tagged with source 0.
        let entry_stmt = linked
            .program
            .stmts
            .iter()
            .find(|s| matches!(s, Stmt::Binding { .. }))
            .expect("an entry statement");
        assert_eq!(entry_stmt.span().source, SourceId(0));

        // The source map resolves the sibling id to the sibling file.
        assert_eq!(linked.sources.source(SourceId(1)).name(), "models.noe");
    }
}
