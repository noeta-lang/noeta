//! M3 Understand pillar (the compiler-answers-cheaply half): `type_at` and `symbols`. Both ride
//! only the public salsa graph + parsed AST — no private LSP resolver (that arrives with the
//! `noeta-ide` extraction at M5, which unlocks `definition`/`references`/`completions`/`signature`).

use crate::analyze::{self, LineIndex, Prepared, SpanLoc};
use noeta_ast::{Program, Stmt};
use rmcp::schemars;
use serde::Serialize;

/// The `type_at` result: the inferred type at a site, addressed by symbol name or position.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TypeAtOutput {
    /// True when a typed expression was found at the site.
    pub found: bool,
    /// The inferred type in surface syntax (`List<int>`, `?string`, `A | B`), empty when not found.
    pub r#type: String,
    /// The span the type belongs to (the tightest typed expression under the site), when found.
    pub location: Option<SpanLoc>,
    /// The byte offset the request resolved to (from `symbol` or `line`/`column`), for transparency.
    pub resolved_offset: Option<u32>,
    /// When not found, a short note on why (no such symbol / no typed expression there).
    pub note: Option<String>,
    /// The type's storage note when non-default (`@packed — 12 bytes`, `flat packed storage — 12
    /// bytes/element, column-major (SoA)`); absent for ordinary boxed storage. The same wording
    /// LSP hover shows (shared `noeta_ide::layout_note`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
}

/// Answer `type_at`. The site is a `symbol` name (first whole-word occurrence in the entry file) or
/// a 1-based `line`/`column`; the type is the tightest `expr_types` span in the entry file covering
/// it — the same "smallest span at the cursor" rule the LSP hover uses, over the same index.
pub fn type_at(
    p: &Prepared,
    symbol: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> TypeAtOutput {
    let text = p.entry_text();
    let index = LineIndex::new(text);
    let ide = noeta_db::linked_checked_ide(&p.db, p.ws);

    let offset = match (symbol, line, column) {
        (Some(name), _, _) => {
            // Prefer the tightest typed *expression* whose source text is exactly this identifier —
            // a use site with a known type — since `expr_types` keys expressions, not declaration
            // sites (a binding target or a parameter name is not itself an expression). Fall back to
            // the first textual occurrence for a symbol that never appears as a bare expression.
            let named = ide
                .expr_types
                .keys()
                .filter(|s| {
                    analyze::in_entry(**s)
                        && s.end > s.start
                        && text.get(s.start as usize..s.end as usize) == Some(name)
                })
                .min_by_key(|s| s.end - s.start)
                .map(|s| s.start);
            match named.or_else(|| analyze::symbol_offset(text, name)) {
                Some(off) => off,
                None => {
                    return not_found(format!("no identifier `{name}` in the entry file"), None);
                }
            }
        }
        (None, Some(l), Some(c)) => index.offset(l, c),
        (None, _, _) => {
            return not_found(
                "provide `symbol` (a name) or both `line` and `column`".to_string(),
                None,
            );
        }
    };

    let tightest = ide
        .expr_types
        .iter()
        .filter(|(span, _)| {
            analyze::in_entry(**span)
                && span.end > span.start
                && span.start <= offset
                && offset <= span.end
        })
        .min_by_key(|(span, _)| span.end - span.start);

    match tightest {
        Some((span, repr)) => TypeAtOutput {
            found: true,
            r#type: repr.to_string(),
            location: Some(index.span_loc(*span)),
            resolved_offset: Some(offset),
            note: None,
            layout: noeta_ide::layout_note(repr, &ide.packed_layouts),
        },
        None => not_found("no typed expression at that site".to_string(), Some(offset)),
    }
}

fn not_found(note: String, offset: Option<u32>) -> TypeAtOutput {
    TypeAtOutput {
        found: false,
        r#type: String::new(),
        location: None,
        resolved_offset: offset,
        note: Some(note),
        layout: None,
    }
}

/// The `symbols` result: the entry file's declaration outline.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SymbolsOutput {
    pub symbols: Vec<SymbolNode>,
}

/// One outline node: a declaration with its kind, a short detail, its span, and its members.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SymbolNode {
    pub name: String,
    /// `function` / `struct` / `class` / `enum` / `impl` / `field` / `variant` / `method`.
    pub kind: String,
    /// A short signature-ish detail (a function's parameter names, an `impl`'s trait+target), when
    /// useful. Parameter *types* are omitted here — call `type_at` or `ast` for precise types.
    pub detail: Option<String>,
    pub location: SpanLoc,
    /// The architectural roles this declaration bears (`Enum.Variant`, from `@role`-tagged
    /// attributes it carries) — the same index `reflect` serves, placed on the map itself.
    pub roles: Vec<String>,
    pub children: Vec<SymbolNode>,
}

