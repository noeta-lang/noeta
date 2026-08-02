//! Pointing the generator at the formatter: config derivation, violation classification, and a
//! delta-debugging minimizer.
//!
//! A fuzzer that only reports "seed 92 fails" is barely more useful than no fuzzer: the generated
//! program is a hundred lines of noise, and the work of turning it into something a human can read
//! is where the time actually goes. So the reproduce → classify → minimize loop lives here rather
//! than in a test, and the same three steps will serve whatever component is fuzzed next.

use noeta_fmt::{
    ArrowStyle, FmtConfig, ParenStyle, SemicolonStyle,
    oracle::{self, Verdict, Violation},
};

/// Derive a formatter configuration from `bytes`.
///
/// Config is an input dimension in its own right: `wrap` alone switches the printer between
/// preserving the author's line breaks and re-deriving layout from a width budget, which are
/// effectively two different printers to be a fixed point over. The corpus harness pins three
/// configurations; this reaches the whole space, including combinations (tabs plus a narrow width,
/// `Preserve` semicolons plus `Add` parens) that no `.editorconfig` in the repository produces.
pub fn config_from(bytes: &[u8]) -> FmtConfig {
    let at = |i: usize| bytes.get(i).copied().unwrap_or(0);
    FmtConfig {
        wrap: at(0) % 2 == 0,
        // Narrow widths are the interesting ones: they force the printer to break in places a
        // 100-column budget never would.
        line_width: [40usize, 60, 80, 100, 120][at(1) as usize % 5],
        match_arm_arrows: if at(2) % 2 == 0 {
            ArrowStyle::Compact
        } else {
            ArrowStyle::Align
        },
        sort_imports: at(3) % 3 == 0,
        parens: if at(4) % 2 == 0 {
            ParenStyle::Remove
        } else {
            ParenStyle::Add
        },
        semicolons: match at(5) % 3 {
            0 => SemicolonStyle::Remove,
            1 => SemicolonStyle::Add,
            _ => SemicolonStyle::Preserve,
        },
        indent_width: [2usize, 4, 8][at(6) as usize % 3],
        use_tabs: at(7) % 4 == 0,
        final_newline: at(8) % 8 != 0,
        trim_trailing: at(9) % 8 != 0,
    }
}

/// A one-line description of a config, for failure messages and reproduction.
pub fn describe(c: &FmtConfig) -> String {
    format!(
        "wrap={} width={} arrows={:?} sort_imports={} parens={:?} semis={:?} indent={} tabs={} final_nl={} trim={}",
        c.wrap,
        c.line_width,
        c.match_arm_arrows,
        c.sort_imports,
        c.parens,
        c.semicolons,
        c.indent_width,
        c.use_tabs,
        c.final_newline,
        c.trim_trailing,
    )
}

/// The program and config a seed denotes. Seeds are the reproduction unit: a failure reports one,
/// and every tool here takes one.
pub fn case(seed: u64, nonce: u32) -> (String, FmtConfig) {
    let bytes = crate::seed_bytes(seed, nonce);
    let src = crate::generate::program(&bytes);
    // Config comes from a different slice of the same buffer, so program shape and config are not
    // correlated.
    let config = config_from(&bytes[16..]);
    (src, config)
}

/// The coarse family a violation belongs to — what to group failures by when triaging a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// The formatter's own safety gate tripped: the output would have parsed to a different AST.
    /// The most serious family — the printer changed the program's meaning and caught itself.
    Safety,
    /// `format(format(x)) != format(x)`.
    NotIdempotent,
    /// A second format pass failed outright.
    ReformatFailed,
    /// Comment texts were lost, duplicated or altered.
    CommentsChanged,
    /// Own-line comments moved to a different construct or depth.
    CommentsMoved,
}

impl Class {
    /// The family of `violation`.
    pub fn of(violation: &Violation) -> Class {
        match violation {
            Violation::Safety(_) => Class::Safety,
            Violation::NotIdempotent { .. } => Class::NotIdempotent,
            Violation::ReformatFailed(_) => Class::ReformatFailed,
            Violation::CommentsChanged { .. } => Class::CommentsChanged,
            Violation::CommentsMoved(_) => Class::CommentsMoved,
        }
    }
}

/// Whether `src` still violates an invariant of the same class under `config`.
///
/// Class equality rather than exact-message equality is what makes the minimizer usable: shrinking
/// a program legitimately changes which comment moved or what the diff looks like, and demanding an
/// identical message would reject nearly every useful reduction. Requiring the same *family* keeps
/// the reduction honest — a minimizer free to land on any failure tends to walk to an unrelated
/// one and report the wrong bug.
pub fn still_fails(src: &str, config: &FmtConfig, class: Class) -> bool {
    match oracle::check("min.noe", src, config) {
        Err(v) => Class::of(&v) == class,
        Ok(Verdict::Clean | Verdict::Declined) => false,
    }
}

/// Reduce `src` to a smaller program that still violates the same class of invariant.
///
/// Line-granular delta debugging: try deleting progressively smaller runs of lines, keep any
/// deletion that preserves the failure. A deletion that breaks the parse makes the formatter
/// decline the input, which reads as "no longer fails" and is rejected — so the reduction stays
/// syntactically valid without the minimizer needing to know any grammar.
pub fn minimize(src: &str, config: &FmtConfig, class: Class) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
    let mut chunk = lines.len().max(1);
    while chunk >= 1 {
        let mut i = 0;
        while i < lines.len() {
            let end = (i + chunk).min(lines.len());
            let mut candidate = lines.clone();
            candidate.drain(i..end);
            let text = candidate.join("\n");
            if !text.trim().is_empty() && still_fails(&text, config, class) {
                lines = candidate;
                // Do not advance: the next run now starts at `i`.
            } else {
                i += 1;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk /= 2;
    }
    lines.join("\n")
}
