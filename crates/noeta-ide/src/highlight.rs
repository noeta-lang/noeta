//! Snippet syntax highlighting for the docs browser (docs-browser-ui arc): classify a standalone
//! Noeta code snippet — a doc page's signature or a ```` ```noeta ```` fence — into colorable
//! spans using the **compiler's own lexer**, so the doc viewer's highlighting can never drift from
//! the language (no third grammar copy; the TextMate grammar covers the editor, this covers docs).
//!
//! Lexical classes plus the same light heuristics the editor grammar uses (a capitalized
//! identifier reads as a type, an identifier before `(` or after `fn` as a function, `@name` as a
//! decorator). Offsets are **UTF-16 code units** — the consumer is a webview slicing JavaScript
//! strings — converted here from the lexer's byte spans. Error-tolerant by construction: the lexer
//! always yields a (possibly partial) stream, so a non-Noeta or mid-edit snippet just highlights
//! sparsely, never fails.

use noeta_lexer::{TokenKind, lex_with_trivia};
use noeta_span::{Source, SourceId};

/// The color class of a highlighted span — the small palette a doc theme styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlClass {
    Keyword,
    String,
    Number,
    Comment,
    Type,
    Function,
    Decorator,
}

impl HlClass {
    /// The short wire/CSS tag (`tok-<tag>` in the doc viewer's stylesheet).
    pub fn as_str(self) -> &'static str {
        match self {
            HlClass::Keyword => "kw",
            HlClass::String => "str",
            HlClass::Number => "num",
            HlClass::Comment => "com",
            HlClass::Type => "ty",
            HlClass::Function => "fn",
            HlClass::Decorator => "dec",
        }
    }
}

/// One highlighted span of a snippet, in UTF-16 code units (JavaScript string offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlSpan {
    pub start: u32,
    pub end: u32,
    pub class: HlClass,
}

/// Whether an identifier is a built-in **scalar** type name — colored as a type even though the
/// lexer sees a plain identifier. Decodes through the [`noeta_ast::BuiltinTy`] funnel and matches
/// exhaustively, replacing a hand list that had drifted (`unit` was missing). The containers and
/// abstract kind-types deliberately answer `false`: their canonical spellings are colored by the
/// uppercase-initial heuristic, while the bare `list`/`map`/`set` spellings collide with the
/// like-named methods (`x.map(…)`) and would steal the function color, so they stay uncolored.
fn is_primitive_type_name(word: &str) -> bool {
    use noeta_ast::BuiltinTy::*;
    match noeta_ast::BuiltinTy::from_name_any(word) {
        // `never` colors as a type name like the other scalars. It is not a keyword — an ordinary
        // `fn never()` is still a function — but in the one position it appears (a return
        // annotation) it reads as the type it is.
        Some(
            Int
            | Float
            | F32
            | F64
            | IntN { .. }
            | Bool
            | Str
            | Bytes
            | Unit
            | Dyn
            | Never
            | Number,
        ) => true,
        Some(List | Set | Map | Option | Result | KindEnum | KindStruct | KindClass) | None => {
            false
        }
    }
}

fn is_keyword(kind: TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        EchoKw
            | MutKw
            | TrueKw
            | FalseKw
            | FnKw
            | ReturnKw
            | YieldKw
            | AsyncKw
            | AwaitKw
            | ConcurrentKw
            | SpawnKw
            | IsolateKw
            | IfKw
            | ThenKw
            | ElseKw
            | ForKw
            | WhileKw
            | BreakKw
            | ContinueKw
            | InKw
            | EnumKw
            | MatchKw
            | StructKw
            | TypeKw
            | ClassKw
            | DestructKw
            | ImplKw
            | TraitKw
            | NamespaceKw
            | UseKw
            | PubKw
            | AsKw
            | IsKw
            | AttributesOfKw
            | TypeOfKw
            | TypeNameKw
            | FieldsOfKw
            | TraitsOfKw
            | FromBytesKw
            | RolesOfKw
            | ParamsOfKw
            | ReturnsOfKw
            | FieldSpecsOfKw
            | VariantsOfKw
            | ConstructKw
            | InvokeKw
    )
}