/// Build the entry file's outline: top-level `fn`/`struct`/`class`/`enum`/`impl`, with fields,
/// variants, and methods as one level of children, in source order. Walks the parsed AST (available
/// even when the program has type errors), not the merged workspace, so it describes *this file*.
pub fn symbols(p: &Prepared) -> SymbolsOutput {
    let parsed = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = &parsed.0.program;
    let index = LineIndex::new(p.entry_text());
    let mut symbols: Vec<SymbolNode> = program
        .stmts
        .iter()
        .filter_map(|stmt| symbol_node(stmt, &index))
        .collect();

    // Annotate the outline with the `@role` index (over the merged program, so a role conferred by
    // an attribute declared in a sibling module still lands; entry-file targets only, since the
    // outline is entry-file only). Nested members key as `Type.member`, matching the index.
    let linked = noeta_db::linked(&p.db, p.ws);
    let role_program: &Program = match &linked.0 {
        Ok(prog) => prog,
        Err(_) => program,
    };
    let info = noeta_ast::reflect::build(role_program);
    let mut role_map: std::collections::HashMap<&str, Vec<String>> =
        std::collections::HashMap::new();
    for r in &info.roles {
        if analyze::in_entry(r.target_span) {
            role_map
                .entry(r.target.as_str())
                .or_default()
                .push(format!("{}.{}", r.enum_name, r.variant));
        }
    }
    annotate_roles(&mut symbols, None, &role_map);
    SymbolsOutput { symbols }
}

/// Attach roles to each node: a top-level node keys by its name, a member by `Parent.name` (the
/// index's `Type.member` convention). `impl` nodes have no single name and never match.
fn annotate_roles(
    nodes: &mut [SymbolNode],
    parent: Option<&str>,
    role_map: &std::collections::HashMap<&str, Vec<String>>,
) {
    for node in nodes {
        let key = match parent {
            Some(parent) => format!("{parent}.{}", node.name),
            None => node.name.clone(),
        };
        if let Some(roles) = role_map.get(key.as_str()) {
            node.roles = roles.clone();
        }
        let name = node.name.clone();
        annotate_roles(&mut node.children, Some(&name), role_map);
    }
}

fn symbol_node(stmt: &Stmt, index: &LineIndex) -> Option<SymbolNode> {
    let node = match stmt {
        Stmt::Fn(f) => SymbolNode {
            name: f.name.clone(),
            kind: "function".to_string(),
            detail: Some(fn_detail(f)),
            location: index.span_loc(f.span),
            roles: Vec::new(),
            children: Vec::new(),
        },
        Stmt::Struct(d) => SymbolNode {
            name: d.name.clone(),
            kind: "struct".to_string(),
            detail: None,
            location: index.span_loc(d.span),
            roles: Vec::new(),
            children: fields_and_methods(&d.fields, &d.methods, index),
        },
        Stmt::Class(d) => SymbolNode {
            name: d.name.clone(),
            kind: "class".to_string(),
            detail: None,
            location: index.span_loc(d.span),
            roles: Vec::new(),
            children: fields_and_methods(&d.fields, &d.methods, index),
        },
        Stmt::Enum(d) => {
            let mut children: Vec<SymbolNode> = d
                .variants
                .iter()
                .map(|v| SymbolNode {
                    name: v.name.clone(),
                    kind: "variant".to_string(),
                    detail: None,
                    location: index.span_loc(v.span),
                    roles: Vec::new(),
                    children: Vec::new(),
                })
                .collect();
            children.extend(d.methods.iter().map(|m| method_node(m, index)));
            SymbolNode {
                name: d.name.clone(),
                kind: "enum".to_string(),
                detail: None,
                location: index.span_loc(d.span),
                roles: Vec::new(),
                children,
            }
        }
        Stmt::Impl(d) => SymbolNode {
            name: format!("{} for {}", d.trait_name, d.target),
            kind: "impl".to_string(),
            detail: None,
            location: index.span_loc(d.span),
            roles: Vec::new(),
            children: d.methods.iter().map(|m| method_node(m, index)).collect(),
        },
        _ => return None,
    };
    Some(node)
}

