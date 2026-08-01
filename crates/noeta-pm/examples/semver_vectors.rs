//! The semver differential — the twelfth cross-repo duplicate, made reproducible.
//!
//! `noeta audit` decides "is this resolved version affected by this advisory" with
//! [`semver::VersionReq::matches`] (`crate::advisory::Advisory::affects`). The registry's web UI
//! answers the same question for a *reader* with a **hand port** of that function,
//! `noeta-registry/src/semver.ts`. Two implementations of one predicate, in two repos, with nothing
//! forcing them together: the port's test file said every expectation "was differentially checked
//! against `semver::VersionReq::matches` itself (a throwaway Rust binary over this exact case
//! list)" — and that binary was in neither repo. The differential ran once, by hand, and could not
//! be re-run.
//!
//! This example *is* that binary, committed. It owns the case list, evaluates every case with the
//! real crate, and emits the answers as JSON:
//!
//! ```text
//! cargo run -p noeta-pm --example semver_vectors > crates/noeta-pm/test_data/wire/semver-vectors.json
//! ```
//!
//! The generated file is part of the shared wire-fixture set (see `test_data/wire/README.md`), so
//! it is hashed by `MANIFEST.sha256`, pinned by [`crate::registry::WIRE_MANIFEST_SHA256`] on both
//! sides, and copied to the registry repo by `scripts/sync-wire-fixtures.sh` like every other
//! fixture. Both suites then *consume* it:
//!
//! - Rust (`semver_vectors_still_match_the_crate` in `src/registry.rs`) re-evaluates every case with
//!   `VersionReq::matches` and fails if the committed file drifted from the crate — so a semver
//!   upgrade that changes a corner is a failing test, not a silent change of meaning.
//! - TypeScript (`test/semver.test.ts`) runs every case through `satisfies` and fails if the hand
//!   port disagrees — so a divergence is a failing build in the repo that owns the port.
//!
//! ## The `expected` encoding
//!
//! `null` means **unknown**: one of the two strings did not parse. That is the TS port's contract
//! (`satisfies` answers `null` rather than guessing, because a wrong "not affected" is the one
//! outcome worth engineering against). Rust's `Advisory::affects` collapses the same case to
//! `false` (`VersionReq::parse(..).is_ok_and(..)` — a malformed advisory range fabricates no hit),
//! which is the same refusal expressed as a verdict. Both are recorded: `expected` is the
//! three-valued match result, `affects` is what `noeta audit` actually does with it.

use semver::{Version, VersionReq};

