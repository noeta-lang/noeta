//! Completion candidates for `textDocument/completion` (slice **L5**).
//!
//! Two forms, chosen by the caller from the cursor context:
//!
//! - [`complete`] — **identifier completion** (C1): the language keywords, the top-level
//!   declarations (functions and types), and the value bindings in scope at the cursor (locals,
//!   parameters, `for`/`match`/closure bindings, and earlier module-level bindings). In-scope
//!   bindings come from [`resolve::visible_at`], so the same scoping walk backs completion and
//!   go-to-definition.
//! - [`members_of`] — **member completion** (C2): the fields, enum variants, and methods of a named
//!   type, offered on a `receiver.member` access once the caller has resolved the receiver's type.
//!
//! A best-effort read of the mid-edit AST — it leans on the recovering parser and the client's own
//! prefix filtering rather than requiring a clean parse. Both return backend-neutral [`Candidate`]s
//! (label + kind + optional detail) that the server maps to LSP `CompletionItem`s.

use std::collections::HashSet;

use noeta_ast::{Program, Stmt};
use noeta_span::SourceId;

use crate::resolve;
use crate::symbols;

/// What a completion candidate is, for the client's icon and the server's `CompletionItemKind`
/// mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Keyword,
    Function,
    Struct,
    Class,
    Enum,
    /// A local, parameter, or other in-scope value binding.
    Variable,
    /// A struct/class field (member completion after `.`).
    Field,
    /// A method (member completion after `.`).
    Method,
    /// An enum variant (member completion after `.`).
    EnumMember,
}

/// One completion candidate: the inserted/filtered text, its kind, and an optional short detail.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub label: String,
    pub kind: CandidateKind,
    pub detail: Option<String>,
}

/// The Noeta surface keywords offered everywhere. Deliberately the *statement/expression* keywords a
/// developer types; the reflection intrinsics (`type_of`, `attributes_of`, …) are omitted as niche.
const KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "class",
    "concurrent",
    "continue",
    "destruct",
    "echo",
    "else",
    "enum",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "is",
    "match",
    "mut",
    "namespace",
    "pub",
    "return",
    "spawn",
    "struct",
    "then",
    "true",
    "type",
    "use",
    "while",
    "yield",
];

/// The completion candidates at `offset` in file `source` of `program`: the top-level declarations
/// (with their precise kinds), then the value bindings in scope at the cursor, then the keywords.
/// De-duplicated by label, keeping the earliest — the scoping walk also binds a top-level function's
/// name into the module scope, so listing the declarations first is what stamps `greet` as a
/// `Function` rather than a bare `Variable`.
pub fn complete(program: &Program, offset: u32, source: SourceId) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    // Top-level declarations, with their precise kinds (a name usable as a call, constructor, or
    // type reference).
    for stmt in &program.stmts {
        let (name, kind) = match stmt {
            Stmt::Fn(decl) => (&decl.name, CandidateKind::Function),
            Stmt::Struct(decl) => (&decl.name, CandidateKind::Struct),
            Stmt::Class(decl) => (&decl.name, CandidateKind::Class),
            Stmt::Enum(decl) => (&decl.name, CandidateKind::Enum),
            _ => continue,
        };
        candidates.push(Candidate {
            label: name.clone(),
            kind,
            detail: None,
        });
    }

    // In-scope value bindings (locals, parameters, loop/match/closure bindings, earlier module-level
    // bindings) — the names relevant where the cursor is. A top-level function's name is also here
    // (bound into the module scope) but was already emitted above with its precise kind, so dedup
    // drops it; a genuine local keeps its `Variable` kind.
    for (name, _span) in resolve::visible_at(program, offset, source) {
        candidates.push(Candidate {
            label: name,
            kind: CandidateKind::Variable,
            detail: None,
        });
    }

    // Keywords last: a same-spelled user name (rare) is the more useful suggestion.
    for keyword in KEYWORDS {
        candidates.push(Candidate {
            label: (*keyword).to_string(),
            kind: CandidateKind::Keyword,
            detail: None,
        });
    }

    dedupe_by_label(candidates)
}

/// The member candidates of the type named `type_name` in `program`: its fields, enum variants, and
/// methods, each with a signature/type detail — for member completion after `.`, once the receiver's
/// type is known. Empty if no such type is declared (or it declares no members). The `program`
/// should be the merged workspace program so a type imported from a sibling resolves.
pub fn members_of(program: &Program, type_name: &str) -> Vec<Candidate> {
    let mut members = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Struct(decl) if decl.name == type_name => {
                for field in &decl.fields {
                    members.push(Candidate {
                        label: field.name.clone(),
                        kind: CandidateKind::Field,
                        detail: field.ty.as_ref().map(symbols::render_type_ref),
                    });
                }
                push_methods(&mut members, &decl.methods);
            }
            Stmt::Class(decl) if decl.name == type_name => {
                for field in &decl.fields {
                    members.push(Candidate {
                        label: field.name.clone(),
                        kind: CandidateKind::Field,
                        detail: field.ty.as_ref().map(symbols::render_type_ref),
                    });
                }
                push_methods(&mut members, &decl.methods);
            }
            Stmt::Enum(decl) if decl.name == type_name => {
                for variant in &decl.variants {
                    members.push(Candidate {
                        label: variant.name.clone(),
                        kind: CandidateKind::EnumMember,
                        detail: symbols::variant_detail(variant),
                    });
                }
                push_methods(&mut members, &decl.methods);
            }
            _ => {}
        }
    }
    dedupe_by_label(members)
}

