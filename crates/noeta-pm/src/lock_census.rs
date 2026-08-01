//! **The `noeta.lock` field census**: every field that reaches the lockfile is classified here as
//! round-tripped, deliberately write-only, or deliberately unwritten — and the classification is
//! checked against a real write/read.
//!
//! ## The bug class
//!
//! [`crate::lock::Lock::read`] is a hand-rolled `table.get("…").and_then(as_str)` walk;
//! [`crate::lock`]'s `render` is a hand-rolled `push_str(format!(…))` walk. They share no schema, and
//! they sit two hundred lines apart in two idioms, so adding a `[[package]]` field means
//! remembering both.
//!
//! The subtle part — and the reason "the two sets should match" is *not* the invariant — is that
//! four fields are already write-only **on purpose**: `native` and `edition` are re-derived from the
//! manifests on every resolve, `source` is a label the reader infers from which keys are present,
//! and a path dependency's `path` comes from the manifest, never from the lock. So a field that is
//! *accidentally* write-only looks exactly like the four that are meant to be, and every test in
//! `lock.rs` constructs `LockedPackage` by hand and so cannot see the difference.
//!
//! Usually the cost is degradation: a missed pin means a re-resolve. **Except** for the trust pins —
//! `scope_trust`, `log_trust`, `advisory_trust` — where a dropped pin silently turns a
//! trust-on-first-use *downgrade defense* into a fresh trust-on-first-use against whatever the
//! registry currently serves. That is the row worth a gate.
//!
//! ## What this gate does
//!
//! [`CENSUS`] classifies **every** field of every type whose values are rendered into `noeta.lock`,
//! and two properties are checked:
//!
//! - **Completeness** — the declarations are parsed out of the sources and every field must appear
//!   in [`CENSUS`] exactly once. Adding a field to `LockedPackage` or to a trust pin fails
//!   [`every_lockfile_field_is_classified`] until its author says which it is.
//! - **Agreement** — one canonical lock is written and read back. A [`Verdict::RoundTrip`] field
//!   must come back through `Lock`'s **public accessors** (a value parsed into a private map and
//!   never exposed is dropped just the same); a [`Verdict::WriteOnly`] field must actually appear
//!   in the rendered text, so a field silently dropped from `render` fails here rather than
//!   masquerading as one of the deliberate four; a [`Verdict::Unwritten`] field must not.
//!
//! ## Why a census and not one serde schema
//!
//! Deriving both directions from a single `#[derive(Serialize, Deserialize)]` wire struct was the
//! first choice considered, and it is the right answer for the `.noeb` payload — but not here, for
//! four reasons.
//!
//! 1. **It would not remove this class.** serde makes both directions *exist*; it says nothing about
//!    whether a field is populated from `LockedPackage` on the way out or consumed into `Lock` on
//!    the way in. The two hand-written TOML walks would become two hand-written conversion
//!    functions, and a field could still be written and never read — the finding, unchanged.
//! 2. **`Lock` is deliberately not the wire shape.** It is a set of pre-keyed projections —
//!    `git_pins` keyed by `(url, GitRef::lock_key())`, `coords`, `shas`, `versions` — built so the
//!    resolve-time lookups are direct. That reshaping, and the legacy-keyless migration that runs
//!    over it, is what `read` is doing beyond parsing.
//! 3. **`read` is lenient per entry; serde is strict per file.** A malformed `[[package]]` currently
//!    `continue`s and every other pin survives. Under `from_str` one bad table drops the whole lock
//!    — including every trust pin — which is precisely the failure this census exists to guard.
//! 4. **The rendering is a committed artifact.** `noeta.lock` is generated *and checked in*; a serde
//!    round-trip would rewrite every lock in the tree for no behavioural gain.
//!
//! The technique — a table checked against the declarations *and* against real behaviour — is the
//! one used by `noeta-ext-abi/tests/constraint_fields.rs` and
//! `noeta-bytecode/tests/op_jump_pc_census.rs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::edition::Edition;
use crate::graph::{LockedPackage, ResolvedSource};
use crate::lock::{AdvisoryTrust, LOCK_NAME, Lock, LogTrust, ScopeTrust};
use crate::manifest::GitRef;

