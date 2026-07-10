//! M3 Introspect pillar: traverse the compiler's artifacts. `ast` (the pretty-printed syntax tree),
//! `bytecode` (what actually runs — the VM disassembly), `pipeline` (a per-stage health summary),
//! `module_graph` (the `use`/`namespace` import edges), and `reflect` (the `@role`/`@semantic`
//! architectural graph). Every one is a pure read over the public salsa graph + AST — the same
//! `reflect::build` and `Module::disassemble` the runtime and `noeta dump` use, so an agent sees
//! ground truth, not a re-derivation.

use crate::analyze::{self, Prepared, SpanLoc};
use noeta_ast::{Pretty, Program, Stmt};
use rmcp::schemars;
use serde::Serialize;

// ---- ast --------------------------------------------------------------------------------------

/// The `ast` result: the entry file's pretty-printed syntax tree (S-expressions with `@start..end`
/// spans), via `noeta_ast::Pretty` — the same printer the compiler's own tests read.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AstOutput {
    pub ast: String,
}

pub fn ast(p: &Prepared) -> AstOutput {
    let parsed = noeta_db::ast(&p.db, analyze::entry_program(p));
    AstOutput {
        ast: parsed.0.program.to_pretty_string(),
    }
}

// ---- bytecode ---------------------------------------------------------------------------------

/// The `bytecode` result: the VM disassembly of the whole workspace, or the first construct the VM
/// does not support (with the reason).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BytecodeOutput {
    /// True when the program compiled to bytecode.
    pub compiled: bool,
    /// The disassembly (opcodes, constant pool, per-proto) when `compiled`; empty otherwise.
    pub disassembly: String,
    /// The reason the program is outside the VM subset, when not `compiled`.
    pub unsupported: Option<String>,
}

pub fn bytecode(p: &Prepared) -> BytecodeOutput {
    // The salsa `bytecode`/`linked_bytecode` queries are `returns(ref)`, so borrow the result.
    let compiled = noeta_db::linked_bytecode(&p.db, p.ws);
    match &compiled.0 {
        Ok(module) => BytecodeOutput {
            compiled: true,
            disassembly: module.disassemble(),
            unsupported: None,
        },
        Err(unsupported) => BytecodeOutput {
            compiled: false,
            disassembly: String::new(),
            unsupported: Some(unsupported.to_string()),
        },
    }
}

// ---- pipeline ---------------------------------------------------------------------------------

/// The `pipeline` result: a per-stage summary (lex → parse → check → compile) — where a program
/// breaks and how big each stage's output is. The agent's "what's the shape of this / where does it
/// fall over" glance before drilling into a specific stage.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PipelineOutput {
    /// Number of lexed tokens in the entry file.
    pub tokens: usize,
    /// Number of top-level statements parsed in the entry file.
    pub top_level_items: usize,
    /// Type-check error / warning counts over the whole workspace.
    pub errors: usize,
    pub warnings: usize,
    /// Whether the workspace compiled to VM bytecode (`false` carries the reason in `note`).
    pub compiles: bool,
    /// A one-line note: the first blocking reason, or `ok`.
    pub note: String,
}

pub fn pipeline(p: &Prepared) -> PipelineOutput {
    let entry = analyze::entry_program(p);
    let tokens = noeta_db::tokens(&p.db, entry).0.tokens.len();
    let top_level_items = noeta_db::ast(&p.db, entry).0.program.stmts.len();
    let checked = noeta_db::linked_checked(&p.db, p.ws);
    let errors = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == noeta_diagnostics::Severity::Error)
        .count();
    let warnings = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == noeta_diagnostics::Severity::Warning)
        .count();
    let compiled = noeta_db::linked_bytecode(&p.db, p.ws);
    let (compiles, note) = match &compiled.0 {
        _ if errors > 0 => (false, format!("{errors} type error(s) — see `check`")),
        Ok(_) => (true, "ok".to_string()),
        Err(u) => (false, u.to_string()),
    };
    PipelineOutput {
        tokens,
        top_level_items,
        errors,
        warnings,
        compiles,
        note,
    }
}

