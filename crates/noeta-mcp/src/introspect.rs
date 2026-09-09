//! The Introspect pillar: traverse the compiler's artifacts. `ast` (the pretty-printed syntax tree),
//! `bytecode` (what actually runs — the VM disassembly), `pipeline` (a per-stage health summary),
//! `module_graph` (the `use`/`namespace` import edges), and `reflect` (the `@role`/`@semantic`
//! architectural graph). Every one is a pure read over the public salsa graph + AST — the same
//! `reflect::build` and `Module::disassemble` the runtime and `noeta dump` use, so an agent sees
//! ground truth, not a re-derivation.

use crate::analyze::{self, LinkStatus, NodeId, NodeKind, Prepared};
use crate::graph::DeclIndex;
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
    /// Whether the workspace linked. A module graph read off an unlinked program still lists the
    /// files, so this is the only thing that says its namespaces and role summaries are partial.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// One file's node: its declared namespace (its module identity), its imports, and the
/// architectural roles its declarations bear.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ModuleNode {
    /// The module's stable identity: its module path as the name, `module` as the kind, its file,
    /// and the file's whole byte range as the span.
    pub id: NodeId,
    /// The file's source name, relative to the project root.
    pub file: String,
    /// The module path this file's location derives — its identity, and what an import edge names.
    /// A file that declares `namespace A.B;` instead reports that.
    pub namespace: String,
    /// True for a module of a dependency package rather than a workspace member.
    pub external: bool,
    /// The dependency package this module belongs to, for an `external` node.
    pub package: Option<String>,
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
    /// The module nodes this import reaches. A `use pkg.alpha` names the package as its path and
    /// the module as the imported leaf, so the node an edge points at is `{module}.{name}` when
    /// that names a module and `module` when it does. Empty for an import of the standard library
    /// or of a native package's namespace, neither of which is a node here.
    pub targets: Vec<String>,
}

pub fn module_graph(p: &Prepared) -> ModuleGraphOutput {
    // The role index over the merged program (a role conferred by an attribute declared in another
    // file still lands), attributed to files via each target's source span.
    let status = LinkStatus::of(p);
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
    let mut modules: Vec<ModuleNode> = members
        .iter()
        .zip(p.sources.iter())
        .enumerate()
        .map(|(source_idx, (member, src))| {
            let parsed = noeta_db::ast(&p.db, *member);
            let mut declared = String::new();
            let mut imports = Vec::new();
            for stmt in &parsed.0.program.stmts {
                match stmt {
                    Stmt::Namespace { path, .. } => declared = path.join("."),
                    Stmt::Use { path, names, .. } => imports.push(ImportEdge {
                        module: path.join("."),
                        names: names.iter().map(|n| n.name.clone()).collect(),
                        targets: Vec::new(),
                    }),
                    _ => {}
                }
            }
            // The module path the file's LOCATION derives is its identity in a package, where a
            // `namespace` statement is refused (E0072) — so a package's modules had no identity at
            // all while their import edges named dotted paths. A manifest-less script keeps its
            // declared namespace.
            let derived = p
                .modules
                .get(source_idx)
                .map(|m| m.namespace.clone())
                .unwrap_or_default();
            let namespace = if derived.is_empty() {
                declared
            } else {
                derived
            };
            node(p, source_idx, src, namespace, imports, &roles_by_source)
        })
        .collect();

    // Resolve each import edge onto the modules it reaches, over every module the program knows —
    // members and dependency packages alike. `use pkg.alpha` spells the package as its path and
    // the module as the imported name, so the node is `{module}.{name}`.
    let known: std::collections::BTreeSet<&str> = p
        .modules
        .iter()
        .map(|m| m.namespace.as_str())
        .filter(|n| !n.is_empty())
        .collect();
    let mut reached: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for module in &mut modules {
        for edge in &mut module.imports {
            if known.contains(edge.module.as_str()) {
                edge.targets.push(edge.module.clone());
            }
            for name in &edge.names {
                let candidate = format!("{}.{name}", edge.module);
                if known.contains(candidate.as_str()) {
                    edge.targets.push(candidate);
                }
            }
            edge.targets.dedup();
            reached.extend(edge.targets.iter().cloned());
        }
    }

    // A dependency package's module an import edge points at becomes a node of its own, so no edge
    // dangles. Only the modules actually imported: a package's whole module set is its business.
    for (index, identity) in p.modules.iter().enumerate() {
        let Some(package) = &identity.package else {
            continue;
        };
        if !reached.contains(&identity.namespace) {
            continue;
        }
        let Some(src) = p.sources.get(index) else {
            continue;
        };
        let mut dep = node(
            p,
            index,
            src,
            identity.namespace.clone(),
            Vec::new(),
            &roles_by_source,
        );
        dep.external = true;
        dep.package = Some(package.clone());
        modules.push(dep);
    }

    ModuleGraphOutput {
        modules,
        linked: status.linked,
        note: status.note(),
        link_diagnostics: status.link_diagnostics,
    }
}

