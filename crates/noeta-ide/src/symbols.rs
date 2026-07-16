//! Document symbols for the outline / breadcrumbs / `@`-symbol-search (slice **L4**).
//!
//! A single-file feature: walk the entry document's AST and produce the hierarchical symbol tree
//! the editor renders — top-level `fn`s and type declarations, with a type's fields/variants and
//! methods nested underneath, plus standalone `impl Trait for Type` blocks. No compiler state and no
//! type-checking: a pure structural read of the parsed program.
//!
//! [`outline`] returns a backend-neutral [`SymbolNode`] tree (name, kind, the full declaration span
//! and the name span, children); the store maps each node to a [`DocumentSymbol`], resolving the
//! two spans to ranges. Keeping the walk free of position mapping is what makes it unit-testable
//! against source without an editor client.

use noeta_ast::{
    EnumDecl, FieldDecl, FnDecl, ImplDecl, Param, Program, Stmt, TypeRef, VariantDecl,
};
use noeta_span::Span;

use crate::offsets::Range;

/// The kind of a declared symbol, for the outline icon. Mirrors the LSP `SymbolKind` values this
/// engine emits, owned here so the engine stays wire-protocol-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Struct,
    Class,
    Enum,
    EnumMember,
    Field,
    Method,
    Interface,
}

/// One resolved node of the document outline — [`SymbolNode`] with its two spans mapped to
/// positional ranges, ready for an adapter to reshape onto its wire.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    /// The range of the whole declaration — the outline range that "contains the cursor".
    pub range: Range,
    /// The range of just the declared name — what is revealed when the symbol is picked.
    pub selection_range: Range,
    pub children: Vec<DocumentSymbol>,
}

/// One node of the document outline: the display name and (optional) detail, the symbol kind,
/// the span of the whole declaration (the outline "range") and of just its name (the "selection
/// range", what is revealed when the symbol is picked), and any nested members.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolNode {
    pub name: String,
    /// A short signature-like detail (a function's parameters/return, a field's type). `None` falls
    /// back to the name in the client.
    pub detail: Option<String>,
    pub kind: SymbolKind,
    /// The span of the whole declaration — the outline range that "contains the cursor".
    pub full_span: Span,
    /// The span of the declared name — the selection range, always contained by `full_span`.
    pub name_span: Span,
    pub children: Vec<SymbolNode>,
}

/// The document outline for `program`: top-level functions and type declarations (with their
/// fields/variants and methods as children) plus standalone `impl` blocks, in source order.
pub fn outline(program: &Program) -> Vec<SymbolNode> {
    let mut symbols = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Fn(decl) => symbols.push(fn_symbol(decl, SymbolKind::Function)),
            Stmt::Struct(decl) => symbols.push(SymbolNode {
                name: decl.name.clone(),
                detail: None,
                kind: SymbolKind::Struct,
                full_span: decl.span,
                name_span: decl.name_span,
                children: type_members(&decl.fields, &decl.methods),
            }),
            Stmt::Class(decl) => symbols.push(SymbolNode {
                name: decl.name.clone(),
                detail: None,
                kind: SymbolKind::Class,
                full_span: decl.span,
                name_span: decl.name_span,
                children: type_members(&decl.fields, &decl.methods),
            }),
            Stmt::Enum(decl) => symbols.push(enum_symbol(decl)),
            Stmt::Impl(decl) => symbols.push(impl_symbol(decl)),
            _ => {}
        }
    }
    symbols
}

/// A function or method symbol, with its signature as detail.
fn fn_symbol(decl: &FnDecl, kind: SymbolKind) -> SymbolNode {
    SymbolNode {
        name: decl.name.clone(),
        detail: Some(fn_signature(decl)),
        kind,
        full_span: decl.span,
        name_span: decl.name_span,
        children: Vec::new(),
    }
}

/// The nested members of a `struct` or `class`: its fields (as `FIELD`) followed by its methods (as
/// `METHOD`), in declaration order within each group.
fn type_members(fields: &[FieldDecl], methods: &[FnDecl]) -> Vec<SymbolNode> {
    let mut members = Vec::with_capacity(fields.len() + methods.len());
    for field in fields {
        members.push(SymbolNode {
            name: field.name.clone(),
            detail: field.ty.as_ref().map(render_type_ref),
            kind: SymbolKind::Field,
            full_span: field.span,
            name_span: field.name_span,
            children: Vec::new(),
        });
    }
    for method in methods {
        members.push(fn_symbol(method, SymbolKind::Method));
    }
    members
}

/// An enum symbol: its variants (as `ENUM_MEMBER`, with any payload as detail) followed by its
/// methods.
fn enum_symbol(decl: &EnumDecl) -> SymbolNode {
    let mut children = Vec::with_capacity(decl.variants.len() + decl.methods.len());
    for variant in &decl.variants {
        children.push(SymbolNode {
            name: variant.name.clone(),
            detail: variant_detail(variant),
            kind: SymbolKind::EnumMember,
            full_span: variant.span,
            name_span: variant.name_span,
            children: Vec::new(),
        });
    }
    for method in &decl.methods {
        children.push(fn_symbol(method, SymbolKind::Method));
    }
    SymbolNode {
        name: decl.name.clone(),
        detail: None,
        kind: SymbolKind::Enum,
        full_span: decl.span,
        name_span: decl.name_span,
        children,
    }
}

/// A standalone `impl Trait for Type` block, shown as an interface-kind node named `Trait for Type`
/// with its methods nested. The selection range is the trait name (the block has no single name).
fn impl_symbol(decl: &ImplDecl) -> SymbolNode {
    SymbolNode {
        name: format!("{} for {}", decl.trait_name, decl.target),
        detail: None,
        kind: SymbolKind::Interface,
        full_span: decl.span,
        name_span: decl.trait_span,
        children: decl
            .methods
            .iter()
            .map(|method| fn_symbol(method, SymbolKind::Method))
            .collect(),
    }
}