/// What a field's presence in `noeta.lock` means.
enum Verdict {
    /// Written **and** read back. `recover` pulls it out of a `Lock` through the public accessors a
    /// real consumer uses; it must equal the value the fixture wrote.
    RoundTrip {
        recover: fn(&Lock) -> Option<String>,
        expect: &'static str,
    },
    /// Written and deliberately never read, with the reason. The rendered text must still contain
    /// `rendered_as` — a field dropped from `render` is a different bug and fails here.
    WriteOnly {
        rendered_as: &'static str,
        reason: &'static str,
    },
    /// Deliberately not written at all, with the reason. The rendered text must not contain
    /// `absent_marker`.
    Unwritten {
        absent_marker: &'static str,
        reason: &'static str,
    },
}

/// The canonical fixture's values, distinct from one another so a recovery that reads the wrong
/// field cannot accidentally pass.
const GIT_ID: &str = "acme/greet";
const GIT_VERSION: &str = "1.2.3";
const GIT_HASH: &str = "d1ce5e11ed";
const GIT_URL: &str = "https://example.com/acme/greet";
const GIT_TAG: &str = "v1.2.3";
const GIT_SHA: &str = "1111111111111111111111111111111111111111";
const BRANCH_ID: &str = "acme/edge";
const BRANCH_URL: &str = "https://example.com/acme/edge";
const BRANCH_NAME: &str = "trunk";
const BRANCH_SHA: &str = "2222222222222222222222222222222222222222";
const PATH_ID: &str = "acme/local";
const PATH_DIR: &str = "../vendored-local";
const NATIVE_DIR: &str = "native-crate";
const KEY_SCOPE: &str = "legacy";
const KEY_HEX: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const KEYLESS_ISSUER: &str = "https://token.actions.githubusercontent.com";
const KEYLESS_IDENTITY: &str =
    "https://github.com/acme/greet/.github/workflows/release.yaml@refs/heads/main";
const LOG_KEY: &str = "10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad10ad";
const LOG_ROOT: &str = "beefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeef";
const LOG_SIZE: u64 = 4242;
const ADVISORY_KEY: &str = "cafecafecafecafecafecafecafecafecafecafecafecafecafecafecafecafe";
const ADVISORY_DIGEST: &str = "0ddba110ddba110ddba110ddba110ddba110ddba110ddba110ddba110ddba110";
const ADVISORY_COUNT: u64 = 77;

