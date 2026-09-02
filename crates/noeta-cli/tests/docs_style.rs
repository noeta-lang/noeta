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
//! "Writing docs" section of `AGENTS.md` for the rule this enforces — the judgment half is the
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
            "the default since",
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
    Rule {
        needles: &[
            "phase 1", "phase 2", "phase 3", "phase 4", "phase 5", "phase 6", "phase 7", "phase 8",
            "phase 9",
        ],
        why: "internal milestone vocabulary — the reader has no phase ledger",
        exempt: &[],
    },
    Rule {
        // American English, everywhere. `analyse` is left out on purpose: it is a prefix of
        // `analyses`, the ordinary American plural of `analysis`, which the tree uses.
        needles: &[
            "colour",
            "behaviour",
            "serialise",
            "serialised",
            "serialisation",
            "neighbour",
            "finalise",
            "finalised",
            "initialise",
            "initialised",
            "organise",
            "organised",
            "judgement",
            "centre",
            "catalogue",
            "favour",
            "honour",
            "defence",
            "licence",
        ],
        why: "British spelling — this project writes American English",
        exempt: &[],
    },
];

/// A pattern with no fixed spelling: a milestone code, or a project label in trailing position.
/// These are the shapes a substring list cannot hold, since the *next* arc is named something
/// nobody has typed yet.
struct Shape {
    /// Reads the line as written (case matters here), returning the offending fragment.
    find: fn(&str) -> Option<String>,
    why: &'static str,
}

const SHAPES: &[Shape] = &[
    Shape {
        find: milestone_code,
        why: "internal milestone code — `P2.4`, `Phase 4`, `M3` and `C2/C5` name a ledger the \
              reader has never seen",
    },
    Shape {
        find: internal_label,
        why: "internal project label — an arc/slice/residual name means nothing outside this repo",
    },
];

