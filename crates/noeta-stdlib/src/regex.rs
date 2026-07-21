//! `std.regex` — regular expressions over the `regex` crate (Ring 3).
//!
//! The engine choice is load-bearing. `regex` is a finite-automata engine with a **linear-time
//! guarantee**: matching is O(pattern × text) regardless of input, so a pattern applied to hostile
//! input cannot blow up. That matters here because `noeta serve` and `std.http` put user input in
//! front of these patterns. The price is the two constructs a backtracker gives you for free —
//! **lookaround and backreferences** — which this engine does not support and rejects at compile
//! time with the engine's own diagnostic. That is the trade we chose: a pattern that fails to
//! compile is a diagnostic, a pattern that hangs a request thread is an outage.
//!
//! Everything here is pure — no Host capability, like `math`/`json`/`vec`.
//!
//! **Offsets are character indices, not byte offsets.** The `regex` crate reports byte offsets into
//! UTF-8; every string position in Noeta (`slice`, `char_at`, `index_of`) is a character index. We
//! convert at this boundary so that `text.slice(m.start(), m.end()) == m.text()` holds for all
//! input, not just ASCII. See [`CharOffsets`] for how that stays O(n) across a whole match list.

use crate::{ErrorKind, StdError};
// `::regex` throughout: this module is itself named `regex`, so a bare `regex::` path here would be
// ambiguous between the engine crate and `crate::regex`.
use ::regex::Regex;

/// The registered extern-type name of a compiled pattern.
pub const PATTERN_TYPE_NAME: &str = "Pattern";

/// `Pattern`'s qualified runtime identity — the [`crate::crypto::HASHER_TYPE_IDENTITY`] twin.
pub const PATTERN_TYPE_IDENTITY: &str = "std.regex.Pattern";

/// The registered extern-type name of a single match.
pub const MATCH_TYPE_NAME: &str = "Match";

/// `Match`'s qualified runtime identity.
pub const MATCH_TYPE_IDENTITY: &str = "std.regex.Match";

/// A compiled pattern — **immutable**, so it is `key_capable` (a pattern can key a map, which is
/// what makes a user-level compile cache expressible in Noeta itself).
///
/// Compiling is the expensive step and this type is the reason it is explicit in the API: there is
/// deliberately no `regex.is_match(pattern, text)` free function. **Measured**, on 200k matches of
/// an email pattern: compile-once-then-match is 0.06s, recompiling per call is 104s — ~1700×, about
/// 520µs per compile. A free function makes those two look identical at the call site.
///
/// DECISION (2026-07-21, Niklas): keep the compile chain — `regex.compile('…').is_match(text)` —
/// as the only form, for consistency with the language's preference for explicitness and static
/// typing. Both alternatives were considered and rejected: an *uncached* free function hides the
/// cliff above, and a *cached* one (Python's `re` keeps ~512 compiled patterns; Go and Java take
/// the same convenience side) trades the cliff for invisible global state — performance depending
/// on cache residency, eviction surprises past the cache bound, and memory growth nothing in the
/// program accounts for. The chain costs 8 characters on a genuine one-shot and keeps the cost
/// legible in the code you read. Users who want caching build it themselves: `Pattern` is
/// `key_capable` precisely so a `Map<Pattern, _>` works. Do not add the convenience form back.
#[derive(Clone, Debug)]
pub struct Pattern(pub Regex);

impl Pattern {
    /// The pattern source — the full identity of a compiled pattern (flags travel inline as
    /// `(?i)`/`(?m)`/…, so there is no second dimension to compare).
    pub fn source(&self) -> &str {
        self.0.as_str()
    }
}

/// Compile a pattern, or report why it did not compile.
///
/// The engine's own error text is preserved verbatim — it is genuinely good (it carries a caret
/// and a span into the pattern), and paraphrasing it would only lose information. This is the one
/// fallible entry point in the module; everything downstream operates on an already-valid pattern.
pub fn compile(pattern: &str) -> Result<Pattern, StdError> {
    Regex::new(pattern).map(Pattern).map_err(|e| StdError {
        kind: ErrorKind::ArgType,
        message: format!("`regex.compile`: {e}"),
    })
}

/// Escape every metacharacter in `text`, so the result matches `text` literally.
pub fn escape(text: &str) -> String {
    ::regex::escape(text)
}

