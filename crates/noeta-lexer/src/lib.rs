//! The lexer: source text → a flat token stream, plus any lexing diagnostics.
//!
//! Token *kinds* are defined declaratively with `logos`; this crate wraps that into
//! spanned [`Token`]s and surfaces lex errors as typed [`Diagnostic`]s through the
//! central catalog. The parser consumes [`Lexed`]; it never re-lexes.
//!
//! M0 scope grows one vertical slice at a time.

use logos::Logos;
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::{Source, Span};

/// The lexical category of a token. Declarative `logos` definitions keep the lexer
/// fast and the token set legible. `logos` resolves overlaps by longest match (so `==`
/// beats `=`) and gives literal `#[token]`s priority over regexes (so `mut` is a
/// keyword, not an identifier). Whitespace is skipped; comments are lexed as tokens and
/// then dropped by [`lex`] (so the parser never sees them), but retained by
/// [`lex_with_trivia`] for the formatter.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
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
    /// A 64-bit float literal with the explicit `f64` suffix (P-NUM-SYM): `1.0f64`, `2.5e3f64`,
    /// `5f64`. `f64` is bit-identical to `float`, so this is just a `float` value whose *type* is
    /// pinned to the strict `f64` — the suffix is the expression-position escape (a bare `1.5`
    /// adapts to `f64` only where a type is expected). Same maximal-munch treatment as `f32`.
    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?f64")]
    #[regex(r"[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?f64")]
    F64Lit,
    /// A **fixed-width integer literal** (Tier W): an integer literal (decimal or `0x`/`0o`/`0b`
    /// radix, with `_` separators) carrying one of the eight width suffixes `i8 i16 i32 i64 u8 u16
    /// u32 u64` — `255u8`, `0xFFi32`, `0b1010u16`, `1_000u32`. Maximal munch picks this over
    /// `IntLit` + an identifier suffix; a bare width name (no leading digits) stays an `Ident` (the
    /// type name). Only integer bodies take the suffix (there is no `1.5u8`). The parser re-slices
    /// the span to recover the magnitude, radix, and width.
    #[regex(r"[0-9][0-9_]*(i8|i16|i32|i64|u8|u16|u32|u64)")]
    #[regex(r"0[xX][0-9A-Fa-f][0-9A-Fa-f_]*(i8|i16|i32|i64|u8|u16|u32|u64)")]
    #[regex(r"0[oO][0-7][0-7_]*(i8|i16|i32|i64|u8|u16|u32|u64)")]
    #[regex(r"0[bB][01][01_]*(i8|i16|i32|i64|u8|u16|u32|u64)")]
    IntNLit,
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
    /// The opener of a `/* … */` block comment. Block comments **nest** (a `/*` inside is matched by
    /// a `*/` before the outer one closes), so `logos` cannot skip them with a regex; [`lex`] sees
    /// this token, scans to the matching close, and drops the whole span (no token is emitted). It
    /// therefore never appears in the token stream. A single `/` is [`Slash`](TokenKind::Slash) and a
    /// `//` line comment is [`LineComment`](TokenKind::LineComment) (longest-match picks `/*`).
    #[token("/*")]
    BlockCommentOpen,
    /// A `// …` line comment, to end of line. Lexed as a token (longest-match beats `/` and `/*`),
    /// then **dropped by [`lex`]** so the parser never sees it; [`lex_with_trivia`] keeps it as a
    /// [`Comment`]. The conformance `// expect:` headers are these.
    // `allow_greedy` acknowledges the `[^\n]*` tail; it stops at the newline, so it is bounded per
    // line (the same shape the previous `#[logos(skip(...))]` line-comment rule used).
    #[regex(r"//[^\n]*", allow_greedy = true)]
    LineComment,
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
            TokenKind::F64Lit => "F64Lit",
            TokenKind::IntNLit => "IntNLit",
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
            TokenKind::BlockCommentOpen => "BlockCommentOpen",
            TokenKind::LineComment => "LineComment",
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
            TokenKind::F64Lit => "an f64 literal",
            TokenKind::IntNLit => "a fixed-width integer literal",
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
            TokenKind::DocText => "a text-tier body",
            TokenKind::BlockCommentOpen => "`/*`",
            TokenKind::LineComment => "`//`",
        }
    }
}

