//! `noeta grammar tree-sitter` — the per-project tree-sitter overlay generator.

use super::support::*;

/// With an output directory, the verb writes `project-tiers.json` (the widened token list) and
/// `queries/injections.scm` (a language rule per tier) from the project's `@tier(…, text: …)`
/// declarations, always including the std `doc` tier.
#[test]
fn writes_overlay_from_declared_tiers() {
    let proj = temp_dir(
        "grammar_overlay_project",
        &[
            (
                "src/main.noe",
                "@tier(spec, text: \"xml\")\nfn run(): void {}\n@spec {\n  <case name=\"x\"/>\n}\n",
            ),
            (
                "src/db.noe",
                "@tier(query, text: \"sql\")\nfn q(): void {}\n",
            ),
        ],
    );
    let out = proj.join("grammar");

    lang()
        .args(["grammar", "tree-sitter"])
        .arg(&proj)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let tiers = std::fs::read_to_string(out.join("project-tiers.json")).unwrap();
    // The std default plus both declared tiers, sorted and deduplicated.
    for name in ["\"doc\"", "\"query\"", "\"spec\""] {
        assert!(
            tiers.contains(name),
            "project-tiers.json missing {name}:\n{tiers}"
        );
    }

    let inj = std::fs::read_to_string(out.join("queries/injections.scm")).unwrap();
    assert!(inj.contains("(#eq? @_tier \"doc\")"));
    assert!(inj.contains("(#set! injection.language \"markdown\")"));
    assert!(inj.contains("(#eq? @_tier \"spec\")"));
    assert!(inj.contains("(#set! injection.language \"xml\")"));
    assert!(inj.contains("(#eq? @_tier \"query\")"));
    assert!(inj.contains("(#set! injection.language \"sql\")"));
}

/// Without an output directory the token list is printed to stdout — a project that declares no
/// custom tier still emits the `doc` default (so the overlay is never empty).
#[test]
fn prints_token_list_to_stdout_with_doc_default() {
    let proj = temp_dir(
        "grammar_stdout_project",
        &[("src/main.noe", "fn main(): void {}\n")],
    );

    lang()
        .args(["grammar", "tree-sitter"])
        .arg(&proj)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"textTiers\""))
        .stdout(predicate::str::contains("\"doc\""));
}

/// The output is byte-for-byte identical across runs (deterministic generation).
#[test]
fn generation_is_deterministic() {
    let proj = temp_dir(
        "grammar_determinism_project",
        &[(
            "src/main.noe",
            "@tier(spec, text: \"xml\")\n@tier(query, text: \"sql\")\nfn run(): void {}\n",
        )],
    );

    let run = || {
        let out = lang()
            .args(["grammar", "tree-sitter"])
            .arg(&proj)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out).unwrap()
    };
    assert_eq!(run(), run());
}