/// One match: the matched span plus every capture group, materialised as owned strings.
///
/// Groups are captured eagerly rather than on demand. That costs a little over a bare scan, and
/// buys one `Match` type covering the whole result surface instead of a `Match`/`Captures` pair
/// that users must learn to choose between — the same "one type over an enum" call `Hasher` makes.
/// The fast path for callers who only want a yes/no is `is_match`, which never builds a `Match` at
/// all.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    /// Group 0 — the whole matched text.
    pub text: String,
    /// Character index of the match start.
    pub start: i64,
    /// Character index one past the match end.
    pub end: i64,
    /// Groups 1..n. `None` is a group that did not participate in this match — a real and
    /// observable state (`(a)|(b)` always leaves one of the two unset), so it is not flattened to
    /// an empty string.
    pub groups: Vec<Option<String>>,
    /// Named groups, by the name in the pattern. A named group also keeps its number in `groups`.
    pub named: Vec<(String, Option<String>)>,
}

impl Match {
    /// Group `n` — 0 is the whole match, 1..n the capture groups. Out of range is `None`, the same
    /// answer as a non-participating group: both mean "no text here", and `char_at`'s
    /// safe-probe precedent says an out-of-range index is `none`, not an error.
    pub fn group(&self, n: i64) -> Option<String> {
        match n {
            0 => Some(self.text.clone()),
            n if n > 0 => self.groups.get((n - 1) as usize).cloned().flatten(),
            _ => None,
        }
    }

    /// The group captured under `name`, or `None` if the pattern has no such name or the group did
    /// not participate.
    pub fn named(&self, name: &str) -> Option<String> {
        self.named
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.clone())
    }
}

/// A byte-offset → character-index converter that stays O(n) across an ascending sequence of
/// offsets.
///
/// The naive conversion is `text[..byte].chars().count()`, which is O(n) per call and therefore
/// O(n²) over a `find_all` on a long subject. Matches arrive in ascending byte order, so this
/// walks the string once and resumes from where the previous query left off. ASCII-only input
/// short-circuits entirely, since there byte offset == char index.
struct CharOffsets<'a> {
    text: &'a str,
    ascii: bool,
    last_byte: usize,
    last_char: i64,
}

impl<'a> CharOffsets<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            ascii: text.is_ascii(),
            last_byte: 0,
            last_char: 0,
        }
    }

    /// The character index of byte offset `byte`. Callers must query in non-decreasing order (the
    /// regex crate yields matches left to right); a lower offset falls back to a fresh count, so a
    /// caller that breaks the discipline is slow rather than wrong.
    fn char_index(&mut self, byte: usize) -> i64 {
        if self.ascii {
            return byte as i64;
        }
        if byte < self.last_byte {
            self.last_byte = 0;
            self.last_char = 0;
        }
        let advance = self.text[self.last_byte..byte].chars().count() as i64;
        self.last_byte = byte;
        self.last_char += advance;
        self.last_char
    }
}

/// Build a [`Match`] from the engine's captures, converting offsets through `offsets`.
fn build_match(pattern: &Regex, caps: &::regex::Captures<'_>, offsets: &mut CharOffsets) -> Match {
    let whole = caps.get(0).expect("group 0 always participates in a match");
    let groups: Vec<Option<String>> = (1..caps.len())
        .map(|i| caps.get(i).map(|m| m.as_str().to_string()))
        .collect();
    let named: Vec<(String, Option<String>)> = pattern
        .capture_names()
        .flatten()
        .map(|name| {
            (
                name.to_string(),
                caps.name(name).map(|m| m.as_str().to_string()),
            )
        })
        .collect();
    Match {
        text: whole.as_str().to_string(),
        // Ascending order: start before end, and each match after the last.
        start: offsets.char_index(whole.start()),
        end: offsets.char_index(whole.end()),
        groups,
        named,
    }
}

/// Does the pattern match anywhere in `text`? The allocation-free fast path — no groups, no
/// offsets, no `Match`.
pub fn is_match(pattern: &Pattern, text: &str) -> bool {
    pattern.0.is_match(text)
}

/// The leftmost match, or `None`.
pub fn find(pattern: &Pattern, text: &str) -> Option<Match> {
    let mut offsets = CharOffsets::new(text);
    pattern
        .0
        .captures(text)
        .map(|caps| build_match(&pattern.0, &caps, &mut offsets))
}