/// A milestone code **in label position**: `(package-manager P2.4)`, `(… startup cache, M3)`,
/// `(package-manager Phase 3, N3.0)`, `the pre-C2 behavior`, `session-checker C2/C5`.
///
/// The token shape alone is far too common to ban, because a type name reads the same way:
/// `Type.F32`, `struct V3`, a `V8/JSC`-style hidden class, and the **F5** key are all one or two
/// capitals over one or two digits. So the shape is only half the test, and position is the other
/// half. A code counts when it **closes a parenthetical** (`… P2.4)`), which is where a ledger
/// reference is written, or when a **hyphenated project name** introduces it (`session-checker
/// C2/C5`, `pre-C2`). Everything above passes: none of them sits in either place.
///
/// Three bounds keep the shape itself honest. Two capitals at most clears `SHA256SUMS` and `UTF8`;
/// two digits and no third clears `Ed25519` and every `E0xxx` diagnostic code this project prints;
/// no letter or `_` after the digits clears `E2E`, `P2P` and `F32_TAG`. A `.`, `:` or backtick in
/// front means a qualified name (`Type.F32`, `ScalarVec::F32`), not a code.
fn milestone_code(line: &str) -> Option<String> {
    let b = line.as_bytes();
    for start in 0..b.len() {
        // Token boundary: a code is never the tail of a longer word, and never the tail of a
        // qualified name (`Type.F32`) or an inline code span.
        if start > 0 && matches!(b[start - 1], b'.' | b':' | b'`' | b'_') {
            continue;
        }
        if start > 0 && b[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut i = start;
        while i < b.len() && b[i].is_ascii_uppercase() && i - start < 2 {
            i += 1;
        }
        let letters = i - start;
        if letters == 0 || (i < b.len() && b[i].is_ascii_uppercase()) {
            continue; // no capitals, or a third one (`SHA256`)
        }
        let digits_at = i;
        while i < b.len() && b[i].is_ascii_digit() && i - digits_at < 2 {
            i += 1;
        }
        if i == digits_at {
            continue; // capitals with no digits: an ordinary acronym
        }
        // `Ed25519` has a third digit; `E0059` has two more.
        if i < b.len() && b[i].is_ascii_digit() {
            continue;
        }
        let mut end = i;
        // `N3.0` — a dotted tail, but only when a digit follows the dot (a sentence period does
        // not extend the code).
        if end + 1 < b.len() && b[end] == b'.' && b[end + 1].is_ascii_digit() {
            end += 2;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
        }
        // `E2E`, `P2P`, `F32_TAG`: a letter or `_` after the digits means this was never a code.
        if end < b.len() && (b[end].is_ascii_alphabetic() || b[end] == b'_') {
            continue;
        }
        if in_label_position(line, start, end) {
            return Some(line[start..end].to_string());
        }
    }
    None
}

/// Where a ledger reference is written, and a type name is not: closing a parenthetical
/// (`(package-manager P2.4)`, and `C2/C5)` for a pair), or introduced by a hyphenated lowercase
/// project name (`session-checker C2/C5`, `pre-C2`).
fn in_label_position(line: &str, start: usize, end: usize) -> bool {
    let b = line.as_bytes();
    // `… P2.4)` — the code closes a group this line opened.
    let mut after = end;
    // A slash-joined pair closes as one label: `C2/C5)`.
    if after < b.len() && b[after] == b'/' {
        while after < b.len() && (b[after].is_ascii_alphanumeric() || b[after] == b'/') {
            after += 1;
        }
    }
    if after < b.len() && b[after] == b')' && line[..start].contains('(') {
        return true;
    }
    // `pre-C2` — a lowercase word hyphenated straight onto the code.
    if start >= 2 && b[start - 1] == b'-' && b[start - 2].is_ascii_lowercase() {
        return true;
    }
    // `session-checker C2/C5` — a hyphenated project name introduces it.
    line[..start]
        .strip_suffix(' ')
        .map(|before| before.rsplit([' ', '(']).next().unwrap_or(""))
        .is_some_and(is_hyphenated_name)
}

/// An internal project label: a hyphenated lowercase name followed by the word that makes it a
/// ledger entry. `text-tiers arc`, `advisory-intake residual a`, `object-model slice 6`,
/// `namespace-protection #1`, and the `, tier 6)` that trails one.
///
/// Two things keep this precise, because `arc`, `slice` and `tier` are ordinary words and `tier`
/// is a user-facing concept here (`--tier debug`, a tier-1 JIT frame). The name in front must be
/// hyphenated, which "the arc of the animation" is not; and the label must sit in **label
/// position**, closing its parenthetical or naming an entry after it. So `a zero-copy slice of
/// the buffer` passes and `object-model slice 6` does not.
fn internal_label(line: &str) -> Option<String> {
    let lower = line.to_lowercase();
    for label in ["arc", "residual", "slice", "milestone"] {
        for (at, _) in lower.match_indices(label) {
            let after = &lower[at + label.len()..];
            // Label position: the end of a parenthetical, or an entry designator (`slice 6`,
            // `residual a`) — never an ordinary noun with a sentence after it.
            let labelled = after.is_empty()
                || after.starts_with([')', ',', ';'])
                || after
                    .strip_prefix(' ')
                    .and_then(|r| r.chars().next())
                    .is_some_and(|c| c.is_ascii_digit())
                || after.strip_prefix(' ').is_some_and(|r| {
                    // A one-letter designator and nothing more: `residual a)`, not `slice of`.
                    r.starts_with(|c: char| c.is_ascii_alphabetic())
                        && r.chars().nth(1).is_none_or(|c| !c.is_ascii_alphanumeric())
                })
                || after.starts_with(|c: char| c.is_ascii_digit());
            if !labelled {
                continue;
            }
            let Some(before) = lower[..at].strip_suffix(' ') else {
                continue;
            };
            let name: &str = before.rsplit([' ', '(']).next().unwrap_or("");
            if is_hyphenated_name(name) {
                return Some(format!("{name} {label}"));
            }
        }
    }
    // `(namespace-protection #1)` — the same label, numbered instead of named.
    for (at, _) in lower.match_indices(" #") {
        let digit = lower[at + 2..].chars().next();
        if !digit.is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let name: &str = lower[..at].rsplit([' ', '(']).next().unwrap_or("");
        if is_hyphenated_name(name) {
            return Some(format!("{name} #"));
        }
    }
    // `(… arc, tier 6)` — a trailing tier label. A parenthetical that closes right after
    // `tier <digit>` is a ledger reference; prose about a tier does not read that way.
    for (at, _) in lower.match_indices("tier ") {
        let rest = &lower[at + 5..];
        let mut chars = rest.chars();
        if chars.next().is_some_and(|c| c.is_ascii_digit()) && chars.next() == Some(')') {
            return Some(format!("tier {})", &rest[..1]));
        }
    }
    None
}

/// `advisory-intake`, `text-tiers` — a lowercase name with an interior hyphen, and nothing else.
fn is_hyphenated_name(word: &str) -> bool {
    let word = word.trim_start_matches(['(', '`', '*']);
    let Some((head, tail)) = word.split_once('-') else {
        return false;
    };
    !head.is_empty()
        && !tail.is_empty()
        && head.chars().all(|c| c.is_ascii_lowercase())
        && tail.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

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
        for shape in SHAPES {
            if let Some(hit) = (shape.find)(line) {
                out.push(format!(
                    "{label}:{}: `{hit}` — {}\n    {}",
                    i + 1,
                    shape.why,
                    line.trim()
                ));
            }
        }
    }
}

