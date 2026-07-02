//! The lexer: source text → a flat token stream, plus any lexing diagnostics.
//!
//! Token *kinds* are defined declaratively with `logos`; this crate wraps that into
//! spanned [`Token`]s and surfaces lex errors as typed [`Diagnostic`]s through the
//! central catalog. The parser consumes [`Lexed`]; it never re-lexes.
//!
//! M0 scope grows one vertical slice at a time.

use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::{Source, Span};
use logos::Logos;

/// The lexical category of a token. Declarative `logos` definitions keep the lexer
/// fast and the token set legible. `logos` resolves overlaps by longest match (so `==`
/// beats `=`) and gives literal `#[token]`s priority over regexes (so `mut` is a
/// keyword, not an identifier). Whitespace and line comments are skipped.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
// Line comments (the conformance `// expect:` headers are these). `allow_greedy`
// acknowledges the `[^\n]*` tail; it stops at the newline, so it is bounded per line.
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
pub enum TokenKind {
    // Keywords
    #[token("echo")]
    EchoKw,
    #[token("mut")]
    MutKw,
    #[token("true")]
    TrueKw,
    #[token("false")]
    FalseKw,
    #[token("fn")]
    FnKw,
    #[token("return")]
    ReturnKw,
    /// `yield expr` — produce the next element of a generator (Track G). A function whose body
    /// contains `yield` is a generator returning an `Iterator<T>`.
    #[token("yield")]
    YieldKw,
    /// `async fn f(): T` — declares an asynchronous function (Track A). Calling it produces a
    /// `Future<T>` rather than running the body; the completion value has the declared inner type.
    #[token("async")]
    AsyncKw,
    /// The postfix suspend operator `expr.await` (Track A). Chains with `?` and further calls
    /// (`fetch(url).await?.text().await?`). A keyword so `.await` parses unambiguously as a postfix
    /// suspend rather than a field access.
    #[token("await")]
    AwaitKw,
    /// `concurrent { … }` — a structured-concurrency scope (Track A.3). Tasks spawned inside it are
    /// joined at the closing brace; nothing outlives the block.
    #[token("concurrent")]
    ConcurrentKw,
    /// `spawn e` — schedule the future `e` as a task in the enclosing `concurrent` scope (Track A.3),
    /// yielding a handle (`Future<T>`) whose `.await` produces the task's result.
    #[token("spawn")]
    SpawnKw,
    /// `isolate f(args)` — run the call in a fresh isolate (own heap, real parallelism), yielding a
    /// handle (`Future<T>`) like `spawn` but constrained to `Send` args/result (isolates milestone).
    #[token("isolate")]
    IsolateKw,
    #[token("if")]
    IfKw,
    /// The conditional-expression keyword: `if cond then a else b`. Forks the grammar from the
    /// statement `if cond { … }` (which uses a brace), so `if` is both a statement and an
    /// expression without lookahead.
    #[token("then")]
    ThenKw,
    #[token("else")]
    ElseKw,
    #[token("for")]
    ForKw,
    #[token("while")]
    WhileKw,
    #[token("break")]
    BreakKw,
    #[token("continue")]
    ContinueKw,
    #[token("in")]
    InKw,
    #[token("enum")]
    EnumKw,
    #[token("match")]
    MatchKw,
    /// Reserved: the value-kind declaration keyword `struct Name { ... }`. Replaces the retired
    /// `type X = { ... }` struct form.
    #[token("struct")]
    StructKw,
    /// Reserved for a future type-alias feature; no grammar consumes it today (the `type X = { ... }`
    /// struct form was retired in favour of `struct`).
    #[token("type")]
    TypeKw,
    #[token("class")]
    ClassKw,
    #[token("destruct")]
    DestructKw,
    #[token("impl")]
    ImplKw,
    #[token("namespace")]
    NamespaceKw,
    #[token("use")]
    UseKw,
    #[token("pub")]
    PubKw,
    /// The checked-narrowing keyword: `x.as<T>()` narrows a `dyn` value to `?T`. A keyword
    /// (not an identifier) so `.as<T>()` is unambiguous against member access + comparison.
    #[token("as")]
    AsKw,
    /// The type-test keyword: `x is T` is a `bool` ("is the runtime value a `T`?"). A keyword so
    /// it parses unambiguously as an operator rather than an identifier.
    #[token("is")]
    IsKw,
    /// The reflection keyword `attributes_of::<T>()` — a manifest query returning the materialized
    /// `#[T(...)]` attributes (each paired with its target). A keyword so the type-argument form
    /// parses unambiguously rather than as an identifier followed by comparisons.
    #[token("attributes_of")]
    AttributesOfKw,
    /// The reflection keyword `type_of(value)` — the runtime [`Type`] descriptor of a value. A
    /// keyword (rather than a plain builtin call) for symmetry with `attributes_of` and so the
    /// reflection surface is lexically distinct.
    #[token("type_of")]
    TypeOfKw,
    /// `from_bytes::<T>(blob)` — deserialize a `bytes` buffer back into a `List<T>` (P-PACK 4.4). A
    /// keyword so the turbofish type argument parses unambiguously; generic over any declared
    /// `@packed` struct (no hardcoded type list — extension-friendly).
    #[token("from_bytes")]
    FromBytesKw,
    /// The reflection keyword `roles_of()` — the compiler-built `(declaration, Role)` index, returned
    /// as `List<RoleBinding>`. A keyword for symmetry with `attributes_of`/`type_of`.
    #[token("roles_of")]
    RolesOfKw,
    /// The reflection keyword `invoke(recv, name, args)` — the fallible by-name invocation
    /// primitive: dispatch a method (on a value) or an associated function (on a type) by a
    /// runtime string name, returning `Result`. A keyword for symmetry with the other reflection
    /// surfaces and so the one surviving runtime-dispatch site is lexically visible.
    #[token("invoke")]
    InvokeKw,
    /// `channel::<T>(capacity)` — construct a bounded, typed channel, yielding the split-endpoint
    /// pair `(Sender<T>, Receiver<T>)` (isolates milestone I.1). A keyword so the turbofish type
    /// argument parses unambiguously, mirroring `from_bytes`/`attributes_of`.
    #[token("channel")]
    ChannelKw,