/// Every non-overlapping match, left to right.
pub fn find_all(pattern: &Pattern, text: &str) -> Vec<Match> {
    let mut offsets = CharOffsets::new(text);
    pattern
        .0
        .captures_iter(text)
        .map(|caps| build_match(&pattern.0, &caps, &mut offsets))
        .collect()
}

/// Replace the leftmost match. `replacement` expands `$1` / `${name}` group references; a literal
/// `$` is written `$$`.
pub fn replace(pattern: &Pattern, text: &str, replacement: &str) -> String {
    pattern.0.replace(text, replacement).into_owned()
}

/// Replace every non-overlapping match. Same expansion rules as [`replace`].
pub fn replace_all(pattern: &Pattern, text: &str, replacement: &str) -> String {
    pattern.0.replace_all(text, replacement).into_owned()
}

/// Split around every match. Adjacent and edge matches yield empty strings — the same contract as
/// the Ring-1 `string.split`, so the two behave alike.
pub fn split(pattern: &Pattern, text: &str) -> Vec<String> {
    pattern.0.split(text).map(|s| s.to_string()).collect()
}

impl crate::ExternValue for Pattern {
    fn type_identity(&self) -> &'static str {
        PATTERN_TYPE_IDENTITY
    }

    /// Equal iff compiled from the same source. Flags live inside the source, so this is full
    /// identity, not an approximation.
    fn eq_value(&self, other: &dyn crate::ExternValue) -> bool {
        match other.as_any().downcast_ref::<Pattern>() {
            Some(o) => self.source() == o.source(),
            None => false,
        }
    }

    /// Ordered by source — total, which is what `key_capable` requires.
    fn cmp_value(&self, other: &dyn crate::ExternValue) -> Option<std::cmp::Ordering> {
        other
            .as_any()
            .downcast_ref::<Pattern>()
            .map(|o| self.source().cmp(o.source()))
    }

    fn hash_value(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.source().hash(&mut h);
        h.finish()
    }

    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<pattern {}>", self.source())
    }

    fn clone_box(&self) -> Box<dyn crate::ExternValue> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl crate::ExternValue for Match {
    fn type_identity(&self) -> &'static str {
        MATCH_TYPE_IDENTITY
    }

    fn eq_value(&self, other: &dyn crate::ExternValue) -> bool {
        match other.as_any().downcast_ref::<Match>() {
            Some(o) => self == o,
            None => false,
        }
    }

    /// Ordered by position, then by text — total over matches, so a `List<Match>` sorts. Two
    /// matches from different subjects are still comparable; position is the primary key because
    /// that is the order `find_all` produces.
    fn cmp_value(&self, other: &dyn crate::ExternValue) -> Option<std::cmp::Ordering> {
        other
            .as_any()
            .downcast_ref::<Match>()
            .map(|o| (self.start, self.end, &self.text).cmp(&(o.start, o.end, &o.text)))
    }

    fn hash_value(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.start.hash(&mut h);
        self.end.hash(&mut h);
        self.text.hash(&mut h);
        h.finish()
    }

    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<match {:?} at {}>", self.text, self.start)
    }

    fn clone_box(&self) -> Box<dyn crate::ExternValue> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// --- registration --------------------------------------------------------------------------------
//
// Wired here rather than in `registry.rs` (where the always-on units live) for the same reason
// `datetime` is: a ring-gated unit keeps its whole surface behind one `#[cfg]` boundary, so
// `--no-default-features` sheds the module, both types, and the engine together.

use noeta_ext_abi::args::{want_arity, want_int, want_str};
use noeta_ext_abi::registry::{
    ExtFn, ExtModule, ExtType, Extension, NativeOut, NativeValue, RetTy::Concrete, Scalar, SigType,
};
use noeta_ext_abi::{Host, no_method_error, type_error};

const PATTERN_SIG: SigType = SigType::Named(PATTERN_TYPE_NAME);
const MATCH_SIG: SigType = SigType::Named(MATCH_TYPE_NAME);
const OPT_MATCH: SigType = SigType::Option(&MATCH_SIG);
const OPT_STR: SigType = SigType::Option(&SigType::String);

