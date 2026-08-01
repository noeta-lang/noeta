//! **One project, one question, three surfaces — and they must give the same answer.**
//!
//! `noeta check`, the editor and the MCP `check` tool each answer "is this project clean". They
//! used to answer it with three different walks and three different sweeps, and the audit's row 5
//! named the exact way they disagreed: a type error inside a `@test { … }` block was reported by
//! the command line and invisible to the other two, because a dev-tier block is stripped before the
//! checker ever sees it.
//!
//! So there is **one fixture** here and one test per surface over it. That is the whole design of
//! this file: if the surfaces drift apart again, the failure names which one went quiet, rather
//! than one suite going red while the others stay green about the same project.
//!
//! Each test drives its surface as close to the real thing as the crate boundary allows:
//!
//! | test | surface | driven through |
//! |---|---|---|
//! | [`noeta_check_reports_a_tier_body_error`] | `noeta check` | the built `noeta` binary |
//! | [`the_mcp_check_tool_reports_a_tier_body_error`] | MCP `check` | [`noeta_mcp::run_check`] |
//! | [`the_editors_project_pull_reports_a_tier_body_error`] | LSP `workspace/diagnostic` | [`noeta_ide::DocumentStore::project_check`] |
//! | [`the_editors_open_document_reports_a_tier_body_error`] | LSP push / `textDocument/diagnostic` | [`noeta_ide::DocumentStore::diagnostics`] |
//!
//! The last two are the editor's two entry sets — the project pull and the open document — and both
//! must report it, because they are the two ways a user can be told.
//!
//! The **reverse** is pinned too ([`no_surface_reports_a_dependencys_tier_body_error`]): a
//! dependency's `@test` bodies are not its consumer's to check, and a surface that swept them would
//! bury the user in errors about code they cannot edit.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// The project every test in this file asks about.
///
/// An app with a clean shipping shape and a **type error inside its own `@test` block** — the exact
/// fault the row is about, and one no surface can see without activating the tier. Its dependency
/// has the same fault in *its* `@test` block, which no surface may report (see the reverse test).
///
/// Returned as the app directory; the dependency sits beside it. `TempDir` is per-process, so
/// parallel test binaries and parallel checkouts never share a fixture path.
fn fixture(name: &str) -> (noeta_test_temp::TempDir, PathBuf) {
    let root = noeta_test_temp::TempDir::new(&format!("three-surfaces-{name}"));
    let (app, toolkit) = (root.join("app"), root.join("toolkit"));
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&toolkit).unwrap();
    std::fs::write(
        toolkit.join("noeta.toml"),
        "[package]\nname = \"acme/toolkit\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    // The dependency's own `@test` body is broken in exactly the same way as the app's. Nothing
    // may report it: a consumer is not responsible for its dependency's test bodies.
    std::fs::write(
        toolkit.join("api.noe"),
        "pub fn one(): int { return 1 }\n\
         \n\
         @test {\n    fn a_dependencys_test(): void { theirs: int = \"lots\" }\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ntoolkit = { path = \"../toolkit\" }\n",
    )
    .unwrap();
    // The shipping shape of this file is clean. Its `@test` body is not.
    std::fs::write(
        app.join("main.noe"),
        "use toolkit.api.one\n\
         \n\
         fn add(a: int, b: int): int { return a + b }\n\
         \n\
         echo add(one(), 2);\n\
         \n\
         @test {\n    fn adds(): void { mine: int = \"lots\" }\n}\n",
    )
    .unwrap();
    (root, app)
}

/// The one fault every surface must find: `mine: int = "lots"` inside the app's `@test` block.
const TIER_BODY_ERROR: &str = "E0007";

/// The dependency's fault, which no surface may report.
const DEPENDENCY_TEST_BINDING: &str = "theirs";

fn app_main(app: &Path) -> String {
    app.join("main.noe").display().to_string()
}

/// **Surface 1 — `noeta check`.** The command line has reported this since its own tier sweep
/// landed; it is here so the three tests sit together and fail together.
#[test]
fn noeta_check_reports_a_tier_body_error() {
    let (_root, app) = fixture("cli");
    let output = Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .arg("check")
        .arg(&app)
        .output()
        .expect("run `noeta check`");
    let rendered = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        rendered.contains(TIER_BODY_ERROR),
        "`noeta check` went quiet about the `@test` body: {rendered}"
    );
    assert!(
        !output.status.success(),
        "a `@test` body that does not compile is a failed check: {rendered}"
    );
    assert!(
        rendered.contains("tiers: test"),
        "the summary must name what it looked inside: {rendered}"
    );
}