/// Every field that reaches `noeta.lock`, and what its presence there means.
///
/// The keys are `Type.field` for a struct and `Type::Variant.field` (or `.0` for a tuple variant)
/// for an enum, spelled exactly as [`declared_fields`] reads them out of the sources.
const CENSUS: &[(&str, Verdict)] = &[
    // ── LockedPackage ─────────────────────────────────────────────────────────────────────────
    (
        "LockedPackage.identity",
        Verdict::RoundTrip {
            recover: |lock| {
                lock.locked_versions()
                    .map(|(id, _)| id.clone())
                    .find(|id| id == GIT_ID)
            },
            expect: GIT_ID,
        },
    ),
    (
        "LockedPackage.version",
        Verdict::RoundTrip {
            recover: |lock| lock.locked_version(GIT_ID).map(Version::to_string),
            expect: GIT_VERSION,
        },
    ),
    (
        "LockedPackage.content_hash",
        Verdict::RoundTrip {
            recover: |lock| lock.content_hash(GIT_ID).map(str::to_string),
            expect: GIT_HASH,
        },
    ),
    (
        "LockedPackage.source",
        Verdict::WriteOnly {
            rendered_as: "source = \"git\"",
            reason: "the reader infers the shape from which keys are present (`url`+`sha` is a git \
                     pin), never from this label — it is written for the human reading the file",
        },
    ),
    (
        "LockedPackage.native",
        Verdict::WriteOnly {
            rendered_as: "native = \"native-crate\"",
            reason: "re-derived from the dependency's own manifest on every resolve; the lock \
                     records it so the file describes the build, not so the build reads it back",
        },
    ),
    (
        "LockedPackage.edition",
        Verdict::WriteOnly {
            rendered_as: "edition = \"2026\"",
            reason: "re-derived from the dependency's own manifest on every resolve (follow-on \
                     F1), like `native`",
        },
    ),
    (
        "LockedPackage.patched",
        Verdict::Unwritten {
            absent_marker: PATH_ID,
            reason: "a `[patch]`ed identity is omitted from the lock entirely — not even a marker \
                     — because a mutable local override is not reproducible state (see `render`)",
        },
    ),
    // ── ResolvedSource ────────────────────────────────────────────────────────────────────────
    (
        "ResolvedSource::Path.path",
        Verdict::WriteOnly {
            rendered_as: "path = \"../vendored-local\"",
            reason: "a path dependency is materialized from the root manifest's declaration, never \
                     from the lock; the pin would only ever disagree with it",
        },
    ),
    (
        "ResolvedSource::Git.url",
        Verdict::RoundTrip {
            recover: |lock| lock.registry_coords(GIT_ID).map(|(u, _, _)| u.to_string()),
            expect: GIT_URL,
        },
    ),
    (
        "ResolvedSource::Git.git_ref",
        Verdict::RoundTrip {
            recover: |lock| lock.registry_coords(GIT_ID).map(|(_, t, _)| t.to_string()),
            expect: GIT_TAG,
        },
    ),
    (
        "ResolvedSource::Git.sha",
        Verdict::RoundTrip {
            recover: |lock| lock.git_sha(GIT_ID).map(str::to_string),
            expect: GIT_SHA,
        },
    ),
    // ── GitRef — the ref a SHA was pinned under, and the key the resolve-time lookup rebuilds ──
    (
        "GitRef::Tag.0",
        Verdict::RoundTrip {
            recover: |lock| {
                lock.git_pin(GIT_URL, &GitRef::Tag(GIT_TAG.to_string()))
                    .map(str::to_string)
            },
            expect: GIT_SHA,
        },
    ),
    (
        "GitRef::Branch.0",
        Verdict::RoundTrip {
            recover: |lock| {
                lock.git_pin(BRANCH_URL, &GitRef::Branch(BRANCH_NAME.to_string()))
                    .map(str::to_string)
            },
            expect: BRANCH_SHA,
        },
    ),
    // ── ScopeTrust — the downgrade defense. A dropped pin here is a fresh TOFU, not a re-resolve ─
    (
        "ScopeTrust::Key.0",
        Verdict::RoundTrip {
            recover: |lock| match lock.trust_for(&format!("{KEY_SCOPE}/anything")) {
                Some(ScopeTrust::Key(key)) => Some(key.clone()),
                _ => None,
            },
            expect: KEY_HEX,
        },
    ),
    (
        "ScopeTrust::Keyless.issuer",
        Verdict::RoundTrip {
            recover: |lock| match lock.trust_for(GIT_ID) {
                Some(ScopeTrust::Keyless { issuer, .. }) => Some(issuer.clone()),
                _ => None,
            },
            expect: KEYLESS_ISSUER,
        },
    ),
    (
        "ScopeTrust::Keyless.identity",
        Verdict::RoundTrip {
            recover: |lock| match lock.trust_for(GIT_ID) {
                Some(ScopeTrust::Keyless { identity, .. }) => Some(identity.clone()),
                _ => None,
            },
            expect: KEYLESS_IDENTITY,
        },
    ),
    // ── LogTrust / AdvisoryTrust — append-only heads; a dropped pin re-TOFUs the whole log ─────
    (
        "LogTrust.public_key",
        Verdict::RoundTrip {
            recover: |lock| lock.log_trust().map(|l| l.public_key.clone()),
            expect: LOG_KEY,
        },
    ),
    (
        "LogTrust.tree_size",
        Verdict::RoundTrip {
            recover: |lock| lock.log_trust().map(|l| l.tree_size.to_string()),
            expect: "4242",
        },
    ),
    (
        "LogTrust.root_hash",
        Verdict::RoundTrip {
            recover: |lock| lock.log_trust().map(|l| l.root_hash.clone()),
            expect: LOG_ROOT,
        },
    ),
    (
        "AdvisoryTrust.public_key",
        Verdict::RoundTrip {
            recover: |lock| lock.advisory_trust().map(|a| a.public_key.clone()),
            expect: ADVISORY_KEY,
        },
    ),
    (
        "AdvisoryTrust.count",
        Verdict::RoundTrip {
            recover: |lock| lock.advisory_trust().map(|a| a.count.to_string()),
            expect: "77",
        },
    ),
    (
        "AdvisoryTrust.digest",
        Verdict::RoundTrip {
            recover: |lock| lock.advisory_trust().map(|a| a.digest.clone()),
            expect: ADVISORY_DIGEST,
        },
    ),
];