const REGEX_FNS: &[ExtFn] = &[
    // The module's only fallible function, and the only way to get a `Pattern`: compiling is
    // explicit precisely so it cannot happen accidentally inside a loop.
    ExtFn {
        name: "compile",
        params: &[SigType::String],
        ret: Concrete(PATTERN_SIG),
    },
    ExtFn {
        name: "escape",
        params: &[SigType::String],
        ret: Concrete(SigType::String),
    },
];

const PATTERN_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "is_match",
        params: &[SigType::String],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "find",
        params: &[SigType::String],
        ret: Concrete(OPT_MATCH),
    },
    ExtFn {
        name: "find_all",
        params: &[SigType::String],
        ret: Concrete(SigType::List(&MATCH_SIG)),
    },
    ExtFn {
        name: "replace",
        params: &[SigType::String, SigType::String],
        ret: Concrete(SigType::String),
    },
    ExtFn {
        name: "replace_all",
        params: &[SigType::String, SigType::String],
        ret: Concrete(SigType::String),
    },
    ExtFn {
        name: "split",
        params: &[SigType::String],
        ret: Concrete(SigType::List(&SigType::String)),
    },
    ExtFn {
        name: "source",
        params: &[],
        ret: Concrete(SigType::String),
    },
];

const MATCH_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "text",
        params: &[],
        ret: Concrete(SigType::String),
    },
    // Character indices, so `subject.slice(m.start(), m.end()) == m.text()`.
    ExtFn {
        name: "start",
        params: &[],
        ret: Concrete(SigType::Int),
    },
    ExtFn {
        name: "end",
        params: &[],
        ret: Concrete(SigType::Int),
    },
    ExtFn {
        name: "group",
        params: &[SigType::Int],
        ret: Concrete(OPT_STR),
    },
    ExtFn {
        name: "named",
        params: &[SigType::String],
        ret: Concrete(OPT_STR),
    },
    ExtFn {
        name: "groups",
        params: &[],
        ret: Concrete(SigType::List(&OPT_STR)),
    },
];

/// Wrap an optional string as the seam's `Option` — the shape `Option<string>` marshals to.
fn opt_out(v: Option<String>) -> NativeOut {
    match v {
        Some(s) => NativeOut::Some(Box::new(NativeOut::Str(s))),
        None => NativeOut::None,
    }
}

fn regex_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "compile" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(compile(
                want_str(func, args, 0)?,
            )?)))
        }
        "escape" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(escape(want_str(func, args, 0)?)))
        }
        _ => Err(crate::no_function_error("regex", func)),
    }
}

fn pattern_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(pattern) = recv.as_any().downcast_ref::<Pattern>() else {
        return Err(type_error(method, PATTERN_TYPE_NAME));
    };
    match method {
        "is_match" => {
            want_arity(method, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(is_match(
                pattern,
                want_str(method, args, 0)?,
            ))))
        }
        "find" => {
            want_arity(method, args, 1)?;
            Ok(match find(pattern, want_str(method, args, 0)?) {
                Some(m) => NativeOut::Some(Box::new(NativeOut::Extern(crate::ExternBox::new(m)))),
                None => NativeOut::None,
            })
        }
        "find_all" => {
            want_arity(method, args, 1)?;
            Ok(NativeOut::List(
                find_all(pattern, want_str(method, args, 0)?)
                    .into_iter()
                    .map(|m| NativeOut::Extern(crate::ExternBox::new(m)))
                    .collect(),
            ))
        }
        "replace" => {
            want_arity(method, args, 2)?;
            Ok(NativeOut::Str(replace(
                pattern,
                want_str(method, args, 0)?,
                want_str(method, args, 1)?,
            )))
        }
        "replace_all" => {
            want_arity(method, args, 2)?;
            Ok(NativeOut::Str(replace_all(
                pattern,
                want_str(method, args, 0)?,
                want_str(method, args, 1)?,
            )))
        }
        "split" => {
            want_arity(method, args, 1)?;
            Ok(NativeOut::List(
                split(pattern, want_str(method, args, 0)?)
                    .into_iter()
                    .map(NativeOut::Str)
                    .collect(),
            ))
        }
        "source" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(pattern.source().to_string()))
        }
        _ => Err(no_method_error(PATTERN_TYPE_NAME, method)),
    }
}

