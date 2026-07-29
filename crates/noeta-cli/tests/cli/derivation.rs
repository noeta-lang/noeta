//! Namespace derivation: a module's path comes from **where its file sits**, not from a
//! declaration.
//!
//! Each test here pins one thing that was broken before derivation and is fixed by construction
//! now — the three the arc ledger names (`plans/namespace-derivation.md`), plus the diagnostics
//! that replaced two silent failures.

use crate::support::*;

/// Lay out a package tree under a private fixture directory: `(relative path, contents)` pairs.
/// Returns the base directory.
fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let base = temp_root().join(format!("noeta_derive_{name}"));
    let _ = std::fs::remove_dir_all(&base);
    for (path, text) in files {
        let full = base.join(path);
        std::fs::create_dir_all(full.parent().expect("a parent")).expect("create dirs");
        std::fs::write(&full, text).expect("write fixture");
    }
    base
}

#[test]
fn a_package_keyed_under_a_non_conventional_name_resolves_under_that_key() {
    // The import key is the manifest's documented decoupling of "the name you write after `use`"
    // from the package's real identity — and before derivation it was inert for any package whose
    // namespace did not lead with its own package half. `para/cli` declares `namespace para.cli`,
    // leading with the *scope*, so re-rooting never fired: keying it `mycli` left `use mycli.cli.…`
    // as "no module `mycli.cli`" while only the package's internal spelling worked. Deriving the
    // path under the consumer's key makes the key real.
    let base = tree(
        "key",
        &[
            (
                "app/noeta.toml",
                "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
                 [dependencies]\nmycli = { path = \"../cli\" }\n",
            ),
            ("app/main.noe", "use mycli.cli.run;\necho run();\n"),
            (
                "cli/noeta.toml",
                "[package]\nname = \"para/cli\"\nversion = \"1.0.0\"\n",
            ),
            (
                "cli/cli.noe",
                "pub fn run(): string {\n    return \"ran\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(base.join("app"))
        .args(["run", "main.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran"));
}

#[test]
fn an_app_subdirectory_module_resolves() {
    // A *dependency* package has always been walked as a tree; the app's own scan was flat, so
    // `src/deep/nested.noe` was invisible to `src/main.noe` — an inconsistency, not a consequence.
    // Note also that `src/` is layout and not a path segment: the module is `dirscan.deep.nested`.
    let base = tree(
        "subdir",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/dirscan\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/main.noe",
                "use dirscan.deep.nested.helper;\necho helper();\n",
            ),
            (
                "src/deep/nested.noe",
                "pub fn helper(): string {\n    return \"nested\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "src/main.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nested"));
}

#[test]
fn two_files_deriving_one_path_name_both_files() {
    // The silent one: two files claiming one namespace dropped the second file's exports, and the
    // failure surfaced at the *importing* file as "module `twice.pieces` has no export `one`" —
    // sending the reader to inspect an import that was correct. Now both files are named, at the
    // second one.
    let base = tree(
        "collision",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/twice\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/main.noe",
                "use twice.pieces.{one, two};\necho one();\necho two();\n",
            ),
            (
                "pieces.noe",
                "pub fn one(): string {\n    return \"one\";\n}\n",
            ),
            (
                "src/pieces.noe",
                "pub fn two(): string {\n    return \"two\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "src/main.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0073"))
        .stderr(predicate::str::contains(
            "two files derive the module path `twice.pieces`",
        ))
        .stderr(predicate::str::contains("pieces.noe"))
        .stderr(predicate::str::contains("src/pieces.noe"));
}

#[test]
fn a_namespace_that_contradicts_the_files_location_is_refused() {
    // `namespace` is still accepted while it is being removed, but only as a restatement.
    let base = tree(
        "mismatch",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n",
            ),
            ("main.noe", "use app.helper.f;\necho f();\n"),
            (
                "helper.noe",
                "namespace totally.unrelated;\n\npub fn f(): string {\n    return \"f\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "main.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0072"))
        .stderr(predicate::str::contains(
            "declares `namespace totally.unrelated`, but its path derives as `app.helper`",
        ));
}

#[test]
fn a_namespace_that_restates_the_derived_path_is_accepted() {
    // The migration is a no-op on a package that already followed the convention: the declaration
    // says exactly what the location derives, so it is left alone (a later slice deletes it).
    let base = tree(
        "agrees",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n",
            ),
            ("main.noe", "use app.helper.f;\necho f();\n"),
            (
                "helper.noe",
                "namespace app.helper;\n\npub fn f(): string {\n    return \"ok\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "main.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn a_file_name_that_is_not_a_path_segment_is_refused_with_a_rename() {
    // No silent `-` → `_` mapping: that would give one module two spellings, which is the thing
    // derivation exists to remove.
    let base = tree(
        "illegal",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n",
            ),
            ("main.noe", "echo \"hi\"\n"),
            (
                "my-utils.noe",
                "pub fn helper(): string {\n    return \"h\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "main.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0074"))
        .stderr(predicate::str::contains(
            "`my-utils` cannot be part of a module path",
        ))
        .stderr(predicate::str::contains("rename it to `my_utils`"));
}

#[test]
fn check_sees_the_same_program_run_links() {
    // `noeta check` groups by *package*, not by directory — otherwise it would check a subdirectory
    // module against an empty sibling pool and disagree with `run` about whether the program links.
    let base = tree(
        "check_pool",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/dirscan\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/main.noe",
                "use dirscan.deep.nested.helper;\necho helper();\n",
            ),
            (
                "src/deep/nested.noe",
                "pub fn helper(): string {\n    return \"nested\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["check", "."])
        .assert()
        .success();
}

#[test]
fn a_lone_script_outside_a_package_keeps_its_declared_namespaces() {
    // No manifest → no package → no prefix, so nothing is derived and a module is identified by the
    // `namespace` it declares, exactly as before. A bare `noeta run` must also not recursively
    // swallow whatever tree it happens to stand in.
    let base = tree(
        "no_package",
        &[
            ("main.noe", "use App.Models.f;\necho f();\n"),
            (
                "models.noe",
                "namespace App.Models;\n\npub fn f(): string {\n    return \"loose\";\n}\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "main.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("loose"));
}
