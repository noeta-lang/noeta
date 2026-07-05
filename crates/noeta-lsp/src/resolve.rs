//! Top-level definition resolution for go-to-definition.
//!
//! Scope for slice **L3**: the two unambiguous, high-value cases — jumping from a **function call**
//! or a **type reference** to the top-level `fn` / `struct` / `class` / `enum` that declares it.
//! These live in a single global namespace with no shadowing among themselves, so a name resolves to
//! its declaration without scope tracking. The reference under the cursor is found from the token
//! stream (see [`crate`]); this module owns the definition table and the name lookup.
//!
//! Deliberately *not* here yet (a documented follow-on): locals and parameters (need a scope-aware
//! walk), methods and fields (need the receiver's type), and cross-module definitions (need the
//! linked workspace). Until then a reference to a local simply yields no jump — never a wrong one.

use std::collections::HashMap;

use noeta_ast::{Program, Stmt};
use noeta_span::Span;

/// The top-level definitions a document offers for go-to-definition, keyed by name → the span of the
/// **declared name** (what the editor jumps to). Two namespaces because a value reference (a call)
/// and a type reference resolve independently; the same spelling could name both.
#[derive(Debug, Default)]
pub struct Definitions {
    /// Top-level function names.
    values: HashMap<String, Span>,
    /// Top-level `struct` / `class` / `enum` names.
    types: HashMap<String, Span>,
}

impl Definitions {
    /// Collect the top-level definitions of `program`. The first declaration of a name wins (a
    /// redeclaration is a checker error surfaced separately; here we just keep resolution stable).
    pub fn collect(program: &Program) -> Definitions {
        let mut defs = Definitions::default();
        for stmt in &program.stmts {
            match stmt {
                Stmt::Fn(decl) => {
                    defs.values
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                Stmt::Struct(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                Stmt::Class(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                Stmt::Enum(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                _ => {}
            }
        }
        defs
    }

    /// The declaration span for `name`, or `None` if it names no top-level definition. Types are
    /// checked before values: the two namespaces rarely collide, and a PascalCase type reference is
    /// the more likely intent when they do.
    pub fn resolve(&self, name: &str) -> Option<Span> {
        self.types
            .get(name)
            .or_else(|| self.values.get(name))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn defs_of(src: &str) -> Definitions {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        Definitions::collect(&parsed.program)
    }

    #[test]
    fn collects_functions_and_types() {
        let defs =
            defs_of("fn greet(): int { return 1 }\nstruct Point { x: int }\nenum Color { Red }");
        assert!(defs.resolve("greet").is_some());
        assert!(defs.resolve("Point").is_some());
        assert!(defs.resolve("Color").is_some());
        assert!(defs.resolve("missing").is_none());
    }

    #[test]
    fn resolves_to_the_name_span_not_the_whole_decl() {
        // `fn greet` — the name starts at byte 3.
        let defs = defs_of("fn greet(): int { return 1 }");
        let span = defs.resolve("greet").unwrap();
        assert_eq!(span.start, 3);
        assert_eq!(span.end, 8); // "greet"
    }
}