/// One module node: its identity, its file, its imports, and the roles its declarations bear. A
/// module has no declared name, so its span is the file's whole byte range.
fn node(
    p: &Prepared,
    index: usize,
    src: &noeta_span::Source,
    namespace: String,
    imports: Vec<ImportEdge>,
    roles_by_source: &std::collections::HashMap<u32, Vec<ModuleRole>>,
) -> ModuleNode {
    let span = noeta_span::Span::new_in(
        noeta_span::SourceId(index as u32),
        0,
        src.text().len() as u32,
    );
    ModuleNode {
        id: p.node_id(&namespace, NodeKind::Module, span),
        file: p.file_name(index).unwrap_or_else(|| src.name().to_string()),
        namespace,
        external: false,
        package: None,
        imports,
        roles: roles_by_source
            .get(&(index as u32))
            .cloned()
            .unwrap_or_default(),
    }
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
    /// Whether the manifest was read off the merged workspace program. When false, a role
    /// conferred by a sibling module or a dependency package is missing and every target's name is
    /// unqualified.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RoleEntry {
    /// The annotated declaration's identity — the same `id` `symbols`, `trace`, `impact` and
    /// `callers` report for it, and the only spelling of its name, file and span.
    pub id: NodeId,
    /// The role as `Enum.Variant`, e.g. `Semantic.EntryPoint`.
    pub role: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AttributeEntry {
    /// The annotated declaration's identity.
    pub id: NodeId,
    /// The attribute's name (e.g. `Route`) — the one field here that is not the target's.
    pub name: String,
    /// The number of literal arguments the attribute carries.
    pub arg_count: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TypeEntry {
    /// The type's identity — what makes a type found here openable, and joinable with `symbols`.
    /// Its name, kind, file and span are the id's.
    pub id: NodeId,
    /// Field names (records/classes) or variant names (enums), in declaration order.
    pub members: Vec<String>,
}

/// Answer `reflect`, optionally filtered to declarations bearing a given `role` (matched against
/// either the bare variant `EntryPoint` or the qualified `Semantic.EntryPoint`, case-insensitively).
pub fn reflect(p: &Prepared, role: Option<&str>) -> ReflectOutput {
    // The merged workspace program when it links; the entry file's own AST otherwise (so a
    // use-resolution failure still yields this file's roles/attributes rather than nothing). Both
    // queries are `returns(ref)`, so borrow rather than move the `Program`.
    let status = LinkStatus::of(p);
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = match &linked.program {
        Ok(prog) => prog,
        Err(_) => &entry.0.program,
    };
    let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
    let info = noeta_ast::reflect::build(program, &native_roles, &Default::default());
    // The declaration inventory over the same program: it carries the kind and the name span every
    // entry's `id` needs, so a role, an attribute and an outline node all identify one declaration
    // the same way.
    let decls = DeclIndex::build(program);
    let id_at =
        |name: &str, span: noeta_span::Span, fallback: NodeKind| match decls.at_name_span(span) {
            Some(decl) => decl.id(p),
            None => p.node_id(name, fallback, span),
        };

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
        .map(|r| RoleEntry {
            id: id_at(&r.target, r.target_span, NodeKind::Function),
            role: format!("{}.{}", r.enum_name, r.variant),
        })
        .collect();

    let attributes = info
        .manifest
        .iter()
        .map(|a| AttributeEntry {
            id: id_at(&a.target, a.target_span, NodeKind::Function),
            name: a.name.clone(),
            arg_count: a.args.len(),
        })
        .collect();

    // A type's declaration span comes from the inventory, keyed by the type's post-link name — the
    // reflection manifest itself carries no location, so a type found here could not be opened.
    let types = info
        .types
        .iter()
        .map(|t| {
            let (kind, members) = match t.kind {
                noeta_ast::reflect::TypeKind::Struct => (NodeKind::Struct, t.fields.clone()),
                noeta_ast::reflect::TypeKind::Class => (NodeKind::Class, t.fields.clone()),
                noeta_ast::reflect::TypeKind::Enum => (
                    NodeKind::Enum,
                    t.variants
                        .iter()
                        .map(|v| v.name.clone())
                        .collect::<Vec<_>>(),
                ),
            };
            let declared = decls
                .decls()
                .iter()
                .find(|d| d.name == t.name && d.kind == kind);
            TypeEntry {
                id: match declared {
                    Some(d) => d.id(p),
                    None => p.unlocated_id(&t.name, kind),
                },
                members,
            }
        })
        .collect();

    ReflectOutput {
        roles,
        attributes,
        types,
        linked: status.linked,
        note: status.note(),
        link_diagnostics: status.link_diagnostics,
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
        // (`(#[Route("/x")] fn handle [n: int]`, `(struct @attribute … Route [path: string]`),
        // so assert on head + name/params without anchoring `(head name` adjacency.
        // A parameter renders with its type annotation: the fmt safety gate compares the dump
        // field-for-field, so dropping `: int` would let two different signatures compare equal.
        assert!(out.ast.contains("fn handle [n: int]"));
        assert!(out.ast.contains("#[Route"));
        assert!(out.ast.contains("(struct "));
        assert!(out.ast.contains("Route [path: string]"));
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
        assert_eq!(out.roles[0].id.name, "handle");
        assert_eq!(out.roles[0].role, "Semantic.EntryPoint");
        // The role is locatable: the target's name span resolves to a file + line, so an agent can
        // join the role index with `symbols`/`definition` output.
        assert_eq!(out.roles[0].id.file.as_deref(), Some("<inline>"));
        assert!(out.roles[0].id.span.expect("role located").line >= 1);
        // The `#[Route(...)]` data attribute is in the manifest.
        assert!(
            out.attributes
                .iter()
                .any(|a| a.name == "Route" && a.id.name == "handle")
        );
        // Declared types with their members.
        let route = out.types.iter().find(|t| t.id.name == "Route").unwrap();
        assert_eq!(route.id.kind, NodeKind::Struct);
        assert_eq!(route.members, vec!["path"]);
        let color = out.types.iter().find(|t| t.id.name == "Color").unwrap();
        assert_eq!(color.members, vec!["Red", "Green"]);
    }

    /// D19: a type the manifest reports is openable. `TypeInfo` carries no span of its own, so the
    /// entry's location comes from the declaration inventory over the same program; without it a
    /// type found by `reflect` could be neither opened nor joined to `symbols`.
    #[test]
    fn reflect_types_carry_a_location_and_an_id() {
        let out = reflect(&prep(), None);
        let route = out.types.iter().find(|t| t.id.name == "Route").unwrap();
        assert_eq!(route.id.file.as_deref(), Some("<inline>"));
        let at = route.id.span.expect("the type is located");
        assert_eq!(at.line, 3, "`struct Route` is on line 3");
        // The id is the same object `symbols` reports for this declaration.
        assert_eq!(route.id.kind, NodeKind::Struct);
        assert_eq!(route.id.name, "Route");
        let outlined = crate::understand::symbols(&prep(), crate::understand::SymbolScope::File)
            .symbols
            .into_iter()
            .find(|s| s.id.name == "Route")
            .expect("Route is in the outline");
        assert_eq!(outlined.id, route.id, "the two tools report one identity");
    }

    /// D5: `reflect` says whether it read the merged program. Its lists are half-answers under a
    /// failed link (a role conferred by a sibling is missing), and nothing on the wire said so.
    #[test]
    fn reflect_reports_the_link_status() {
        let clean = reflect(&prep(), None);
        assert!(clean.linked);
        assert!(clean.link_diagnostics.is_empty());
        assert_eq!(clean.note, None);

        let broken = broken_link_project("mcp_reflect_link_status");
        let p = prepare(&None, &Some(broken.display().to_string())).expect("prepare");
        let out = reflect(&p, None);
        assert!(!out.linked, "one unresolvable `use` breaks the link");
        assert!(
            !out.link_diagnostics.is_empty(),
            "the diagnostics that stopped it are reported"
        );
        assert!(
            out.note
                .as_deref()
                .is_some_and(|n| n.contains("did not link")),
            "note: {:?}",
            out.note
        );
    }

    /// D6: in a package a `namespace` statement is refused, so a module's identity is the one its
    /// LOCATION derives. Every node reported an empty namespace while its own import edges named
    /// dotted paths, which left the graph with no joinable node at all.
    #[test]
    fn module_graph_fills_a_package_module_identity_from_its_location() {
        noeta_stdlib::registry::default_seeded();
        let entry = three_module_project("mcp_module_identity");
        let p = prepare(&None, &Some(entry.display().to_string())).expect("prepare");
        let out = module_graph(&p);
        assert!(out.linked, "note: {:?}", out.note);
        let namespaces: std::collections::BTreeSet<&str> =
            out.modules.iter().map(|m| m.namespace.as_str()).collect();
        assert!(
            namespaces.contains("joined.alpha") && namespaces.contains("joined.beta"),
            "namespaces: {namespaces:?}"
        );
        // Every intra-project import edge resolves onto a module the graph carries as a node.
        let main = out
            .modules
            .iter()
            .find(|m| m.namespace == "joined.main")
            .unwrap();
        for edge in &main.imports {
            assert!(!edge.targets.is_empty(), "edge `{}` dangles", edge.module);
            for target in &edge.targets {
                assert!(
                    out.modules.iter().any(|m| &m.namespace == target),
                    "edge target `{target}` is no node: {namespaces:?}"
                );
            }
        }
        // And each node's id is a module identity, joinable by file.
        let alpha = out
            .modules
            .iter()
            .find(|m| m.namespace == "joined.alpha")
            .unwrap();
        assert_eq!(alpha.id.kind, NodeKind::Module);
        assert_eq!(alpha.id.name, "joined.alpha");
        assert_eq!(alpha.id.file.as_deref(), Some("src/alpha.noe"));
        assert!(!alpha.external);
    }

    /// D6, the other half: a dependency package's module an edge points at is a node of its own,
    /// marked external and named by its package, so no edge leaves the graph.
    #[test]
    fn module_graph_lists_an_imported_dependency_module_as_an_external_node() {
        noeta_stdlib::registry::default_seeded();
        let entry = dep_role_project("mcp_module_graph_dep");
        let p = prepare(&None, &Some(entry.display().to_string())).expect("prepare");
        let out = module_graph(&p);
        let dep = out
            .modules
            .iter()
            .find(|m| m.namespace == "toolkit.api")
            .unwrap_or_else(|| {
                panic!(
                    "the imported dependency module is a node: {:?}",
                    out.modules.iter().map(|m| &m.namespace).collect::<Vec<_>>()
                )
            });
        assert!(dep.external);
        assert_eq!(dep.package.as_deref(), Some("toolkit"));
        assert_eq!(dep.id.kind, NodeKind::Module);
        // The app's own import edge points at it, so the edge no longer dangles.
        assert!(
            out.modules.iter().any(|m| {
                m.imports
                    .iter()
                    .any(|e| e.targets.iter().any(|t| t == "toolkit.api"))
            }),
            "no edge reaches the dependency module"
        );
    }

    /// A package whose entry imports a module that does not exist — the shape that makes every
    /// graph tool fall back to the entry file's own parse.
    fn broken_link_project(name: &str) -> noeta_test_temp::TempPath {
        let root = noeta_test_temp::TempDir::new(&format!("mcp-{name}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/broken\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("alpha.noe"),
            "pub fn alpha_only(): int { return 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            "use broken.nonexistent\n\
             use broken.alpha\n\
             @attribute(Function)\n@role(Semantic.EntryPoint)\nstruct Entry { name: string }\n\
             #[Entry(\"m\")]\nfn entry(): int { return alpha.alpha_only() }\necho entry()\n",
        )
        .unwrap();
        root.into_child("src/main.noe")
    }

    /// The three-module fixture, shared with the trace tests: `main` calls into `alpha` and `beta`.
    fn three_module_project(name: &str) -> noeta_test_temp::TempPath {
        let root = noeta_test_temp::TempDir::new(&format!("mcp-{name}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/joined\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("alpha.noe"),
            "pub fn shared(): int { return 1 }\npub fn alpha_only(): int { return shared() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("beta.noe"),
            "pub fn shared(): int { return 2 }\npub fn beta_only(): int { return shared() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            "use joined.alpha\nuse joined.beta\n\
             fn entry(): int { return alpha.alpha_only() + beta.beta_only() }\necho entry()\n",
        )
        .unwrap();
        root.into_child("src/main.noe")
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
    ///
    /// The root is per-process: `/tmp/noeta-mcp-tests/<name>` was shared by every checkout and every
    /// concurrent test binary, each of which opened by `remove_dir_all`ing it. The returned path
    /// carries the root's guard, so the project survives until the caller is done with it — returning
    /// a bare `PathBuf` would delete the tree at this function's `return`.
    fn dep_role_project(name: &str) -> noeta_test_temp::TempPath {
        let root = noeta_test_temp::TempDir::new(&format!("mcp-{name}"));
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
            "@attribute(Function)\n@role(Semantic.TrustBoundary)\npub struct Tool { name: string }\n",
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
        root.into_child("app/main.noe")
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
            .map(|r| (r.id.name.as_str(), r.role.as_str()))
            .collect();
        // Targets are **qualified** now: the entry sits inside a package, so it derives a module
        // path (`app.main`) and its declarations carry qualified identities. What this test is
        // about — that a role conferred by a *dependency* is indexed at all — is unchanged; only
        // the spelling of the target moved.
        assert!(
            roles.contains(&("app.main.from_dep", "Semantic.TrustBoundary")),
            "the dependency-conferred role must be indexed: {roles:?}"
        );
        assert!(
            roles.contains(&("app.main.from_local", "Semantic.Sink")),
            "the same-file role still reports: {roles:?}"
        );
        // The attribute is listed under the package's **qualified** identity — proof the link
        // resolved, rather than falling back to the entry's own unlinked AST.
        assert!(
            out.attributes
                .iter()
                .any(|a| a.id.name == "app.main.from_dep" && a.name == "toolkit.api.Tool"),
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
        let out = crate::run_check(&crate::CheckArgs {
            source: None,
            file: Some(entry.display().to_string()),
        })
        .expect("check");
        assert!(out.ok, "diagnostics: {:?}", out.diagnostics);
    }

    /// A project whose dependency ships a **native extension** this process has not composed:
    /// `acme/imgfx` registers `imgfx.raw` from Rust, and its own `.noe` module imports it.
    fn native_dep_project(name: &str) -> noeta_test_temp::TempPath {
        let root = noeta_test_temp::TempDir::new(&format!("mcp-{name}"));
        let (app, dep) = (root.join("app"), root.join("imgfx"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(dep.join("native")).unwrap();
        std::fs::write(
            dep.join("noeta.toml"),
            "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
        )
        .unwrap();
        // The crate need only exist: nothing here builds it, and resolution validates its presence.
        std::fs::write(
            dep.join("native").join("Cargo.toml"),
            "[package]\nname = \"imgfx-native\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        // The package's Noeta half imports the namespace its Rust half registers — the shape every
        // real native package has (para/api's `.noe` code imports `para.api.url`), and the reason an
        // uncomposed toolchain reports an unresolved-import cascade rather than one error.
        for module in ["fx", "util"] {
            std::fs::write(
                dep.join(format!("{module}.noe")),
                "use imgfx.raw\npub fn one(): int { return raw.one(); }\n",
            )
            .unwrap();
        }
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nimgfx = { path = \"../imgfx\" }\n\
             [trust]\nnative = [\"acme/imgfx\"]\n",
        )
        .unwrap();
        std::fs::write(app.join("main.noe"), "use imgfx.raw\necho raw.one();\n").unwrap();
        root.into_child("app/main.noe")
    }

    /// `check` refuses — visibly — rather than reporting a program it cannot link.
    ///
    /// `noeta mcp` had no composition step at all, so in any project with a `[trust] native`
    /// dependency the tool the generated `AGENTS.md` offers *as* `noeta check` answered with an
    /// E0019 per file, on code the composed CLI compiles cleanly. The server now delegates to the
    /// project's composed toolchain when one is built; when it is not — the case this test pins,
    /// since no test process is composed — the answer must be one honest error naming the package
    /// and the command that fixes it, never the cascade.
    #[test]
    fn check_refuses_a_program_whose_native_extension_is_not_composed() {
        noeta_stdlib::registry::default_seeded();
        let entry = native_dep_project("mcp_check_uncomposed");
        let out = crate::run_check(&crate::CheckArgs {
            source: None,
            file: Some(entry.display().to_string()),
        })
        .expect("check");
        assert!(!out.ok, "a refusal is not a pass");
        assert_eq!(
            out.diagnostics.len(),
            1,
            "the unresolved-import cascade is withheld: {:?}",
            out.diagnostics
        );
        assert_eq!(out.uncomposed, vec!["acme/imgfx".to_string()]);
        let message = &out.diagnostics[0].message;
        assert!(message.contains("acme/imgfx"), "message: {message}");
        assert!(message.contains("noeta check"), "message: {message}");
    }

    /// The control: a project with no native dependency is unaffected — nothing is withheld, and
    /// the ordinary whole-program check still runs.
    #[test]
    fn a_pure_noeta_project_is_never_withheld() {
        noeta_stdlib::registry::default_seeded();
        let entry = dep_role_project("mcp_check_pure_noeta");
        let out = crate::run_check(&crate::CheckArgs {
            source: None,
            file: Some(entry.display().to_string()),
        })
        .expect("check");
        assert!(out.uncomposed.is_empty());
    }
}
