//! Lexical retrieval: the field-weighted **BM25F** core, and the workspace-wide **code index**
//! over a linked program's declarations that `noeta mcp`'s `code_search` tool serves.
//!
//! Two consumers share the core. [`guide`](crate::guide) ranks the language wiki's sections over
//! three text fields; [`CodeIndex`] ranks a project's declarations over eight, from the leaf name
//! down to the identifiers its body mentions. One implementation of tokenization, term saturation,
//! IDF and length normalization, so a fix to either ranker is a fix to both.
//!
//! The code index answers the question the rest of the graph tools cannot: an agent holding a
//! sentence rather than a name. `symbols` outlines a file, `definition` and `references` need a
//! name already, and `docs_search` reads the language guide — so "where does an order get
//! persisted?" had no first move. Here it is a query over the names, `@doc` prose, signatures,
//! `@role` bindings, attributes, file paths and body identifiers of every declaration the linked
//! program holds.
//!
//! Every part is deterministic: no model, no randomness, ties broken by name. The index is built
//! per call from a prepared workspace, so it always describes the program as it is now.

use std::collections::HashMap;

use noeta_ast::{EnumDecl, FieldDecl, FnDecl, Program, Stmt, VariantDecl};
use noeta_span::Span;

// ---- tokenization ------------------------------------------------------------------------------

/// The shortest token worth indexing. One-character runs (`T`, `x`, a stray `a`) carry no
/// retrieval signal and would dominate every document length.
const MIN_TOKEN: usize = 2;

/// Split text into lowercased index terms.
///
/// Word runs are alphanumerics plus `_`, so a Noeta identifier survives whole — `read_line_async`
/// and `E0059` are each one term, and a query for either lands on the documents that actually name
/// it. Each compound *also* emits its parts (`read`, `line`, `async` from the snake case;
/// `http`, `error` from `HttpError`'s camel case), so a reader who searches for the halves still
/// finds the whole. Emitting both is deliberate double-counting: the compound is the rarer term,
/// so IDF weighting makes an exact-identifier hit outrank a coincidental parts-only one, which is
/// the ranking a reader means by typing the identifier.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        let start = out.len();
        let whole = run.to_lowercase();
        if whole.len() >= MIN_TOKEN {
            out.push(whole.clone());
        }
        // Snake-case parts, then camel-case parts. Both are skipped when the run has no
        // boundary, so a plain word contributes exactly one term.
        if whole.contains('_') {
            out.extend(
                whole
                    .split('_')
                    .filter(|p| p.len() >= MIN_TOKEN)
                    .map(str::to_string),
            );
        }
        for part in camel_parts(run) {
            if part.len() >= MIN_TOKEN && part != whole {
                out.push(part);
            }
        }
        // Stems last, and *in addition to* the exact forms above: an exact match stays the rarer
        // term, so it still outranks a stem-only one.
        let exact: Vec<String> = out[start..].to_vec();
        for t in exact {
            let s = stem(&t);
            if s != t {
                out.push(s);
            }
        }
        run.clear();
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            run.push(ch);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Suffixes stripped by [`stem`], longest first so `interpolation` loses `ation` rather than `ion`.
const SUFFIXES: &[&str] = &[
    "ization", "ational", "ations", "ation", "ities", "ility", "ically", "ingly", "ables", "ible",
    "able", "ings", "ing", "ions", "ion", "ies", "ers", "ed", "es", "er", "ly", "s",
];

/// The shortest stem worth producing. Below this, stripping turns distinct words into the same
/// two or three letters.
const MIN_STEM: usize = 4;

/// Reduce a word to a crude stem, so a query finds the documents that inflect it differently.
///
/// Morphology is what makes a prose query land: `derive` should find `derivable`, `persist` should
/// find `persisted`, and exact-token matching alone loses both. On a fourteen-query set over the
/// language guide, the queries an exact-token ranker lost were every one whose answer inflects the
/// term — `derive Display`, `string interpolation`, `error propagation operator`.
///
/// Identifiers are left alone — a token holding `_` or a digit is a name (`try_parse`, `E0059`),
/// where English suffix rules mean nothing.
pub fn stem(token: &str) -> String {
    if token.len() < MIN_STEM || token.contains('_') || token.chars().any(|c| c.is_ascii_digit()) {
        return token.to_string();
    }
    for suffix in SUFFIXES {
        if let Some(base) = token.strip_suffix(suffix)
            && base.len() >= MIN_STEM
        {
            return base.to_string();
        }
    }
    token.to_string()
}

/// The lowercased camel-case segments of one word run: a segment break falls where a
/// lowercase/digit is followed by an uppercase (`HttpError` → `http`, `error`). A run with no
/// such boundary yields nothing, so callers can treat an empty result as "not camel case".
pub fn camel_parts(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let has_boundary = chars
        .windows(2)
        .any(|w| (w[0].is_lowercase() || w[0].is_numeric()) && w[1].is_uppercase());
    if !has_boundary {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() && (chars[i - 1].is_lowercase() || chars[i - 1].is_numeric()) {
            parts.push(std::mem::take(&mut cur).to_lowercase());
        }
        cur.push(c);
    }
    parts.push(cur.to_lowercase());
    parts
}

// ---- excerpts ----------------------------------------------------------------------------------