fn match_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(m) = recv.as_any().downcast_ref::<Match>() else {
        return Err(type_error(method, MATCH_TYPE_NAME));
    };
    match method {
        "text" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(m.text.clone()))
        }
        "start" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(m.start)))
        }
        "end" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(m.end)))
        }
        "group" => {
            want_arity(method, args, 1)?;
            Ok(opt_out(m.group(want_int(method, args, 0)?)))
        }
        "named" => {
            want_arity(method, args, 1)?;
            Ok(opt_out(m.named(want_str(method, args, 0)?)))
        }
        "groups" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::List(
                m.groups.iter().cloned().map(opt_out).collect(),
            ))
        }
        _ => Err(no_method_error(MATCH_TYPE_NAME, method)),
    }
}

const REGEX_DOCS: &[(&str, &str)] = &[
    (
        "compile",
        "Compile `pattern` into a reusable `Pattern`. Errors if the pattern is invalid — including \
         lookaround (`(?=…)`, `(?<=…)`) and backreferences (`\\1`), which this engine does not \
         support in exchange for its linear-time matching guarantee.",
    ),
    (
        "escape",
        "Escape every metacharacter in `text`, so the result matches `text` literally.",
    ),
];

const PATTERN_DOCS: &[(&str, &str)] = &[
    (
        "is_match",
        "Does the pattern match anywhere in `text`? The cheapest question — no groups, no offsets.",
    ),
    ("find", "The leftmost match, or `none`."),
    (
        "find_all",
        "Every non-overlapping match, left to right, as a `List<Match>`.",
    ),
    (
        "replace",
        "Replace the leftmost match. The replacement expands `$1` / `${name}` group references; \
         write `$$` for a literal `$`.",
    ),
    (
        "replace_all",
        "Replace every non-overlapping match, with the same expansion rules as `replace`.",
    ),
    (
        "split",
        "Split `text` around every match. Adjacent and edge matches yield empty strings, matching \
         `string.split`.",
    ),
    ("source", "The pattern source this was compiled from."),
];

const MATCH_DOCS: &[(&str, &str)] = &[
    ("text", "The matched text (group 0)."),
    (
        "start",
        "The character index where the match begins — a character index, not a byte offset, so it \
         composes with `slice` and `char_at`.",
    ),
    ("end", "The character index one past the match end."),
    (
        "group",
        "Capture group `n` — 0 is the whole match. `none` if the group did not participate in this \
         match, or `n` is out of range.",
    ),
    (
        "named",
        "The group captured under `name` (from `(?<name>…)`), or `none`.",
    ),
    (
        "groups",
        "Groups 1..n in order, each `some(text)` or `none` for a group that did not participate.",
    ),
];

const REGEX_MODULES: &[ExtModule] = &[ExtModule {
    name: "regex",
    functions: REGEX_FNS,
    dispatch: regex_dispatch,
    // Ring-attributed so the AOT footprint scan drops the engine and its Unicode tables from a
    // binary that never imports `std.regex`.
    ring: Some("ring-regex"),
    docs: REGEX_DOCS,
    ..ExtModule::DEFAULTS
}];

const REGEX_TYPES: &[ExtType] = &[
    ExtType {
        name: PATTERN_TYPE_NAME,
        namespace: "std.regex",
        methods: PATTERN_METHODS,
        dispatch: pattern_method_dispatch,
        // Immutable, ordered by source, hashed by source — so a pattern can key a map, which is
        // what lets a user express a compile cache in Noeta itself.
        key_capable: true,
        docs: PATTERN_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: MATCH_TYPE_NAME,
        namespace: "std.regex",
        methods: MATCH_METHODS,
        dispatch: match_method_dispatch,
        // Ordered (so a match list sorts) but not a key: matches are results, not identities.
        key_capable: false,
        docs: MATCH_DOCS,
        ..ExtType::DEFAULTS
    },
];

/// The `std.regex` extension unit (Ring 3), registered only under the `ring-regex` feature.
#[derive(Debug, Clone, Copy)]
pub struct RegexExtension;

