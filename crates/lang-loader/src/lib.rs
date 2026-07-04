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

use lang_ast::{Program, Stmt, UseName};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId, SourceMap};

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

/// The raw [`Source`]s of a workspace: the entry plus its sibling module files, each with its own
/// [`SourceId`] (entry = 0, siblings 1..) assigned identically to [`link`]. Lexing/parsing happen
/// downstream, so this only reads and labels files — it feeds the salsa module graph (`lang-db`,
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

/// Resolve and merge an *already-parsed* entry against already-parsed candidate modules — the pure
/// linking core shared by [`link`] (which lexes/parses first) and the salsa module-graph query
/// (`lang-db`, M1.9.3), which feeds it programs straight from the memoized `ast` queries. `entry`
/// is the entry's [`Source`] (so import errors render against it); `modules` are the cleanly-parsed
/// candidate module programs (each declaring its `namespace`). Returns the merged [`Program`] — each
/// resolved import's declaration ahead of the entry's own statements — or the `use`-resolution
/// diagnostics (E0019 private/missing export, E0020 name collision).
pub fn link_parsed(
    entry: &Source,
    entry_program: &Program,
    modules: &[&Program],
) -> Result<Program, Vec<LoadDiagnostic>> {
    // A module contributes only if it declares a namespace to resolve against.
    let module_views: Vec<ModuleView> = modules
        .iter()
        .filter_map(|prog| {
            module_namespace(prog).map(|namespace| ModuleView {
                namespace,
                stmts: &prog.stmts,
            })
        })
        .collect();

    // The entry's own top-level declaration names, pre-scanned so an import colliding with a local
    // declaration is caught regardless of source order. As each import resolves, its name joins the
    // set; a name that fails to insert (a duplicate) is a collision — a second import of the same
    // name, or one that shadows a local declaration. The merged reference would be ambiguous.
    let mut declared: HashSet<String> = entry_program
        .stmts
        .iter()
        .filter_map(decl_name)
        .map(str::to_string)
        .collect();

    // Resolve the entry's imports against the module namespaces, pulling each resolved name's
    // real declaration and trimming it from the `use` so the runtime makes no opaque stub for it.
    let mut imported: Vec<Stmt> = Vec::new();
    let mut linked_stmts: Vec<Stmt> = Vec::with_capacity(entry_program.stmts.len());
    let mut errors: Vec<LoadDiagnostic> = Vec::new();
    for stmt in &entry_program.stmts {
        match stmt {
            Stmt::Use { path, names, span } => {
                let mut unresolved: Vec<UseName> = Vec::new();
                for name in names {
                    match resolve(&module_views, path, &name.name) {
                        Resolution::Resolved(decl) => {
                            if declared.insert(name.name.clone()) {
                                imported.push(*decl);
                            } else {
                                errors.push(collision_error(entry, path, name));
                            }
                        }
                        // No module declares this namespace: fall back to the opaque stub (M0).
                        Resolution::NoModule => unresolved.push(name.clone()),
                        // The namespace exists but does not export the name — a hard error: a
                        // private declaration is not importable, and an absent one is a typo.
                        Resolution::Private => {
                            errors.push(import_error(entry, path, name, Visibility::Private))
                        }
                        Resolution::Missing => {
                            errors.push(import_error(entry, path, name, Visibility::Missing))
                        }
                    }
                }
                // Keep the `use` only for names no module provided (opaque-stub fallback).
                if !unresolved.is_empty() {
                    linked_stmts.push(Stmt::Use {
                        path: path.clone(),
                        names: unresolved,
                        span: *span,
                    });
                }
            }
            other => linked_stmts.push(other.clone()),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut stmts = imported;
    stmts.append(&mut linked_stmts);
    Ok(Program {
        stmts,
        span: entry_program.span,
    })
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