/// A short excerpt of `text`: the line covering the most of the query, trimmed and length-capped;
/// falls back to the opening line.
///
/// "Most of the query" is distinct terms matched, then total matched length — so a line naming
/// `try_parse` beats one that merely says `ParseFailure`.
pub fn snippet(text: &str, terms: &[String]) -> String {
    let score_line = |line: &str| -> (usize, usize) {
        let ll = line.to_lowercase();
        let matched: Vec<&String> = terms.iter().filter(|t| ll.contains(t.as_str())).collect();
        (matched.len(), matched.iter().map(|t| t.len()).sum())
    };
    // `Reverse(i)` breaks ties toward the *earliest* line, which reads as the more introductory
    // one; `max_by_key` alone would return the last.
    let best = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .max_by_key(|(i, l)| {
            let (n, len) = score_line(l);
            (n, len, std::cmp::Reverse(*i))
        })
        .filter(|(_, l)| score_line(l).0 > 0)
        .map(|(_, l)| l);
    let line = best
        .or_else(|| text.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim();
    window_on_match(line, terms)
}

/// The excerpt's width, in characters.
pub const SNIPPET_WIDTH: usize = 200;

/// How much context to keep before the match when the window has to scroll.
const SNIPPET_LEAD: usize = 40;

/// Cut `line` down to [`SNIPPET_WIDTH`] characters *around the first matching term* rather than
/// from the start. A prose line often runs past 400 characters, so a head-only cut routinely
/// returned an excerpt that did not contain the thing searched for.
pub fn window_on_match(line: &str, terms: &[String]) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= SNIPPET_WIDTH {
        return line.to_string();
    }
    // Character offset of the earliest term match, located on a lowercased copy so the index maps
    // back to `chars` one-to-one (`to_lowercase` can change length, so build it per character).
    let lower: String = chars.iter().flat_map(|c| c.to_lowercase()).collect();
    let first = if lower.chars().count() == chars.len() {
        terms
            .iter()
            .filter_map(|t| lower.find(t.as_str()))
            .map(|byte_idx| lower[..byte_idx].chars().count())
            .min()
    } else {
        // A character whose lowercase is multi-character (e.g. `İ`) breaks the 1:1 mapping; fall
        // back to a head window rather than report a wrong offset.
        None
    };

    let start = match first {
        Some(at) if at > SNIPPET_LEAD => at - SNIPPET_LEAD,
        _ => 0,
    };
    let end = (start + SNIPPET_WIDTH).min(chars.len());
    let body: String = chars[start..end].iter().collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body.trim(),
        if end < chars.len() { "…" } else { "" }
    )
}

// ---- BM25F -------------------------------------------------------------------------------------

/// BM25's term-frequency saturation. Past a few occurrences, one more mention says almost nothing
/// extra about relevance, which is what a raw weighted sum gets wrong: a long document wins by
/// repetition.
const K1: f32 = 1.2;

/// A length floor for normalization, as a fraction of the corpus average.
///
/// BM25 reads "short document containing the term" as "document about the term". That is wrong for
/// the boilerplate a corpus is full of: a **See also** list of 19 tokens against a corpus average
/// of 179 has its terms scored ~3×, and a five-line list of cross-links outranks the section that
/// explains the operator. Treating anything shorter than this fraction of the average as if it were
/// that long removes the windfall, while leaving normalization to do its real work — separating a
/// focused document from a sprawling one.
const LENGTH_FLOOR_RATIO: f32 = 0.6;

/// Multiplier for a document whose text contains the query verbatim. A phrase match is strong
/// evidence, but not so strong that it should outrank a document that is genuinely *about* the
/// terms, so this is a nudge rather than an override.
pub const PHRASE_BOOST: f32 = 1.6;

/// One field's BM25F parameters: how much a term occurrence there is worth, and how much the
/// field's length discounts it.
#[derive(Debug, Clone, Copy)]
pub struct FieldWeight {
    /// The multiplier applied to the raw term frequency before saturation.
    pub weight: f32,
    /// The length normalization, `0.0` for a field whose length says nothing about relevance (a
    /// title, a name) and up to `1.0` for one where a long field really is a diluted one.
    pub b: f32,
}

impl FieldWeight {
    pub const fn new(weight: f32, b: f32) -> FieldWeight {
        FieldWeight { weight, b }
    }
}

/// One document reduced to what scoring needs.
#[derive(Debug)]
struct IndexedDoc {
    /// Per-field term frequencies: `tf["order"][field]`.
    tf: HashMap<String, Vec<u32>>,
    /// Per-field token counts, for length normalization.
    len: Vec<f32>,
    /// Every field's text, lowercased and joined, for the verbatim-phrase check.
    haystack: String,
}

/// A field-weighted lexical index: **BM25F** over documents whose fields carry their own weight
/// and length normalization.
///
/// The two properties that make it a ranker rather than a term counter are length normalization
/// (so a long document does not win by holding more text) and IDF (so a query's common word does
/// not drown out the rare one that discriminates). Matching is over *terms*, never substrings, so
/// `int` cannot match `print`.
#[derive(Debug)]
pub struct Bm25f {
    fields: Vec<FieldWeight>,
    docs: Vec<IndexedDoc>,
    /// How many documents contain each term.
    df: HashMap<String, u32>,
    /// Mean per-field token count across the corpus.
    avg_len: Vec<f32>,
    /// The document count, as the `N` of the IDF formula.
    n: f32,
}

