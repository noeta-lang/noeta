//! The declaration inventory every graph tool addresses nodes through: one walk over the linked
//! program that names each declaration the way the post-link vocabulary names it, records the kind
//! and the two spans, and answers "which declaration is this?" for a qualified name, a leaf name,
//! or a span.
//!
//! Two rules make the tools joinable. Every declaration is named exactly once, so `symbols`,
//! `trace`, `impact` and `callers` cannot report the same function under different spellings, and
//! a declaration inside a `@test`/`@bench` block is in the inventory like any other.

use noeta_ast::{Program, Stmt};
use noeta_span::Span;

use crate::analyze::{NodeId, NodeKind, Prepared};

/// One declaration of the linked program.
#[derive(Debug, Clone)]
pub struct Decl {
    /// The post-link name: namespace-qualified for a package (`app.main.handle`), `Type.method`
    /// for a method, `Type.field` for a member.
    pub name: String,
    pub kind: NodeKind,
    /// The declared name's span — the join key, and what a [`NodeId`] carries.
    pub name_span: Span,
    /// The whole declaration's span.
    pub decl_span: Span,
}

impl Decl {
    /// This declaration's identity on the wire.
    pub fn id(&self, p: &Prepared) -> NodeId {
        p.node_id(&self.name, self.kind, self.name_span)
    }
}