/// **Surface 2 — the MCP `check` tool.** The generated `AGENTS.md` offers it *as* the equivalent of
/// `noeta check`, so an agent that earns a green here must not be able to fail the command line.
///
/// It is asked about the **directory**, which is the invocation it could not accept at all before
/// this arc: the tool took a single `.noe` file and checked whatever entry that file happened to be.
#[test]
fn the_mcp_check_tool_reports_a_tier_body_error() {
    noeta_stdlib::registry::default_seeded();
    let (_root, app) = fixture("mcp");
    let out = noeta_mcp::run_check(&noeta_mcp::CheckArgs {
        source: None,
        file: Some(app.display().to_string()),
    })
    .expect("the tool answers");
    assert!(
        !out.ok,
        "the agent surface went quiet about the `@test` body: {:?}",
        out.diagnostics
    );
    assert!(
        out.diagnostics.iter().any(|d| d.code == TIER_BODY_ERROR),
        "expected {TIER_BODY_ERROR} from inside the tier block, got {:?}",
        out.diagnostics
    );
    assert_eq!(
        out.tiers_checked,
        vec!["test".to_string()],
        "the response must name what it looked inside"
    );
}

/// **Surface 3 — the editor's project pull** (`workspace/diagnostic`).
///
/// This is the entry set the editor was missing entirely: push diagnostics cover the *open*
/// documents, so a fault in a file nobody has opened was reported by the command line and by
/// nothing the user could see.
#[test]
fn the_editors_project_pull_reports_a_tier_body_error() {
    noeta_stdlib::registry::default_seeded();
    let (_root, app) = fixture("lsp-project");
    // Nothing is open: this is the editor answering about files on disk.
    let store = noeta_ide::DocumentStore::default();
    let checked = store.project_check(&app);
    let codes: Vec<&str> = checked
        .diagnostics
        .iter()
        .map(|d| d.diagnostic.code.code())
        .collect();
    assert!(
        codes.contains(&TIER_BODY_ERROR),
        "the editor's project pull went quiet about the `@test` body: {codes:?}"
    );
    assert_eq!(
        checked.tiers_checked,
        vec!["test".to_string()],
        "the editor must sweep the same shapes the CLI does"
    );
}

/// **Surface 3b — the editor's open document** (push diagnostics, and the `textDocument/diagnostic`
/// pull that shares their engine).
///
/// The narrow per-keystroke path. It is allowed to differ from the project pull in *which entries*
/// it covers — one document, not a project — and in nothing else, so the fault under the cursor
/// must underline here exactly as it is reported everywhere else.
#[test]
fn the_editors_open_document_reports_a_tier_body_error() {
    noeta_stdlib::registry::default_seeded();
    let (_root, app) = fixture("lsp-document");
    let uri = format!("file://{}", app_main(&app));
    let mut store = noeta_ide::DocumentStore::default();
    store.open(&uri, std::fs::read_to_string(app.join("main.noe")).unwrap());
    let (diags, _text) = store.diagnostics(&uri).expect("the document is open");
    let codes: Vec<&str> = diags.iter().map(|d| d.code.code()).collect();
    assert!(
        codes.contains(&TIER_BODY_ERROR),
        "the editor went quiet about the `@test` body under the cursor: {codes:?}"
    );
}

/// **The reverse.** A dependency's `@test` bodies are not its consumer's to check.
///
/// `noeta_check::code_tiers_in` already enforces this — only blocks written in the **root** package
/// spawn a pass — and this pins it across all three surfaces at once, because a surface that lost
/// the rule would bury the user in errors about a package they cannot edit. The dependency's fault
/// is byte-identical to the app's; only its provenance differs.
#[test]
fn no_surface_reports_a_dependencys_tier_body_error() {
    noeta_stdlib::registry::default_seeded();
    let (_root, app) = fixture("dependency");

    let cli = Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .arg("check")
        .arg(&app)
        .output()
        .expect("run `noeta check`");
    let rendered = String::from_utf8_lossy(&cli.stderr).to_string();
    assert!(
        !rendered.contains(DEPENDENCY_TEST_BINDING),
        "`noeta check` reported a dependency's `@test` body: {rendered}"
    );

    let mcp = noeta_mcp::run_check(&noeta_mcp::CheckArgs {
        source: None,
        file: Some(app.display().to_string()),
    })
    .expect("the tool answers");
    assert!(
        !mcp.diagnostics
            .iter()
            .any(|d| d.message.contains(DEPENDENCY_TEST_BINDING)),
        "the MCP tool reported a dependency's `@test` body: {:?}",
        mcp.diagnostics
    );

    let store = noeta_ide::DocumentStore::default();
    let editor = store.project_check(&app);
    assert!(
        !editor
            .diagnostics
            .iter()
            .any(|d| d.diagnostic.message.contains(DEPENDENCY_TEST_BINDING)),
        "the editor reported a dependency's `@test` body: {:?}",
        editor
            .diagnostics
            .iter()
            .map(|d| &d.diagnostic.message)
            .collect::<Vec<_>>()
    );
}