impl Bm25f {
    /// Index `docs`, each a per-field text list in `fields` order. A document with fewer texts
    /// than there are fields leaves the rest empty.
    pub fn build<'a>(
        fields: Vec<FieldWeight>,
        docs: impl IntoIterator<Item = Vec<std::borrow::Cow<'a, str>>>,
    ) -> Bm25f {
        let arity = fields.len();
        let mut indexed: Vec<IndexedDoc> = Vec::new();
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut total_len = vec![0.0f32; arity];
        for texts in docs {
            let mut tf: HashMap<String, Vec<u32>> = HashMap::new();
            let mut len = vec![0.0f32; arity];
            let mut haystack = String::new();
            for (field, text) in texts.iter().enumerate().take(arity) {
                let tokens = tokenize(text);
                len[field] = tokens.len() as f32;
                for t in tokens {
                    tf.entry(t).or_insert_with(|| vec![0; arity])[field] += 1;
                }
                if !haystack.is_empty() {
                    haystack.push('\n');
                }
                haystack.push_str(&text.to_lowercase());
            }
            for term in tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            for (i, l) in len.iter().enumerate() {
                total_len[i] += l;
            }
            indexed.push(IndexedDoc { tf, len, haystack });
        }
        let n = indexed.len().max(1) as f32;
        let avg_len = total_len
            .iter()
            // A zero average would divide by zero on an empty corpus; 1.0 is inert there because
            // every length is then zero too.
            .map(|total| if *total > 0.0 { total / n } else { 1.0 })
            .collect();
        Bm25f {
            fields,
            docs: indexed,
            df,
            avg_len,
            n,
        }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The BM25F score of document `doc` against the (deduplicated) query terms.
    pub fn score(&self, doc: usize, terms: &[String]) -> f32 {
        let Some(d) = self.docs.get(doc) else {
            return 0.0;
        };
        let mut score = 0.0;
        for term in terms {
            let Some(tf) = d.tf.get(term) else {
                continue;
            };
            // Combine the fields into one saturating pseudo-frequency *before* applying K1 — that
            // is what makes this BM25F rather than several independent BM25 scores summed, and it
            // is why a term in both the name and the body cannot be counted twice over.
            let mut pseudo_tf = 0.0;
            for (f, field) in self.fields.iter().enumerate() {
                let raw = tf[f] as f32;
                if raw == 0.0 {
                    continue;
                }
                let len = d.len[f].max(self.avg_len[f] * LENGTH_FLOOR_RATIO);
                let norm = 1.0 - field.b + field.b * len / self.avg_len[f];
                pseudo_tf += field.weight * raw / norm;
            }
            score += self.idf(term) * pseudo_tf / (K1 + pseudo_tf);
        }
        score
    }

    /// The fields of `doc` that hold at least one query term, in field order — what a result
    /// carries so a reader can see *why* it ranked.
    pub fn matched_fields(&self, doc: usize, terms: &[String]) -> Vec<usize> {
        let Some(d) = self.docs.get(doc) else {
            return Vec::new();
        };
        (0..self.fields.len())
            .filter(|f| {
                terms
                    .iter()
                    .any(|t| d.tf.get(t).is_some_and(|tf| tf[*f] > 0))
            })
            .collect()
    }

    /// Whether `doc`'s text contains `needle` verbatim (both already lowercased).
    pub fn contains_phrase(&self, doc: usize, needle: &str) -> bool {
        self.docs
            .get(doc)
            .is_some_and(|d| d.haystack.contains(needle))
    }

    /// Lucene's smoothed IDF — `ln(1 + (N - df + 0.5) / (df + 0.5))`. The `1 +` keeps it positive
    /// for a term present in every document, where the textbook form goes negative and would let a
    /// ubiquitous word *subtract* from a score.
    fn idf(&self, term: &str) -> f32 {
        let df = *self.df.get(term).unwrap_or(&0) as f32;
        (1.0 + (self.n - df + 0.5) / (df + 0.5)).ln()
    }
}

/// The deduplicated terms of a query. Deduplicated because BM25 sums over the query's *distinct*
/// terms; a repeated word should not buy a document a second helping of the same evidence.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tokenize(query) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The lowercased, whitespace-collapsed query to look for verbatim, or `None` for a single-word
/// query — where a phrase match is just a term match and the boost would fire on every hit.
pub fn phrase_needle(query: &str) -> Option<String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    (words.len() > 1).then(|| words.join(" ").to_lowercase())
}

// ---- the code index ----------------------------------------------------------------------------

/// What a declaration **is**, in the vocabulary the index filters on. The same set of declarations
/// the call graph and the declaration inventory address, so a hit names a node the other tools
/// answer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Variant,
    Field,
    Trait,
    Impl,
}

impl DeclKind {
    pub const ALL: [DeclKind; 9] = [
        DeclKind::Function,
        DeclKind::Method,
        DeclKind::Struct,
        DeclKind::Class,
        DeclKind::Enum,
        DeclKind::Variant,
        DeclKind::Field,
        DeclKind::Trait,
        DeclKind::Impl,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DeclKind::Function => "function",
            DeclKind::Method => "method",
            DeclKind::Struct => "struct",
            DeclKind::Class => "class",
            DeclKind::Enum => "enum",
            DeclKind::Variant => "variant",
            DeclKind::Field => "field",
            DeclKind::Trait => "trait",
            DeclKind::Impl => "impl",
        }
    }
}

impl std::fmt::Display for DeclKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DeclKind {
    type Err = String;

    fn from_str(s: &str) -> Result<DeclKind, String> {
        let want = s.trim().to_ascii_lowercase();
        DeclKind::ALL
            .into_iter()
            .find(|k| k.as_str() == want)
            .ok_or_else(|| format!("unknown declaration kind `{s}`"))
    }
}

/// The fields a declaration is indexed under. Each is a different statement about what the
/// declaration is *about*, and the weights order them accordingly: its own name is the strongest
/// statement, the identifiers its body happens to mention the weakest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeField {
    /// The leaf name (`place_order`), the one an author types.
    Name,
    /// The whole post-link name (`shop.orders.place_order`), so a module or type name finds its
    /// members.
    Qualified,
    /// The kind word plus the dev tier it was declared in, so `test` narrows to test code.
    Kind,
    /// `@role` bindings and `#[...]` attribute names — author-written semantics.
    Roles,
    /// The `@doc { … }` prose. The field a query with no identifier in it lands on.
    Doc,
    /// The rendered signature, so parameter and return type names are searchable.
    Signature,
    /// The identifiers the declaration's own source mentions: what it calls, the members it
    /// reaches, the words in its string literals.
    Body,
    /// The declaring file's path segments.
    Path,
}