/// What looking a name up found.
pub enum Lookup<'a> {
    Found(&'a Decl),
    /// The leaf matched several declarations; the caller reports them so the agent can pick one.
    Ambiguous(Vec<&'a Decl>),
    Missing,
}

/// Every declaration of a program, addressable by name or by span.
#[derive(Debug, Default)]
pub struct DeclIndex {
    decls: Vec<Decl>,
}

impl DeclIndex {
    /// Walk `program` — the linked one where there is one, so names carry their post-link
    /// qualification — into the inventory.
    pub fn build(program: &Program) -> DeclIndex {
        let mut index = DeclIndex::default();
        index.collect(&program.stmts);
        index
    }

    pub fn decls(&self) -> &[Decl] {
        &self.decls
    }

    /// The declaration `query` names: an exact post-link name, else the one declaration whose name
    /// ends in `query` on a segment boundary (`shared` finds `pkg.alpha.shared`, `Counter.bump`
    /// finds `pkg.main.Counter.bump`).
    pub fn lookup(&self, query: &str) -> Lookup<'_> {
        if let Some(exact) = self.decls.iter().find(|d| d.name == query) {
            return Lookup::Found(exact);
        }
        let suffix = format!(".{query}");
        let leaves: Vec<&Decl> = self
            .decls
            .iter()
            .filter(|d| d.name.ends_with(&suffix))
            .collect();
        match leaves.len() {
            0 => Lookup::Missing,
            1 => Lookup::Found(leaves[0]),
            _ => Lookup::Ambiguous(leaves),
        }
    }

    /// The declaration whose **name** span is exactly `span` — how a resolved navigation target
    /// (which lands on the declared name) becomes an identity.
    pub fn at_name_span(&self, span: Span) -> Option<&Decl> {
        self.decls.iter().find(|d| d.name_span == span)
    }

    /// The declaration whose name span covers `offset` in `source`, else the tightest declaration
    /// containing it — the identity of "whatever is at this position".
    pub fn at_offset(&self, source: noeta_span::SourceId, offset: u32) -> Option<&Decl> {
        let on = |s: Span| s.source == source && s.start <= offset && offset <= s.end;
        self.decls.iter().find(|d| on(d.name_span)).or_else(|| {
            self.decls
                .iter()
                .filter(|d| on(d.decl_span))
                .min_by_key(|d| d.decl_span.end - d.decl_span.start)
        })
    }

    /// Recurse a statement list, descending into `@tier { … }` blocks so a fixture type or a
    /// test function is in the inventory like any other declaration.
    fn collect(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::Fn(decl) => self.push(
                    decl.name.to_string(),
                    NodeKind::Function,
                    decl.name_span,
                    decl.span,
                ),
                Stmt::Struct(decl) => {
                    let name = decl.name.to_string();
                    self.push(name.clone(), NodeKind::Struct, decl.name_span, decl.span);
                    self.members(&name, &decl.fields, &decl.methods);
                }
                Stmt::Class(decl) => {
                    let name = decl.name.to_string();
                    self.push(name.clone(), NodeKind::Class, decl.name_span, decl.span);
                    self.members(&name, &decl.fields, &decl.methods);
                }
                Stmt::Enum(decl) => {
                    let name = decl.name.to_string();
                    self.push(name.clone(), NodeKind::Enum, decl.name_span, decl.span);
                    for variant in &decl.variants {
                        self.push(
                            format!("{name}.{}", variant.name),
                            NodeKind::Variant,
                            variant.name_span,
                            variant.span,
                        );
                    }
                    for method in &decl.methods {
                        self.push(
                            format!("{name}.{}", method.name),
                            NodeKind::Method,
                            method.name_span,
                            method.span,
                        );
                    }
                }
                Stmt::Trait(decl) => {
                    let name = decl.name.to_string();
                    self.push(name.clone(), NodeKind::Trait, decl.name_span, decl.span);
                    for method in &decl.methods {
                        self.push(
                            format!("{name}.{}", method.sig.name),
                            NodeKind::Method,
                            method.sig.name_span,
                            method.sig.span,
                        );
                    }
                }
                Stmt::Impl(decl) => {
                    // Named the way the call graph names a standalone impl's methods
                    // (`Target.method`), so an impl method has one identity across both.
                    self.push(
                        format!("{} for {}", decl.trait_name, decl.target),
                        NodeKind::Impl,
                        decl.trait_span,
                        decl.span,
                    );
                    for method in &decl.methods {
                        self.push(
                            format!("{}.{}", decl.target, method.name),
                            NodeKind::Method,
                            method.name_span,
                            method.span,
                        );
                    }
                }
                Stmt::TierBlock { items, .. } => self.collect(items),
                _ => {}
            }
        }
    }

    fn members(
        &mut self,
        owner: &str,
        fields: &[noeta_ast::FieldDecl],
        methods: &[noeta_ast::FnDecl],
    ) {
        for field in fields {
            self.push(
                format!("{owner}.{}", field.name),
                NodeKind::Field,
                field.name_span,
                field.span,
            );
        }
        for method in methods {
            self.push(
                format!("{owner}.{}", method.name),
                NodeKind::Method,
                method.name_span,
                method.span,
            );
        }
    }

    fn push(&mut self, name: String, kind: NodeKind, name_span: Span, decl_span: Span) {
        self.decls.push(Decl {
            name,
            kind,
            name_span,
            decl_span,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    fn index_of(src: &str) -> DeclIndex {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = noeta_lexer::lex(&source);
        DeclIndex::build(&noeta_parser::parse(&source, &lexed.tokens).program)
    }

    fn names(index: &DeclIndex) -> Vec<(&str, &'static str)> {
        index
            .decls()
            .iter()
            .map(|d| (d.name.as_str(), d.kind.as_str()))
            .collect()
    }

    #[test]
    fn every_declaration_kind_is_named_once() {
        let index = index_of(
            "fn top(): int { return 1 }\n\
             struct S { x: int\n  fn m(): int { return 1 } }\n\
             enum E { A; B }\n\
             trait T { fn t(): int }\n\
             impl T for S { fn t(): int { return 1 } }\n",
        );
        let got = names(&index);
        assert!(got.contains(&("top", "function")), "{got:?}");
        assert!(got.contains(&("S", "struct")));
        assert!(got.contains(&("S.x", "field")));
        assert!(got.contains(&("S.m", "method")));
        assert!(got.contains(&("E.A", "variant")));
        assert!(got.contains(&("T", "trait")));
        assert!(got.contains(&("T for S", "impl")));
        // The standalone impl's method is named the way the call graph names it.
        assert_eq!(
            index.decls().iter().filter(|d| d.name == "S.t").count(),
            1,
            "{got:?}"
        );
    }

    /// D9: a declaration written inside a `@test` block is in the inventory like any other.
    #[test]
    fn tier_block_declarations_are_indexed() {
        let index = index_of(
            "fn helper(): int { return 1 }\n\
             @test {\n  struct Fixture { n: int\n    fn build(): int { return helper() } }\n\
               fn uses_fixture(): void { assert(true) }\n}\n",
        );
        let got = names(&index);
        assert!(got.contains(&("Fixture", "struct")), "{got:?}");
        assert!(got.contains(&("Fixture.build", "method")), "{got:?}");
        assert!(got.contains(&("uses_fixture", "function")), "{got:?}");
    }

    #[test]
    fn a_leaf_resolves_when_it_is_unique_and_lists_the_ties_when_it_is_not() {
        let index =
            index_of("struct A { fn go(): int { return 1 } }\nfn solo(): int { return 1 }\n");
        assert!(matches!(index.lookup("solo"), Lookup::Found(d) if d.name == "solo"));
        assert!(matches!(index.lookup("go"), Lookup::Found(d) if d.name == "A.go"));
        assert!(matches!(index.lookup("A.go"), Lookup::Found(d) if d.name == "A.go"));
        assert!(matches!(index.lookup("ghost"), Lookup::Missing));
    }

    #[test]
    fn an_ambiguous_leaf_reports_every_candidate() {
        let index = index_of(
            "struct A { fn go(): int { return 1 } }\nstruct B { fn go(): int { return 2 } }\n",
        );
        match index.lookup("go") {
            Lookup::Ambiguous(all) => {
                let mut names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
                names.sort();
                assert_eq!(names, vec!["A.go", "B.go"]);
            }
            _ => panic!("two `go` methods must be ambiguous"),
        }
    }
}
