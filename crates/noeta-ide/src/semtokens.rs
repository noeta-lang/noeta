//! Semantic tokens for `textDocument/semanticTokens/full`.
//!
//! Compiler-accurate classification of the identifiers the static TextMate grammar cannot tell
//! apart: a function reference vs a variable vs a member/property, and a type declaration. The server
//! sends these as an overlay — the client keeps TextMate colouring for keywords, strings, numbers,
//! and comments, and refines the identifiers with what the compiler actually resolved.
//!
//! Deliberately the *precise* subset: every span here is a single identifier token, taken from the
//! resolver's exact indices — the scope-aware def/use index (functions vs variables), the recorded
//! member accesses (properties), and the top-level name tables (function/type declarations). Type
//! *references* inside annotations are left to TextMate (a `List<int>` reference is one span covering
//! the arguments too, not a clean name token). [`highlights`] returns the classified spans; the
//! server delta-encodes them against the [`LEGEND`].

use std::collections::HashSet;

use noeta_ast::Program;
use noeta_span::Span;

use crate::resolve::{DefUse, Definitions};

/// The semantic token kinds this server emits, in the order of [`LEGEND`] (the index is the wire
/// `tokenType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemKind {
    Type = 0,
    Function = 1,
    Variable = 2,
    Property = 3,
}

/// The token-type legend, indexed by [`SemKind`]. Advertised at `initialize`; the wire tokens carry
/// indices into it.
pub const LEGEND: [&str; 4] = ["type", "function", "variable", "property"];

/// One delta-encoded semantic token, per the LSP wire layout (`token_type` indexes [`LEGEND`]).
/// Field-compatible with the wire `SemanticToken`, owned here so the engine stays
/// wire-protocol-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticToken {
    pub delta_line: u32,
    pub delta_start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers_bitset: u32,
}

/// The classified identifier spans of `program`, each a single token. Sorted by position with at most
/// one classification per span (the first source wins). Value references come from the def/use index
/// (function vs variable by what the use resolves to), member accesses are properties, and the
/// top-level declaration names are functions/types.
pub fn highlights(program: &Program) -> Vec<(Span, SemKind)> {
    let defs = Definitions::collect(program);
    let function_defs: HashSet<Span> = defs.value_spans().collect();
    let def_use = DefUse::build(program);

    let mut spans: Vec<(Span, SemKind)> = Vec::new();

    // Value references: a function if the use resolves to a top-level function, else a variable.
    for (use_span, def_span) in def_use.all_refs() {
        spans.push((use_span, value_kind(def_span, &function_defs)));
    }
    // Every binding *declaration* (including unreferenced ones), classified the same way.
    for span in def_use.binding_spans() {
        spans.push((span, value_kind(span, &function_defs)));
    }
    // Top-level type declaration names.
    for span in defs.type_spans() {
        spans.push((span, SemKind::Type));
    }
    // Member accesses `x.member` — fields and methods alike, as properties.
    for (_, name_span, _) in def_use.member_occurrences() {
        spans.push((name_span, SemKind::Property));
    }

    dedupe_and_sort(spans)
}

fn value_kind(def_span: Span, function_defs: &HashSet<Span>) -> SemKind {
    if function_defs.contains(&def_span) {
        SemKind::Function
    } else {
        SemKind::Variable
    }
}

/// Keep one classification per span (first wins) and order by position, as the wire format's delta
/// encoding requires.
fn dedupe_and_sort(mut spans: Vec<(Span, SemKind)>) -> Vec<(Span, SemKind)> {
    spans.sort_by_key(|(span, _)| (span.start, span.end));
    let mut seen = HashSet::new();
    spans.retain(|(span, _)| seen.insert(span.start));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn classify(src: &str) -> Vec<(String, SemKind)> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        highlights(&program)
            .into_iter()
            .map(|(span, kind)| (src[span.range()].to_string(), kind))
            .collect()
    }

    #[test]
    fn classifies_functions_variables_types_and_properties() {
        let src = "struct Point { x: int }\nfn make(): int { return 1 }\ntotal = make()\np = Point { x: 1 }\nv = p.x";
        let got = classify(src);
        let kind_of = |name: &str| got.iter().find(|(n, _)| n == name).map(|(_, k)| *k);
        assert_eq!(kind_of("Point"), Some(SemKind::Type));
        assert_eq!(kind_of("make"), Some(SemKind::Function)); // declaration and call
        assert_eq!(kind_of("total"), Some(SemKind::Variable));
        assert_eq!(kind_of("x"), Some(SemKind::Property)); // the `p.x` access
    }

    #[test]
    fn a_parameter_use_is_a_variable_not_a_function() {
        let src = "fn f(count: int): int { return count }";
        let got = classify(src);
        // `count` appears as parameter decl and use — classified variable, while `f` is a function.
        assert!(
            got.iter()
                .any(|(n, k)| n == "count" && *k == SemKind::Variable)
        );
        assert!(got.iter().any(|(n, k)| n == "f" && *k == SemKind::Function));
    }

    #[test]
    fn spans_are_sorted_and_unique() {
        let src = "fn f(): int { return 1 }\na = f()\nb = f()";
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        let spans = highlights(&program);
        let mut starts: Vec<u32> = spans.iter().map(|(s, _)| s.start).collect();
        let sorted = {
            let mut s = starts.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(
            starts, sorted,
            "must be position-ordered for delta encoding"
        );
        starts.dedup();
        assert_eq!(starts.len(), spans.len(), "no duplicate spans");
    }
}
