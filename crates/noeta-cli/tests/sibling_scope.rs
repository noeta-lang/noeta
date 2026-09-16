//! **A `use` binds in the file that writes it, and every surface must agree about that.**
//!
//! Two `.noe` files sit in a directory with no manifest. One writes `use std.io`; the other writes
//! `io.errln(…)` and imports nothing. The second file is wrong, and `noeta run` says so.
//!
//! `noeta check` said it was clean. So did the MCP `check` tool and both of the editor's paths,
//! because all four reach the same salsa link, and that link handed every cleanly-parsing member of
//! the directory to the linker as an **import driver**. A namespace-less file has no α-rename table,
//! so its retained `use` kept the bare name `io` and landed in the merged program's flat top-level
//! scope — the entry's own scope. The batch loader escaped it only by never parsing such a file
//! (`sibling_is_inert`), which made the rule hold on one front end and not the other.
//!
//! The fixture here is deliberately the smallest thing that shows it: a lone script beside another
//! lone script, which is how most people meet the language before they write a `noeta.toml`.
//!
//! | test | surface | driven through |
//! |---|---|---|
//! | [`noeta_run_rejects_the_unimported_name`] | `noeta run` (the oracle) | the built `noeta` binary |
//! | [`noeta_check_does_not_inherit_a_siblings_import`] | `noeta check` | the built `noeta` binary |
//! | [`the_mcp_check_tool_does_not_inherit_a_siblings_import`] | MCP `check` | [`noeta_mcp::run_check`] |
//! | [`the_editors_project_pull_does_not_inherit_a_siblings_import`] | LSP `workspace/diagnostic` | [`noeta_ide::DocumentStore::project_check`] |
//! | [`the_editors_open_document_does_not_inherit_a_siblings_import`] | LSP push / `textDocument/diagnostic` | [`noeta_ide::DocumentStore::diagnostics`] |
//!
//! Agreement is a claim in **both** directions, so the other two tests pin the other one:
//! [`no_surface_reports_a_siblings_unresolved_import`] is the same fault mirrored (a broken `use` in
//! the sibling was reported against an entry that never wrote it), and
//! [`a_sibling_that_declares_a_namespace_stays_importable`] is the control that the repair did not
//! simply cut every file off from its neighbours.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// The name the victim file reaches for and never imports.
const UNIMPORTED_NAME: &str = "io";

/// What a file naming something nothing brought into scope must be told.
const UNRESOLVED_NAME: &str = "E0005";

/// What an import naming a module that does not exist must be told.
const UNKNOWN_MODULE: &str = "E0019";

/// **A directory with no manifest and two unrelated scripts in it.**
///
/// `victim.noe` writes `io.errln(…)` with no `use` of its own. `sibling.noe` writes `use std.io` and
/// is otherwise nothing to do with it. Neither file declares a `namespace`, so neither is a module
/// of the other under any surface — which is the whole point: `sibling.noe` must contribute nothing
/// at all, not even a binding.
///
/// Returned as the temp root (kept alive by the caller) and the victim's path.
fn leak_fixture(name: &str) -> (noeta_test_temp::TempDir, PathBuf) {
    let root = noeta_test_temp::TempDir::new(&format!("sibling-scope-{name}"));
    let dir = root.join("scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("sibling.noe"),
        "use std.io\nio.errln(\"sibling\")\n",
    )
    .unwrap();
    let victim = dir.join("victim.noe");
    std::fs::write(&victim, "io.errln(\"victim\")\n").unwrap();
    (root, victim)
}

/// `noeta <verb> <path>`, with the cache pinned inside the test target directory so parallel
/// checkouts never share one.
fn noeta(verb: &str, path: &Path) -> std::process::Output {
    Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .arg(verb)
        .arg(path)
        .output()
        .unwrap_or_else(|err| panic!("run `noeta {verb}`: {err}"))
}

/// Every diagnostic code one of the salsa surfaces produced, as rendered strings.
fn codes(checked: &noeta_project::ProjectCheck) -> Vec<String> {
    checked
        .diagnostics
        .iter()
        .map(|d| d.diagnostic.code.code().to_string())
        .collect()
}

/// **The oracle, and the non-vacuity guard for everything below.**
///
/// If `noeta run` ever accepts this file, every other test in this file is asserting that two
/// surfaces agree about nothing.
#[test]
fn noeta_run_rejects_the_unimported_name() {
    let (_root, victim) = leak_fixture("run");
    let output = noeta("run", &victim);
    let rendered = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        rendered.contains(UNRESOLVED_NAME) && rendered.contains(UNIMPORTED_NAME),
        "`noeta run` must reject a file naming `{UNIMPORTED_NAME}` with no import of it, or the \
         agreement tests below compare two surfaces about a program that is fine: {rendered}"
    );
    assert!(
        !output.status.success(),
        "a program that does not compile is a failed run: {rendered}"
    );
}

/// **Surface 1 — `noeta check`.** The reported symptom: exit 0 and "0 error(s)" on a file `noeta
/// run` refuses, because an unrelated file in the same directory happened to import `std.io`.
#[test]
fn noeta_check_does_not_inherit_a_siblings_import() {
    let (_root, victim) = leak_fixture("cli");
    let output = noeta("check", &victim);
    let rendered = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        rendered.contains(UNRESOLVED_NAME),
        "`noeta check` called a file clean that `noeta run` refuses — a sibling's `use std.io` \
         bound `{UNIMPORTED_NAME}` in a file that imports nothing: {rendered}"
    );
    assert!(
        !output.status.success(),
        "a file that does not compile is a failed check: {rendered}"
    );
}

