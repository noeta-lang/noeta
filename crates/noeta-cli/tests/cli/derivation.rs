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
fn check_derives_no_module_path_for_a_data_directory_program() {
    // The other half of the test above, and the half that stayed broken: `noeta run` on a migration
    // was fixed by the loader's entry path, but `noeta check .` derives through the *editor's* copy
    // of the derivation (shared with the LSP and the MCP `check` tool), which inlined the two lines
    // and so never learned the data-directory rule. Every project wired for `noeta migrate` — the
    // documented, tool-generated `20260727150655_create_todos.noe` naming — therefore failed
    // `noeta check` on its own project root with E0074, telling the author to rename a file the
    // toolchain had named for them.
    // The **dependency is load-bearing in this fixture**, and it is what kept the defect out of
    // sight: a migration is checked as a one-member workspace (the package walk prunes it, so it is
    // no pool member), and the salsa linker only *applies* derived paths when some member or
    // dependency module actually derives one — an illegal path alone does not count. With no
    // dependency the lone migration therefore never reached `apply_derived_paths` and the wrong path
    // it had been given stayed silent; a single `dep = { path = … }` — which every project wired for
    // `noeta migrate` has, `para/db` being one — makes it speak. That is exactly `para-db`'s own
    // example app and the `test-todo` sample.
    let base = tree(
        "data_dir_check",
        &[
            (
                "dep/noeta.toml",
                "[package]\nname = \"local/dep\"\nversion = \"0.1.0\"\n",
            ),
            (
                "dep/dep.noe",
                "pub fn helper(): string {\n    return \"h\";\n}\n",
            ),
            (
                "app/noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n\
                 [dependencies]\ndep = { path = \"../dep\" }\n\
                 [db]\nmigrations = \"migrations\"\nseeds = \"seeds\"\n",
            ),
            ("app/src/main.noe", "use dep.helper;\necho helper();\n"),
            (
                "app/migrations/20260727150655_create_todos.noe",
                "use dep.helper;\necho helper();\n",
            ),
            (
                "app/seeds/20260727150656_sample_todos.noe",
                "use dep.helper;\necho helper();\n",
            ),
        ],
    );

    lang()
        .current_dir(base.join("app"))
        .args(["check", "."])
        .assert()
        .success()
        .stderr(predicate::str::contains("E0074").not())
        // Not skipped — a migration is a program, and a program that does not compile has to be
        // reported. All three files are checked; only the module *path* is not derived.
        .stderr(predicate::str::contains("checked 3 files"));
}

#[test]
fn check_still_refuses_an_illegal_module_name_outside_a_data_directory() {
    // The guard on the fix above: the exception is the *data directory*, not the timestamped shape
    // of the name. The same file one directory over is an ordinary module, and no `use` can spell
    // it — so `check` must still say so, or the fix would have bought a green project check by
    // going blind.
    let base = tree(
        "data_dir_check_guard",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n\
                 [db]\nmigrations = \"migrations\"\nseeds = \"seeds\"\n",
            ),
            ("src/main.noe", "echo \"app\"\n"),
            (
                "src/20260727150655_create_todos.noe",
                "echo \"not a migration\"\n",
            ),
        ],
    );

    lang()
        .current_dir(&base)
        .args(["check", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0074"))
        .stderr(predicate::str::contains(
            "rename it to `_20260727150655_create_todos`",
        ));
}

#[test]
fn check_refuses_an_illegal_module_name_with_no_legally_derived_sibling() {
    // The guard above passes for a reason that is not the one it looks like. Its `src/main.noe`
    // derives a legal path, and the salsa linker only ran the derivation pass at all when *some*
    // member or dependency module derived one — an illegal path is not a derived path. Take the
    // legal sibling away and the same package went silent: `check` said "0 error(s)" and exited 0
    // over a file `run` refuses with E0074, which is the check-vs-run divergence in its purest
    // form. Any dependency hides it too (every dep module derives), which is why every project
    // that ever hit this had one.
    //
    // So the fixture is deliberately the smallest workspace that can hold the bug: one package,
    // no dependencies, one module, and that module's name unspellable.
    let base = tree(
        "illegal_only_member",
        &[
            (
                "noeta.toml",
                "[package]\nname = \"local/app\"\nversion = \"0.1.0\"\n",
            ),
            ("src/my-utils.noe", "echo \"hi\";\n"),
        ],
    );

    // `check` must report it…
    lang()
        .current_dir(&base)
        .args(["check", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0074"))
        .stderr(predicate::str::contains("rename it to `my_utils`"));

    // …and must agree with `run` on the same file, which is the property under test.
    lang()
        .current_dir(&base)
        .args(["run", "src/my-utils.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0074"));
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

/// **A `.noe` file in a subtree the package walk prunes is still a file of the package**, and both
/// surfaces must treat it as one.
///
/// The walk prunes a directory holding a `Cargo.toml` (a native package keeps its engine crates in
/// its tree, whose `tests/`/`examples/` hold `.noe` *inputs*, not modules). The **entry** is never
/// asked that question — `read_siblings` prunes only the directories it descends into — so
/// `noeta run app/tools/probe.noe` gives `probe.noe` every module of `app`. `noeta check` looked the
/// entry up among the walked members, missed it, and checked it as a lone file: E0019 for an import
/// `run` resolves.
#[test]
fn a_pruned_entry_sees_its_package_on_both_surfaces() {
    let base = tree(
        "pruned_entry",
        &[
            (
                "app/noeta.toml",
                "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
            ),
            (
                "app/lib.noe",
                "pub fn helper(): string { return \"from the package\" }\n",
            ),
            (
                "app/tools/Cargo.toml",
                "[package]\nname = \"probe-engine\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("app/tools/probe.noe", "use app.lib.helper\necho helper()\n"),
        ],
    );
    let app = base.join("app");

    lang()
        .current_dir(&app)
        .args(["run", "tools/probe.noe"])
        .assert()
        .success()
        .stdout("from the package\n");

    lang()
        .current_dir(&app)
        .args(["check", "tools/probe.noe"])
        .assert()
        .success()
        .stderr(predicate::str::contains("E0019").not());
}

/// **The same fault in the direction that is not an error message but a missing one.**
///
/// `src/tools/probe.noe` and `tools/probe.noe` both derive `app.tools.probe`, across the prune.
/// `noeta run` links the pruned entry beside the walked one and refuses the program (E0073).
/// `noeta check .` linked the pruned entry ALONE — nothing to collide against — and reported "0
/// error(s)" with exit 0 for a tree `run` will not build. A checker that exits 0 where the compiler
/// exits 1 is the one failure shape no other test in this suite catches.
#[test]
fn a_derived_path_collision_across_a_prune_fails_check_as_it_fails_run() {
    let base = tree(
        "pruned_collision",
        &[
            (
                "app/noeta.toml",
                "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
            ),
            // `src/` is a layout convention, not a segment: this is `app.tools.probe`.
            (
                "app/src/tools/probe.noe",
                "pub fn walked(): int { return 1 }\n",
            ),
            (
                "app/tools/Cargo.toml",
                "[package]\nname = \"probe-engine\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            // …and so is this, from the other side of the prune.
            ("app/tools/probe.noe", "pub fn shadow(): int { return 2 }\n"),
        ],
    );
    let app = base.join("app");

    lang()
        .current_dir(&app)
        .args(["run", "tools/probe.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0073"));

    lang()
        .current_dir(&app)
        .args(["check", "."])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0073"))
        .stderr(predicate::str::contains(
            "two files derive the module path `app.tools.probe`",
        ));
}