impl CodeField {
    pub const ALL: [CodeField; 8] = [
        CodeField::Name,
        CodeField::Qualified,
        CodeField::Kind,
        CodeField::Roles,
        CodeField::Doc,
        CodeField::Signature,
        CodeField::Body,
        CodeField::Path,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CodeField::Name => "name",
            CodeField::Qualified => "qualified",
            CodeField::Kind => "kind",
            CodeField::Roles => "roles",
            CodeField::Doc => "doc",
            CodeField::Signature => "signature",
            CodeField::Body => "body",
            CodeField::Path => "path",
        }
    }

    /// This field's BM25F parameters.
    ///
    /// Names carry no length normalization: a three-word name is not a diluted one-word name.
    /// Prose, signatures and bodies do, on a rising scale, because there a long field really does
    /// dilute each term it holds.
    fn params(self) -> FieldWeight {
        match self {
            CodeField::Name => FieldWeight::new(8.0, 0.0),
            CodeField::Qualified => FieldWeight::new(4.0, 0.0),
            CodeField::Kind => FieldWeight::new(1.0, 0.0),
            CodeField::Roles => FieldWeight::new(3.0, 0.0),
            CodeField::Doc => FieldWeight::new(3.0, 0.6),
            CodeField::Signature => FieldWeight::new(2.0, 0.4),
            CodeField::Body => FieldWeight::new(1.0, 0.75),
            CodeField::Path => FieldWeight::new(1.5, 0.0),
        }
    }
}

/// Multiplier for a hit whose leaf name **is** one of the query's terms. Typing a name means
/// "that declaration", and no amount of prose about it should outrank the thing itself.
const EXACT_NAME_BOOST: f32 = 4.0;

/// Multiplier for a hit whose leaf name *starts with* a query term (`place` → `place_order`).
const PREFIX_NAME_BOOST: f32 = 1.6;

/// The shortest query term that may earn [`PREFIX_NAME_BOOST`]. Below it, a prefix is a
/// coincidence — `is` prefixes half the predicates in a program.
const MIN_PREFIX: usize = 3;

/// One source file as the index reads it, indexed by [`noeta_span::SourceId`].
#[derive(Debug, Clone, Copy)]
pub struct IndexedSource<'a> {
    /// The name to report — the path the caller wants a hit attributed to.
    pub name: &'a str,
    /// The file's text, which body and signature extraction slice.
    pub text: &'a str,
}

/// One indexed declaration: its identity, where it sits, and the facts a result carries.
#[derive(Debug, Clone)]
pub struct CodeDecl {
    /// The post-link name: namespace-qualified for a package (`app.main.handle`), `Type.method`
    /// for a method, `Type.field` for a member.
    pub name: String,
    /// The last segment of [`Self::name`] — what an author types.
    pub leaf: String,
    pub kind: DeclKind,
    /// The declared name's span. The join key to every other graph tool's node identity.
    pub name_span: Span,
    /// The whole declaration's span.
    pub decl_span: Span,
    /// The `@tier` block it was written inside (`test`, `bench`), when it was.
    pub tier: Option<String>,
    /// The `Enum.Variant` roles it bears.
    pub roles: Vec<String>,
    /// The `#[...]` attribute names it carries.
    pub attributes: Vec<String>,
    /// The rendered signature (`fn place_order(id: int) -> Order`).
    pub signature: String,
    /// The `@doc { … }` prose attached to it, empty when it has none.
    pub doc: String,
    /// The declaration's own source text with any nested declaration cut out — what the
    /// [`CodeField::Body`] field indexes.
    pub body: String,
    /// The declaring file.
    pub file: String,
}

/// One ranked hit.
#[derive(Debug, Clone)]
pub struct CodeHit {
    /// Index into [`CodeIndex::decls`].
    pub decl: usize,
    pub score: f32,
    /// The fields that held a query term.
    pub matched: Vec<CodeField>,
    /// A line of evidence: the `@doc` prose that matched, else the signature.
    pub snippet: String,
}

/// What narrows a search before it is scored.
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// Keep only this kind of declaration.
    pub kind: Option<DeclKind>,
    /// Keep only declarations bearing one of these roles. A bare variant (`EntryPoint`) matches a
    /// qualified binding (`Semantic.EntryPoint`); matching is case-insensitive.
    pub roles: Vec<String>,
}

impl SearchFilter {
    fn keeps(&self, decl: &CodeDecl) -> bool {
        if self.kind.is_some_and(|k| k != decl.kind) {
            return false;
        }
        if self.roles.is_empty() {
            return true;
        }
        self.roles.iter().any(|want| {
            let want = want.trim().to_ascii_lowercase();
            decl.roles.iter().any(|have| {
                let have = have.to_ascii_lowercase();
                have == want || have.ends_with(&format!(".{want}"))
            })
        })
    }
}

/// The lexical index over a linked program's declarations.
#[derive(Debug)]
pub struct CodeIndex {
    decls: Vec<CodeDecl>,
    bm25: Bm25f,
}

