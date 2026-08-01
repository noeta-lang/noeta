//! **The language's vocabulary, and the four editor files that restate it.**
//!
//! The lexer's `#[token("…")]` declarations, [`noeta_ast::BuiltinTy`] and
//! [`noeta_builtins::PRELUDE`] are the three censuses of what Noeta's words *are*. Every other
//! list is a copy. Inside Rust the copies are now derivations — `highlight::is_keyword` asks the
//! token, `completion::keywords` filters the census, `noeta-check`'s prelude set filters
//! `PRELUDE` — and a derivation cannot drift. Two files are not Rust: the VS Code TextMate grammar
//! (JSON) and the tree-sitter grammar with its highlight queries (JavaScript and S-expressions).
//! They cannot be compile-forced, so they are **generated from the same censuses and checked
//! against them here**.
//!
//! # Why generate rather than census
//!
//! The audit that opened this row sized the fix as a census — "grep the two grammar files,
//! assert coverage in both directions with an allow-list". A census would have caught the four
//! live drifts, but it leaves the fixing to a human editing by hand the files whose disease is
//! silent hand-editing. Generation removes the hand: [`regions`] declares each managed
//! span of each file, this test renders what it *should* contain from the censuses, and
//! `NOETA_UPDATE_EDITOR_VOCABULARY=1 cargo test -p noeta-ide --test editor_vocabulary` writes it.
//! Checking is the default; the same code does both, so the check can never disagree with the
//! generator. It is the technique the repo already ships for tiers (`noeta grammar tree-sitter`
//! emits `project-tiers.json` and `generated-tiers.tmLanguage.json`), pointed at vocabulary.
//!
//! Generation is not available everywhere, and where it is not, this file says so and censuses
//! instead. A keyword in `grammar.js` is not a list entry — it is a literal inside the *production
//! rule that uses it* (`'fn'` lives in `function_declaration`), and no generator can place it
//! there. So the 33 word tokens `grammar.js` carries are checked for **coverage in both
//! directions** against the lexer, with [`grammar_js_omissions`] carrying the rule behind each
//! deliberate absence. That is the audit's census, applied only where generation cannot reach.
//!
//! # What this would have caught
//!
//! Every drift the audit found, by name: `Any` missing from both grammars' built-in type lists;
//! `Enum`/`Struct`/`Class` missing from TextMate's; TextMate's prelude rule still matching
//! `signal|computed|effect|len` (gone from the prelude two arcs ago) and never matching
//! `Ok`/`Err`/`some`/`none`; the thirteen reflection intrinsics highlighted by TextMate and by
//! nothing at all in tree-sitter, so VS Code and Neovim coloured the same file differently. Plus
//! two the audit did not find: `type` was in `grammar.js` but absent from `highlights.scm`, so
//! `type Meters = float` never coloured its keyword under tree-sitter, and the two grammars
//! disagreed about `await` (a control-flow keyword in TextMate, a coroutine keyword in
//! tree-sitter).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use noeta_ast::BuiltinTy;
use noeta_builtins::{PreludeForm, prelude_names};
use noeta_lexer::{ReservedRole, ReservedWord};

// ---------------------------------------------------------------------------------------------
// The vocabulary model: one classification, read by both grammars.
// ---------------------------------------------------------------------------------------------

/// The colour family a reserved word belongs to.
///
/// This is the one piece of vocabulary knowledge that is **not** derivable from the compiler: no
/// pass cares whether `while` is "control flow" and `spawn` is "concurrency", but every syntax
/// highlighter does. Before this it was written out four times — twice in TextMate scopes, twice
/// in tree-sitter captures — and the two grammars had already disagreed (TextMate filed `await`
/// under control flow, tree-sitter under coroutines, so the same word was a different colour in
/// two editors).
///
/// Held once, keyed by the word's own [`ReservedRole`] where the role already decides it, and by
/// [`family_of`] where it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    /// Introduces a declaration or binding: `fn`, `struct`, `type`, `mut`, `destruct`, …
    Declaration,
    /// Brings names in or out: `use`, `namespace`, `pub`.
    Import,
    /// Directs control: `if`, `for`, `match`, `return`, `yield`, …
    ControlFlow,
    /// Coroutines and structured concurrency: `async`, `await`, `spawn`, `isolate`, `concurrent`.
    Concurrency,
    /// `echo` — the one statement keyword that is neither.
    Echo,
    /// Spelled like a word, used like an operator: `as`, `is`.
    OperatorWord,
    /// `true` / `false`.
    Boolean,
    /// The reflection primitives (`type_of`, `field_specs_of`, …).
    Reflection,
}