/// Highlight one standalone snippet. Returns non-overlapping spans sorted by start, in UTF-16
/// offsets. Unclassified tokens (operators, punctuation, plain identifiers) get no span — they
/// render in the default code foreground.
pub fn highlight_code(text: &str) -> Vec<HlSpan> {
    let source = Source::new(SourceId::FIRST, "snippet.noe", text);
    let lexed = lex_with_trivia(&source);
    let toks = &lexed.tokens;

    let mut spans: Vec<(u32, u32, HlClass)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| toks[j].kind);
        let next = toks.get(i + 1).map(|t| t.kind);
        let class = if is_keyword(t.kind) {
            Some(HlClass::Keyword)
        } else {
            use TokenKind::*;
            match t.kind {
                StringLit | RawStr | TemplateStr => Some(HlClass::String),
                FloatLit | F32Lit | F64Lit | IntNLit | IntLit => Some(HlClass::Number),
                // A `@doc { … }` body inside a snippet is prose — muted like a comment.
                DocText => Some(HlClass::Comment),
                // `@name` — the tier/decorator sigil and its name color together.
                At => Some(HlClass::Decorator),
                Ident => {
                    let word = text
                        .get(t.span.start as usize..t.span.end as usize)
                        .unwrap_or("");
                    if prev == Some(At) {
                        Some(HlClass::Decorator)
                    } else if word == "self" {
                        Some(HlClass::Keyword)
                    } else if is_primitive_type_name(word)
                        || word.chars().next().is_some_and(|c| c.is_uppercase())
                    {
                        Some(HlClass::Type)
                    } else if prev == Some(FnKw) || next == Some(LParen) {
                        Some(HlClass::Function)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        if let Some(class) = class {
            spans.push((t.span.start, t.span.end, class));
        }
    }
    // Comments are trivia — not in the token stream — with spans covering their delimiters.
    for c in &lexed.comments {
        spans.push((c.span.start, c.span.end, HlClass::Comment));
    }
    spans.sort_by_key(|s| s.0);

    to_utf16(text, spans)
}

/// Convert byte-offset spans to UTF-16 code-unit spans by one walk over the text. Span boundaries
/// from the lexer are always char-aligned; positions inside a multi-byte char are filled with the
/// preceding boundary's value defensively.
fn to_utf16(text: &str, spans: Vec<(u32, u32, HlClass)>) -> Vec<HlSpan> {
    let mut u16_at_byte = vec![0u32; text.len() + 1];
    let mut acc = 0u32;
    for (b, ch) in text.char_indices() {
        for slot in u16_at_byte.iter_mut().skip(b).take(ch.len_utf8()) {
            *slot = acc;
        }
        acc += ch.len_utf16() as u32;
    }
    u16_at_byte[text.len()] = acc;
    spans
        .into_iter()
        .filter(|(s, e, _)| (*s as usize) < u16_at_byte.len() && (*e as usize) < u16_at_byte.len())
        .map(|(s, e, class)| HlSpan {
            start: u16_at_byte[s as usize],
            end: u16_at_byte[e as usize],
            class,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(text: &str) -> Vec<(String, HlClass)> {
        // Spans back to (slice, class) via UTF-16 offsets over a JS-like unit walk — for ASCII
        // tests the offsets are byte offsets too.
        highlight_code(text)
            .into_iter()
            .map(|s| (text[s.start as usize..s.end as usize].to_string(), s.class))
            .collect()
    }

    #[test]
    fn keywords_functions_types_numbers_strings() {
        let got = classes("fn add(a: int, b: Point): int { return a + 1 }");
        assert!(got.contains(&("fn".into(), HlClass::Keyword)));
        assert!(got.contains(&("add".into(), HlClass::Function)));
        assert!(got.contains(&("int".into(), HlClass::Type)));
        assert!(got.contains(&("Point".into(), HlClass::Type)));
        assert!(got.contains(&("return".into(), HlClass::Keyword)));
        assert!(got.contains(&("1".into(), HlClass::Number)));
    }

    #[test]
    fn strings_comments_decorators_and_calls() {
        let got = classes("// a note\n@test fn t() { echo \"hi\" }\nmath.sqrt(2.0)");
        assert!(got.contains(&("// a note".into(), HlClass::Comment)));
        assert!(got.contains(&("@".into(), HlClass::Decorator)));
        assert!(got.contains(&("test".into(), HlClass::Decorator)));
        assert!(got.contains(&("\"hi\"".into(), HlClass::String)));
        assert!(got.contains(&("sqrt".into(), HlClass::Function)));
        assert!(got.contains(&("2.0".into(), HlClass::Number)));
        assert!(got.contains(&("echo".into(), HlClass::Keyword)));
    }

    #[test]
    fn offsets_are_utf16_code_units() {
        // "π" is 2 bytes but 1 UTF-16 unit: the keyword after it must use UTF-16 offsets.
        let text = "x = \"π\"\nreturn x";
        let spans = highlight_code(text);
        let ret = spans
            .iter()
            .find(|s| s.class == HlClass::Keyword)
            .expect("return classified");
        // In UTF-16 units: x(0) space(1) =(2) space(3) "(4) π(5) "(6) \n(7) → return at 8..14.
        assert_eq!((ret.start, ret.end), (8, 14));
    }

    #[test]
    fn non_noeta_text_degrades_gracefully() {
        // A TOML-ish snippet just highlights sparsely (strings), never panics.
        let got = classes("[dependencies]\ngeom = { path = \"../geom\" }");
        assert!(got.contains(&("\"../geom\"".into(), HlClass::String)));
    }

    #[test]
    fn spans_are_sorted_and_non_overlapping() {
        let spans = highlight_code("fn f(a: int): int { return a }");
        for w in spans.windows(2) {
            assert!(w[0].end <= w[1].start, "spans overlap or unsorted");
        }
    }
}