/// A token: its kind and where it sits in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// A comment recovered from the source. Comments are *trivia* — they carry no token and never reach
/// the parser — so they are collected only by [`lex_with_trivia`], for the formatter to reattach.
/// The span indexes the full comment (including its `//` or `/* … */` delimiters); the consumer
/// slices the source for the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comment {
    pub span: Span,
    pub kind: CommentKind,
}

/// Whether a [`Comment`] is a `// …` line comment or a `/* … */` block comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    Line,
    Block,
}

/// The result of lexing: the token stream and any diagnostics produced along the way.
/// Lexing is error-tolerant — it always returns a (possibly partial) stream.
///
/// `comments` is empty from [`lex`] (the compile path never pays for trivia) and populated only by
/// [`lex_with_trivia`].
#[derive(Debug, Clone, Default)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
    pub comments: Vec<Comment>,
    /// The text-tier names this file itself declares (`@tier(<name>, …, text: "…")`), scanned
    /// from the token stream. Already applied to this result (the two-pass self-use in
    /// [`lex_in`]); surfaced so multi-file callers (the loader) can union the sets across a
    /// program and re-lex files that *use* a tier some other file declares.
    pub text_tier_decls: Vec<String>,
}

/// The set of tier names whose `@<name> { … }` bodies are **verbatim text** rather than code
/// (text tiers, object-model slice 6f generalized by the text-tiers arc). The lexer captures a
/// member's body as one [`TokenKind::DocText`] token instead of tokenizing it, so arbitrary
/// prose/markup never produces lex errors.
///
/// The default set is std's `{doc}` — every bare `lex` call (tests, snippets, tools without a
/// manifest) behaves exactly as before. Pipeline paths that know more (the loader accumulating
/// dependency `@tier(…, text: "…")` declarations) pass an extended set via [`lex_in`]. Same-file
/// declarations need no help at all: [`lex_in`] discovers them itself (see the two-pass note
/// there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTiers(std::collections::HashSet<String>);

impl Default for TextTiers {
    fn default() -> Self {
        TextTiers(std::iter::once("doc".to_string()).collect())
    }
}

impl TextTiers {
    /// The std set (`{doc}`) extended with `names` — how the pipeline folds in the text tiers
    /// declared by dependencies.
    pub fn with(names: impl IntoIterator<Item = String>) -> Self {
        let mut set = TextTiers::default();
        set.0.extend(names);
        set
    }

    /// Whether `name` is a text tier in this set.
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    /// Add a tier name to the set.
    pub fn insert(&mut self, name: String) {
        self.0.insert(name);
    }
}

/// Lex a source file into a token stream, discarding comments (the compile path). Byte-for-byte
/// identical token output to lexing with trivia collection off — the parser never sees a comment.
/// Text-tier bodies are captured for the default [`TextTiers`] set plus any `@tier(…, text: "…")`
/// declaration in the file itself.
pub fn lex(source: &Source) -> Lexed {
    lex_in(source, &TextTiers::default())
}

/// Lex a source file, additionally collecting every comment into [`Lexed::comments`] (the formatter
/// path). The token stream is identical to [`lex`]'s; only trivia recovery is added.
pub fn lex_with_trivia(source: &Source) -> Lexed {
    lex_with_trivia_in(source, &TextTiers::default())
}

/// [`lex`] with an explicit text-tier set — the pipeline entry point when declarations from other
/// files (dependencies) are in play.
///
/// **Two-pass self-use:** a file can *declare* a text tier and use it, in either order, with no
/// caller help. After a first pass, the token stream is scanned for `@tier(<name>, …, text: …)`
/// declarations; if that discovers text-tier names the supplied set lacks, the file is re-lexed
/// once with the augmented set, so `@<name> { prose }` bodies capture verbatim. Only files
/// declaring text tiers pay the second pass.
pub fn lex_in(source: &Source, text_tiers: &TextTiers) -> Lexed {
    lex_impl(source, false, text_tiers)
}