    // Literals and names
    /// A double-quoted string literal, quotes included. A backslash escapes the next
    /// character (so `\"` does not close the string); the parser unescapes the contents and
    /// processes `${...}` interpolation.
    #[regex(r#""([^"\\]|\\.)*""#)]
    StringLit,
    /// A single-quoted *raw* string literal, quotes included. No interpolation; the only escapes
    /// are `\'` (a literal quote) and `\\` (a literal backslash) — every other character,
    /// including `{`, `$`, and `\n`, is literal. Ideal for regex, paths, and JSON blobs.
    #[regex(r#"'([^'\\]|\\.)*'"#)]
    RawStr,
    /// A backtick *template* string, quotes included. Multiline with `${...}` interpolation (like
    /// a double-quoted string), but the common leading indentation and the leading/trailing blank
    /// line are stripped — a dedented text block for SQL/HTML/email templates.
    #[regex(r#"`([^`\\]|\\.)*`"#)]
    TemplateStr,
    /// A float literal: a decimal with a fractional part and/or a scientific exponent. `_` digit
    /// separators are allowed and stripped by the parser. Examples: `4.2`, `1_000.5`, `1.5e-3`,
    /// `2e10`. (A bare `42` with no `.`/`e` is an [`TokenKind::IntLit`].)
    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?")]
    #[regex(r"[0-9][0-9_]*[eE][+-]?[0-9][0-9_]*")]
    FloatLit,
    /// A 32-bit float literal: a numeric literal with the `f32` suffix (P-PACK Phase 3). Examples:
    /// `1.0f32`, `2.5e3f32`, `5f32` (an integer with the suffix is an `f32`, not an `int`). Maximal
    /// munch picks this over `FloatLit`/`IntLit` + an `f32` identifier, while a bare `f32` (no leading
    /// digits) stays an identifier — the type name.
    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?f32")]
    #[regex(r"[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?f32")]
    F32Lit,
    /// An integer literal: decimal, or `0x`/`0o`/`0b` radix-prefixed, with optional `_` digit
    /// separators (stripped by the parser). Examples: `42`, `1_000_000`, `0xDE_AD`, `0o755`, `0b1010`.
    #[regex(r"[0-9][0-9_]*")]
    #[regex(r"0[xX][0-9A-Fa-f][0-9A-Fa-f_]*")]
    #[regex(r"0[oO][0-7][0-7_]*")]
    #[regex(r"0[bB][01][01_]*")]
    IntLit,
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,