/// The declarations the census covers: `(crate-relative source file, type name)`.
const DECLARATIONS: &[(&str, &str)] = &[
    ("src/graph.rs", "LockedPackage"),
    ("src/graph.rs", "ResolvedSource"),
    ("src/manifest.rs", "GitRef"),
    ("src/lock.rs", "ScopeTrust"),
    ("src/lock.rs", "LogTrust"),
    ("src/lock.rs", "AdvisoryTrust"),
];

// ── the fixture ────────────────────────────────────────────────────────────────────────────────

fn git_pkg() -> LockedPackage {
    LockedPackage {
        identity: GIT_ID.to_string(),
        version: Version::parse(GIT_VERSION).unwrap(),
        content_hash: GIT_HASH.to_string(),
        source: ResolvedSource::Git {
            url: GIT_URL.to_string(),
            git_ref: GitRef::Tag(GIT_TAG.to_string()),
            sha: GIT_SHA.to_string(),
        },
        native: Some(NATIVE_DIR.to_string()),
        edition: Edition::E2026,
        patched: false,
    }
}

fn branch_pkg() -> LockedPackage {
    LockedPackage {
        identity: BRANCH_ID.to_string(),
        version: Version::new(0, 1, 0),
        content_hash: "edge0edge0".to_string(),
        source: ResolvedSource::Git {
            url: BRANCH_URL.to_string(),
            git_ref: GitRef::Branch(BRANCH_NAME.to_string()),
            sha: BRANCH_SHA.to_string(),
        },
        native: None,
        edition: Edition::E2026,
        patched: false,
    }
}

/// The path package is also the **patched** one: a `[patch]`ed identity must leave no trace, which
/// is what `LockedPackage.patched`'s `Unwritten` verdict asserts. Its `path` is still rendered by
/// the unpatched twin below.
fn patched_path_pkg() -> LockedPackage {
    LockedPackage {
        identity: PATH_ID.to_string(),
        version: Version::new(0, 2, 0),
        content_hash: "10ca110ca1".to_string(),
        source: ResolvedSource::Path {
            path: PathBuf::from(PATH_DIR),
        },
        native: None,
        edition: Edition::E2026,
        patched: true,
    }
}

/// The unpatched path package — same source shape, a different identity, so `path = …` is rendered.
/// Written out field by field rather than with `..patched_path_pkg()`: every fixture in this file
/// names every field, so adding one to `LockedPackage` stops compiling here too.
fn path_pkg() -> LockedPackage {
    LockedPackage {
        identity: "acme/vendored".to_string(),
        version: Version::new(0, 3, 0),
        content_hash: "0e0d0e0d0e".to_string(),
        source: ResolvedSource::Path {
            path: PathBuf::from(PATH_DIR),
        },
        native: None,
        edition: Edition::E2026,
        patched: false,
    }
}

fn canonical_lock_text(dir: &Path) -> String {
    let mut scope_trust = std::collections::BTreeMap::new();
    scope_trust.insert(KEY_SCOPE.to_string(), ScopeTrust::Key(KEY_HEX.to_string()));
    scope_trust.insert(
        GIT_ID.to_string(),
        ScopeTrust::Keyless {
            issuer: KEYLESS_ISSUER.to_string(),
            identity: KEYLESS_IDENTITY.to_string(),
        },
    );
    let log = LogTrust {
        public_key: LOG_KEY.to_string(),
        tree_size: LOG_SIZE,
        root_hash: LOG_ROOT.to_string(),
    };
    let advisory = AdvisoryTrust {
        public_key: ADVISORY_KEY.to_string(),
        count: ADVISORY_COUNT,
        digest: ADVISORY_DIGEST.to_string(),
    };
    crate::lock::write(
        dir,
        &[git_pkg(), branch_pkg(), path_pkg(), patched_path_pkg()],
        &scope_trust,
        Some(&log),
        Some(&advisory),
    )
    .expect("writing the canonical lock");
    std::fs::read_to_string(dir.join(LOCK_NAME)).expect("reading the canonical lock back")
}