/// The colour family of one reserved word.
///
/// Two of the eight families are decided by the lexer itself — a word's [`ReservedRole`] already
/// says whether it is a boolean literal or a reflection primitive, so those are not restated. The
/// rest are a genuine editorial judgement and are made here, once, for both grammars.
///
/// A **new keyword lands here as a named failure** rather than silently in whichever bucket a
/// wildcard would have swept it into: the `_` arm panics with the word, so adding one to the lexer
/// fails this test until somebody decides what colour it is.
fn family_of(word: ReservedWord) -> Family {
    match word.role {
        ReservedRole::BooleanLiteral => return Family::Boolean,
        ReservedRole::Reflection => return Family::Reflection,
        ReservedRole::Keyword => {}
    }
    match word.word {
        "fn" | "mut" | "struct" | "class" | "enum" | "type" | "impl" | "trait" | "destruct" => {
            Family::Declaration
        }
        "use" | "namespace" | "pub" => Family::Import,
        "if" | "then" | "else" | "for" | "while" | "break" | "continue" | "in" | "match"
        | "return" | "yield" => Family::ControlFlow,
        "async" | "await" | "spawn" | "isolate" | "concurrent" => Family::Concurrency,
        "echo" => Family::Echo,
        "as" | "is" => Family::OperatorWord,
        other => panic!(
            "`{other}` is a new reserved word with no editor colour family. Add it to \
             `family_of` in crates/noeta-ide/tests/editor_vocabulary.rs — every syntax \
             highlighter needs to know what colour it is, and until it does, `{other}` \
             renders as a plain identifier in VS Code, Neovim, Helix and Zed alike."
        ),
    }
}

/// Every reserved word of one family, in the lexer's own declaration order.
fn words_in(family: Family) -> Vec<&'static str> {
    ReservedWord::all()
        .into_iter()
        .filter(|w| family_of(*w) == family)
        .map(|w| w.word)
        .collect()
}

/// The **scalar** built-in type spellings — the ones that are types wherever they appear.
///
/// Decoded through [`BuiltinTy`] rather than listed, exactly as `highlight::is_primitive_type_name`
/// does, so the two agree by construction. Includes every spelling, so the `dyn`/`Any` and
/// `void`/`unit` aliases are both covered — `Any` was missing from both grammars.
fn primitive_type_names() -> Vec<String> {
    BuiltinTy::all()
        .into_iter()
        .filter(|ty| {
            matches!(
                ty,
                BuiltinTy::Int
                    | BuiltinTy::Float
                    | BuiltinTy::F32
                    | BuiltinTy::F64
                    | BuiltinTy::IntN { .. }
                    | BuiltinTy::Bool
                    | BuiltinTy::Str
                    | BuiltinTy::Bytes
                    | BuiltinTy::Unit
                    | BuiltinTy::Dyn
                    | BuiltinTy::Never
                    | BuiltinTy::Number
            )
        })
        .flat_map(spellings_of)
        .collect()
}

/// The **container and kind** built-in type spellings, canonical only.
///
/// The bare `list`/`map`/`set` spellings are deliberately excluded — they collide with the
/// like-named methods (`xs.map(f)`) and would steal the colour from a call. That is the same
/// policy `highlight::is_primitive_type_name` documents, stated once and applied to both grammars.
fn container_type_names() -> Vec<String> {
    BuiltinTy::all()
        .into_iter()
        .filter(|ty| {
            matches!(
                ty,
                BuiltinTy::List
                    | BuiltinTy::Set
                    | BuiltinTy::Map
                    | BuiltinTy::Option
                    | BuiltinTy::Result
                    | BuiltinTy::KindEnum
                    | BuiltinTy::KindStruct
                    | BuiltinTy::KindClass
            )
        })
        .map(|ty| {
            spellings_of(ty)
                .into_iter()
                .next()
                .expect("a container has a canonical spelling")
        })
        .collect()
}

/// Every surface spelling of one built-in constructor, the fixed-width family expanded.
fn spellings_of(ty: BuiltinTy) -> Vec<String> {
    match ty {
        BuiltinTy::IntN { signed, bits } => vec![BuiltinTy::int_width_name(signed, bits)],
        other => other.spellings().iter().map(|s| (*s).to_string()).collect(),
    }
}

