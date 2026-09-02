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

/// The set of fence languages a page may use.
///
/// `noeta` is the one the sample gate extracts. Everything else is inert prose as far as
/// `doc_samples` is concerned, which is exactly why this list is closed: a block tagged with
/// anything outside it compiles nowhere and is checked by nothing.
const FENCE_LANGUAGES: &[&str] = &[
    "", "noeta", "text", "console", "toml", "sh", "rust", "yaml", "lua", "json",
];

/// **Every fenced block carries a language this repo knows.**
///
/// `.noe` is the file extension, so ```` ```noe ```` is the natural typo for ```` ```noeta ````,
/// and it is invisible: `doc_samples` extracts on the exact tag `noeta`, so a mistagged block of
/// real Noeta is never compiled and never run. Three had accumulated across two pages, one of them
/// a full worked example of a method bundle.
///
/// That is the `.noe`-in-a-`#[test]` hazard in a form grep for `.noe` does not find. A closed set
/// turns it into a failing test at the moment it is written.
#[test]
fn every_fence_names_a_language_the_repo_knows() {
    let docs = repo_root().join("docs");
    let mut violations = Vec::new();

    for entry in std::fs::read_dir(&docs).expect("read docs/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read page");

        let mut open = false;
        for (i, line) in text.lines().enumerate() {
            let Some(rest) = line.strip_prefix("```") else {
                continue;
            };
            // A closing fence carries no info string; only opening fences are checked.
            if open {
                open = false;
                continue;
            }
            open = true;
            let lang = rest.split_whitespace().next().unwrap_or("");
            if !FENCE_LANGUAGES.contains(&lang) {
                violations.push(format!(
                    "  {stem}.md:{}: ```{lang} is not a language this repo knows — \
                     did you mean ```noeta? (a mistagged block is compiled by nothing)",
                    i + 1
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "documentation uses a fence language nothing checks:\n\n{}\n",
        violations.join("\n")
    );
}