    // Punctuation and operators
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    // `...` (spread) / `..=` (inclusive range) / `..` (exclusive range) / `.` overlap;
    // logos resolves by longest match.
    #[token("...")]
    DotDotDot,
    #[token("..=")]
    DotDotEq,
    #[token("..")]
    DotDot,
    #[token(".")]
    Dot,
    // `::` must precede `:`; logos resolves the overlap by longest match.
    #[token("::")]
    ColonColon,
    #[token(":")]
    Colon,
    // `??=` / `??` must precede `?`; logos resolves the overlap by longest match.
    #[token("??=")]
    QuestionQuestionEq,
    #[token("??")]
    QuestionQuestion,
    #[token("?")]
    Question,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("=>")]
    FatArrow,
    /// The thin arrow `->`, used in function types (`(int) -> int`). Maximal-munch beats `-`/`-=`.
    #[token("->")]
    Arrow,
    #[token("|>")]
    PipeGt,
    #[token("===")]
    EqEqEq,
    #[token("!==")]
    NotEqEq,
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("+=")]
    PlusEq,
    #[token("-=")]
    MinusEq,
    #[token("*=")]
    StarEq,
    #[token("/=")]
    SlashEq,
    #[token("%=")]
    PercentEq,
    #[token("~=")]
    TildeEq,
    #[token("&&")]
    AmpAmp,
    #[token("||")]
    PipePipe,
    /// A bare `|` — the union-type separator (`int | string`) **and** bitwise-OR in expression
    /// position (P-BITS Tier B). logos longest-match keeps `||` (`PipePipe`) and `|>` (`PipeGt`)
    /// intact; the type and expression grammars are disjoint, so one token serves both roles.
    #[token("|")]
    Pipe,
    /// A bare `&` — bitwise-AND on `int` (P-BITS Tier B). logos longest-match keeps `&&` (`AmpAmp`)
    /// intact, so a single `&` is unambiguous.
    #[token("&")]
    Amp,
    /// `^` — bitwise-XOR on `int` (P-BITS Tier B).
    #[token("^")]
    Caret,
    /// `<<` — left shift on `int` (P-BITS Tier B). logos longest-match takes `<<` over two `<`
    /// (`Lt`); `<<` never opens a generic (nested generics open `Foo<Bar<`, never adjacent), so this
    /// is safe. The right shift `>>` is *not* a token — it is composed from two adjacent `Gt` in the
    /// expression parser so nested generic closes (`List<Map<K, V>>`) keep lexing as two `Gt`.
    #[token("<<")]
    Shl,
    #[token("=")]
    Eq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("~")]
    Tilde,
    #[token("!")]
    Bang,
    /// `#`, introducing a data attribute (`#[Route("/users")]`).
    #[token("#")]
    Hash,
    /// `@`, introducing a codegen directive (`@derive(...)`).
    #[token("@")]
    At,
    /// The **verbatim body** of a `@doc { … }` text tier (object-model slice 6f): the raw source
    /// between the braces, captured without tokenizing it (so arbitrary prose/markdown never
    /// produces lex errors). Never produced by `logos` — it is synthesized by [`lex`] when it sees a
    /// `@doc {`; the literal pattern here is an unmatchable sentinel (NUL bytes never occur in
    /// source) that only exists to give the variant a `logos` rule.
    #[token("\0\0__doctext__\0\0")]
    DocText,
}