// ---- module_graph -----------------------------------------------------------------------------

/// The `module_graph` result: every file in the workspace and the modules it imports — the
/// `namespace`/`use` dependency edges.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ModuleGraphOutput {
    pub modules: Vec<ModuleNode>,
}

/// One file's node: its declared namespace (its module identity), its imports, and the
/// architectural roles its declarations bear.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ModuleNode {
    /// The file's source name (path or `<inline>`).
    pub file: String,
    /// The file's declared `namespace A.B;`, or empty when it declares none.
    pub namespace: String,
    /// The modules this file imports, one per `use`.
    pub imports: Vec<ImportEdge>,
    /// The `@role` bindings declared in this file (`target` + `Enum.Variant`) — the architectural
    /// labels on the import graph itself, attributed by each role target's source span.
    pub roles: Vec<ModuleRole>,
}

/// One role binding summarized on a module node.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ModuleRole {
    pub target: String,
    pub role: String,
}

/// One `use A.B.{x, y};` edge: the imported module path and the names it brings in.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ImportEdge {
    /// The dotted module path imported from (`A.B`).
    pub module: String,
    /// The names imported (each an imported leaf or its `as` alias's original name).
    pub names: Vec<String>,
}

pub fn module_graph(p: &Prepared) -> ModuleGraphOutput {
    // The role index over the merged program (a role conferred by an attribute declared in another
    // file still lands), attributed to files via each target's source span.
    let linked = noeta_db::linked(&p.db, p.ws);
    let mut roles_by_source: std::collections::HashMap<u32, Vec<ModuleRole>> =
        std::collections::HashMap::new();
    if let Ok(program) = &linked.0 {
        for r in &noeta_ast::reflect::build(program).roles {
            roles_by_source
                .entry(r.target_span.source.0)
                .or_default()
                .push(ModuleRole {
                    target: r.target.clone(),
                    role: format!("{}.{}", r.enum_name, r.variant),
                });
        }
    }
    let modules = p
        .sources
        .iter()
        .enumerate()
        .map(|(source_idx, src)| {
            let sp = noeta_db::source_program(&p.db, src);
            let parsed = noeta_db::ast(&p.db, sp);
            let mut namespace = String::new();
            let mut imports = Vec::new();
            for stmt in &parsed.0.program.stmts {
                match stmt {
                    Stmt::Namespace { path, .. } => namespace = path.join("."),
                    Stmt::Use { path, names, .. } => imports.push(ImportEdge {
                        module: path.join("."),
                        names: names.iter().map(|n| n.name.clone()).collect(),
                    }),
                    _ => {}
                }
            }
            ModuleNode {
                file: src.name().to_string(),
                namespace,
                imports,
                roles: roles_by_source
                    .get(&(source_idx as u32))
                    .cloned()
                    .unwrap_or_default(),
            }
        })
        .collect();
    ModuleGraphOutput { modules }
}

// ---- reflect ----------------------------------------------------------------------------------