/// The `///` lines of a source file: what clap prints for a `Command` variant or an `#[arg]`
/// field. A `//!` module header and a `//` comment are not printed, and are not scanned.
fn doc_comments(text: &str) -> String {
    text.lines()
        .filter(|l| l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The help strings a *builder* supplies, which no `///` scan can see: `.help("…")` and
/// `.about("…")` on a `clap::Arg`/`Command`, and the `help:`/`about:` fields of an `ExtCommand`
/// an extension contributes. Both reach `--help` exactly like a doc comment, and the global
/// `--color` flag is built this way.
///
/// Multi-line literals are read to their closing quote, so a help string wrapped with a trailing
/// `\` is scanned whole rather than by its first line.
fn help_literals(text: &str) -> String {
    const OPENERS: [&str; 5] = [".help(", ".about(", ".long_about(", "help: ", "about: "];
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let Some(start) = OPENERS
            .iter()
            .filter_map(|o| text[at..].find(o).map(|i| at + i + o.len()))
            .min()
        else {
            break;
        };
        // The literal's opening quote. Only whitespace may precede it, so a `help: CONSTANT`
        // (or a `.about(` whose argument is a call) is skipped rather than swallowing the file
        // up to some unrelated quote.
        let Some(open) = bytes[start..]
            .iter()
            .position(|c| !matches!(c, b' ' | b'\n' | b'\r' | b'\t'))
            .map(|i| start + i)
            .filter(|i| bytes[*i] == b'"')
        else {
            at = start;
            continue;
        };
        let mut i = open + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        let end = i.min(bytes.len());
        out.push(text[open + 1..end].to_string());
        at = end + 1;
    }
    out.join("\n")
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
/// not, so only the doc comments are scanned. The builder-supplied strings are scanned beside
/// them: the global `--color`, `--watch` and `--stdio` flags carry their help in a `.help("…")`
/// call, and a doc-comment-only scan reads none of it.
#[test]
fn user_facing_help_carries_no_internal_vocabulary() {
    let src = repo_root().join("crates/noeta-cli/src/lib.rs");
    let text = std::fs::read_to_string(&src).expect("the CLI surface is readable");
    let literals = help_literals(&text);
    assert!(
        literals.contains("When to color diagnostics"),
        "the global flags' builder help moved — this scan is reading the wrong strings"
    );
    let help = format!("{}\n{literals}", doc_comments(&text));
    let mut violations = Vec::new();
    scan("crates/noeta-cli/src/lib.rs", "lib", &help, &mut violations);
    assert!(
        violations.is_empty(),
        "`noeta --help` is read by users, not maintainers \
         (AGENTS.md → Writing docs):\n\n{}\n",
        violations.join("\n")
    );
}

/// The commands an **extension** contributes print help too, and theirs is data rather than doc
/// comments: an `ExtCommand`'s `about` and each `ArgSpec`'s `help`, which `noeta test --help`,
/// `noeta bench --help`, `noeta doc --help` and `noeta serve --help` render. Same rule, same
/// reader, different storage.
#[test]
fn contributed_command_help_carries_no_internal_vocabulary() {
    let sources = [
        "crates/noeta-cli/src/tier_runner.rs",
        "crates/noeta-stdlib/src/serve.rs",
    ];
    let mut violations = Vec::new();
    for src in sources {
        let text = std::fs::read_to_string(repo_root().join(src)).expect("the source is readable");
        let literals = help_literals(&text);
        assert!(
            !literals.is_empty(),
            "{src} declares no help strings — this scan is reading the wrong file"
        );
        scan(src, "ext", &literals, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "a contributed command's `--help` is read by users, not maintainers \
         (AGENTS.md → Writing docs):\n\n{}\n",
        violations.join("\n")
    );
}

/// The same rules over the **conformance harness's** help, for the same reason: `--help` is read by
/// whoever is trying to verify a change, and a flag that announces an internal milestone tells that
/// reader nothing about what the flag does. Its `///` comments are the help text; the `//!` header
/// and ordinary comments are not printed and are not scanned.
#[test]
fn the_harness_help_carries_no_internal_vocabulary() {
    let src = repo_root().join("crates/noeta-conformance/src/main.rs");
    let text = std::fs::read_to_string(&src).expect("the harness surface is readable");
    // The flag block only: a `///` on a private function below it is rustdoc for a maintainer, and
    // holding internal notes to the user-facing rule would be a different (and wrong) claim.
    let help: String = text
        .lines()
        .skip_while(|l| !l.starts_with("struct Cli {"))
        .take_while(|l| !l.starts_with('}'))
        .filter(|l| l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        help.contains("--jit-differential"),
        "the harness's flag help moved — this scan is reading the wrong lines"
    );
    let mut violations = Vec::new();
    scan(
        "crates/noeta-conformance/src/main.rs",
        "main",
        &help,
        &mut violations,
    );
    assert!(
        violations.is_empty(),
        "`noeta-conformance --help` is read by whoever is verifying a change \
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
        // Every one of these shipped in `noeta --help`.
        "Add a dependency to the nearest `noeta.toml` (package-manager P2.4).",
        "Publish this package to the registry index (package-manager P2.5).",
        "Report the dependency tree's trust footprint (package-manager Phase 4)",
        "Claim a registry scope (namespace-protection #1). Self-service.",
        "Issue, file, or monitor security advisories (advisory-intake arc).",
        "List the reports queued for triage (advisory-intake residual a).",
        "Monitor the transparency log (advisory-intake arc, tier 6): verify",
        "Generate editor grammar artifacts for declared text tiers (text-tiers arc).",
        "cached compilations (`*.noeb` — the transparent startup cache, M3)",
        "Entries type-check before running — the default since session-checker C2/C5.",
        "Skip per-entry type checking (the pre-C2 behavior: every entry runs)",
        "When to colour diagnostics: auto, always, or never",
        "the declared-tier dispatch keeps its behaviour byte for byte",
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
        // The shapes above are token-tight on purpose. Each of these is real text from this
        // tree or its user-facing help, and each one sits a character away from a rule.
        "Manage the Ed25519 signing key used to attest published packages.",
        "verify the `.vsix` against the release's SHA256SUMS",
        "Explain a diagnostic code: `noeta explain E0059` (`e0059` works too).",
        "the p2p transport dials over QUIC",
        "an E2E run of the browser smoke test",
        "encoded as UTF8 and hashed with SHA256",
        "Install this exact release tag (e.g. `v0.2.0`) instead of the latest.",
        "Arm the tier-1 JIT while sampling (default: tier-0 pinned).",
        "Activate a dev-tier for this run, e.g. `--tier debug`, repeatable.",
        "a tier 1 frame is labeled in the flamegraph",
        "the two analyses ask about the same property from opposite sides",
        "a zero-copy slice of the backing buffer",
        "the arc of the animation is interpolated",
        // Lines that live in `docs/` today, each one a capital-over-digit token that is a type
        // name, a key or a product, and none of them in label position.
        "open a `.noe` file and press **F5** — the active file launches under the debugger",
        "the run profiles the extension picks up (F5 debugging over `noeta dap`)",
        "@packed struct V3 { x: f32  y: f32  z: f32 }",
        "ys = from_bytes::<V3>(blob)             // -> List<V3>",
        "echo vec.add_all(ps, ps)                // a column V3 list",
        "the scalars `Type.Int`, `Type.Float`, `Type.F32`, `Type.F64`",
        "// xs: Type.List(Type.F32) optional=false",
        "f32        : QNAN | F32_TAG | bits32                   (immediate packed f32)",
        "each aggregate points to a **shape** (a V8/JSC-style hidden class)",
        "This is the standard `-O0`-style trade: identical semantics, full observability.",
        "a reduction returns `NativeOut::Scalars(ScalarVec::F32(…))`",
    ] {
        let mut violations = Vec::new();
        scan("case", "Some-Page", ok, &mut violations);
        assert!(
            violations.is_empty(),
            "false positive on: {ok}\n{violations:?}"
        );
    }
}

/// The release this checkout *is*. Every crate inherits it (`version.workspace = true`), so
/// `noeta-cli`'s own package version is the workspace version.
const RELEASE: &str = env!("CARGO_PKG_VERSION");

/// The toolchain's own crates, read from the tree rather than listed here — a crate added later is
/// covered without anyone remembering to extend a table.
fn toolchain_crates() -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(repo_root().join("crates")).expect("crates/ is readable") {
        let name = entry.expect("readable entry").file_name();
        let name = name.to_string_lossy().to_string();
        if name.starts_with("noeta-") {
            out.push(name);
        }
    }
    out
}

/// The canonical toolchain repository. A `tag =` beside this URL pins a real release; the same
/// key beside `github.com/acme/…` pins a *fictional* package and is none of our business.
const TOOLCHAIN_REPO: &str = "github.com/noeta-lang/noeta";

/// Pull the value out of `key = "value"`, if the line has one.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let after = line.split_once(key)?.1;
    let after = after.trim_start().strip_prefix('=')?.trim_start();
    let rest = after.strip_prefix('"')?;
    rest.split_once('"').map(|(v, _)| v)
}

/// The version a line requires of `krate`, written either bare (`krate = "0.6"`) or as a table
/// (`krate = { version = "0.6" }`). A table pinning `git`/`tag` instead has no requirement to read —
/// the toolchain-repository rule above covers that form.
fn requirement_for<'a>(line: &'a str, krate: &str) -> Option<&'a str> {
    let after = line.split_once(krate)?.1;
    let after = after.trim_start().strip_prefix('=')?.trim_start();
    if let Some(table) = after.strip_prefix('{') {
        let end = table.find('}')?;
        return value_of(&table[..end], "version");
    }
    let rest = after.strip_prefix('"')?;
    rest.split_once('"').map(|(v, _)| v)
}