// ── the declaration parse ──────────────────────────────────────────────────────────────────────

/// The body of a braced item starting at the first `{` at or after `from`, brace-matched.
fn braced_body(src: &str, from: usize) -> &str {
    let start = src[from..].find('{').expect("a `{` after the item header") + from;
    let mut depth = 0usize;
    for (i, c) in src[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[start + 1..start + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces");
}

/// The comment-free, whitespace-collapsed body of `ty`'s declaration in `file`.
fn declaration_body(file: &str, ty: &str) -> (bool, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let (is_enum, at) = match (
        src.find(&format!("pub struct {ty} {{")),
        src.find(&format!("pub enum {ty} {{")),
    ) {
        (Some(at), _) => (false, at),
        (None, Some(at)) => (true, at),
        (None, None) => panic!("`{ty}` is not declared in {file}"),
    };
    let body = braced_body(&src, at);
    let stripped: String = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with('#') && !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (is_enum, stripped)
}

/// Every field of `ty` as the census spells it: `Type.field`, `Type::Variant.field`, or
/// `Type::Variant.0` for a tuple variant. A unit variant contributes nothing.
fn declared_fields(file: &str, ty: &str) -> Vec<String> {
    let (is_enum, body) = declaration_body(file, ty);
    if !is_enum {
        return named_fields(&body)
            .into_iter()
            .map(|f| format!("{ty}.{f}"))
            .collect();
    }

    let mut out = Vec::new();
    let bytes: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        // Skip to the start of the next variant name.
        while i < bytes.len() && !bytes[i].is_ascii_uppercase() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
            i += 1;
        }
        let variant: String = bytes[start..i].iter().collect();
        while i < bytes.len() && bytes[i] == ' ' {
            i += 1;
        }
        match bytes.get(i) {
            Some('{') => {
                let (inner, next) = balanced(&bytes, i, '{', '}');
                for f in named_fields(&inner) {
                    out.push(format!("{ty}::{variant}.{f}"));
                }
                i = next;
            }
            Some('(') => {
                let (inner, next) = balanced(&bytes, i, '(', ')');
                for n in 0..top_level_commas(&inner) + 1 {
                    out.push(format!("{ty}::{variant}.{n}"));
                }
                i = next;
            }
            // A unit variant, or the trailing `,` of the previous one.
            _ => {}
        }
    }
    out
}

/// The text between `open` at `at` and its match, plus the index just past the closer.
fn balanced(chars: &[char], at: usize, open: char, close: char) -> (String, usize) {
    let mut depth = 0usize;
    for i in at..chars.len() {
        if chars[i] == open {
            depth += 1;
        } else if chars[i] == close {
            depth -= 1;
            if depth == 0 {
                return (chars[at + 1..i].iter().collect(), i + 1);
            }
        }
    }
    panic!("unbalanced `{open}`");
}

fn top_level_commas(inner: &str) -> usize {
    let mut depth = 0i32;
    let mut n = 0usize;
    for c in inner.chars() {
        match c {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            ',' if depth == 0 => n += 1,
            _ => {}
        }
    }
    n
}