/// The `reflect` result: the `@role`/`@semantic` architectural graph plus the `#[...]` attribute
/// manifest and the declared types — built by the same `reflect::build` the runtime `roles_of()` /
/// `attributes_of()` read, so it is exactly what the program sees at runtime.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReflectOutput {
    /// The `(declaration, role)` index — each declaration bearing a `@role(Enum.Variant)` attribute,
    /// paired with the architectural role it confers. Filtered when the request named a `role`.
    pub roles: Vec<RoleEntry>,
    /// The `#[Name(...)]` data-attribute manifest — which declarations carry which attributes.
    pub attributes: Vec<AttributeEntry>,
    /// Every declared struct/class/enum, with member names.
    pub types: Vec<TypeEntry>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RoleEntry {
    /// The annotated declaration's name.
    pub target: String,
    /// The role as `Enum.Variant`, e.g. `Semantic.EntryPoint`.
    pub role: String,
    /// The file the annotated declaration lives in.
    pub file: Option<String>,
    /// The declaration name's source location — joinable with `symbols`/`definition` output.
    pub location: Option<SpanLoc>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AttributeEntry {
    /// The annotated declaration's name.
    pub target: String,
    /// The attribute's name (e.g. `Route`).
    pub name: String,
    /// The number of literal arguments the attribute carries.
    pub arg_count: usize,
    /// The file the annotated declaration lives in.
    pub file: Option<String>,
    /// The declaration name's source location.
    pub location: Option<SpanLoc>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TypeEntry {
    pub name: String,
    /// `struct` / `class` / `enum`.
    pub kind: String,
    /// Field names (records/classes) or variant names (enums), in declaration order.
    pub members: Vec<String>,
}

/// Answer `reflect`, optionally filtered to declarations bearing a given `role` (matched against
/// either the bare variant `EntryPoint` or the qualified `Semantic.EntryPoint`, case-insensitively).
pub fn reflect(p: &Prepared, role: Option<&str>) -> ReflectOutput {
    // The merged workspace program when it links; the entry file's own AST otherwise (so a
    // use-resolution failure still yields this file's roles/attributes rather than nothing). Both
    // queries are `returns(ref)`, so borrow rather than move the `Program`.
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = match &linked.0 {
        Ok(prog) => prog,
        Err(_) => &entry.0.program,
    };
    let info = noeta_ast::reflect::build(program);

    let want = role.map(|r| r.trim().to_ascii_lowercase());
    let roles = info
        .roles
        .iter()
        .filter(|r| match &want {
            None => true,
            Some(w) => {
                r.variant.to_ascii_lowercase() == *w
                    || format!("{}.{}", r.enum_name, r.variant).to_ascii_lowercase() == *w
            }
        })
        .map(|r| {
            let at = analyze::locate_span(p, r.target_span);
            RoleEntry {
                target: r.target.clone(),
                role: format!("{}.{}", r.enum_name, r.variant),
                file: at.as_ref().map(|(file, _)| file.clone()),
                location: at.map(|(_, loc)| loc),
            }
        })
        .collect();

    let attributes = info
        .manifest
        .iter()
        .map(|a| {
            let at = analyze::locate_span(p, a.target_span);
            AttributeEntry {
                target: a.target.clone(),
                name: a.name.clone(),
                arg_count: a.args.len(),
                file: at.as_ref().map(|(file, _)| file.clone()),
                location: at.map(|(_, loc)| loc),
            }
        })
        .collect();

    let types = info
        .types
        .iter()
        .map(|t| {
            let (kind, members) = match t.kind {
                noeta_ast::reflect::TypeKind::Struct => ("struct", t.fields.clone()),
                noeta_ast::reflect::TypeKind::Class => ("class", t.fields.clone()),
                noeta_ast::reflect::TypeKind::Enum => (
                    "enum",
                    t.variants
                        .iter()
                        .map(|v| v.name.clone())
                        .collect::<Vec<_>>(),
                ),
            };
            TypeEntry {
                name: t.name.clone(),
                kind: kind.to_string(),
                members,
            }
        })
        .collect();

    ReflectOutput {
        roles,
        attributes,
        types,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    const SRC: &str = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

#[Route(\"/x\")]
fn handle(n: int): int {
  xs = [1, 2, 3];
  return n + xs.len();
}

enum Color { Red; Green }
";

    fn prep() -> Prepared {
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    #[test]
    fn ast_pretty_prints_with_spans() {
        let out = ast(&prep());
        assert!(out.ast.starts_with("(program @0.."));
        assert!(out.ast.contains("(fn handle"));
        assert!(out.ast.contains("(struct Route"));
    }

    #[test]
    fn bytecode_disassembles_a_clean_program() {
        let out = bytecode(&prep());
        assert!(out.compiled, "unsupported: {:?}", out.unsupported);
        assert!(out.disassembly.contains("==="), "no protos in disassembly");
        assert!(out.unsupported.is_none());
    }

    #[test]
    fn pipeline_summarizes_each_stage() {
        let out = pipeline(&prep());
        assert!(out.tokens > 0);
        assert_eq!(out.top_level_items, 3); // struct, fn, enum
        assert_eq!(out.errors, 0);
        assert!(out.compiles);
        assert_eq!(out.note, "ok");
    }

    #[test]
    fn pipeline_reports_the_first_blocking_stage() {
        // `let` is not a binding keyword — this is a parse/type failure the summary surfaces.
        let p = prepare(
            &Some("fn f(): int { let x = 1; return x; }".to_string()),
            &None,
        )
        .unwrap();
        let out = pipeline(&p);
        assert!(out.errors > 0);
        assert!(!out.compiles);
        assert!(out.note.contains("error"));
    }

    #[test]
    fn module_graph_reports_namespace_and_imports() {
        let src = "namespace App.Web;\nuse App.Models.{User, Order};\nuse std.{math};\nfn f(): int { return 1; }\n";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = module_graph(&p);
        assert_eq!(out.modules.len(), 1);
        let node = &out.modules[0];
        assert_eq!(node.namespace, "App.Web");
        let mods: Vec<&str> = node.imports.iter().map(|e| e.module.as_str()).collect();
        assert!(mods.contains(&"App.Models"));
        assert!(mods.contains(&"std"));
        let models = node
            .imports
            .iter()
            .find(|e| e.module == "App.Models")
            .unwrap();
        assert_eq!(models.names, vec!["User", "Order"]);
        assert!(node.roles.is_empty(), "no @role bindings in this module");
    }

    #[test]
    fn module_graph_carries_role_summaries() {
        let p = prep(); // the shared fixture: `handle` bears `#[Route]` → `Semantic.EntryPoint`
        let out = module_graph(&p);
        let node = &out.modules[0];
        assert!(
            node.roles
                .iter()
                .any(|r| r.target == "handle" && r.role == "Semantic.EntryPoint"),
            "roles: {:?}",
            node.roles
        );
    }

    #[test]
    fn reflect_surfaces_roles_attributes_and_types() {
        let out = reflect(&prep(), None);
        // The `@role(Semantic.EntryPoint)` attribute confers the role on the declaration it annotates.
        assert_eq!(out.roles.len(), 1);
        assert_eq!(out.roles[0].target, "handle");
        assert_eq!(out.roles[0].role, "Semantic.EntryPoint");
        // The role is locatable: the target's name span resolves to a file + line, so an agent can
        // join the role index with `symbols`/`definition` output.
        assert_eq!(out.roles[0].file.as_deref(), Some("<inline>"));
        assert!(out.roles[0].location.expect("role located").start.line >= 1);
        // The `#[Route(...)]` data attribute is in the manifest.
        assert!(
            out.attributes
                .iter()
                .any(|a| a.name == "Route" && a.target == "handle")
        );
        // Declared types with their members.
        let route = out.types.iter().find(|t| t.name == "Route").unwrap();
        assert_eq!(route.kind, "struct");
        assert_eq!(route.members, vec!["path"]);
        let color = out.types.iter().find(|t| t.name == "Color").unwrap();
        assert_eq!(color.members, vec!["Red", "Green"]);
    }

    #[test]
    fn reflect_filters_by_role() {
        // Bare variant and qualified forms both match; a non-matching role yields nothing.
        assert_eq!(reflect(&prep(), Some("EntryPoint")).roles.len(), 1);
        assert_eq!(reflect(&prep(), Some("Semantic.EntryPoint")).roles.len(), 1);
        assert_eq!(reflect(&prep(), Some("Sink")).roles.len(), 0);
    }
}