/// The payload detail of an enum variant: its associated field types as `(A, B)`, or `None` for a
/// fieldless variant. Shared with member completion.
pub(crate) fn variant_detail(variant: &VariantDecl) -> Option<String> {
    if variant.fields.is_empty() {
        return None;
    }
    let fields = variant
        .fields
        .iter()
        .map(param_detail)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("({fields})"))
}

/// A function/method signature detail: `(name: T, …) -> R`, omitting the arrow when there is no
/// declared return type. Shared with member completion.
pub(crate) fn fn_signature(decl: &FnDecl) -> String {
    let params = decl
        .params
        .iter()
        .map(param_detail)
        .collect::<Vec<_>>()
        .join(", ");
    match &decl.ret {
        Some(ret) => format!("({params}) -> {}", render_type_ref(ret)),
        None => format!("({params})"),
    }
}

/// One parameter (or variant field) rendered as `name: T`, or just `name` when it has no annotation.
/// Shared with signature help.
pub(crate) fn param_detail(param: &Param) -> String {
    match &param.ty {
        Some(ty) => format!("{}: {}", param.name, render_type_ref(ty)),
        None => param.name.clone(),
    }
}

/// Render a surface [`TypeRef`] back to compact source syntax for symbol detail (`List<int>`, `?T`,
/// `A | B`, `(A, B)`, `(A) -> R`). Shared with member completion.
pub fn render_type_ref(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Named { name, args, .. } => {
            if args.is_empty() {
                name.clone()
            } else {
                format!("{}<{}>", name, join(args))
            }
        }
        TypeRef::DynTrait { trait_name, .. } => format!("dyn {trait_name}"),
        TypeRef::Optional { inner, .. } => format!("?{}", render_type_ref(inner)),
        TypeRef::Union { members, .. } => members
            .iter()
            .map(render_type_ref)
            .collect::<Vec<_>>()
            .join(" | "),
        TypeRef::Tuple { elements, .. } => format!("({})", join(elements)),
        TypeRef::Fn { params, ret, .. } => {
            format!("({}) -> {}", join(params), render_type_ref(ret))
        }
    }
}

/// Render a comma-separated list of type references.
fn join(types: &[TypeRef]) -> String {
    types
        .iter()
        .map(render_type_ref)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn outline_of(src: &str) -> Vec<SymbolNode> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        outline(&parse(&source, &lexed.tokens).program)
    }

    #[test]
    fn top_level_fn_becomes_a_function_symbol_with_signature() {
        let syms = outline_of("fn add(a: int, b: int): int { return a + b }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "add");
        assert_eq!(syms[0].kind, SymbolKind::Function);
        assert_eq!(syms[0].detail.as_deref(), Some("(a: int, b: int) -> int"));
        // The selection range is the name, contained by the whole-declaration range.
        assert!(syms[0].name_span.start >= syms[0].full_span.start);
        assert!(syms[0].name_span.end <= syms[0].full_span.end);
    }

    #[test]
    fn struct_nests_fields_then_methods() {
        let syms =
            outline_of("struct Point {\n  x: int\n  y: int\n  fn norm(): int { return self.x }\n}");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, SymbolKind::Struct);
        let kids = &syms[0].children;
        assert_eq!(kids.len(), 3);
        assert_eq!(
            (kids[0].name.as_str(), kids[0].kind),
            ("x", SymbolKind::Field)
        );
        assert_eq!(kids[0].detail.as_deref(), Some("int"));
        assert_eq!(
            (kids[1].name.as_str(), kids[1].kind),
            ("y", SymbolKind::Field)
        );
        assert_eq!(
            (kids[2].name.as_str(), kids[2].kind),
            ("norm", SymbolKind::Method)
        );
    }

    #[test]
    fn enum_nests_variants_with_payload_detail() {
        let syms = outline_of("enum Shape {\n  Dot\n  Circle(radius: int)\n}");
        assert_eq!(syms[0].kind, SymbolKind::Enum);
        let kids = &syms[0].children;
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].name, "Dot");
        assert_eq!(kids[0].kind, SymbolKind::EnumMember);
        assert_eq!(kids[0].detail, None); // fieldless
        assert_eq!(kids[1].name, "Circle");
        assert_eq!(kids[1].detail.as_deref(), Some("(radius: int)"));
    }

    #[test]
    fn class_is_a_class_symbol() {
        let syms = outline_of("class Counter {\n  n: int\n}");
        assert_eq!(syms[0].kind, SymbolKind::Class);
        assert_eq!(syms[0].children.len(), 1);
    }

    #[test]
    fn standalone_impl_lists_its_methods() {
        let syms = outline_of("struct R {}\nimpl Show for R {\n  fn show(): int { return 1 }\n}");
        let imp = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Interface)
            .unwrap();
        assert_eq!(imp.name, "Show for R");
        assert_eq!(imp.children.len(), 1);
        assert_eq!(imp.children[0].name, "show");
        assert_eq!(imp.children[0].kind, SymbolKind::Method);
    }

    #[test]
    fn generic_and_optional_types_render_in_detail() {
        let syms = outline_of("fn find(xs: List<int>): ?int { return none }");
        assert_eq!(syms[0].detail.as_deref(), Some("(xs: List<int>) -> ?int"));
    }

    #[test]
    fn non_declarations_produce_no_symbols() {
        let syms = outline_of("x = 1\necho x");
        assert!(syms.is_empty());
    }
}