impl TokenKind {
    /// A short, stable symbolic name used in token-stream snapshots.
    pub fn label(self) -> &'static str {
        match self {
            TokenKind::EchoKw => "EchoKw",
            TokenKind::MutKw => "MutKw",
            TokenKind::TrueKw => "TrueKw",
            TokenKind::FalseKw => "FalseKw",
            TokenKind::FnKw => "FnKw",
            TokenKind::ReturnKw => "ReturnKw",
            TokenKind::YieldKw => "YieldKw",
            TokenKind::AsyncKw => "AsyncKw",
            TokenKind::AwaitKw => "AwaitKw",
            TokenKind::ConcurrentKw => "ConcurrentKw",
            TokenKind::SpawnKw => "SpawnKw",
            TokenKind::IsolateKw => "IsolateKw",
            TokenKind::IfKw => "IfKw",
            TokenKind::ThenKw => "ThenKw",
            TokenKind::ElseKw => "ElseKw",
            TokenKind::ForKw => "ForKw",
            TokenKind::WhileKw => "WhileKw",
            TokenKind::BreakKw => "BreakKw",
            TokenKind::ContinueKw => "ContinueKw",
            TokenKind::InKw => "InKw",
            TokenKind::EnumKw => "EnumKw",
            TokenKind::MatchKw => "MatchKw",
            TokenKind::StructKw => "StructKw",
            TokenKind::TypeKw => "TypeKw",
            TokenKind::ClassKw => "ClassKw",
            TokenKind::DestructKw => "DestructKw",
            TokenKind::ImplKw => "ImplKw",
            TokenKind::NamespaceKw => "NamespaceKw",
            TokenKind::UseKw => "UseKw",
            TokenKind::PubKw => "PubKw",
            TokenKind::AsKw => "AsKw",
            TokenKind::IsKw => "IsKw",
            TokenKind::AttributesOfKw => "AttributesOfKw",
            TokenKind::TypeOfKw => "TypeOfKw",
            TokenKind::FromBytesKw => "FromBytesKw",
            TokenKind::RolesOfKw => "RolesOfKw",
            TokenKind::InvokeKw => "InvokeKw",
            TokenKind::ChannelKw => "ChannelKw",
            TokenKind::ColonColon => "ColonColon",
            TokenKind::StringLit => "StringLit",
            TokenKind::RawStr => "RawStr",
            TokenKind::TemplateStr => "TemplateStr",
            TokenKind::FloatLit => "FloatLit",
            TokenKind::F32Lit => "F32Lit",
            TokenKind::IntLit => "IntLit",
            TokenKind::Ident => "Ident",
            TokenKind::Semicolon => "Semicolon",
            TokenKind::Comma => "Comma",
            TokenKind::DotDotDot => "DotDotDot",
            TokenKind::DotDotEq => "DotDotEq",
            TokenKind::DotDot => "DotDot",
            TokenKind::Dot => "Dot",
            TokenKind::Colon => "Colon",
            TokenKind::QuestionQuestionEq => "QuestionQuestionEq",
            TokenKind::QuestionQuestion => "QuestionQuestion",
            TokenKind::Question => "Question",
            TokenKind::LParen => "LParen",
            TokenKind::RParen => "RParen",
            TokenKind::LBrace => "LBrace",
            TokenKind::RBrace => "RBrace",
            TokenKind::LBracket => "LBracket",
            TokenKind::RBracket => "RBracket",
            TokenKind::FatArrow => "FatArrow",
            TokenKind::Arrow => "Arrow",
            TokenKind::PipeGt => "PipeGt",
            TokenKind::EqEqEq => "EqEqEq",
            TokenKind::NotEqEq => "NotEqEq",
            TokenKind::EqEq => "EqEq",
            TokenKind::NotEq => "NotEq",
            TokenKind::LtEq => "LtEq",
            TokenKind::GtEq => "GtEq",
            TokenKind::PlusEq => "PlusEq",
            TokenKind::MinusEq => "MinusEq",
            TokenKind::StarEq => "StarEq",
            TokenKind::SlashEq => "SlashEq",
            TokenKind::PercentEq => "PercentEq",
            TokenKind::TildeEq => "TildeEq",
            TokenKind::AmpAmp => "AmpAmp",
            TokenKind::PipePipe => "PipePipe",
            TokenKind::Pipe => "Pipe",
            TokenKind::Amp => "Amp",
            TokenKind::Caret => "Caret",
            TokenKind::Shl => "Shl",
            TokenKind::Eq => "Eq",
            TokenKind::Lt => "Lt",
            TokenKind::Gt => "Gt",
            TokenKind::Plus => "Plus",
            TokenKind::Minus => "Minus",
            TokenKind::Star => "Star",
            TokenKind::Slash => "Slash",
            TokenKind::Percent => "Percent",
            TokenKind::Tilde => "Tilde",
            TokenKind::Bang => "Bang",
            TokenKind::Hash => "Hash",
            TokenKind::At => "At",
            TokenKind::DocText => "DocText",
        }
    }

    /// A human-facing form used in diagnostics ("expected `;`, found ...").
    pub fn describe(self) -> &'static str {
        match self {
            TokenKind::EchoKw => "`echo`",
            TokenKind::MutKw => "`mut`",
            TokenKind::TrueKw => "`true`",
            TokenKind::FalseKw => "`false`",
            TokenKind::FnKw => "`fn`",
            TokenKind::ReturnKw => "`return`",
            TokenKind::YieldKw => "`yield`",
            TokenKind::AsyncKw => "`async`",
            TokenKind::AwaitKw => "`await`",
            TokenKind::ConcurrentKw => "`concurrent`",
            TokenKind::SpawnKw => "`spawn`",
            TokenKind::IsolateKw => "`isolate`",
            TokenKind::IfKw => "`if`",
            TokenKind::ThenKw => "`then`",
            TokenKind::ElseKw => "`else`",
            TokenKind::ForKw => "`for`",
            TokenKind::WhileKw => "`while`",
            TokenKind::BreakKw => "`break`",
            TokenKind::ContinueKw => "`continue`",
            TokenKind::InKw => "`in`",
            TokenKind::EnumKw => "`enum`",
            TokenKind::MatchKw => "`match`",
            TokenKind::StructKw => "`struct`",
            TokenKind::TypeKw => "`type`",
            TokenKind::ClassKw => "`class`",
            TokenKind::DestructKw => "`destruct`",
            TokenKind::ImplKw => "`impl`",
            TokenKind::NamespaceKw => "`namespace`",
            TokenKind::UseKw => "`use`",
            TokenKind::PubKw => "`pub`",
            TokenKind::AsKw => "`as`",
            TokenKind::IsKw => "`is`",
            TokenKind::AttributesOfKw => "`attributes_of`",
            TokenKind::TypeOfKw => "`type_of`",
            TokenKind::FromBytesKw => "`from_bytes`",
            TokenKind::RolesOfKw => "`roles_of`",
            TokenKind::InvokeKw => "`invoke`",
            TokenKind::ChannelKw => "`channel`",
            TokenKind::ColonColon => "`::`",
            TokenKind::StringLit => "a string literal",
            TokenKind::RawStr => "a raw string literal",
            TokenKind::TemplateStr => "a template string literal",
            TokenKind::FloatLit => "a float literal",
            TokenKind::F32Lit => "an f32 literal",
            TokenKind::IntLit => "an integer literal",
            TokenKind::Ident => "an identifier",
            TokenKind::Semicolon => "`;`",
            TokenKind::Comma => "`,`",
            TokenKind::DotDotDot => "`...`",
            TokenKind::DotDotEq => "`..=`",
            TokenKind::DotDot => "`..`",
            TokenKind::Dot => "`.`",
            TokenKind::Colon => "`:`",
            TokenKind::QuestionQuestionEq => "`??=`",
            TokenKind::QuestionQuestion => "`??`",
            TokenKind::Question => "`?`",
            TokenKind::LParen => "`(`",
            TokenKind::RParen => "`)`",
            TokenKind::LBrace => "`{`",
            TokenKind::RBrace => "`}`",
            TokenKind::LBracket => "`[`",
            TokenKind::RBracket => "`]`",
            TokenKind::FatArrow => "`=>`",
            TokenKind::Arrow => "`->`",
            TokenKind::PipeGt => "`|>`",
            TokenKind::EqEqEq => "`===`",
            TokenKind::NotEqEq => "`!==`",
            TokenKind::EqEq => "`==`",
            TokenKind::NotEq => "`!=`",
            TokenKind::LtEq => "`<=`",
            TokenKind::GtEq => "`>=`",
            TokenKind::PlusEq => "`+=`",
            TokenKind::MinusEq => "`-=`",
            TokenKind::StarEq => "`*=`",
            TokenKind::SlashEq => "`/=`",
            TokenKind::PercentEq => "`%=`",
            TokenKind::TildeEq => "`~=`",
            TokenKind::AmpAmp => "`&&`",
            TokenKind::PipePipe => "`||`",
            TokenKind::Pipe => "`|`",
            TokenKind::Amp => "`&`",
            TokenKind::Caret => "`^`",
            TokenKind::Shl => "`<<`",
            TokenKind::Eq => "`=`",
            TokenKind::Lt => "`<`",
            TokenKind::Gt => "`>`",
            TokenKind::Plus => "`+`",
            TokenKind::Minus => "`-`",
            TokenKind::Star => "`*`",
            TokenKind::Slash => "`/`",
            TokenKind::Percent => "`%`",
            TokenKind::Tilde => "`~`",
            TokenKind::Bang => "`!`",
            TokenKind::Hash => "`#`",
            TokenKind::At => "`@`",
            TokenKind::DocText => "a `@doc` text body",
        }
    }
}

