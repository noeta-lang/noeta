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
    if let Ok(program) = &linked.program {
        let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
        for r in &noeta_ast::reflect::build(program, &native_roles, &Default::default()).roles {
            roles_by_source
                .entry(r.target_span.source.0)
                .or_default()
                .push(ModuleRole {
                    target: r.target.clone(),
                    role: format!("{}.{}", r.enum_name, r.variant),
                });
        }
    }
    // The workspace's own member inputs (entry + siblings, in `sources` order) — reading their
    // memoized per-file parses instead of minting duplicate inputs per call (ide-workspaces).
    //
    // `Prepared::sources` is the *whole* canonical ordering — members first, then every dependency
    // package's modules — while `members` is only the leading member run. Zipping pairs each member
    // with its own source and stops at the shorter of the two, so a program that has dependencies
    // cannot index past the members: iterating `sources` and indexing `members` panicked with
    // `index out of bounds` on any project with a `noeta.toml` dependency, which is to say on every
    // real package.
    let members = p.ws.members(&p.db);
    let modules = members
        .iter()
        .zip(p.sources.iter())
        .enumerate()
        .map(|(source_idx, (member, src))| {
            let parsed = noeta_db::ast(&p.db, *member);
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
    let program: &Program = match &linked.program {
        Ok(prog) => prog,
        Err(_) => &entry.0.program,
    };
    let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
    let info = noeta_ast::reflect::build(program, &native_roles, &Default::default());

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
        noeta_stdlib::registry::default_seeded();
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    #[test]
    fn ast_pretty_prints_with_spans() {
        let out = ast(&prep());
        assert!(out.ast.starts_with("(program @0.."));
        // Attributes/directives print between the head and the name
        // (`(#[Route("/x")] fn handle [n]`, `(struct @attribute … Route [path]`),
        // so assert on head + name/params without anchoring `(head name` adjacency.
        assert!(out.ast.contains("fn handle [n]"));
        assert!(out.ast.contains("#[Route"));
        assert!(out.ast.contains("(struct "));
        assert!(out.ast.contains("Route [path]"));
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

    /// A two-package project on disk: `acme/toolkit` declares a `@role(Semantic.TrustBoundary)`
    /// attribute struct, and `acme/app` depends on it by path and applies the attribute to a
    /// function (alongside a same-file role, as the control). Returns the app's entry path.
    fn dep_role_project(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join("noeta-mcp-tests").join(name);
        let _ = std::fs::remove_dir_all(&root);
        let (app, toolkit) = (root.join("app"), root.join("toolkit"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&toolkit).unwrap();
        std::fs::write(
            toolkit.join("noeta.toml"),
            "[package]\nname = \"acme/toolkit\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            toolkit.join("api.noe"),
            "namespace toolkit.api;\n\
             @attribute(Function)\n@role(Semantic.TrustBoundary)\npub struct Tool { name: string }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\ntoolkit = { path = \"../toolkit\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("main.noe"),
            "use toolkit.api.Tool\n\
             @attribute(Function)\n@role(Semantic.Sink)\nstruct Local { name: string }\n\
             #[Tool(\"dep\")]\nfn from_dep(): void { return }\n\
             #[Local(\"own\")]\nfn from_local(): void { return }\n\
             echo \"ok\";\n",
        )
        .unwrap();
        app.join("main.noe")
    }

    /// A role conferred by a **dependency package's** `@role`-bearing attribute reaches `reflect`.
    ///
    /// It used not to: the MCP built its workspace from the entry and its *siblings* only, so the
    /// package declaring the `@role` was never linked. The attribute *application* sits in the
    /// entry, so `attributes` listed it while `roles` came back empty — the half-delivered feature
    /// where "what can a language model reach in this program?" is answerable in-language
    /// (`roles_of()`) but not off the agent surface. The same-file `@role` is the control: it
    /// always worked, and must keep working.
    #[test]
    fn reflect_sees_a_role_conferred_by_a_dependency_package() {
        noeta_stdlib::registry::default_seeded();
        let entry = dep_role_project("mcp_reflect_dep_role");
        let p = prepare(&None, &Some(entry.display().to_string())).expect("prepare");

        let out = reflect(&p, None);
        let roles: Vec<(&str, &str)> = out
            .roles
            .iter()
            .map(|r| (r.target.as_str(), r.role.as_str()))
            .collect();
        assert!(
            roles.contains(&("from_dep", "Semantic.TrustBoundary")),
            "the dependency-conferred role must be indexed: {roles:?}"
        );
        assert!(
            roles.contains(&("from_local", "Semantic.Sink")),
            "the same-file role still reports: {roles:?}"
        );
        // The attribute is listed under the package's **qualified** identity — proof the link
        // resolved, rather than falling back to the entry's own unlinked AST.
        assert!(
            out.attributes
                .iter()
                .any(|a| a.target == "from_dep" && a.name == "toolkit.api.Tool"),
            "attributes: {:?}",
            out.attributes
        );
        // Filtering by the dependency-conferred role finds it too.
        assert_eq!(reflect(&p, Some("TrustBoundary")).roles.len(), 1);
    }

    /// The same resolution makes `check` see the dependency: before it, a program importing a
    /// package was analyzed as one whose import does not exist, so the agent surface reported
    /// errors on code `noeta run` compiles cleanly.
    #[test]
    fn check_resolves_a_dependency_package() {
        noeta_stdlib::registry::default_seeded();
        let entry = dep_role_project("mcp_check_dep_package");
        let resolved =
            crate::resolve_workspace(&None, &Some(entry.display().to_string())).expect("resolve");
        assert!(
            !resolved.deps.is_empty(),
            "the path dependency must resolve into the workspace"
        );
        let out = crate::run_check(&resolved);
        assert!(out.ok, "diagnostics: {:?}", out.diagnostics);
    }
}