/// Append each method as a `Method` candidate carrying its signature.
fn push_methods(members: &mut Vec<Candidate>, methods: &[noeta_ast::FnDecl]) {
    for method in methods {
        members.push(Candidate {
            label: method.name.clone(),
            kind: CandidateKind::Method,
            detail: Some(symbols::fn_signature(method)),
        });
    }
}

/// Keep the first candidate for each label, preserving order.
fn dedupe_by_label(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(c.label.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::Source;

    fn complete_at(src: &str, offset: u32) -> Vec<Candidate> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        complete(&program, offset, SourceId::FIRST)
    }

    fn labels_of(candidates: &[Candidate], kind: CandidateKind) -> Vec<&str> {
        candidates
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| c.label.as_str())
            .collect()
    }

    #[test]
    fn keywords_are_always_offered() {
        let cands = complete_at("", 0);
        let kws = labels_of(&cands, CandidateKind::Keyword);
        assert!(kws.contains(&"fn"));
        assert!(kws.contains(&"match"));
        assert!(kws.contains(&"struct"));
    }

    #[test]
    fn top_level_declarations_are_offered_with_their_kinds() {
        let src = "fn greet(): int { return 1 }\nstruct Point { x: int }\nenum Color { Red }";
        // Cursor at end of file.
        let cands = complete_at(src, src.len() as u32);
        assert!(labels_of(&cands, CandidateKind::Function).contains(&"greet"));
        assert!(labels_of(&cands, CandidateKind::Struct).contains(&"Point"));
        assert!(labels_of(&cands, CandidateKind::Enum).contains(&"Color"));
    }

    #[test]
    fn parameters_and_locals_are_offered_inside_a_function() {
        let src = "fn f(count: int): int {\n  total = count + 1\n  return total\n}";
        // Cursor on the `return total` line — both the parameter and the local are in scope.
        let offset = src.find("return").unwrap() as u32;
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(vars.contains(&"count"), "parameter in scope; got {vars:?}");
        assert!(vars.contains(&"total"), "local in scope; got {vars:?}");
    }

    #[test]
    fn a_binding_is_not_visible_before_its_own_initializer() {
        // Inside `x`'s initializer, `x` is not yet in scope, but the parameter `n` is.
        let src = "fn f(n: int): int {\n  x = n + 1\n  return x\n}";
        let offset = src.find("n + 1").unwrap() as u32 + 1; // on the `n` in the initializer
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(vars.contains(&"n"));
        assert!(!vars.contains(&"x"), "x not visible in its own initializer");
    }

    #[test]
    fn out_of_scope_locals_are_not_offered() {
        // `inner` is local to `a`; completing in `b` must not see it.
        let src = "fn a() {\n  inner = 1\n}\nfn b() {\n  return 0\n}";
        let offset = src.find("return 0").unwrap() as u32;
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(
            !vars.contains(&"inner"),
            "leaked a's local into b; got {vars:?}"
        );
    }

    fn members(src: &str, type_name: &str) -> Vec<Candidate> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        members_of(&program, type_name)
    }

    #[test]
    fn members_of_a_class_lists_fields_and_methods() {
        let src = "class Counter { n: int\n  fn get(): int { return self.n }\n}";
        let ms = members(src, "Counter");
        let field = ms.iter().find(|c| c.label == "n").unwrap();
        assert_eq!(field.kind, CandidateKind::Field);
        assert_eq!(field.detail.as_deref(), Some("int"));
        let method = ms.iter().find(|c| c.label == "get").unwrap();
        assert_eq!(method.kind, CandidateKind::Method);
        assert_eq!(method.detail.as_deref(), Some("() -> int"));
    }

    #[test]
    fn members_of_an_enum_lists_variants() {
        let src = "enum Shape {\n  Dot\n  Circle(radius: int)\n}";
        let ms = members(src, "Shape");
        assert_eq!(
            ms.iter().find(|c| c.label == "Dot").unwrap().kind,
            CandidateKind::EnumMember
        );
        let circle = ms.iter().find(|c| c.label == "Circle").unwrap();
        assert_eq!(circle.detail.as_deref(), Some("(radius: int)"));
    }

    #[test]
    fn members_of_unknown_type_is_empty() {
        assert!(members("struct Point { x: int }", "Nope").is_empty());
    }

    #[test]
    fn labels_are_unique() {
        let src = "fn f() { return 0 }";
        let cands = complete_at(src, src.len() as u32);
        let mut labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
        let total = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), total, "duplicate completion labels");
    }
}