/// `0.6.0` → `0.6`. A crates.io requirement names the compatible range, not the patch.
fn major_minor(version: &str) -> &str {
    match version.match_indices('.').nth(1) {
        Some((i, _)) => &version[..i],
        None => version,
    }
}

/// Scan one page for dependency declarations that name the toolchain, and report the stale ones.
fn stale_declarations(stem: &str, text: &str, crates: &[String], out: &mut Vec<String>) {
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        // A git pin on the toolchain repository must name this release exactly.
        if let Some(tag) = value_of(line, "tag").filter(|_| line.contains(TOOLCHAIN_REPO)) {
            let want = format!("v{RELEASE}");
            if tag != want {
                out.push(format!(
                    "{stem}.md:{n}: pins the toolchain at `{tag}`, but this is `{want}` — a \
                     reader copying it gets a different toolchain than the page describes"
                ));
            }
        }
        // A published-crate requirement must name this release's compatible range.
        for krate in crates {
            let Some(req) = requirement_for(line, krate) else {
                continue;
            };
            let bare = req.trim_start_matches(['^', '~', '=']);
            if bare != major_minor(RELEASE) && bare != RELEASE {
                out.push(format!(
                    "{stem}.md:{n}: requires `{krate} = \"{req}\"`, but this is `{RELEASE}` — the \
                     range a reader copies must resolve the release they are reading about"
                ));
            }
        }
    }
}