/// [`lex_with_trivia`] with an explicit text-tier set (see [`lex_in`]).
pub fn lex_with_trivia_in(source: &Source, text_tiers: &TextTiers) -> Lexed {
    lex_impl(source, true, text_tiers)
}

fn lex_impl(source: &Source, collect_trivia: bool, text_tiers: &TextTiers) -> Lexed {
    let mut lexed = lex_pass(source, collect_trivia, text_tiers);
    // Two-pass self-use: fold in text tiers this file itself declares (`@tier(x, …, text: "…")`)
    // and re-lex so their `@x { … }` bodies capture verbatim. Declaration order doesn't matter —
    // the scan sees the whole stream. At most one extra pass: the re-scan works from pass-1
    // tokens, and a declaration can only *disappear* in pass 2 (were it inside another text
    // body — prose, not a declaration), never appear.
    let declared = declared_text_tiers(source.text(), &lexed.tokens);
    if declared.iter().any(|name| !text_tiers.contains(name)) {
        let mut augmented = text_tiers.clone();
        for name in &declared {
            augmented.insert(name.clone());
        }
        lexed = lex_pass(source, collect_trivia, &augmented);
    }
    lexed.text_tier_decls = declared;
    lexed
}

fn lex_pass(source: &Source, collect_trivia: bool, text_tiers: &TextTiers) -> Lexed {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();
    let mut comments = Vec::new();
    let text = source.text();
    let mut lexer = TokenKind::lexer(text);

    while let Some(result) = lexer.next() {
        let span = Span::from(lexer.span());
        match result {
            Ok(TokenKind::LineComment) => {
                // Trivia: dropped from the token stream; kept for the formatter when collecting.
                if collect_trivia {
                    comments.push(Comment {
                        span,
                        kind: CommentKind::Line,
                    });
                }
            }
            Ok(TokenKind::LBrace) if opens_text_block(text, &tokens, text_tiers) => {
                // A text-tier `@<name> {` (object-model slice 6f, generalized): capture the
                // brace-delimited body **verbatim** as one `DocText` token instead of tokenizing
                // it, so arbitrary prose/markup never produces lex errors. Emit `{`, the raw body,
                // and `}`, then advance the lexer past the whole span. The body's braces must
                // balance (the matching `}` closes the block); `\{`/`\}` are literal braces and
                // `\\` a literal backslash — not counted (see `matching_brace`).
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
                        // The tier name is the token just before the `{` (opens_text_block
                        // established the `@ <ident> {` shape).
                        let name = tokens
                            .last()
                            .map(|t| &text[t.span.start as usize..t.span.end as usize])
                            .unwrap_or("doc");
                        diagnostics.push(unterminated_text_block(span, name));
                        tokens.push(Token {
                            kind: TokenKind::LBrace,
                            span,
                        });
                    }
                }
            }
            Ok(TokenKind::BlockCommentOpen) => {
                // A `/* … */` block comment (nesting). Scan from just after the `/*` to the matching
                // close and drop the whole span — no token is emitted, exactly like a line comment.
                // An unterminated comment consumes to end-of-input and reports a diagnostic.
                let (comment_end, terminated) = match block_comment_end(text, span.end) {
                    Some(end) => {
                        lexer.bump((end - span.end) as usize);
                        (end, true)
                    }
                    None => {
                        diagnostics.push(unterminated_block_comment(span));
                        lexer.bump(text.len() - span.end as usize);
                        (text.len() as u32, false)
                    }
                };
                // Retain even an unterminated comment when collecting: the formatter refuses to
                // reformat sources with lex diagnostics, but keeping the span costs nothing.
                if collect_trivia && terminated {
                    comments.push(Comment {
                        span: Span::new(span.start, comment_end),
                        kind: CommentKind::Block,
                    });
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
        comments,
        text_tier_decls: Vec::new(),
    }
}

/// Whether the just-lexed `{` opens a text-tier body `@<name> { … }` — i.e. the two preceding
/// tokens are `@` then an identifier in the text-tier set. (Every other `@<tier> {` is a code
/// block tokenized normally. A text tier takes no directive args — `text:` and `config:` are
/// mutually exclusive on the declaration — so `@<name>(…) {` never needs this check.)
fn opens_text_block(text: &str, tokens: &[Token], text_tiers: &TextTiers) -> bool {
    let [.., at, name] = tokens else { return false };
    at.kind == TokenKind::At
        && name.kind == TokenKind::Ident
        && text
            .get(name.span.start as usize..name.span.end as usize)
            .is_some_and(|name| text_tiers.contains(name))
}

/// Scan a lexed token stream for `@tier(<name>, …, text: <string>)` — or `expr: <Type>`, an
/// expression tier's marker (expr-tiers arc), whose bodies capture the same way — declarations
/// and return the declared text-tier names; the lexical shape is fixed, so this needs no parse.
/// Powers [`lex_in`]'s two-pass self-use: a file's own text-tier declarations take effect within
/// the file, whatever the order of declaration and use.
fn declared_text_tiers(text: &str, tokens: &[Token]) -> Vec<String> {
    let ident = |t: &Token| -> Option<&str> {
        (t.kind == TokenKind::Ident).then(|| &text[t.span.start as usize..t.span.end as usize])
    };
    let mut names = Vec::new();
    for (i, window) in tokens.windows(4).enumerate() {
        let [at, kw, open, name_tok] = window else {
            unreachable!()
        };
        if !(at.kind == TokenKind::At
            && ident(kw) == Some("tier")
            && open.kind == TokenKind::LParen)
        {
            continue;
        }
        let Some(name) = ident(name_tok) else {
            continue;
        };
        // Within the directive's parens (depth-tracked to its close), look for a `text: <string>`
        // or `expr: <Type>` key — either marker makes the tier's bodies verbatim-captured.
        let mut depth = 1u32;
        let mut rest = tokens[i + 4..].iter().peekable();
        while let Some(tok) = rest.next() {
            match tok.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::Ident
                    if depth == 1
                        && matches!(ident(tok), Some("text" | "expr"))
                        && rest.peek().is_some_and(|t| t.kind == TokenKind::Colon) =>
                {
                    names.push(name.to_string());
                    break;
                }
                _ => {}
            }
        }
    }
    names
}