/// Every case the port's tests claimed to have checked, plus the corners they did not.
///
/// Grouped by the rule each one pins; the group label is carried into the JSON so a failure names
/// the rule rather than an index. Add cases here, never in `test/semver.test.ts` — that file is a
/// consumer of this list, and a case it invented would be checked against nothing.
const CASES: &[(&str, &[(&str, &str)])] = &[
    (
        "caret: ^1.2.3 := >=1.2.3, <2.0.0",
        &[
            ("1.2.3", "^1.2.3"),
            ("1.2.4", "^1.2.3"),
            ("1.9.9", "^1.2.3"),
            ("1.2.2", "^1.2.3"),
            ("2.0.0", "^1.2.3"),
            ("1.2.3", "1.2.3"), // a bare version defaults to caret
            ("1.9.0", "1.2.3"),
            ("2.0.0", "1.2.3"),
        ],
    ),
    (
        "caret: the minor is the breaking axis below 1.0",
        &[
            ("0.2.3", "^0.2.3"),
            ("0.2.9", "^0.2.3"),
            ("0.3.0", "^0.2.3"),
            ("0.2.2", "^0.2.3"),
            ("0.0.3", "^0.0.3"),
            ("0.0.4", "^0.0.3"),
            ("0.0.2", "^0.0.3"),
        ],
    ),
    (
        "caret: a partial requirement widens",
        &[
            ("1.9.9", "^1"),
            ("2.0.0", "^1"),
            ("0.9.9", "^0"),
            ("1.0.0", "^0"),
            ("1.2.9", "^1.2"),
            ("1.3.0", "^1.2"),
            ("0.2.9", "^0.2"),
            ("0.3.0", "^0.2"),
        ],
    ),
    (
        "tilde",
        &[
            ("1.2.3", "~1.2.3"),
            ("1.2.9", "~1.2.3"),
            ("1.3.0", "~1.2.3"),
            ("1.2.2", "~1.2.3"),
            ("1.2.9", "~1.2"),
            ("1.3.0", "~1.2"),
            ("1.9.9", "~1"),
            ("2.0.0", "~1"),
        ],
    ),
    (
        "comparators",
        &[
            ("1.2.3", "=1.2.3"),
            ("1.2.4", "=1.2.3"),
            ("1.2.3", "=1.2"),
            ("1.9.9", "=1"),
            ("1.3.0", ">1.2.3"),
            ("1.2.3", ">1.2.3"),
            ("1.2.3", ">=1.2.3"),
            ("1.2.2", ">=1.2.3"),
            ("1.2.3", "<1.2.4"),
            ("1.2.4", "<1.2.4"),
            ("1.2.3", "<=1.2.3"),
            ("1.2.4", "<=1.2.3"),
        ],
    ),
    (
        "wildcards",
        &[
            ("1.5.0", "1.*"),
            ("2.0.0", "1.*"),
            ("1.2.9", "1.2.*"),
            ("1.3.0", "1.2.*"),
            ("1.0.0", "*"),
            ("1.0.0", "1.x"),
            ("1.0.0", "1.X"),
        ],
    ),
    (
        "the advisory fixture's range (NOETA-2026-0001)",
        &[
            ("1.0.0", ">=1.0.0, <1.2.0"),
            ("1.1.9", ">=1.0.0, <1.2.0"),
            ("1.2.0", ">=1.0.0, <1.2.0"),
            ("0.9.9", ">=1.0.0, <1.2.0"),
        ],
    ),
    (
        "pre-release: only a comparator that opted into the release line matches",
        &[
            ("1.2.4-alpha", "^1.2.3"),
            ("1.2.3-alpha", "^1.2.3"),
            ("1.2.3-beta", ">=1.2.3-alpha, <1.3.0"),
            ("1.2.3-alpha", ">=1.2.3-alpha, <1.3.0"),
            ("1.0.0-alpha", "*"),
            ("1.0.0", "*"),
            ("1.0.0-alpha", ">=1.0.0-alpha"),
        ],
    ),
    (
        "pre-release: SemVer §11 precedence",
        &[
            ("1.0.0", ">1.0.0-alpha"),
            ("1.0.0-alpha.1", ">1.0.0-alpha.1, <1.0.1"),
            ("1.0.0-alpha.2", ">1.0.0-alpha.1, <1.0.1"),
            ("1.0.0-alpha.beta", ">1.0.0-alpha.1, <1.0.1"),
            ("1.0.0-alpha", ">1.0.0-alpha.1, <1.0.1"),
            ("1.0.0-1", ">1.0.0-alpha.1, <1.0.1"),
            ("1.0.0-alpha.1.2", ">1.0.0-alpha.1, <1.0.1"),
        ],
    ),
    (
        "build metadata is accepted and ignored",
        &[("1.2.3+build.5", "^1.2.3"), ("1.2.3", "^1.2.3+build.5")],
    ),
    (
        "unparseable input is unknown, never a verdict",
        &[
            ("1.0.0", "not a range"),
            ("1.0.0", ">=1.0.0,"),
            ("1.0.0", "1.*.3"),
            ("not-a-version", "^1"),
            ("1.0", "^1"),
            ("1", "^1"),
            ("", "^1"),
            ("1.0.0", "=="),
            ("1.0.0", ""),
            ("1.0.0", "   "),
        ],
    ),
    (
        "whitespace and multi-comparator forms",
        &[
            ("1.1.0", ">=1.0.0 , <1.2.0"),
            ("1.1.0", ">=1.0.0,<1.2.0"),
            ("1.1.0", ">= 1.0.0, < 1.2.0"),
            ("1.2.4", "^1.2.3, ^1.2.4"),
            ("1.2.3", "^1.2.3, ^1.2.4"),
        ],
    ),
];

/// The three-valued match result, mirroring the TS port's `boolean | null`.
fn evaluate(version: &str, req: &str) -> Option<bool> {
    let v = Version::parse(version).ok()?;
    let r = VersionReq::parse(req).ok()?;
    Some(r.matches(&v))
}

/// JSON string escaping — the whole file is written by hand because this example must build with
/// `noeta-pm`'s **default** (empty) feature set, and `serde_json` is behind `registry-http`.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(
        "  \"_\": \"GENERATED by `cargo run -p noeta-pm --example semver_vectors` — do not edit. \
         Every `expected` is `semver::VersionReq::matches` itself; `affects` is what \
         `Advisory::affects` (and so `noeta audit`) returns, which collapses unknown to false.\",\n",
    );
    out.push_str("  \"cases\": [\n");
    let mut n = 0usize;
    let total: usize = CASES.iter().map(|(_, cases)| cases.len()).sum();
    for (rule, cases) in CASES {
        for (version, req) in *cases {
            let expected = evaluate(version, req);
            let affects = VersionReq::parse(req)
                .is_ok_and(|r| Version::parse(version).is_ok_and(|v| r.matches(&v)));
            n += 1;
            out.push_str(&format!(
                "    {{ \"rule\": {}, \"version\": {}, \"req\": {}, \"expected\": {}, \"affects\": {} }}{}\n",
                quote(rule),
                quote(version),
                quote(req),
                match expected {
                    Some(true) => "true",
                    Some(false) => "false",
                    None => "null",
                },
                affects,
                if n == total { "" } else { "," }
            ));
        }
    }
    out.push_str("  ]\n}\n");
    print!("{out}");
}
