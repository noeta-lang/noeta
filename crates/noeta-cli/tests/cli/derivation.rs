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
fn a_namespace_declaration_is_refused_whatever_it_says() {
    // `namespace` is retired. A path is derived from where the file sits, so a declaration can only
    // restate the derivation or contradict it — this one contradicts, and the next one restates.
    // Both are refused, which is the whole point: neither earns a line of source.
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
                "namespace totally.unrelated\n\npub fn f(): string {\n    return \"f\";\n}\n",
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
            "a module's path is derived from where its file sits, so it cannot be declared",
        ))
        // The help names the path this file actually has, so the fix is mechanical. The message
        // deliberately does not echo what was declared — the rendered snippet shows the line, and
        // for a *dependency* the node has already been re-rooted out of the author's spelling.
        .stderr(predicate::str::contains("derives as `app.helper`"));
}

#[test]
fn a_namespace_that_restates_the_derived_path_is_refused_too() {
    // The contract this reverses: while derivation was landing, a declaration that agreed was left
    // alone so a package kept compiling while its declarations were deleted file by file. That
    // window is closed — an agreeing declaration is still a second place to keep in sync, and the
    // only reason to keep the syntax was to be lenient during the migration.
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
                "namespace app.helper\n\npub fn f(): string {\n    return \"ok\";\n}\n",
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
            "a module's path is derived from where its file sits, so it cannot be declared",
        ))
        // Refused even though it agrees, and the help still names the derivation — an author who
        // wrote the right thing gets the same one-line fix as one who wrote the wrong thing.
        .stderr(predicate::str::contains("derives as `app.helper`"));
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
fn a_program_in_a_data_directory_is_run_without_deriving_a_module_path() {
    // A migration/seed is a program the driver runs directly as its *entry*, named for when it runs
    // (`20260719000002_…`) — a stem no `use` could spell. The package walk already prunes `migrations/`
    // and `seeds/`, but running such a file as an entry went through `entry_module_path`, which derived
    // a path from that timestamped stem and reported E0074 — the very failure the data-directory concept
    // exists to prevent. A file inside a declared data directory has no module path (it is Declared),
    // so it runs.
    //
    // It also links with *no* package siblings. `main.noe` here carries executable top-level statements
    // naming its own top-level binding — valid as the program root, but a module that only links as one.
    // Pulling it in as a sibling of the migration reported `whom` unresolved (E0005) against code the
    // migration never wrote; a standalone data-directory program excludes it.
    let base = tree(
        "data_dir_entry",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n\
                 [db]\nmigrations = \"migrations\"\nseeds = \"seeds\"\n",
            ),
            ("main.noe", "whom = \"app\"\necho \"hello ${whom}\"\n"),
            (
                "migrations/20260719000002_create_todos.noe",
                "echo \"ran the migration program\"\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["run", "migrations/20260719000002_create_todos.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran the migration program"))
        .stdout(predicate::str::contains("hello app").not())
        .stderr(predicate::str::contains("E0074").not())
        .stderr(predicate::str::contains("E0072").not())
        .stderr(predicate::str::contains("E0005").not());
}

#[test]
fn a_lone_script_outside_a_package_keeps_its_declared_namespaces() {
    // The one place `namespace` survives, and it survives because it has to: no manifest → no
    // package → no prefix, so there is nothing to derive a path *from*, and a declaration is the
    // only way a loose sibling can be addressed at all. Retirement is therefore scoped to files
    // whose path *can* be derived — inside a package, where a declaration could only restate it.
    // (A bare `noeta run` must also not recursively swallow whatever tree it stands in.)
    let base = tree(
        "no_package",
        &[
            ("main.noe", "use App.Models.f;\necho f();\n"),
            (
                "models.noe",
                "namespace App.Models\n\npub fn f(): string {\n    return \"loose\";\n}\n",
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