/// **Surface 2 — the MCP `check` tool.** The one that matters most for an agent: `ok: true` is the
/// signal a coding agent acts on, and it was true for a file that will not build.
#[test]
fn the_mcp_check_tool_does_not_inherit_a_siblings_import() {
    noeta_stdlib::registry::default_seeded();
    let (_root, victim) = leak_fixture("mcp");
    let out = noeta_mcp::run_check(&noeta_mcp::CheckArgs {
        source: None,
        file: Some(victim.display().to_string()),
    })
    .expect("the tool answers");
    assert!(
        !out.ok,
        "the agent surface reported `ok` for a file `noeta run` refuses: {:?}",
        out.diagnostics
    );
    assert!(
        out.diagnostics.iter().any(|d| d.code == UNRESOLVED_NAME),
        "expected {UNRESOLVED_NAME} for the unimported name, got {:?}",
        out.diagnostics
    );
}

/// **Surface 3 — the editor's project pull** (`workspace/diagnostic`).
#[test]
fn the_editors_project_pull_does_not_inherit_a_siblings_import() {
    noeta_stdlib::registry::default_seeded();
    let (_root, victim) = leak_fixture("lsp-project");
    let store = noeta_ide::DocumentStore::default();
    let checked = store.project_check(&victim);
    assert!(
        codes(&checked).contains(&UNRESOLVED_NAME.to_string()),
        "the editor's project pull called a file clean that `noeta run` refuses: {:?}",
        codes(&checked)
    );
}

/// **Surface 3b — the editor's open document** (push diagnostics, and the
/// `textDocument/diagnostic` pull sharing their engine).
///
/// The per-keystroke path builds its workspace from the document's own directory, so it saw the
/// sibling too: the squiggle under `io.errln` simply was not drawn.
#[test]
fn the_editors_open_document_does_not_inherit_a_siblings_import() {
    noeta_stdlib::registry::default_seeded();
    let (_root, victim) = leak_fixture("lsp-document");
    let uri = format!("file://{}", victim.display());
    let mut store = noeta_ide::DocumentStore::default();
    store.open(&uri, std::fs::read_to_string(&victim).unwrap());
    let (diags, _text) = store.diagnostics(&uri).expect("the document is open");
    let found: Vec<&str> = diags.iter().map(|d| d.code.code()).collect();
    assert!(
        found.contains(&UNRESOLVED_NAME),
        "the editor drew no squiggle under a name nothing imported: {found:?}"
    );
}

/// **The same fault mirrored: `check` must not reject what `run` accepts.**
///
/// The sibling's `use zzz.nope` names no module. Driving it from the entry's link reported E0019
/// while checking a file that never wrote the import — an error `noeta run victim.noe` does not
/// produce, attributed to a file the user did not ask about. Agreement is a claim in both
/// directions, and this is the one a repair aimed only at the quiet direction would break.
#[test]
fn no_surface_reports_a_siblings_unresolved_import() {
    noeta_stdlib::registry::default_seeded();
    let root = noeta_test_temp::TempDir::new("sibling-scope-mirror");
    let dir = root.join("scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sibling.noe"), "use zzz.nope\n").unwrap();
    let victim = dir.join("victim.noe");
    std::fs::write(&victim, "echo 1;\n").unwrap();

    let run = noeta("run", &victim);
    assert!(
        run.status.success(),
        "the oracle must accept this program, or the assertions below prove nothing: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let check = noeta("check", &victim);
    let rendered = String::from_utf8_lossy(&check.stderr).to_string();
    assert!(
        !rendered.contains(UNKNOWN_MODULE),
        "`noeta check` reported a sibling's broken import against an entry that never wrote it, \
         on a program `noeta run` accepts: {rendered}"
    );
    assert!(
        check.status.success(),
        "`noeta check` failed a program `noeta run` accepts: {rendered}"
    );

    let mcp = noeta_mcp::run_check(&noeta_mcp::CheckArgs {
        source: None,
        file: Some(victim.display().to_string()),
    })
    .expect("the tool answers");
    assert!(
        mcp.ok,
        "the agent surface failed a program `noeta run` accepts: {:?}",
        mcp.diagnostics
    );

    let store = noeta_ide::DocumentStore::default();
    let editor = store.project_check(&victim);
    assert!(
        codes(&editor).is_empty(),
        "the editor reported a sibling's broken import against this entry: {:?}",
        codes(&editor)
    );
    drop(root);
}

/// **The control: a neighbour that really is a module stays one.**
///
/// A file in a manifest-less directory becomes a module by declaring a `namespace`, and then a
/// sibling may `use` it. That is the case the repair must leave alone — cutting every file off from
/// its neighbours would end the disagreement by deleting the feature, and it would do it silently,
/// because the surface that went quiet is the one nobody reads.
#[test]
fn a_sibling_that_declares_a_namespace_stays_importable() {
    noeta_stdlib::registry::default_seeded();
    let root = noeta_test_temp::TempDir::new("sibling-scope-control");
    let dir = root.join("scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("helper.noe"),
        "namespace Helper\npub fn twice(n: int): int { return n * 2 }\n",
    )
    .unwrap();
    let main = dir.join("main.noe");
    std::fs::write(&main, "use Helper.twice\necho twice(21);\n").unwrap();

    let run = noeta("run", &main);
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    assert!(
        run.status.success() && stdout.contains("42"),
        "a namespaced neighbour is a module and `use Helper.twice` must resolve to it: stdout \
         {stdout:?}, stderr {:?}",
        String::from_utf8_lossy(&run.stderr)
    );

    let check = noeta("check", &main);
    assert!(
        check.status.success(),
        "`noeta check` refused a program `noeta run` executes: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let store = noeta_ide::DocumentStore::default();
    let editor = store.project_check(&main);
    assert!(
        codes(&editor).is_empty(),
        "the editor refused a program `noeta run` executes: {:?}",
        codes(&editor)
    );
    drop(root);
}