/// A token: its kind and where it sits in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// The result of lexing: the token stream and any diagnostics produced along the way.
/// Lexing is error-tolerant — it always returns a (possibly partial) stream.
#[derive(Debug, Clone, Default)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lex a source file into a token stream.
pub fn lex(source: &Source) -> Lexed {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();
    let text = source.text();
    let mut lexer = TokenKind::lexer(text);

    while let Some(result) = lexer.next() {
        let span = Span::from(lexer.span());
        match result {
            Ok(TokenKind::LBrace) if opens_doc_block(text, &tokens) => {
                // A `@doc {` (object-model slice 6f): capture the brace-delimited body **verbatim**
                // as one `DocText` token instead of tokenizing it, so arbitrary prose/markdown never
                // produces lex errors. Emit `{`, the raw body, and `}`, then advance the lexer past
                // the whole span. The body's braces must balance (the matching `}` closes the block).
                let open_end = span.end;
                match matching_brace(text, open_end) {
                    Some(close_start) => {
                        tokens.push(Token {
                            kind: TokenKind::LBrace,
                            span,
                        });
                        tokens.push(Token {
                            kind: TokenKind::DocText,
                            span: Span::new(open_end, close_start),
                        });
                        tokens.push(Token {
                            kind: TokenKind::RBrace,
                            span: Span::new(close_start, close_start + 1),
                        });
                        // The lexer cursor sits just after `{` (`open_end`); skip to just after the
                        // matching `}` so tokenizing resumes there.
                        lexer.bump((close_start + 1 - open_end) as usize);
                    }
                    None => {
                        diagnostics.push(unterminated_doc_block(span));
                        tokens.push(Token {
                            kind: TokenKind::LBrace,
                            span,
                        });
                    }
                }
            }
            Ok(kind) => tokens.push(Token { kind, span }),
            Err(()) => diagnostics.push(lex_error(source, span)),
        }
    }

    let tokens = insert_terminators(source, tokens);

    Lexed {
        tokens,
        diagnostics,
    }
}