impl Extension for RegexExtension {
    fn name(&self) -> &'static str {
        "std.regex"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        REGEX_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        REGEX_TYPES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_rejects_a_bad_pattern_and_keeps_the_engine_diagnostic() {
        let err = compile("a(b").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ArgType);
        assert!(err.message.starts_with("`regex.compile`:"));
        // The engine's own span-carrying text survives.
        assert!(err.message.contains("unclosed group"), "{}", err.message);
    }

    /// Lookaround and backreferences are the documented cost of the linear-time engine — they must
    /// fail loudly at compile time, not silently mean something else.
    #[test]
    fn lookaround_and_backreferences_are_rejected_at_compile_time() {
        assert!(compile(r"(?=foo)").is_err());
        assert!(compile(r"(?<=foo)bar").is_err());
        assert!(compile(r"(\w)\1").is_err());
    }

    /// The load-bearing invariant: offsets are CHARACTER indices, so they compose with `slice`.
    /// Byte offsets would put this match at 7..10 instead of 3..6.
    #[test]
    fn offsets_are_character_indices_not_bytes() {
        let p = compile("bar").unwrap();
        let subject = "héllo bar"; // 'é' is two bytes
        let m = find(&p, subject).unwrap();
        assert_eq!((m.start, m.end), (6, 9));
        let sliced: String = subject
            .chars()
            .skip(m.start as usize)
            .take((m.end - m.start) as usize)
            .collect();
        assert_eq!(sliced, m.text);

        // …and across a multi-match scan, where the incremental converter has to stay correct as
        // it resumes from each previous offset.
        let p = compile("é").unwrap();
        let found = find_all(&p, "aébéc");
        assert_eq!(
            found.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(),
            vec![(1, 2), (3, 4)]
        );
    }

    #[test]
    fn groups_named_and_non_participating() {
        let p = compile(r"(?<year>\d{4})-(\d{2})|(never)").unwrap();
        let m = find(&p, "on 2026-07 ok").unwrap();
        assert_eq!(m.group(0).as_deref(), Some("2026-07"));
        assert_eq!(m.group(1).as_deref(), Some("2026"));
        assert_eq!(m.group(2).as_deref(), Some("07"));
        // The alternation's other branch did not participate — `None`, not `Some("")`.
        assert_eq!(m.group(3), None);
        assert_eq!(m.named("year").as_deref(), Some("2026"));
        assert_eq!(m.named("nope"), None);
        // Out of range probes are `none`, matching `char_at`.
        assert_eq!(m.group(99), None);
        assert_eq!(m.group(-1), None);
    }

    #[test]
    fn find_all_replace_and_split() {
        let p = compile(r"\d+").unwrap();
        assert!(is_match(&p, "a1b22"));
        assert!(!is_match(&p, "abc"));
        assert_eq!(find_all(&p, "a1b22c333").len(), 3);
        assert_eq!(replace(&p, "a1b22", "#"), "a#b22");
        assert_eq!(replace_all(&p, "a1b22", "#"), "a#b#");
        assert_eq!(split(&p, "a1b22c"), vec!["a", "b", "c"]);

        // `$1` expansion, and `$$` for a literal dollar.
        let p = compile(r"(\w+)@(\w+)").unwrap();
        assert_eq!(replace_all(&p, "me@here", "$2/$1"), "here/me");
        assert_eq!(replace_all(&p, "me@here", "$$"), "$");
    }

    /// `Pattern` promises `key_capable`: a total order, and a hash keyed to the same identity as
    /// equality. `Match` is ordered too, so a match list sorts.
    #[test]
    fn extern_value_contracts() {
        use crate::ExternValue;
        let a = compile("ab+").unwrap();
        let b = compile("ab+").unwrap();
        let c = compile("zz").unwrap();
        assert!(a.eq_value(&b));
        assert!(!a.eq_value(&c));
        assert_eq!(a.hash_value(), b.hash_value());
        assert_eq!(a.cmp_value(&b), Some(std::cmp::Ordering::Equal));
        assert_eq!(a.cmp_value(&c), Some(std::cmp::Ordering::Less));
        assert_eq!((&a as &dyn ExternValue).display_string(), "<pattern ab+>");

        let p = compile(r"\d").unwrap();
        let ms = find_all(&p, "1 2");
        assert_eq!(ms[0].cmp_value(&ms[1]), Some(std::cmp::Ordering::Less));
        assert!(!ms[0].eq_value(&ms[1]));
    }

    #[test]
    fn escape_makes_a_literal() {
        let escaped = escape("a.b*c");
        let p = compile(&escaped).unwrap();
        assert!(is_match(&p, "a.b*c"));
        assert!(!is_match(&p, "axbyc"));
    }
}