/// Find the `}` that closes a text-tier body whose opening `{` ends at `open_end`, returning its
/// byte offset. Braces nest (a `{` in the body must be matched by a `}` before the block closes),
/// so a balanced code snippet inside prose is fine; an unbalanced `}` is treated as the closer and
/// an unbalanced `{` runs to end-of-input (`None` — an unterminated block). Exactly three escape
/// sequences are honored — `\{` and `\}` (literal braces, not counted) and `\\` (a literal
/// backslash, so `\\{` is a backslash then a *counted* brace); every other backslash is ordinary
/// text (markup formats need their own escapes untouched). The tree-sitter scanner and the
/// TextMate rule implement this same count, so editors and the compiler always agree on where a
/// body ends.
fn matching_brace(text: &str, open_end: u32) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut depth: u32 = 1;
    let mut i = open_end as usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if matches!(bytes.get(i + 1), Some(b'{' | b'}' | b'\\')) => i += 2,
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i as u32);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Undo the text-body escapes for consumers of the *content* (extraction, hover, runners): `\{`,
/// `\}`, `\\` become `{`, `}`, `\`; everything else — including every other backslash sequence —
/// is untouched. The formatter must NOT use this: it re-emits source, where the escapes stay.
pub fn unescape_text_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('{' | '}' | '\\')) {
            out.push(chars.next().unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

/// Find the end of a `/* … */` block comment whose opening `/*` ends at `open_end`, returning the
/// byte offset just past the matching `*/`. Comments **nest**: an inner `/*` must be closed by a
/// `*/` before the outer one closes. Returns `None` if no matching `*/` appears before end-of-input.
fn block_comment_end(text: &str, open_end: u32) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut depth: u32 = 1;
    let mut i = open_end as usize;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Some(i as u32);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The diagnostic for a `/*` block comment that never closes (no matching `*/` before end-of-input).
fn unterminated_block_comment(open: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnexpectedEndOfInput,
        open,
        "unterminated block comment",
    )
    .with_help("add a closing `*/`; block comments nest, so each `/*` needs its own `*/`")
}

/// The diagnostic for a text-tier `@<name> {` whose body never closes (no matching `}` before
/// end-of-input).
fn unterminated_text_block(open: Span, tier: &str) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnexpectedEndOfInput,
        open,
        format!("unterminated `@{tier}` block"),
    )
    .with_help(
        "add a closing `}` to end the text block; braces inside the body must balance \
         (write a literal unbalanced brace as `\\{` or `\\}`)",
    )
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