/// Whether the just-lexed `{` opens a `@doc { … }` text-tier body — i.e. the two preceding tokens
/// are `@` then the identifier `doc`. (Only `@doc` is a text tier; every other `@<tier> {` is a
/// code block tokenized normally.)
fn opens_doc_block(text: &str, tokens: &[Token]) -> bool {
    let [.., at, name] = tokens else { return false };
    at.kind == TokenKind::At
        && name.kind == TokenKind::Ident
        && text.get(name.span.start as usize..name.span.end as usize) == Some("doc")
}

/// Find the `}` that closes a brace block whose opening `{` ends at `open_end`, returning its byte
/// offset. Braces nest (a `{` in the body must be matched by a `}` before the block closes), so a
/// balanced code snippet inside doc prose is fine; an unbalanced `}` is treated as the closer and an
/// unbalanced `{` runs to end-of-input (`None` — an unterminated block).
fn matching_brace(text: &str, open_end: u32) -> Option<u32> {
    let mut depth: u32 = 1;
    for (offset, byte) in text.as_bytes().iter().enumerate().skip(open_end as usize) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset as u32);
                }
            }
            _ => {}
        }
    }
    None
}

/// The diagnostic for a `@doc {` whose body never closes (no matching `}` before end-of-input).
fn unterminated_doc_block(open: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnexpectedEndOfInput,
        open,
        "unterminated `@doc` block",
    )
    .with_help("add a closing `}` to end the doc block; braces inside the body must balance")
}

/// Insert synthetic statement terminators (`;`) at newlines (object-model slice 7): a line end can
/// terminate a statement, so most `;` become optional. logos discards newlines, so the gap between
/// two consecutive tokens is inspected in the *source text* for a newline — no token-set change.
///
/// A `;` is inserted in a gap iff **all** hold: the gap contains a newline; the bracket nesting of
/// `(`/`[` is zero (inside a call/list/index a newline never terminates); the preceding token can
/// *end* a statement ([`is_statement_ending`]); and the following token does not *continue* the line
/// ([`is_leading_continuation`], so a leading `.`/`|>`/operator/`else`/closing brace joins the
/// lines). At most one `;` per gap, so blank lines never yield empty statements. `{`/`}` are not
/// depth-tracked: a `}` is a leading-continuation (suppressing a `;` before it), so multi-line
/// blocks and `{...}` literals are unaffected and the parser's terminator tolerates a peeked `}`.
fn insert_terminators(source: &Source, tokens: Vec<Token>) -> Vec<Token> {
    let text = source.text();
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut depth: u32 = 0;
    for tok in tokens {
        if let Some(prev) = out.last()
            && depth == 0
            && is_statement_ending(prev.kind)
            && !is_leading_continuation(tok.kind)
            && gap_has_newline(text, prev.span.end, tok.span.start)
        {
            let at = prev.span.end;
            out.push(Token {
                kind: TokenKind::Semicolon,
                span: Span::empty_at_in(prev.span.source, at),
            });
        }
        match tok.kind {
            TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
            _ => {}
        }
        out.push(tok);
    }
    out
}

/// Whether the source between two byte offsets contains a newline (the gap between two tokens —
/// whitespace and/or a line comment logos skipped).
fn gap_has_newline(text: &str, start: u32, end: u32) -> bool {
    text.get(start as usize..end as usize)
        .is_some_and(|gap| gap.contains('\n'))
}