/// `name` for every `name: Type` at the top level of a (whitespace-collapsed) field list.
fn named_fields(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut depth = 0i32;
    let mut i = 0usize;
    let mut word = String::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '(' | '<' | '[' | '{' => {
                depth += 1;
                word.clear();
            }
            ')' | '>' | ']' | '}' => {
                depth -= 1;
                word.clear();
            }
            ':' if depth == 0 => {
                // `pub name:` — `pub` is a separate word, so `word` holds the field name.
                if !word.is_empty() && word != "pub" {
                    out.push(word.clone());
                }
                word.clear();
                // Skip the type, to the next top-level `,`.
                let mut d = 0i32;
                i += 1;
                while i < chars.len() {
                    match chars[i] {
                        '(' | '<' | '[' | '{' => d += 1,
                        ')' | '>' | ']' | '}' => d -= 1,
                        ',' if d == 0 => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            c if c.is_ascii_alphanumeric() || c == '_' => word.push(c),
            _ => word.clear(),
        }
        i += 1;
    }
    out
}

// ── the properties ─────────────────────────────────────────────────────────────────────────────

/// Every declared field is classified exactly once. Adding a field to `LockedPackage`, a trust pin
/// or a source variant fails here until its author says whether the lock reads it back.
#[test]
fn every_lockfile_field_is_classified() {
    let declared: Vec<String> = DECLARATIONS
        .iter()
        .flat_map(|(file, ty)| declared_fields(file, ty))
        .collect();
    assert!(
        declared.len() >= 15,
        "the declaration parse found only {} fields — a declaration's shape changed and this \
         census is no longer reading it: {declared:?}",
        declared.len()
    );

    let classified: BTreeSet<&str> = CENSUS.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        classified.len(),
        CENSUS.len(),
        "CENSUS has a duplicate entry"
    );

    let declared_set: BTreeSet<&str> = declared.iter().map(String::as_str).collect();
    let unclassified: Vec<&&str> = declared_set.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "these fields reach `noeta.lock` and are unclassified:\n  {}\n\nEach is either read back \
         by `Lock::read` (`RoundTrip`, with the accessor that recovers it) or deliberately not \
         (`WriteOnly`, with the reason it is safe to drop). Four fields are legitimately write-only \
         — `source`, `native`, `edition` and a path dependency's `path` — which is exactly why an \
         *accidentally* write-only field cannot be spotted by eye and has to be declared here.",
        unclassified
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let stale: Vec<&&str> = classified.difference(&declared_set).collect();
    assert!(
        stale.is_empty(),
        "CENSUS classifies fields that are no longer declared: {stale:?}"
    );
}

/// The classification is checked against a real write/read: every `RoundTrip` field comes back
/// through the public accessors, every `WriteOnly` field is genuinely written, and the `Unwritten`
/// one leaves no trace.
#[test]
fn the_census_matches_what_the_lockfile_actually_carries() {
    let dir = crate::test_temp::TempDir::new("lock-census");
    let text = canonical_lock_text(&dir);
    let lock = Lock::read(&dir);

    let mut dropped = Vec::new();
    for (field, verdict) in CENSUS {
        match verdict {
            Verdict::RoundTrip { recover, expect } => {
                let got = recover(&lock);
                if got.as_deref() != Some(*expect) {
                    dropped.push(format!(
                        "{field}: written as {expect:?}, read back as {got:?}"
                    ));
                }
            }
            Verdict::WriteOnly {
                rendered_as,
                reason,
            } => {
                assert!(
                    text.contains(rendered_as),
                    "{field} is classified write-only ({reason}) but `render` did not write \
                     {rendered_as:?} at all — that is a different bug, and the lock now describes a \
                     build it cannot reproduce:\n{text}"
                );
            }
            Verdict::Unwritten {
                absent_marker,
                reason,
            } => {
                assert!(
                    !text.contains(absent_marker),
                    "{field} is classified unwritten ({reason}) but {absent_marker:?} appears in \
                     the rendered lock:\n{text}"
                );
            }
        }
    }

    assert!(
        dropped.is_empty(),
        "these fields are classified `RoundTrip` but do not survive write → read:\n  {}\n\nA pin \
         that is written and never read back is silent: for a version or a hash it costs a \
         re-resolve, but for `scope_trust` / `log_trust` / `advisory_trust` it turns a \
         trust-on-first-use *downgrade defense* into a fresh trust-on-first-use against whatever \
         the registry currently serves. Either restore the read side, or — if the drop is \
         deliberate — reclassify the field `WriteOnly` with the reason.",
        dropped.join("\n  ")
    );
}

/// `LOCK_VERSION` is checked strictly (an unrecognised version reads as an empty lock, so the build
/// re-resolves rather than misreading pins). That is the right failure direction, and this pins it
/// against a well-meant loosening.
#[test]
fn an_unrecognised_lock_version_yields_no_pins() {
    let dir = crate::test_temp::TempDir::new("lock-census-version");
    let text = canonical_lock_text(&dir);
    assert!(Lock::read(&dir).log_trust().is_some(), "the fixture pins");

    let bumped = text.replacen("version = 2", "version = 999", 1);
    std::fs::write(dir.join(LOCK_NAME), &bumped).unwrap();
    let lock = Lock::read(&dir);
    assert!(lock.log_trust().is_none());
    assert!(lock.advisory_trust().is_none());
    assert!(lock.trust_for(GIT_ID).is_none());
    assert!(lock.locked_version(GIT_ID).is_none());
}