impl CodeIndex {
    /// Index every declaration of `program`, reading signatures, bodies and paths out of
    /// `sources` (indexed by [`noeta_span::SourceId`], the order the linker assigned).
    pub fn build(program: &Program, sources: &[IndexedSource<'_>]) -> CodeIndex {
        CodeIndex::build_over(&[program], sources)
    }

    /// Index the declarations of several programs into one corpus, first program first.
    ///
    /// A workspace-wide search needs both halves of what the database holds. The **linked**
    /// program names its declarations the way every other graph tool does, so pass it first and
    /// its qualified names win; it reaches only the modules the entry imports, so passing each
    /// member's own parse after it covers the siblings nothing imports. A declaration already
    /// indexed under one program is skipped when another names it again, matched on its name span
    /// — the identity the engine itself keys declarations by.
    pub fn build_over(programs: &[&Program], sources: &[IndexedSource<'_>]) -> CodeIndex {
        let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
        let mut roles: HashMap<Span, Vec<String>> = HashMap::new();
        let mut attributes: HashMap<Span, Vec<String>> = HashMap::new();
        let mut docs: HashMap<Span, String> = HashMap::new();
        for program in programs {
            let info = noeta_ast::reflect::build(program, &native_roles, &Default::default());
            for r in &info.roles {
                let bound = format!("{}.{}", r.enum_name, r.variant);
                let have = roles.entry(r.target_span).or_default();
                if !have.contains(&bound) {
                    have.push(bound);
                }
            }
            for a in &info.manifest {
                let have = attributes.entry(a.target_span).or_default();
                if !have.contains(&a.name) {
                    have.push(a.name.clone());
                }
            }
            // `@doc { … }` prose, keyed by the name span of what it documents — the key the doc
            // resolver itself reports, so prose lands on its declaration rather than on a name
            // that happens to match.
            for block in noeta_check::resolve_docs(program) {
                if let noeta_check::DocTarget::Decl { name_span, .. } = block.target {
                    let text = noeta_check::dedent_doc(&block.text).trim().to_string();
                    let prose = docs.entry(name_span).or_default();
                    if prose.contains(&text) {
                        continue;
                    }
                    if !prose.is_empty() {
                        prose.push('\n');
                    }
                    prose.push_str(&text);
                }
            }
        }

        let mut walk = Walk {
            sources,
            roles: &roles,
            attributes: &attributes,
            docs: &docs,
            seen: std::collections::HashSet::new(),
            decls: Vec::new(),
        };
        for program in programs {
            walk.stmts(&program.stmts, None);
        }
        let decls = walk.decls;

        let fields = CodeField::ALL.iter().map(|f| f.params()).collect();
        let bm25 = Bm25f::build(
            fields,
            decls.iter().map(|d| {
                use std::borrow::Cow;
                vec![
                    Cow::Borrowed(d.leaf.as_str()),
                    Cow::Borrowed(d.name.as_str()),
                    Cow::Owned(match &d.tier {
                        Some(tier) => format!("{} {tier}", d.kind),
                        None => d.kind.to_string(),
                    }),
                    Cow::Owned(format!("{} {}", d.roles.join(" "), d.attributes.join(" "))),
                    Cow::Borrowed(d.doc.as_str()),
                    Cow::Borrowed(d.signature.as_str()),
                    Cow::Borrowed(d.body.as_str()),
                    Cow::Borrowed(d.file.as_str()),
                ]
            }),
        );
        CodeIndex { decls, bm25 }
    }

    pub fn decls(&self) -> &[CodeDecl] {
        &self.decls
    }

    /// Rank the declarations `filter` keeps against `query`, best first, at most `limit` of them.
    ///
    /// Ordering is total and deterministic: score descending, then name, then declaration order,
    /// so two runs over the same program return the same list in the same order.
    pub fn search(&self, query: &str, filter: &SearchFilter, limit: usize) -> Vec<CodeHit> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let phrase = phrase_needle(query);
        let mut hits: Vec<CodeHit> = Vec::new();
        for (i, decl) in self.decls.iter().enumerate() {
            if !filter.keeps(decl) {
                continue;
            }
            let mut score = self.bm25.score(i, &terms);
            if score <= 0.0 {
                continue;
            }
            score *= name_boost(&decl.leaf, &terms);
            if phrase
                .as_ref()
                .is_some_and(|needle| self.bm25.contains_phrase(i, needle))
            {
                score *= PHRASE_BOOST;
            }
            let matched = self
                .bm25
                .matched_fields(i, &terms)
                .into_iter()
                .map(|f| CodeField::ALL[f])
                .collect();
            let evidence = if decl.doc.trim().is_empty() {
                &decl.signature
            } else {
                &decl.doc
            };
            hits.push(CodeHit {
                decl: i,
                score,
                matched,
                snippet: snippet(evidence, &terms),
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| self.decls[a.decl].name.cmp(&self.decls[b.decl].name))
                .then(a.decl.cmp(&b.decl))
        });
        hits.truncate(limit);
        hits
    }
}

/// The multiplier a leaf name earns from the query naming it: exact first, then a prefix.
fn name_boost(leaf: &str, terms: &[String]) -> f32 {
    let leaf = leaf.to_ascii_lowercase();
    if terms.contains(&leaf) {
        return EXACT_NAME_BOOST;
    }
    if terms
        .iter()
        .any(|t| t.len() >= MIN_PREFIX && leaf.starts_with(t.as_str()))
    {
        return PREFIX_NAME_BOOST;
    }
    1.0
}

/// The declaration's own source text, with each nested declaration cut out of it — a type's body
/// text is the type, not the concatenated text of every method it holds.
///
/// Slicing the source rather than walking the expression tree is deliberate: it reaches call
/// targets, member accesses, string-literal words and comments in one pass, and a new expression
/// form cannot silently fall out of the index the way an unvisited AST variant would.
fn body_text(sources: &[IndexedSource<'_>], decl_span: Span, children: &[Span]) -> String {
    let Some(source) = sources.get(decl_span.source.0 as usize) else {
        return String::new();
    };
    let text = source.text;
    let end = (decl_span.end as usize).min(text.len());
    let mut cursor = (decl_span.start as usize).min(end);
    let mut holes: Vec<&Span> = children
        .iter()
        .filter(|c| c.source == decl_span.source)
        .collect();
    holes.sort_by_key(|c| c.start);
    let mut out = String::new();
    for hole in holes {
        let (from, to) = (hole.start as usize, (hole.end as usize).min(end));
        if from < cursor || to > end {
            continue;
        }
        if let Some(part) = text.get(cursor..from) {
            out.push_str(part);
        }
        cursor = to;
    }
    if let Some(part) = text.get(cursor..end) {
        out.push_str(part);
    }
    out
}

/// The walk that turns a program into the declaration inventory, in source order.
///
/// It names every declaration the way the linked program does — a method as `Type.method`, a
/// variant as `Enum.Variant` — so a hit's name is one `trace`, `impact` and `callers` accept, and
/// it descends into `@tier { … }` blocks so a fixture or a test function is searchable like any
/// other declaration.
struct Walk<'a> {
    sources: &'a [IndexedSource<'a>],
    roles: &'a HashMap<Span, Vec<String>>,
    attributes: &'a HashMap<Span, Vec<String>>,
    docs: &'a HashMap<Span, String>,
    /// The name spans already indexed, so a second program naming the same declaration does not
    /// index it twice.
    seen: std::collections::HashSet<Span>,
    decls: Vec<CodeDecl>,
}