/// Whether a token can be the **last** token of a statement, so a following newline terminates it:
/// a value (literal/identifier/`true`/`false`), a closing bracket, the postfix try `?`, or one of
/// the self-contained jump keywords (`return`/`break`/`continue`).
fn is_statement_ending(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident
            | TokenKind::IntLit
            | TokenKind::FloatLit
            | TokenKind::F32Lit
            | TokenKind::StringLit
            | TokenKind::RawStr
            | TokenKind::TemplateStr
            | TokenKind::TrueKw
            | TokenKind::FalseKw
            | TokenKind::RParen
            | TokenKind::RBrace
            | TokenKind::RBracket
            | TokenKind::Question
            | TokenKind::AwaitKw
            | TokenKind::ReturnKw
            | TokenKind::BreakKw
            | TokenKind::ContinueKw
    )
}

/// Whether a token, when it **starts** the next line, continues the previous statement (so no `;` is
/// inserted before it): an infix/postfix operator, member/pipeline/range/coalesce punctuation, a
/// separator, a closing bracket, or a clause keyword that extends the construct (`else`/`then`/`in`).
fn is_leading_continuation(kind: TokenKind) -> bool {
    matches!(
        kind,
        // Arithmetic / comparison / logical / union operators.
        TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::EqEq
            | TokenKind::NotEq
            | TokenKind::EqEqEq
            | TokenKind::NotEqEq
            | TokenKind::LtEq
            | TokenKind::GtEq
            | TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::AmpAmp
            | TokenKind::PipePipe
            | TokenKind::Pipe
            // Member / pipeline / coalesce / range.
            | TokenKind::Dot
            | TokenKind::PipeGt
            | TokenKind::QuestionQuestion
            | TokenKind::DotDot
            | TokenKind::DotDotEq
            // Separators / qualifiers / arrows.
            | TokenKind::Comma
            | TokenKind::Colon
            | TokenKind::ColonColon
            | TokenKind::FatArrow
            | TokenKind::Arrow
            // A closing bracket finishing a multi-line construct.
            | TokenKind::RParen
            | TokenKind::RBrace
            | TokenKind::RBracket
            // Postfix-ish / clause keywords that extend the preceding construct.
            | TokenKind::AsKw
            | TokenKind::IsKw
            | TokenKind::ElseKw
            | TokenKind::ThenKw
            | TokenKind::InKw
    )
}

/// Classify an un-tokenizable slice into the most specific diagnostic available.
fn lex_error(source: &Source, span: Span) -> Diagnostic {
    let slice = source.slice(span);
    if slice.starts_with('"') {
        Diagnostic::error(
            DiagnosticCode::UnterminatedString,
            span,
            "unterminated string literal",
        )
        .with_help("add a closing `\"` to terminate the string")
    } else {
        Diagnostic::error(
            DiagnosticCode::UnexpectedCharacter,
            span,
            format!("unexpected character `{slice}`"),
        )
    }
}