/// A version in the wiki is either a **declaration a reader copies** — which must name this
/// release — or an illustration, which must not name a real one at all.
///
/// This gate covers the first kind, and only the first kind: a `tag =` beside the toolchain's own
/// repository URL, and a requirement on one of the toolchain's own crates. Everything else that
/// looks like a version in `docs/` is a float literal, a cache size, a percentage, or a *fictional*
/// package (`acme/http = "^1.0"`), and a rule wide enough to catch those would fire on all of them.
///
/// The checklist this replaces did not work. `Extension-Compatibility.md` told package authors to
/// depend on `noeta-conformance` at `v0.2.0` for **five releases**, on the very page the release
/// procedure names — the two lines it *listed* were updated each time and the two beside them were
/// not. An invariant that lives in a procedure decays exactly this way; one that fails a test does
/// not.
#[test]
fn dependency_snippets_name_the_current_release() {
    let crates = toolchain_crates();
    let docs = repo_root().join("docs");
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(&docs).expect("docs/ is readable") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .expect("a .md file has a stem")
            .to_string_lossy()
            .to_string();
        let text = std::fs::read_to_string(&path).expect("a doc page is readable");
        stale_declarations(&stem, &text, &crates, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "documentation pins a release this checkout is not:\n  {}",
        violations.join("\n  ")
    );
}