/// Byte offsets (token starts) at which a newline **terminates** the preceding statement — the rule
/// [`insert_terminators`] uses, *minus* the [`is_statement_ending`] gate on the previous token, and
/// with the `(`/`[` depth measured **relative to the innermost `{`** rather than absolutely.
///
/// The lexer only synthesizes a `;` when the previous token can end a statement, which misses complete
/// statements whose last token cannot (a generic-close `>` in `x is List<int>`, indistinguishable at
/// the token level from a dangling `>` comparison). The parser consults these offsets as a **soft**
/// terminator — a peek that the expression grammar never sees, so trailing-operator continuation
/// (`1 +\n2`) is unaffected: the offset is only honored at a point where the expression is already
/// complete.
///
/// **Brace-relative depth:** a `{` saves the current bracket depth and resets it to zero; the matching
/// `}` restores it. A `{ … }` body nested inside a call — `xs.map(fn(n) { … })` — thus gets exactly
/// the newline treatment the same block has at top level, fixing the wart where a multi-statement
/// closure body inside `(`/`[` required explicit `;` separators. Non-block `{…}` constructs (map
/// literals, match, struct literals) are also reset, which is harmless: the parser only consults these
/// offsets at statement boundaries, and those constructs already occur at depth zero today, where the
/// leading-continuation and comma gates keep their gaps unmarked or unconsulted. Runs on the
/// post-[`insert_terminators`] token stream (synthetic `;` are zero-width and change nothing).
pub fn newline_terminator_offsets(source: &Source, tokens: &[Token]) -> Vec<u32> {
    let text = source.text();
    let mut out = Vec::new();
    let mut depth: u32 = 0;
    let mut saved: Vec<u32> = Vec::new(); // bracket depth outside each enclosing `{`
    let mut prev: Option<&Token> = None;
    for tok in tokens {
        if let Some(p) = prev
            && depth == 0
            && !is_leading_continuation(tok.kind)
            && gap_has_newline(text, p.span.end, tok.span.start)
        {
            out.push(tok.span.start);
        }
        match tok.kind {
            TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
            TokenKind::LBrace => {
                saved.push(depth);
                depth = 0;
            }
            TokenKind::RBrace => depth = saved.pop().unwrap_or(0),
            _ => {}
        }
        prev = Some(tok);
    }
    out
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
            | TokenKind::F64Lit
            | TokenKind::IntNLit
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

/// Public view of [`is_leading_continuation`]: whether a token, when it **starts** the next line,
/// joins the previous line rather than beginning a new statement. The formatter uses this to decide
/// whether stripping a trailing `;` is safe — if the next statement's first token continues the line
/// (e.g. a unary `-`), the `;` is the only thing keeping the two statements apart and must be kept.
pub fn token_continues_line(kind: TokenKind) -> bool {
    is_leading_continuation(kind)
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
    use noeta_span::SourceId;

    fn lex_str(text: &str) -> (Source, Lexed) {
        let source = Source::new(SourceId::FIRST, "test.noe", text);
        let lexed = lex(&source);
        (source, lexed)
    }

    /// Opt-in throughput check (F1): confirm that lexing comments as dropped tokens did not regress
    /// the hot `lex` path. Compares a comment-heavy source against the same source with comments
    /// stripped; the ratio must stay modest. Ignored by default (timings are noisy in CI); run with
    /// `cargo test -p noeta-lexer -- --ignored --nocapture lex_comment_overhead`.
    #[test]
    #[ignore = "timing-sensitive; run explicitly"]
    fn lex_comment_overhead_is_small() {
        let mut with_comments = String::new();
        let mut without = String::new();
        for i in 0..2000 {
            with_comments.push_str(&format!("x = {i} + {i}; // comment number {i}\n"));
            without.push_str(&format!("x = {i} + {i};\n"));
        }
        let src_c = Source::new(SourceId::FIRST, "c", with_comments);
        let src_p = Source::new(SourceId::FIRST, "p", without);

        let time = |s: &Source| {
            let start = std::time::Instant::now();
            let mut n = 0usize;
            for _ in 0..200 {
                n += lex(s).tokens.len();
            }
            (start.elapsed(), n)
        };
        let (t_plain, _) = time(&src_p);
        let (t_comments, _) = time(&src_c);
        eprintln!("lex plain={t_plain:?} with-comments={t_comments:?}");
        // Comment-bearing source lexes the same real tokens plus dropped comment tokens; it must not
        // cost dramatically more than the comment-free source.
        assert!(
            t_comments.as_secs_f64() < t_plain.as_secs_f64() * 2.0 + 0.01,
            "comment lexing regressed: plain={t_plain:?} comments={t_comments:?}"
        );
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
    fn block_comments_are_stripped_and_nest() {
        // A `/* ... */` comment (nesting, mid-expression) produces no tokens; `/` stays division.
        let (_source, lexed) = lex_str("x = 1 /* a /* nested */ b */ + 2\ny = 5 / 5\n");
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let kinds: Vec<_> = lexed
            .tokens
            .iter()
            .map(|t| t.kind)
            .filter(|k| *k != TokenKind::Semicolon)
            .collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Ident, // x
                TokenKind::Eq,
                TokenKind::IntLit, // 1
                TokenKind::Plus,
                TokenKind::IntLit, // 2
                TokenKind::Ident,  // y
                TokenKind::Eq,
                TokenKind::IntLit, // 5
                TokenKind::Slash,  // `/` is still division, not a comment
                TokenKind::IntLit, // 5
            ]
        );
    }

    #[test]
    fn reports_unterminated_block_comment() {
        let (_source, lexed) = lex_str("x = 1\n/* never closed\n");
        assert_eq!(lexed.diagnostics.len(), 1);
        assert_eq!(
            lexed.diagnostics[0].code,
            DiagnosticCode::UnexpectedEndOfInput
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
        // `lex` never surfaces comments.
        assert!(lexed.comments.is_empty());
    }

    #[test]
    fn lex_with_trivia_captures_comments_with_spans() {
        let text = "// lead\necho \"x\"; // trail\n/* block */\n";
        let source = Source::new(SourceId(0), "t", text);
        let lexed = lex_with_trivia(&source);
        assert!(lexed.diagnostics.is_empty());

        let comments: Vec<(CommentKind, &str)> = lexed
            .comments
            .iter()
            .map(|c| (c.kind, &text[c.span.start as usize..c.span.end as usize]))
            .collect();
        assert_eq!(
            comments,
            vec![
                (CommentKind::Line, "// lead"),
                (CommentKind::Line, "// trail"),
                (CommentKind::Block, "/* block */"),
            ]
        );
    }

    #[test]
    fn trivia_collection_never_changes_the_token_stream() {
        // The parser must see identical tokens whether or not trivia is collected.
        for text in [
            "echo 1 // c\n",
            "/* a */ fn f() { return 2 }",
            "x = \"http://not-a-comment\"; // real\n",
            "/* nested /* inner */ still */ echo 3",
            "echo 4",
        ] {
            let source = Source::new(SourceId(0), "t", text);
            let plain = lex(&source);
            let trivia = lex_with_trivia(&source);
            assert_eq!(
                plain.tokens, trivia.tokens,
                "token stream diverged for {text:?}"
            );
            assert!(plain.comments.is_empty());
        }
    }

    #[test]
    fn block_comment_inside_string_is_not_a_comment() {
        // A `//`/`/*` inside a string literal is part of the string token, never trivia.
        let source = Source::new(SourceId(0), "t", "x = \"a // b /* c */ d\"");
        let lexed = lex_with_trivia(&source);
        assert!(lexed.comments.is_empty(), "{:?}", lexed.comments);
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
    fn text_body_escapes_are_not_counted() {
        // `\{` and `\}` are literal braces (not counted by the balance scan); `\\` is a literal
        // backslash, so `\\}` below is a backslash then the *counted* closer of the inner `{x`.
        // Every other backslash (markdown's `\*`) passes through untouched.
        let (source, lexed) = lex_str("@doc {\n  a \\{ not counted \\} b {x\\\\} \\*c\n}\nx = 1\n");
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let body = source.slice(lexed.tokens[3].span);
        assert_eq!(body, "\n  a \\{ not counted \\} b {x\\\\} \\*c\n");
        // Unescaping (what content consumers see) undoes exactly the three sequences.
        assert_eq!(
            unescape_text_body(body),
            "\n  a { not counted } b {x\\} \\*c\n"
        );
        // Lexing resumed after the body: the trailing assignment tokenized.
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TokenKind::Eq));
    }

    #[test]
    fn escaped_closer_extends_the_body() {
        // A body whose only `}` is escaped never closes — the escape genuinely suppresses the
        // count (this is the unbalanced-prose case `\}` exists for, in reverse).
        let (_source, lexed) = lex_str("@doc { all escaped \\} so never closed\n");
        assert!(
            lexed
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
        );
    }

    #[test]
    fn declared_text_tier_captures_in_same_file() {
        // Two-pass self-use: a `@tier(spec, text: "xml")` declaration makes `@spec { … }` bodies
        // capture verbatim in the same file. The body is invalid as code (an unmatched quote runs
        // to end-of-input in pass 1); pass 2 captures it raw, and the pass-1 error is discarded
        // with the pass-1 stream.
        let src = "@tier(spec, text: \"xml\")\nfn run_specs(roots: List<TierRoot>): void {}\n@spec {\n  <case name=\"unterminated>\n}\n";
        let (source, lexed) = lex_str(src);
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let doc = lexed
            .tokens
            .iter()
            .find(|t| t.kind == TokenKind::DocText)
            .expect("body captured as DocText");
        assert_eq!(source.slice(doc.span), "\n  <case name=\"unterminated>\n");
    }

    #[test]
    fn text_tier_use_before_declaration_captures() {
        // Declaration order doesn't matter: the decl scan sees the whole pass-1 stream, so a
        // `@spec { … }` above the `@tier(spec, text: …)` decl still captures. (The prose here must
        // survive pass-1 tokenization well enough to leave the decl intact — quote-free.)
        let src = "@spec {\n  <case attr=unterminated>\n}\n@tier(spec, text: \"xml\")\nfn run_specs(roots: List<TierRoot>): void {}\n";
        let (source, lexed) = lex_str(src);
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
        assert_eq!(
            source.slice(lexed.tokens[3].span),
            "\n  <case attr=unterminated>\n"
        );
    }

    #[test]
    fn undeclared_custom_tier_stays_code() {
        // Without a `text:` declaration (here: a code tier's decl, no `text:` key), `@spec {` is an
        // ordinary code block — the open tier set does not imply raw capture.
        let src = "@tier(spec, config: SpecKnobs)\nfn run_specs(roots: List<TierRoot>): void {}\n@spec { fn t() { return 1 } }\n";
        let (_source, lexed) = lex_str(src);
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert!(!kinds.contains(&TokenKind::DocText));
        assert!(kinds.contains(&TokenKind::FnKw));
    }

    #[test]
    fn pipeline_supplied_text_tier_set_captures_without_local_decl() {
        // The loader path: a dependency declared the tier, the consumer file only uses it.
        let source = Source::new(
            SourceId(0),
            "<t>",
            "@spec {\n  # prose \"unmatched\n}\n".to_string(),
        );
        let lexed = lex_in(&source, &TextTiers::with(["spec".to_string()]));
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TokenKind::DocText));
    }

    #[test]
    fn token_dump_is_stable() {
        let (source, lexed) = lex_str("echo \"hi\";");
        insta::assert_snapshot!(dump_tokens(&source, &lexed));
    }
}