fn fields_and_methods(
    fields: &[noeta_ast::FieldDecl],
    methods: &[noeta_ast::FnDecl],
    index: &LineIndex,
) -> Vec<SymbolNode> {
    let mut children: Vec<SymbolNode> = fields
        .iter()
        .map(|f| SymbolNode {
            name: f.name.clone(),
            kind: "field".to_string(),
            detail: None,
            location: index.span_loc(f.span),
            roles: Vec::new(),
            children: Vec::new(),
        })
        .collect();
    children.extend(methods.iter().map(|m| method_node(m, index)));
    children
}

fn method_node(f: &noeta_ast::FnDecl, index: &LineIndex) -> SymbolNode {
    SymbolNode {
        name: f.name.clone(),
        kind: "method".to_string(),
        detail: Some(fn_detail(f)),
        location: index.span_loc(f.span),
        roles: Vec::new(),
        children: Vec::new(),
    }
}

/// A function's short detail: `fn name(p0, p1)` — parameter names only (the AST pretty-printer's
/// own convention). Precise parameter/return types come from `type_at` / `ast`.
fn fn_detail(f: &noeta_ast::FnDecl) -> String {
    let params = f
        .params
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("fn {}({params})", f.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    const SRC: &str = "\
fn handle(n: int): int {
  xs = [1, 2, 3];
  return n + xs.len();
}

struct Point { x: int; y: int }

enum Color { Red; Green }
";

    fn prep() -> Prepared {
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    #[test]
    fn type_at_a_symbol_reports_the_inferred_type() {
        let p = prep();
        // `xs` is a `List<int>` binding; `n` is an `int` parameter — both recovered from a use site.
        let xs = type_at(&p, Some("xs"), None, None);
        assert!(xs.found, "note: {:?}", xs.note);
        assert_eq!(xs.r#type, "List<int>");
        assert!(xs.location.is_some());

        let n = type_at(&p, Some("n"), None, None);
        assert!(n.found);
        assert_eq!(n.r#type, "int");
    }

    #[test]
    fn type_at_notes_packed_storage() {
        let p = prepare(
            &Some(
                "@packed(layout: column) struct Vec3 { x: f32; y: f32; z: f32 }\n\
                 v = Vec3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }\n\
                 vs = [v]\n\
                 echo vs.len()\n"
                    .to_string(),
            ),
            &None,
        )
        .unwrap();
        let v = type_at(&p, Some("v"), None, None);
        assert_eq!(v.r#type, "Vec3");
        assert_eq!(
            v.layout.as_deref(),
            Some("@packed — 12 bytes, column-major lists (SoA)")
        );
        let vs = type_at(&p, Some("vs"), None, None);
        assert_eq!(vs.r#type, "List<Vec3>");
        assert_eq!(
            vs.layout.as_deref(),
            Some("flat packed storage — 12 bytes/element, column-major (SoA)")
        );
        // Ordinary boxed storage says nothing.
        let xs = type_at(&prep(), Some("xs"), None, None);
        assert_eq!(xs.layout, None);
    }

    #[test]
    fn type_at_reports_missing_and_unknown_sites() {
        let p = prep();
        assert!(!type_at(&p, Some("nope"), None, None).found);
        // Neither symbol nor position: an explanatory miss, not a panic.
        let none = type_at(&p, None, None, None);
        assert!(!none.found);
        assert!(none.note.unwrap().contains("symbol"));
    }

    #[test]
    fn symbols_outlines_declarations_and_members() {
        let p = prep();
        let out = symbols(&p);
        let kinds: Vec<(&str, &str)> = out
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect();
        assert!(kinds.contains(&("handle", "function")));
        assert!(kinds.contains(&("Point", "struct")));
        assert!(kinds.contains(&("Color", "enum")));
        // The struct carries its fields as children.
        let point = out.symbols.iter().find(|s| s.name == "Point").unwrap();
        let fields: Vec<&str> = point.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(fields, vec!["x", "y"]);
        // The function detail lists parameter names.
        let handle = out.symbols.iter().find(|s| s.name == "handle").unwrap();
        assert_eq!(handle.detail.as_deref(), Some("fn handle(n)"));
    }

    #[test]
    fn symbols_carries_architectural_roles() {
        // `@role(Semantic.EntryPoint)` rides the `Route` attribute; `handle` bears `#[Route]`, so
        // the outline node for `handle` shows the role — the architecture on the map itself.
        let src = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

#[Route(\"/x\")]
fn handle(n: int): int { return n }

fn helper(): int { return 1 }
";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = symbols(&p);
        let handle = out.symbols.iter().find(|s| s.name == "handle").unwrap();
        assert_eq!(handle.roles, vec!["Semantic.EntryPoint"]);
        let helper = out.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(helper.roles.is_empty(), "unannotated fn carries no role");
    }
}