impl Walk<'_> {
    fn stmts(&mut self, stmts: &[Stmt], tier: Option<&str>) {
        for stmt in stmts {
            match stmt {
                Stmt::Fn(decl) => {
                    self.function(decl, decl.name.to_string(), DeclKind::Function, tier)
                }
                Stmt::Struct(decl) => {
                    let name = decl.name.to_string();
                    self.container(
                        &name,
                        DeclKind::Struct,
                        decl.name_span,
                        decl.span,
                        format!("struct {name}"),
                        &member_spans(&decl.fields, &decl.methods),
                        tier,
                    );
                    self.members(&name, &decl.fields, &decl.methods, tier);
                }
                Stmt::Class(decl) => {
                    let name = decl.name.to_string();
                    self.container(
                        &name,
                        DeclKind::Class,
                        decl.name_span,
                        decl.span,
                        format!("class {name}"),
                        &member_spans(&decl.fields, &decl.methods),
                        tier,
                    );
                    self.members(&name, &decl.fields, &decl.methods, tier);
                }
                Stmt::Enum(decl) => self.enumeration(decl, tier),
                Stmt::Trait(decl) => {
                    let name = decl.name.to_string();
                    let children: Vec<Span> = decl.methods.iter().map(|m| m.sig.span).collect();
                    self.container(
                        &name,
                        DeclKind::Trait,
                        decl.name_span,
                        decl.span,
                        format!("trait {name}"),
                        &children,
                        tier,
                    );
                    for method in &decl.methods {
                        self.function(
                            &method.sig,
                            format!("{name}.{}", method.sig.name),
                            DeclKind::Method,
                            tier,
                        );
                    }
                }
                Stmt::Impl(decl) => {
                    let name = format!("{} for {}", decl.trait_name, decl.target);
                    let children: Vec<Span> = decl.methods.iter().map(|m| m.span).collect();
                    self.container(
                        &name,
                        DeclKind::Impl,
                        decl.trait_span,
                        decl.span,
                        format!("impl {name}"),
                        &children,
                        tier,
                    );
                    // An impl method is named the way the call graph names it, so one declaration
                    // has one identity across both.
                    for method in &decl.methods {
                        self.function(
                            method,
                            format!("{}.{}", decl.target, method.name),
                            DeclKind::Method,
                            tier,
                        );
                    }
                }
                Stmt::TierBlock {
                    tier: name, items, ..
                } => self.stmts(items, Some(name)),
                _ => {}
            }
        }
    }

    fn members(
        &mut self,
        owner: &str,
        fields: &[FieldDecl],
        methods: &[FnDecl],
        tier: Option<&str>,
    ) {
        for field in fields {
            let ty = field
                .ty
                .as_ref()
                .map(crate::symbols::render_type_ref)
                .unwrap_or_default();
            self.push(
                format!("{owner}.{}", field.name),
                DeclKind::Field,
                field.name_span,
                field.span,
                format!("{}: {ty}", field.name),
                &[],
                tier,
            );
        }
        for method in methods {
            self.function(
                method,
                format!("{owner}.{}", method.name),
                DeclKind::Method,
                tier,
            );
        }
    }

    fn enumeration(&mut self, decl: &EnumDecl, tier: Option<&str>) {
        let name = decl.name.to_string();
        let mut children: Vec<Span> = decl.variants.iter().map(|v| v.span).collect();
        children.extend(decl.methods.iter().map(|m| m.span));
        self.container(
            &name,
            DeclKind::Enum,
            decl.name_span,
            decl.span,
            format!("enum {name}"),
            &children,
            tier,
        );
        for variant in &decl.variants {
            self.variant(&name, variant, tier);
        }
        for method in &decl.methods {
            self.function(
                method,
                format!("{name}.{}", method.name),
                DeclKind::Method,
                tier,
            );
        }
    }

    fn variant(&mut self, owner: &str, variant: &VariantDecl, tier: Option<&str>) {
        let payload = crate::symbols::variant_detail(variant).unwrap_or_default();
        self.push(
            format!("{owner}.{}", variant.name),
            DeclKind::Variant,
            variant.name_span,
            variant.span,
            format!("{}{payload}", variant.name),
            &[],
            tier,
        );
    }

    fn function(&mut self, decl: &FnDecl, name: String, kind: DeclKind, tier: Option<&str>) {
        let signature = format!("fn {}{}", decl.name, crate::symbols::fn_signature(decl));
        self.push(name, kind, decl.name_span, decl.span, signature, &[], tier);
    }

    /// A declaration that holds other declarations: its body text stops at each member's span, so
    /// a type is indexed on what the type says rather than on everything written inside it.
    #[allow(clippy::too_many_arguments)] // Each argument is one fact about one declaration.
    fn container(
        &mut self,
        name: &str,
        kind: DeclKind,
        name_span: Span,
        decl_span: Span,
        signature: String,
        children: &[Span],
        tier: Option<&str>,
    ) {
        self.push(
            name.to_string(),
            kind,
            name_span,
            decl_span,
            signature,
            children,
            tier,
        );
    }

    #[allow(clippy::too_many_arguments)] // Each argument is one fact about one declaration.
    fn push(
        &mut self,
        name: String,
        kind: DeclKind,
        name_span: Span,
        decl_span: Span,
        signature: String,
        children: &[Span],
        tier: Option<&str>,
    ) {
        if !self.seen.insert(name_span) {
            return;
        }
        let leaf = name.rsplit('.').next().unwrap_or(&name).to_string();
        let file = self
            .sources
            .get(name_span.source.0 as usize)
            .map(|s| s.name.to_string())
            .unwrap_or_default();
        self.decls.push(CodeDecl {
            leaf,
            kind,
            name_span,
            decl_span,
            tier: tier.map(str::to_string),
            roles: self.roles.get(&name_span).cloned().unwrap_or_default(),
            attributes: self.attributes.get(&name_span).cloned().unwrap_or_default(),
            signature,
            doc: self.docs.get(&name_span).cloned().unwrap_or_default(),
            body: body_text(self.sources, decl_span, children),
            file,
            name,
        });
    }
}

