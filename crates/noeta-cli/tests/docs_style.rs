//! Documentation style gate: the wiki describes what Noeta **is**, and the toolchain's
//! user-facing help says what a flag does — neither is a changelog.
//!
//! Prose drifts into history one sentence at a time, and each sentence looks harmless in its own
//! diff: a note that a page "used to say" something, a retracted recommendation, a measurement of
//! how a thing behaved before it was fixed, a setting that "no longer names" what it used to. The
//! reader never saw any of it, so a changelog sentence inside a reference page reads as a live
//! rule. Internal milestone labels are the same failure in a different register: `P-AOT L1` and
//! `(interim)` mean nothing outside this repo, and four `noeta build` flags shipped announcing
//! theirs in `--help`.
//!
//! This is a **lint over high-precision patterns, not a proof**. It catches the phrasings that
//! have actually appeared here; a page written as a changelog in other words will pass. See the
//! "Writing docs" section of `AGENTS.md` for the rule this enforces — the judgement half is the
//! author's.
//!
//! Every pattern below is verified clean against the tree as it stands. If one starts firing on a
//! legitimate sentence, narrow the pattern (and say why) rather than deleting the rule: an earlier
//! draft of this gate matched a bare `used to`, which fires inside "ref**used to** save".

use std::path::PathBuf;

/// A banned pattern, as a plain-substring or simple-alternation matcher over lowercased text, with
/// the reason a reader is worse off for it.
struct Rule {
    /// Alternatives; a line matches the rule when it contains any of them (already lowercase).
    needles: &'static [&'static str],
    why: &'static str,
    /// Files this rule does not apply to, by file stem.
    exempt: &'static [&'static str],
}

const RULES: &[Rule] = &[
    Rule {
        needles: &[
            "used to say",
            "used to report",
            "used to be",
            "used to mean",
            "used to require",
            "used to name",
            "was previously",
            "were previously",
            "was formerly",
            "the old advice",
            "the old behavior",
            "the old behaviour",
            "no longer names",
            "has since been",
        ],
        why: "history — the reader never saw the old behavior; state the current rule",
        exempt: &[],
    },
    Rule {
        needles: &["p-aot", "p-wasm", "(interim)", "interim"],
        why: "internal milestone vocabulary — meaningless outside this repo",
        exempt: &[],
    },
    Rule {
        needles: &[
            "slice 0", "slice 1", "slice 2", "slice 3", "slice 4", "slice 5", "slice 6", "slice 7",
            "slice 8", "slice 9", "arc 0", "arc 1", "arc 2", "arc 3", "arc 4", "arc 5", "arc 6",
            "arc 7", "arc 8", "arc 9",
        ],
        why: "internal milestone vocabulary — the reader has no arc/slice ledger",
        exempt: &[],
    },
    Rule {
        needles: &["plans/"],
        why: "the wiki must not send a reader to a roadmap document",
        // The contributor guide is where pointing at the roadmap belongs.
        exempt: &["Contributing"],
    },
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One violation, rendered as a clickable location plus the offending line.
fn scan(label: &str, stem: &str, text: &str, out: &mut Vec<String>) {
    for (i, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        for rule in RULES {
            if rule.exempt.contains(&stem) {
                continue;
            }
            if let Some(hit) = rule.needles.iter().find(|n| lower.contains(**n)) {
                out.push(format!(
                    "{label}:{}: `{hit}` — {}\n    {}",
                    i + 1,
                    rule.why,
                    line.trim()
                ));
            }
        }
    }
}

#[test]
fn the_wiki_describes_the_present() {
    let docs = repo_root().join("docs");
    let mut violations = Vec::new();
    let mut pages = 0;
    for entry in std::fs::read_dir(&docs).expect("docs/ is readable") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let text = std::fs::read_to_string(&path).expect("page is UTF-8");
        pages += 1;
        scan(&format!("docs/{stem}.md"), &stem, &text, &mut violations);
    }
    assert!(
        pages > 20,
        "only {pages} pages scanned — is docs/ where it was?"
    );
    assert!(
        violations.is_empty(),
        "the wiki describes what Noeta is, not what it was \
         (AGENTS.md → Writing docs):\n\n{}\n",
        violations.join("\n")
    );
}

/// The same rules over the CLI's **user-facing** help. A `///` on a clap `Command` variant or an
/// `#[arg]` field is printed by `noeta <cmd> --help`; a `//!` module header or a `//` comment is
/// not, so only the doc comments are scanned.
#[test]
fn user_facing_help_carries_no_internal_vocabulary() {
    let src = repo_root().join("crates/noeta-cli/src/lib.rs");
    let text = std::fs::read_to_string(&src).expect("the CLI surface is readable");
    let help: String = text
        .lines()
        .filter(|l| l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut violations = Vec::new();
    scan("crates/noeta-cli/src/lib.rs", "lib", &help, &mut violations);
    assert!(
        violations.is_empty(),
        "`noeta --help` is read by users, not maintainers \
         (AGENTS.md → Writing docs):\n\n{}\n",
        violations.join("\n")
    );
}

/// The gate is only worth having if it fails on the thing it is meant to catch. Pins each rule
/// against a sentence of exactly the shape that reached `main`.
#[test]
fn the_gate_catches_what_actually_shipped() {
    let cases = [
        "This page used to say the annotation was optional; it never was.",
        "the old advice to sleep in slices is retired",
        "It no longer names a provider (that moved to `[directives]`).",
        "Compile a program to a self-contained bundle (P-AOT L1).",
        "compile in `@debug` blocks (object-model slice 6)",
        "the design is in `plans/interruptible-host-io.md`",
    ];
    for case in cases {
        let mut violations = Vec::new();
        scan("case", "Some-Page", case, &mut violations);
        assert!(
            !violations.is_empty(),
            "the gate would not have caught: {case}"
        );
    }
    // ...and does not fire on the legitimate sentences that live in the tree today.
    for ok in [
        "a `--max-regress` gate it refused to save",
        "no previously-seen advisory has disappeared",
        "an in-development package not yet cut into releases",
        "One test that never returns can no longer swallow the whole run.",
    ] {
        let mut violations = Vec::new();
        scan("case", "Some-Page", ok, &mut violations);
        assert!(
            violations.is_empty(),
            "false positive on: {ok}\n{violations:?}"
        );
    }
}
