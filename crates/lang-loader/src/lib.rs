//! Multi-file module loading and linking (M1.9).
//!
//! A program is rooted at an *entry* `.lang` file. Sibling `.lang` files in the entry's
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
//! module's parse error renders against that module, not the entry). Rendering of *check/runtime*
//! diagnostics that land on merged-in declarations against the right source is M1.9.2; for now
//! the caller renders those against the entry source (positive linked programs produce none).

use std::io;
use std::path::Path;

use lang_ast::{Program, Stmt, UseName};
use lang_diagnostics::Diagnostic;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

/// A loaded, linked program ready to type-check and run.
#[derive(Debug)]
pub struct Linked {
    /// The merged program: each resolved imported declaration, in resolution order, followed by
    /// the entry's own statements (with resolved names removed from their `use` lists).
    pub program: Program,
    /// The entry source — what check/runtime diagnostics render against.
    pub entry: Source,
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

/// Gather the `.lang` files in the entry's directory other than the entry itself, in sorted
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
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "lang"))
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

    // Parse each sibling. Only cleanly-parsed modules that declare a namespace contribute their
    // declarations; a module that fails to parse (or declares no namespace) cannot be resolved
    // against and is skipped — surfacing a *referenced-but-broken* module's errors is M1.9.2.
    let mut modules: Vec<ParsedModule> = Vec::new();
    for (i, raw) in siblings.iter().enumerate() {
        let source = Source::new(
            SourceId((i + 1) as u32),
            raw.name.as_str(),
            raw.text.as_str(),
        );
        let lexed = lex(&source);
        if !lexed.diagnostics.is_empty() {
            continue;
        }
        let parsed = parse(&source, &lexed.tokens);
        if !parsed.diagnostics.is_empty() {
            continue;
        }
        if let Some(namespace) = module_namespace(&parsed.program) {
            modules.push(ParsedModule {
                namespace,
                stmts: parsed.program.stmts,
            });
        }
    }

    // Resolve the entry's imports against the module namespaces, pulling each resolved name's
    // real declaration and trimming it from the `use` so the runtime makes no opaque stub for it.
    let mut imported: Vec<Stmt> = Vec::new();
    let mut linked_stmts: Vec<Stmt> = Vec::with_capacity(entry_parsed.program.stmts.len());
    for stmt in entry_parsed.program.stmts {
        match stmt {
            Stmt::Use { path, names, span } => {
                let mut unresolved: Vec<UseName> = Vec::new();
                for name in names {
                    match resolve(&modules, &path, &name.name) {
                        Some(decl) => imported.push(decl),
                        None => unresolved.push(name),
                    }
                }
                // Keep the `use` only for names no module provided (opaque-stub fallback).
                if !unresolved.is_empty() {
                    linked_stmts.push(Stmt::Use {
                        path,
                        names: unresolved,
                        span,
                    });
                }
            }
            other => linked_stmts.push(other),
        }
    }

    let mut stmts = imported;
    stmts.append(&mut linked_stmts);
    let program = Program {
        stmts,
        span: entry_parsed.program.span,
    };
    Ok(Linked { program, entry })
}

/// A parsed sibling module: its declared namespace path and its statements.
struct ParsedModule {
    namespace: Vec<String>,
    stmts: Vec<Stmt>,
}

/// The namespace a module declares (`namespace App.Models;`), if any.
fn module_namespace(program: &Program) -> Option<Vec<String>> {
    program.stmts.iter().find_map(|stmt| match stmt {
        Stmt::Namespace { path, .. } => Some(path.clone()),
        _ => None,
    })
}

/// Find the declaration named `name` exported by the module whose namespace is `path`. Returns a
/// clone of the declaration statement, ready to merge into the linked program.
fn resolve(modules: &[ParsedModule], path: &[String], name: &str) -> Option<Stmt> {
    let module = modules.iter().find(|m| m.namespace == path)?;
    module
        .stmts
        .iter()
        .find(|stmt| decl_name(stmt) == Some(name))
        .cloned()
}

/// The name a top-level declaration introduces (a class, record, enum, or function); `None` for
/// statements that declare no importable name.
fn decl_name(stmt: &Stmt) -> Option<&str> {
    match stmt {
        Stmt::Class(decl) => Some(&decl.name),
        Stmt::Record(decl) => Some(&decl.name),
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
            "models.lang",
            "namespace App.Models;\nclass User {\n  name: string\n  id: int\n  fn new(name: string, id: int): User { return User { name: name, id: id }; }\n}\n",
        );
        let entry =
            "namespace App.Main;\nuse App.Models.User;\nu = User.new(\"Ada\", 7);\necho u.name;\n";
        let linked = link("main.lang", entry, std::slice::from_ref(&models)).unwrap();
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
        let linked = link("main.lang", entry, &[]).unwrap();
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { .. }))
        );
    }

    #[test]
    fn entry_parse_error_is_reported_against_the_entry() {
        let errs = link("main.lang", "echo $;", &[]).unwrap_err();
        assert!(!errs.is_empty());
        assert!(errs.iter().all(|e| e.source.name() == "main.lang"));
    }
}