/// The gate above catches the drift it exists for, and stays quiet on the versions that are
/// deliberately not this release.
#[test]
fn the_release_gate_reads_declarations_and_not_illustrations() {
    let crates: Vec<String> = vec!["noeta-ext-abi".into(), "noeta-conformance".into()];
    let stale =
        format!("noeta-conformance = {{ git = \"https://{TOOLCHAIN_REPO}\", tag = \"v0.2.0\" }}");
    for caught in [
        stale.as_str(),
        "noeta-ext-abi = \"0.2\"",
        "noeta-ext-abi = { version = \"0.4.0\" }",
    ] {
        let mut violations = Vec::new();
        stale_declarations("Some-Page", caught, &crates, &mut violations);
        assert!(
            !violations.is_empty(),
            "the gate would not have caught: {caught}"
        );
    }
    // A fictional package's tag, a user's declared floor, and the hypothetical older pin in the
    // composition prose are all versions that must NOT be dragged along by a release.
    for ok in [
        "http = { git = \"https://github.com/acme/http\", tag = \"v1.2.0\" }",
        "toolchain = \">=0.2\"",
        "A package pinned at an older tag (say `v0.2.0`) still composes under a newer binary.",
        "codec = { version = \"^1.0\", package = \"acme/imgcodec\" }",
    ] {
        let mut violations = Vec::new();
        stale_declarations("Some-Page", ok, &crates, &mut violations);
        assert!(
            violations.is_empty(),
            "false positive on: {ok}\n{violations:?}"
        );
    }
}