/// The declaration spans of a type's members, for the hole list its own body text skips.
fn member_spans(fields: &[FieldDecl], methods: &[FnDecl]) -> Vec<Span> {
    fields
        .iter()
        .map(|f| f.span)
        .chain(methods.iter().map(|m| m.span))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_loader::ModulePath;
    use noeta_span::{Source, SourceId};

    /// Link `files` (the entry first) and index the workspace: the linked program for its
    /// qualified names, then every member's own parse so a sibling nothing imports is searchable
    /// too. The same pair the `code_search` tool indexes.
    fn index_of(files: &[(&str, &str)]) -> CodeIndex {
        let db = noeta_db::LangDatabase::default();
        let sources: Vec<Source> = files
            .iter()
            .enumerate()
            .map(|(i, (name, text))| Source::new(SourceId(i as u32), *name, *text))
            .collect();
        let paths: Vec<ModulePath> = files
            .iter()
            .map(|(name, _)| ModulePath::Derived(vec![name.trim_end_matches(".noe").to_string()]))
            .collect();
        let ws = noeta_db::workspace(
            &db,
            &sources[0],
            &sources[1..],
            noeta_lexer::Edition::DEFAULT,
            &paths,
        );
        let linked = match &noeta_db::linked(&db, ws).program {
            Ok(program) => program.clone(),
            Err(diagnostics) => panic!("the fixture must link: {diagnostics:?}"),
        };
        let members: Vec<noeta_ast::Program> = ws
            .members(&db)
            .iter()
            .map(|m| noeta_db::ast_in(&db, ws, *m).0.program.clone())
            .collect();
        let mut programs: Vec<&noeta_ast::Program> = vec![&linked];
        programs.extend(members.iter());
        let indexed: Vec<IndexedSource<'_>> = sources
            .iter()
            .map(|s| IndexedSource {
                name: s.name(),
                text: s.text(),
            })
            .collect();
        CodeIndex::build_over(&programs, &indexed)
    }

    /// One declaration's score for `query`, or zero when the query does not reach it.
    fn score_of(index: &CodeIndex, query: &str, name: &str) -> f32 {
        index
            .search(query, &SearchFilter::default(), 50)
            .into_iter()
            .find(|h| index.decls()[h.decl].name == name)
            .map(|h| h.score)
            .unwrap_or(0.0)
    }

    /// The names of the ranked hits, best first.
    fn ranked(index: &CodeIndex, query: &str, filter: &SearchFilter, limit: usize) -> Vec<String> {
        index
            .search(query, filter, limit)
            .into_iter()
            .map(|h| index.decls()[h.decl].name.clone())
            .collect()
    }

    const MAIN: &str = "\
use orders
use billing

@attribute(Function)
@role(Semantic.EntryPoint)
struct Route { path: string }

@doc { The service entry point every request arrives at. }
#[Route(\"/orders\")]
fn handle(id: int): int { return orders.place_order(id) + billing.place_order(id) }
";

    const ORDERS: &str = "\
@doc { Persists a completed purchase to the durable ledger on disk. }
pub fn place_order(id: int): int { return id }

@doc { One purchase, while it is still being assembled. }
pub struct Order {
    id: int
    fn total(): int { return self.id }
}
";

    const BILLING: &str = "\
@doc { Charges a stored card for the amount owed. }
pub fn place_order(amount: int): int { return amount }

@doc { The audit record written for one charge. }
pub struct Receipt { total: int }
";

    fn fixture() -> CodeIndex {
        index_of(&[
            ("main.noe", MAIN),
            ("orders.noe", ORDERS),
            ("billing.noe", BILLING),
        ])
    }

    /// Every declaration the three modules hold is in the index, under its post-link name.
    #[test]
    fn the_index_holds_every_declaration_of_the_linked_program() {
        let index = fixture();
        let names: Vec<&str> = index.decls().iter().map(|d| d.name.as_str()).collect();
        for want in [
            "orders.place_order",
            "billing.place_order",
            "orders.Order",
            "orders.Order.total",
            "billing.Receipt.total",
        ] {
            assert!(names.contains(&want), "missing {want}: {names:?}");
        }
    }

    /// A module the entry never imports is still part of the project, so it is still searchable —
    /// the linked program alone reaches only what the entry pulls in.
    #[test]
    fn a_sibling_nothing_imports_is_still_indexed() {
        let index = index_of(&[
            ("main.noe", "fn handle(id: int): int { return id }\n"),
            (
                "audit.noe",
                "@doc { Appends one line to the tamper-evident journal. }\npub fn record(n: int): int { return n }\n",
            ),
        ]);
        let names: Vec<&str> = index.decls().iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"record"), "{names:?}");
        let hits = ranked(
            &index,
            "append a line to the tamper-evident journal",
            &SearchFilter::default(),
            3,
        );
        assert_eq!(hits.first().map(String::as_str), Some("record"), "{hits:?}");
    }

    /// The seed-mapping question: a sentence with no identifier in it lands on the declaration
    /// whose `@doc` prose answers it, and not on the two declarations named for the same domain.
    #[test]
    fn a_query_with_no_identifier_finds_the_declaration_its_prose_describes() {
        let index = fixture();
        let hits = ranked(
            &index,
            "which code writes a completed purchase to the durable ledger",
            &SearchFilter::default(),
            5,
        );
        assert_eq!(
            hits.first().map(String::as_str),
            Some("orders.place_order"),
            "{hits:?}"
        );
    }

    /// Two modules declare `place_order` and two types declare `total`. In both cases the owner
    /// the query names decides which one leads.
    ///
    /// The two halves reach different fields. A module name is in the qualified name *and* in the
    /// file path, so either can carry it; a type name is in the qualified name alone. The type
    /// half compares scores rather than positions, because equal scores order by name and would
    /// hand `billing.Receipt.total` the lead without ranking it at all.
    #[test]
    fn a_colliding_leaf_ranks_by_the_owner_the_query_names() {
        let index = fixture();
        let leading = |query: &str, leaf: &str| -> Option<String> {
            let suffix = format!(".{leaf}");
            ranked(&index, query, &SearchFilter::default(), 10)
                .into_iter()
                .find(|n| n.ends_with(&suffix))
        };
        assert_eq!(
            leading("billing place_order", "place_order").as_deref(),
            Some("billing.place_order")
        );
        assert_eq!(
            leading("orders place_order", "place_order").as_deref(),
            Some("orders.place_order")
        );
        // `Receipt` names no file and appears in no member's own text, so the qualified name is
        // the only field that can carry it. Naming the owner must raise that member's score, and
        // must leave the same-named member of another type where it was.
        let named = score_of(&index, "receipt total", "billing.Receipt.total");
        let bare = score_of(&index, "total", "billing.Receipt.total");
        assert!(
            named > bare,
            "naming the owner must reach its member: {named} vs {bare}"
        );
        let rival = score_of(&index, "receipt total", "orders.Order.total");
        assert_eq!(
            rival,
            score_of(&index, "total", "orders.Order.total"),
            "another type's member must not move when `Receipt` is named"
        );
        assert_eq!(
            leading("order total", "total").as_deref(),
            Some("orders.Order.total")
        );
    }

    /// Typing a name means "that declaration": the two functions called `place_order` outrank the
    /// entry point whose prose and body are about placing an order.
    #[test]
    fn an_exact_name_outranks_a_prose_match() {
        let index = fixture();
        let hits = ranked(&index, "place_order", &SearchFilter::default(), 5);
        assert!(
            hits.iter().take(2).all(|n| n.ends_with(".place_order")),
            "the named declarations must lead: {hits:?}"
        );
    }

    /// A name written **inside a sentence** still returns the declaration that bears it, against a
    /// declaration whose prose answers more of the sentence's other words.
    ///
    /// This is what the exact-name boost buys. BM25 saturates each term, so a document matching
    /// four of the query's words outscores one matching a single word however strongly, and the
    /// declaration the reader actually named would come second.
    #[test]
    fn a_name_inside_a_sentence_still_returns_its_declaration() {
        let index = index_of(&[
            ("main.noe", "fn run(id: int): int { return id }\n"),
            (
                "orders.noe",
                "pub fn checkout(id: int): int { return id }\n",
            ),
            (
                "mail.noe",
                "@doc { Emails the buyer about the pending basket. }\n\
                 pub fn notify(id: int): int { return id }\n",
            ),
        ]);
        let hits = ranked(
            &index,
            "checkout the pending basket and email the buyer",
            &SearchFilter::default(),
            5,
        );
        assert_eq!(hits.first().map(String::as_str), Some("checkout"), "{hits:?}");
    }

    /// `kind` narrows to one shape of declaration, and drops the functions a bare query returns.
    #[test]
    fn a_kind_filter_narrows_the_answer() {
        let index = fixture();
        let unfiltered = ranked(&index, "purchase", &SearchFilter::default(), 10);
        assert!(
            unfiltered.iter().any(|n| n == "orders.place_order"),
            "{unfiltered:?}"
        );
        let structs = ranked(
            &index,
            "purchase",
            &SearchFilter {
                kind: Some(DeclKind::Struct),
                ..SearchFilter::default()
            },
            10,
        );
        assert!(!structs.is_empty(), "a struct answers this query");
        for name in &structs {
            let decl = index
                .decls()
                .iter()
                .find(|d| &d.name == name)
                .expect("hit resolves");
            assert_eq!(decl.kind, DeclKind::Struct, "{structs:?}");
        }
    }

    /// `roles` keeps only the declarations bearing the role, matching a bare variant against the
    /// qualified binding the reflection index carries.
    #[test]
    fn a_role_filter_keeps_only_the_role_bearers() {
        let index = fixture();
        let entry = ranked(
            &index,
            "order",
            &SearchFilter {
                roles: vec!["EntryPoint".to_string()],
                ..SearchFilter::default()
            },
            10,
        );
        assert_eq!(entry, vec!["main.handle".to_string()], "{entry:?}");
        let qualified = ranked(
            &index,
            "order",
            &SearchFilter {
                roles: vec!["Semantic.EntryPoint".to_string()],
                ..SearchFilter::default()
            },
            10,
        );
        assert_eq!(qualified, entry, "a qualified role names the same set");
    }

    /// A hit says which fields earned it, so a reader can tell a name match from a prose one.
    #[test]
    fn a_hit_reports_the_fields_that_matched() {
        let index = fixture();
        let hits = index.search("durable ledger", &SearchFilter::default(), 3);
        let top = hits.first().expect("a hit");
        assert!(top.matched.contains(&CodeField::Doc), "{:?}", top.matched);
        assert!(!top.snippet.is_empty());
    }

    /// A type's body text stops at its members, so the type is indexed on what the type says.
    #[test]
    fn a_types_body_text_excludes_its_members() {
        let index = fixture();
        let order = index
            .decls()
            .iter()
            .find(|d| d.name == "orders.Order")
            .expect("the struct is indexed");
        assert!(order.body.contains("struct Order"), "{}", order.body);
        assert!(
            !order.body.contains("self.id"),
            "the method body belongs to the method: {}",
            order.body
        );
    }
}