/// Render a token stream to a stable textual form for snapshot tests.
pub fn dump_tokens(source: &Source, lexed: &Lexed) -> String {
    let mut out = String::new();
    for token in &lexed.tokens {
        out.push_str(&format!(
            "{} @{}..{} {:?}\n",
            token.kind.label(),
            token.span.start,
            token.span.end,
            source.slice(token.span),
        ));
    }
    for diag in &lexed.diagnostics {
        out.push_str(&format!(
            "DIAGNOSTIC {} @{}..{}\n",
            diag.code, diag.span.start, diag.span.end
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_span::SourceId;

    fn lex_str(text: &str) -> (Source, Lexed) {
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let lexed = lex(&source);
        (source, lexed)
    }

    #[test]
    fn lexes_echo_string() {
        let (source, lexed) = lex_str("echo \"hello\";");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::EchoKw,
                TokenKind::StringLit,
                TokenKind::Semicolon
            ]
        );
        // The string token spans the quotes.
        assert_eq!(source.slice(lexed.tokens[1].span), "\"hello\"");
    }

    #[test]
    fn string_literal_spans_escaped_quotes() {
        // An escaped quote does not terminate the string; the whole literal is one token.
        let (source, lexed) = lex_str(r#"echo "say \"hi\"";"#);
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::EchoKw,
                TokenKind::StringLit,
                TokenKind::Semicolon
            ]
        );
        assert_eq!(source.slice(lexed.tokens[1].span), r#""say \"hi\"""#);
    }

    #[test]
    fn reports_unterminated_string() {
        let (_source, lexed) = lex_str("echo \"oops;");
        assert_eq!(lexed.diagnostics.len(), 1);
        assert_eq!(
            lexed.diagnostics[0].code,
            DiagnosticCode::UnterminatedString
        );
    }

    #[test]
    fn reports_unexpected_character() {
        let (_source, lexed) = lex_str("echo `;");
        assert!(
            lexed
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::UnexpectedCharacter)
        );
    }

    #[test]
    fn lexes_numeric_literal_forms() {
        let (_source, lexed) = lex_str("1_000 0xFF 0o755 0b1010 4.2 1.5e3 2e-2");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::IntLit,   // 1_000 — decimal with separator
                TokenKind::IntLit,   // 0xFF  — hex
                TokenKind::IntLit,   // 0o755 — octal
                TokenKind::IntLit,   // 0b1010 — binary
                TokenKind::FloatLit, // 4.2
                TokenKind::FloatLit, // 1.5e3 — scientific
                TokenKind::FloatLit, // 2e-2  — scientific, no fractional part
            ]
        );
    }

    #[test]
    fn skips_line_comments() {
        let (_source, lexed) = lex_str("// a comment\necho \"x\"; // trailing\n");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::EchoKw,
                TokenKind::StringLit,
                TokenKind::Semicolon
            ]
        );
    }

    #[test]
    fn lexes_record_class_and_spread_tokens() {
        let (_source, lexed) = lex_str("type class ...a");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        // `...a` is `DotDotDot` (spread) then `Ident` — longest match keeps `...` whole.
        assert_eq!(
            kinds,
            vec![
                TokenKind::TypeKw,
                TokenKind::ClassKw,
                TokenKind::DotDotDot,
                TokenKind::Ident
            ]
        );
    }

    #[test]
    fn lexes_question_and_double_question() {
        let (_source, lexed) = lex_str("a? ?? b");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        // `??` stays one token (longest match), not two `?`.
        assert_eq!(
            kinds,
            vec![
                TokenKind::Ident,
                TokenKind::Question,
                TokenKind::QuestionQuestion,
                TokenKind::Ident
            ]
        );
    }

    #[test]
    fn lexes_coalesce_assign_as_one_token() {
        let (_source, lexed) = lex_str("a ??= b ?? c");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        // `??=` is one token (longest match), distinct from `??` and `?`.
        assert_eq!(
            kinds,
            vec![
                TokenKind::Ident,
                TokenKind::QuestionQuestionEq,
                TokenKind::Ident,
                TokenKind::QuestionQuestion,
                TokenKind::Ident
            ]
        );
    }

    #[test]
    fn lexes_namespace_and_use_keywords() {
        let (_source, lexed) = lex_str("namespace App.Orders; use App.Models.User;");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::NamespaceKw,
                TokenKind::Ident,
                TokenKind::Dot,
                TokenKind::Ident,
                TokenKind::Semicolon,
                TokenKind::UseKw,
                TokenKind::Ident,
                TokenKind::Dot,
                TokenKind::Ident,
                TokenKind::Dot,
                TokenKind::Ident,
                TokenKind::Semicolon,
            ]
        );
    }

    #[test]
    fn doc_block_body_is_captured_verbatim() {
        // `@doc { … }` (object-model slice 6f): the body is captured as one `DocText` token instead
        // of being tokenized, so arbitrary prose — `#`, `*`, `"` quotes, `$`, even a balanced code
        // snippet `{ … }` — produces no lex errors. The tokens are `@ doc { <DocText> }`.
        let (source, lexed) =
            lex_str("@doc {\n  # Title with `code` and \"quotes\" {x}\n}\nx = 1\n");
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            &kinds[..5],
            &[
                TokenKind::At,
                TokenKind::Ident,
                TokenKind::LBrace,
                TokenKind::DocText,
                TokenKind::RBrace,
            ]
        );
        // The DocText token spans the raw interior between the braces, verbatim.
        assert_eq!(
            source.slice(lexed.tokens[3].span),
            "\n  # Title with `code` and \"quotes\" {x}\n"
        );
        // Lexing resumes after the closing `}`: the trailing `x = 1` tokenizes normally.
        assert!(kinds.contains(&TokenKind::Eq));
    }

    #[test]
    fn unterminated_doc_block_reports_diagnostic() {
        // A `@doc {` whose braces never balance runs to end-of-input and is reported, not silently
        // swallowed.
        let (_source, lexed) = lex_str("@doc {\n  # never closed\n");
        assert!(
            lexed
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
        );
    }

    #[test]
    fn non_doc_tier_block_is_tokenized_normally() {
        // Only `@doc {` triggers raw capture; a `@test {` body is ordinary code, tokenized as usual
        // (no `DocText`).
        let (_source, lexed) = lex_str("@test { fn t() { return 1 } }\n");
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert!(!kinds.contains(&TokenKind::DocText));
        assert!(kinds.contains(&TokenKind::FnKw));
    }

    #[test]
    fn token_dump_is_stable() {
        let (source, lexed) = lex_str("echo \"hi\";");
        insta::assert_snapshot!(dump_tokens(&source, &lexed));
    }
}