/// The prelude names a program can *call* — the value half of [`noeta_builtins::PRELUDE`].
fn prelude_value_names() -> Vec<&'static str> {
    prelude_names(PreludeForm::Value).collect()
}

// ---------------------------------------------------------------------------------------------
// The managed regions.
// ---------------------------------------------------------------------------------------------

/// One span of an editor file whose content is derived from a census.
struct Region {
    /// Repo-relative path, for the failure message.
    path: &'static str,
    /// What this region is, in the failure message.
    what: &'static str,
    /// The rendered text the censuses say it should hold.
    expected: String,
    /// How to find the current text in the file.
    locate: Locate,
}

/// How a region is located in its file — the two file formats need different anchors.
enum Locate {
    /// A TextMate pattern's `match` string, found by the pattern's `name` scope. The scope is
    /// unique in the file, so the anchor is exact without parsing JSON structurally.
    TextMateScope(&'static str),
    /// Everything between two marker lines, exclusive.
    Between {
        begin: &'static str,
        end: &'static str,
    },
}

/// `\b(a|b|c)\b` — the TextMate alternation shape every keyword and type rule uses.
fn alternation(words: &[impl AsRef<str>]) -> String {
    let body = words
        .iter()
        .map(|w| w.as_ref())
        .collect::<Vec<_>>()
        .join("|");
    format!("\\b({body})\\b")
}

/// Every managed region of every editor file, with the text the censuses derive for it.
fn regions() -> Vec<Region> {
    const TM: &str = "editors/vscode-noeta/syntaxes/noeta.tmLanguage.json";
    const SCM: &str = "editors/tree-sitter-noeta/queries/highlights.scm";
    const JS: &str = "editors/tree-sitter-noeta/grammar.js";

    let mut out = vec![
        // -- TextMate: keywords, one pattern per colour family ---------------------------------
        Region {
            path: TM,
            what: "the control-flow keywords",
            expected: alternation(&words_in(Family::ControlFlow)),
            locate: Locate::TextMateScope("keyword.control.flow.noeta"),
        },
        Region {
            path: TM,
            what: "the concurrency keywords",
            expected: alternation(&words_in(Family::Concurrency)),
            locate: Locate::TextMateScope("keyword.control.concurrency.noeta"),
        },
        Region {
            path: TM,
            what: "the `echo` keyword",
            expected: alternation(&words_in(Family::Echo)),
            locate: Locate::TextMateScope("keyword.other.echo.noeta"),
        },
        Region {
            path: TM,
            what: "the import keywords",
            expected: alternation(&words_in(Family::Import)),
            locate: Locate::TextMateScope("keyword.control.import.noeta"),
        },
        Region {
            path: TM,
            what: "the word operators",
            expected: alternation(&words_in(Family::OperatorWord)),
            locate: Locate::TextMateScope("keyword.operator.word.noeta"),
        },
        Region {
            path: TM,
            what: "the reflection primitives",
            expected: alternation(&words_in(Family::Reflection)),
            locate: Locate::TextMateScope("keyword.other.reflection.noeta"),
        },
        Region {
            path: TM,
            what: "the declaration keywords",
            expected: alternation(&words_in(Family::Declaration)),
            locate: Locate::TextMateScope("storage.type.noeta"),
        },
        Region {
            path: TM,
            what: "the boolean literals",
            expected: alternation(&words_in(Family::Boolean)),
            locate: Locate::TextMateScope("constant.language.boolean.noeta"),
        },
        // -- TextMate: built-in types and the prelude ------------------------------------------
        Region {
            path: TM,
            what: "the scalar built-in type names",
            expected: alternation(&primitive_type_names()),
            locate: Locate::TextMateScope("support.type.primitive.noeta"),
        },
        Region {
            path: TM,
            what: "the container and kind built-in type names",
            expected: alternation(&container_type_names()),
            locate: Locate::TextMateScope("support.type.builtin.noeta"),
        },
        Region {
            path: TM,
            what: "the prelude functions",
            expected: format!("\\b({})\\s*(?=\\()", prelude_value_names().join("|")),
            locate: Locate::TextMateScope("support.function.builtin.noeta"),
        },
    ];

    // -- tree-sitter: the highlight query's keyword captures ------------------------------------
    //
    // Grouped by capture rather than by family: three families share `@keyword`, so they share one
    // bracket list. The boolean literals are absent on purpose (see `scm_omissions`).
    let mut scm = String::new();
    for (capture, families) in SCM_CAPTURES {
        let words: Vec<&str> = families.iter().flat_map(|f| words_in(*f)).collect();
        scm.push_str("[\n");
        for chunk in words.chunks(8) {
            scm.push_str("  ");
            scm.push_str(
                &chunk
                    .iter()
                    .map(|w| format!("\"{w}\""))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            scm.push('\n');
        }
        scm.push_str(&format!("] {capture}\n\n"));
    }
    // The reflection primitives are not grammar tokens — `grammar.js` parses `type_of::<T>()` as an
    // ordinary `turbofish_call` over an identifier — so they cannot be captured as anonymous nodes
    // the way every other keyword is. A predicate over the identifier is the only form available,
    // and its absence is why VS Code coloured them and Neovim did not.
    scm.push_str(
        "; The reflection primitives are identifiers to the grammar (`turbofish_call` takes an\n\
         ; `identifier`), so they are captured by spelling rather than as anonymous keyword nodes.\n\
         ((identifier) @function.builtin\n  (#any-of? @function.builtin\n",
    );
    for chunk in words_in(Family::Reflection).chunks(4) {
        scm.push_str("    ");
        scm.push_str(
            &chunk
                .iter()
                .map(|w| format!("\"{w}\""))
                .collect::<Vec<_>>()
                .join(" "),
        );
        scm.push('\n');
    }
    scm.push_str("  ))\n");
    out.push(Region {
        path: SCM,
        what: "the keyword highlight captures",
        expected: scm,
        locate: Locate::Between {
            begin: "; --- BEGIN GENERATED VOCABULARY ---",
            end: "; --- END GENERATED VOCABULARY ---",
        },
    });

    // -- tree-sitter: the `primitive_type` rule --------------------------------------------------
    //
    // The one part of `grammar.js` that IS a flat vocabulary list, so the one part that can be
    // generated. Everything else in that file is vocabulary embedded in structure.
    let mut js = String::new();
    js.push_str(
        "    // `never` is a type NAME, not a keyword: an ordinary identifier spelled `never`\n\
         \x20   // elsewhere still parses as one, since `$.identifier` is also a `$._type` and the\n\
         \x20   // grammar declares `word: $.identifier`, so these literals are only recognised\n\
         \x20   // where a type is expected. The same is true of `unit`, `number` and `Any`.\n\
         \x20   primitive_type: _ => choice(\n",
    );
    for chunk in primitive_type_names().chunks(8) {
        js.push_str("      ");
        js.push_str(
            &chunk
                .iter()
                .map(|w| format!("'{w}',"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        js.push('\n');
    }
    js.push_str("    ),\n");
    out.push(Region {
        path: JS,
        what: "the `primitive_type` rule",
        expected: js,
        locate: Locate::Between {
            begin: "    // --- BEGIN GENERATED VOCABULARY ---",
            end: "    // --- END GENERATED VOCABULARY ---",
        },
    });

    out
}

/// Which colour families each tree-sitter capture collects. Three families share `@keyword`; the
/// booleans and the reflection primitives are handled outside this table.
const SCM_CAPTURES: &[(&str, &[Family])] = &[
    (
        "@keyword",
        &[Family::Declaration, Family::Import, Family::Echo],
    ),
    ("@keyword.control", &[Family::ControlFlow]),
    ("@keyword.coroutine", &[Family::Concurrency]),
    ("@keyword.operator", &[Family::OperatorWord]),
];

/// Reserved words `grammar.js` deliberately does not carry as a token, **with the reason**.
///
/// An allow-list without a reason is a list of things nobody has looked at yet; the reason is what
/// lets a later reader tell "deliberate" from "forgotten". And an allow-list of *names* is one more
/// hand list to forget, which is the disease this whole file treats — so the entries are derived
/// from the property that makes them omissible. The reason is not prose *about* the list; it is the
/// rule that produces it.
fn grammar_js_omissions() -> (&'static str, Vec<&'static str>) {
    (
        "a reflection primitive is not a grammar token: `type_of::<T>()` parses as an ordinary \
         `turbofish_call` over an `identifier`, so the grammar never needs the literal. \
         Highlighting them is done by spelling, in queries/highlights.scm's `#any-of?` rule.",
        words_in(Family::Reflection),
    )
}

/// Reserved words the tree-sitter **highlight query** deliberately does not name, with the reason.
/// Derived from the same property, for the same reason as [`grammar_js_omissions`].
fn scm_omissions() -> (&'static str, Vec<&'static str>) {
    (
        "captured structurally instead: `(boolean_literal) @boolean` colours the whole literal \
         node, which is more precise than matching its two spellings.",
        words_in(Family::Boolean),
    )
}

// ---------------------------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>/ sits two below the repo root")
        .to_path_buf()
}

fn read(path: &str) -> String {
    let full = repo_root().join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {}: {e}", full.display()))
}

/// Whether the run should rewrite the files rather than check them.
fn updating() -> bool {
    std::env::var_os("NOETA_UPDATE_EDITOR_VOCABULARY").is_some()
}

/// The current text of a region, plus the exact byte range it occupies in `text`.
fn locate(text: &str, locate: &Locate, what: &str, path: &str) -> (String, std::ops::Range<usize>) {
    match locate {
        Locate::TextMateScope(scope) => {
            // The scope name is unique in the file, so anchoring on it is exact. From there the
            // `match` (or `name`, if `match` comes first) sibling is the next `"match":` — which is
            // always adjacent in this grammar, both orders included.
            let anchor = format!("\"{scope}\"");
            let at = text.find(&anchor).unwrap_or_else(|| {
                panic!("{path}: no pattern carries the scope {anchor} — {what} has no home")
            });
            let window_start = text[..at].rfind('{').unwrap_or(0);
            let window_end = text[at..]
                .find('}')
                .map(|i| at + i)
                .unwrap_or_else(|| text.len());
            let window = &text[window_start..window_end];
            let key = window.find("\"match\": \"").unwrap_or_else(|| {
                panic!("{path}: the {anchor} pattern has no `match` — {what} has no home")
            });
            let value_start = window_start + key + "\"match\": \"".len();
            let value_end = value_start
                + text[value_start..]
                    .find('"')
                    .expect("a JSON string is terminated");
            // JSON escapes `\` as `\\`; the region's text is the unescaped regex.
            (
                text[value_start..value_end].replace("\\\\", "\\"),
                value_start..value_end,
            )
        }
        Locate::Between { begin, end } => {
            let b = text.find(begin).unwrap_or_else(|| {
                panic!(
                    "{path}: the marker line `{begin}` is gone, so {what} is no longer generated \
                     — restore it or this gate is checking nothing"
                )
            });
            let after = b + begin.len();
            let nl = text[after..]
                .find('\n')
                .map(|i| after + i + 1)
                .unwrap_or(after);
            let e = text[nl..]
                .find(end)
                .map(|i| nl + i)
                .unwrap_or_else(|| panic!("{path}: `{begin}` has no matching `{end}`"));
            (text[nl..e].to_string(), nl..e)
        }
    }
}

/// The identifier-shaped words in a chunk of grammar text, for the both-directions diff.
///
/// `quoted` picks how a region spells its vocabulary: a TextMate `match` is a bare regex, where
/// every identifier-shaped run is a word, while a tree-sitter query or grammar rule quotes its
/// tokens and surrounds them with prose that is not vocabulary at all.
fn words_of(text: &str, quoted: bool) -> BTreeSet<String> {
    if quoted {
        let mut out = single_quoted_words(text);
        out.extend(double_quoted_words(text));
        return out;
    }
    let mut out = BTreeSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.insert(chars[start..i].iter().collect());
        } else {
            i += 1;
        }
    }
    out
}

/// Every `'word'` in `text`. Resynchronising rather than pair-consuming: a stray apostrophe (or a
/// literal that is not a word, like `'::'`) costs one character of progress instead of shifting
/// every literal after it by one, which is how a naive scan of `grammar.js` silently found six
/// tokens instead of thirty-three.
fn single_quoted_words(text: &str) -> BTreeSet<String> {
    quoted_words(text, '\'')
}

/// Every `"word"` in `text`, by the same resynchronising scan.
fn double_quoted_words(text: &str) -> BTreeSet<String> {
    quoted_words(text, '"')
}

fn quoted_words(text: &str, delim: char) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != delim {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < chars.len() && (chars[j].is_ascii_alphabetic() || chars[j] == '_') {
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j < chars.len() && chars[j] == delim {
                out.insert(chars[i + 1..j].iter().collect());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// **Every generated region of every editor file holds what the censuses derive for it.**
///
/// The failure names the words, in both directions: a word the census has and the file does not
/// (a keyword added to the lexer, a built-in type added to `BuiltinTy`, a prelude name added), and
/// a word the file has and the census does not (a keyword *removed* from the language — the
/// direction a one-way census misses, and the reason `signal`/`computed`/`effect`/`len` sat in the
/// TextMate grammar for two arcs after they left the prelude).
#[test]
fn every_editor_vocabulary_region_is_the_census() {
    let mut edits: Vec<(&str, std::ops::Range<usize>, String)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for region in regions() {
        let text = read(region.path);
        let (actual, range) = locate(&text, &region.locate, region.what, region.path);
        if actual == region.expected {
            continue;
        }
        if updating() {
            edits.push((region.path, range, region.expected.clone()));
            continue;
        }
        let quoted = matches!(region.locate, Locate::Between { .. });
        let (have, want) = (
            words_of(&actual, quoted),
            words_of(&region.expected, quoted),
        );
        let missing: Vec<&String> = want.difference(&have).collect();
        let extra: Vec<&String> = have.difference(&want).collect();
        let mut why = String::new();
        if !missing.is_empty() {
            why.push_str(&format!("\n    missing from the file: {missing:?}"));
        }
        if !extra.is_empty() {
            why.push_str(&format!(
                "\n    in the file but not in the language: {extra:?}"
            ));
        }
        if why.is_empty() {
            why.push_str("\n    same words, different order or layout");
        }
        failures.push(format!(
            "{}: {} has drifted from the census{why}\n    have: {actual:?}\n    want: {:?}",
            region.path, region.what, region.expected
        ));
    }

    if updating() {
        apply(edits);
        return;
    }
    assert!(
        failures.is_empty(),
        "the editor grammars no longer state the language's vocabulary.\n\n{}\n\nRegenerate with \
         `NOETA_UPDATE_EDITOR_VOCABULARY=1 cargo test -p noeta-ide --test editor_vocabulary`.",
        failures.join("\n\n")
    );
}

/// Write the regenerated regions back, latest-offset-first per file so earlier ranges stay valid.
fn apply(mut edits: Vec<(&str, std::ops::Range<usize>, String)>) {
    edits.sort_by(|a, b| a.0.cmp(b.0).then(b.1.start.cmp(&a.1.start)));
    let mut current: Option<(&str, String)> = None;
    for (path, range, replacement) in edits {
        if current.as_ref().map(|(p, _)| *p) != Some(path) {
            if let Some((p, text)) = current.take() {
                std::fs::write(repo_root().join(p), text).expect("write");
            }
            current = Some((path, read(path)));
        }
        let (_, text) = current.as_mut().expect("a file is open");
        // TextMate regions live inside a JSON string, so backslashes go back doubled.
        let encoded = if path.ends_with(".json") {
            replacement.replace('\\', "\\\\")
        } else {
            replacement
        };
        text.replace_range(range, &encoded);
    }
    if let Some((p, text)) = current {
        std::fs::write(repo_root().join(p), text).expect("write");
    }
}

/// **`grammar.js` carries a token for every keyword, and no token for a word the lexer dropped.**
///
/// The census the audit asked for, applied where generation cannot reach: a keyword in a
/// tree-sitter grammar is a literal inside the production rule that uses it, so no generator can
/// place `'fn'` inside `function_declaration`. Coverage is still checkable in both directions, and
/// the deliberate absences carry their reason in [`grammar_js_omissions`].
#[test]
fn the_tree_sitter_grammar_carries_every_keyword_token() {
    let text = read("editors/tree-sitter-noeta/grammar.js");
    // Single-quoted literals are tree-sitter's token spelling. Whole-line `//` comments go first,
    // so the prose above `turbofish_call` (which names the intrinsics) cannot make this pass
    // vacuously; trailing comments are left alone because `line_comment: _ => token(seq('//', …))`
    // puts the delimiter inside a literal, and cutting there would unbalance every quote after it.
    let code: String = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    // `field('name', …)` labels the child node, it does not tokenize a word — and tree-sitter
    // spells them with the same single quotes as tokens. Dropping the label leaves only literals
    // that really are grammar tokens, which is what makes the reverse direction below meaningful
    // rather than a list of every field name in the file.
    let code = code.replace("field('", "field(");
    let tokens = single_quoted_words(&code);
    assert!(
        tokens.len() > 20,
        "the grammar.js literal scrape found only {} words — it has stopped matching the file, so \
         this check is passing vacuously",
        tokens.len()
    );

    let (reason, omitted) = grammar_js_omissions();
    let census: BTreeSet<&str> = ReservedWord::all().iter().map(|w| w.word).collect();
    let allowed: BTreeSet<&str> = omitted.iter().copied().collect();
    for word in &allowed {
        assert!(
            census.contains(word),
            "`{word}` is allow-listed as an omission from grammar.js but the lexer no longer \
             reserves it — drop the entry"
        );
    }
    let missing: Vec<&&str> = census
        .iter()
        .filter(|w| !tokens.contains(**w) && !allowed.contains(**w))
        .collect();
    assert!(
        missing.is_empty(),
        "grammar.js has no token for {missing:?} — the tree-sitter parser will treat {} as a \
         plain identifier, so Neovim/Helix/Zed will not colour it and may misparse the \
         construct it introduces. Add it to the rule that uses it, or allow-list it with a \
         reason beside `{reason}`",
        if missing.len() == 1 { "it" } else { "them" },
    );

    // The reverse direction: every word literal the grammar tokenizes is vocabulary the language
    // still has. This is the direction a one-way census misses, and the one that catches a keyword
    // being *removed* — the grammar would go on tokenizing it, stealing the spelling from the
    // identifier it is now free to be.
    let types: BTreeSet<String> = BuiltinTy::all()
        .into_iter()
        .flat_map(spellings_of)
        .collect();
    let stale: Vec<&String> = tokens
        .iter()
        .filter(|w| {
            !census.contains(w.as_str())
                && !types.contains(w.as_str())
                && !GRAMMAR_JS_NON_VOCABULARY.contains(&w.as_str())
        })
        .collect();
    assert!(
        stale.is_empty(),
        "grammar.js tokenizes {stale:?}, which is neither a reserved word nor a built-in type \
         name. If the language dropped it, drop the token — a stale keyword token steals the \
         spelling from the identifier it is now free to be. If it was never vocabulary, list it \
         in `GRAMMAR_JS_NON_VOCABULARY`"
    );
}

/// Word-shaped literals in `grammar.js` that are not language vocabulary, with the reason.
///
/// Everything else the file quotes in single quotes is a keyword or a built-in type name, and the
/// reverse-direction check above insists on it — so this list is what stops that check from being
/// widened by accident.
const GRAMMAR_JS_NON_VOCABULARY: &[&str] = &[
    // The grammar's own name, in `grammar({ name: 'noeta' })`.
    "noeta",
    // `self` — a contextual receiver, lexed as an ordinary identifier by the compiler (it is not in
    // the token table) but given its own node here so `(self) @variable.builtin` can colour it.
    "self",
    // The default verbatim text tier. A tier NAME, not a keyword: `@doc { … }` is a directive whose
    // body the grammar captures raw, and a per-project `project-tiers.json` widens the set.
    "doc", // The wildcard pattern `_`.
    "_",
];

/// **`highlights.scm` names every keyword the grammar tokenizes, and nothing the lexer dropped.**
///
/// The generated region is the whole keyword section, so the forward direction is guaranteed by
/// construction. What this adds is the *reverse* direction over the file as a whole — a stale
/// capture left outside the generated region would still colour a word the language no longer has
/// — and the check that the deliberate omissions are still deliberate.
#[test]
fn the_highlight_query_names_every_keyword() {
    let text = read("editors/tree-sitter-noeta/queries/highlights.scm");
    let mut named = BTreeSet::new();
    let mut rest = text.as_str();
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        named.insert(after[..close].to_string());
        rest = &after[close + 1..];
    }

    let (reason, omitted) = scm_omissions();
    let allowed: BTreeSet<&str> = omitted.iter().copied().collect();
    let census: BTreeSet<&str> = ReservedWord::all().iter().map(|w| w.word).collect();
    for word in &allowed {
        assert!(
            census.contains(word),
            "`{word}` is allow-listed as an omission from highlights.scm but the lexer no longer \
             reserves it — drop the entry"
        );
    }
    let missing: Vec<&&str> = census
        .iter()
        .filter(|w| !named.contains(**w) && !allowed.contains(**w))
        .collect();
    assert!(
        missing.is_empty(),
        "highlights.scm never mentions {missing:?}, so tree-sitter editors render {} as plain \
         text where VS Code colours it. Either the generated region is stale or the word belongs \
         in the omissions list with a reason beside `{reason}`",
        if missing.len() == 1 { "it" } else { "them" },
    );

    let stale: Vec<&String> = named
        .iter()
        .filter(|w| {
            let is_word = w
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            is_word
                && !census.contains(w.as_str())
                && !SCM_NON_KEYWORD_STRINGS.contains(&w.as_str())
        })
        .collect();
    assert!(
        stale.is_empty(),
        "highlights.scm colours {stale:?} as a keyword, but the lexer does not reserve {} — a \
         removed keyword still being highlighted. Drop the rule, or list the string in \
         `SCM_NON_KEYWORD_STRINGS` if it is not vocabulary at all",
        if stale.len() == 1 { "it" } else { "them" },
    );
}

/// Word-shaped strings in `highlights.scm` that are not vocabulary — the interpolation delimiter's
/// halves and anything else the query quotes for structure rather than for a keyword.
const SCM_NON_KEYWORD_STRINGS: &[&str] = &[];

/// **The TextMate rules that are not generated are still subsets of the ones that are.**
///
/// Two patterns in the `declarations` repository are *structural* rather than vocabulary: they
/// match a keyword and the name that follows it, so the name binds as a function or a type. They
/// cannot be generated — which keyword introduces a *named* declaration is not something any
/// census knows — but they can be held to the census they draw from, which is what stops a
/// keyword being dropped from them silently or a non-declaration keyword being added.
///
/// This is the allow-list entry for those two patterns, written as a check instead of a name.
#[test]
fn the_hand_written_textmate_captures_stay_within_the_declaration_family() {
    let text = read("editors/vscode-noeta/syntaxes/noeta.tmLanguage.json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let patterns = value
        .pointer("/repository/declarations/patterns")
        .and_then(|v| v.as_array())
        .expect("the declarations repository is an array of patterns");
    let family: BTreeSet<&str> = words_in(Family::Declaration).into_iter().collect();
    let mut checked = 0;
    for pattern in patterns {
        // Only the capture rules: the generated `storage.type.noeta` alternation is checked above.
        if pattern.get("captures").is_none() {
            continue;
        }
        let m = pattern
            .get("match")
            .and_then(|v| v.as_str())
            .expect("a pattern matches something");
        let group = m
            .split_once("\\b(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inner, _)| inner)
            .unwrap_or_else(|| panic!("the capture rule {m:?} has no leading keyword group"));
        for word in group.split('|') {
            assert!(
                family.contains(word),
                "the TextMate declaration capture rule binds the name after `{word}`, but \
                 `{word}` is not a declaration keyword any more. Either the language changed or \
                 the rule is stale — a name bound by a keyword nobody has is a name never bound"
            );
        }
        checked += 1;
    }
    assert_eq!(
        checked, 2,
        "expected exactly the `fn` and type-introducing capture rules; the repository has changed \
         shape, so this check is no longer looking at what it thinks it is"
    );
}

/// **The TextMate grammar is still valid JSON after everything above.**
///
/// The generator writes into a JSON string by byte range, which is the cheapest correct thing to do
/// and also the easiest to get subtly wrong: an unescaped backslash in a regex turns the whole file
/// into a parse error, and VS Code would then silently fall back to no highlighting at all rather
/// than report it. Parsing the file is what turns that into a test failure.
#[test]
fn the_textmate_grammar_stays_valid_json() {
    let text = read("editors/vscode-noeta/syntaxes/noeta.tmLanguage.json");
    let value: serde_json::Value =
        serde_json::from_str(&text).expect("noeta.tmLanguage.json is valid JSON");
    assert_eq!(
        value.get("scopeName").and_then(|v| v.as_str()),
        Some("source.noeta"),
    );
    // And every regex the generator writes round-trips out of the JSON *as a regex*, not as a
    // half-escaped one: `\b` must have survived as a single backslash. Checked against the census
    // rather than a pasted prefix, so the assertion cannot go stale when the vocabulary moves.
    let flow = alternation(&words_in(Family::ControlFlow));
    let found = value
        .pointer("/repository/keywords/patterns")
        .and_then(|v| v.as_array())
        .expect("the keywords repository is an array of patterns")
        .iter()
        .any(|p| p.get("match").and_then(|m| m.as_str()) == Some(flow.as_str()));
    assert!(
        found,
        "no pattern decodes to the control-flow alternation {flow:?} — the generator's escaping \
         did not survive a JSON round trip"
    );
}
