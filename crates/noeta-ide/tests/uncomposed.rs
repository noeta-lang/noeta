//! The editor over a project whose dependency ships a **native extension this process has not
//! composed**.
//!
//! A package's native extension is statically linked Rust: the modules and types it registers exist
//! in a process only because that process *is* the composed toolchain. `noeta lsp` never composed,
//! so in any project with a `[trust] native` dependency the editor could not link the program at
//! all — every `use` of the package's namespace came back E0019 and a file `noeta check` compiles
//! cleanly showed as broken, in every file of the project at once.
//!
//! `noeta lsp` now delegates to the project's composed toolchain when one is already built. When it
//! is not — the case here, since no test process is composed — the editor must report the one real
//! cause instead of the cascade it causes. Nineteen confident wrong squiggles are worse than one
//! accurate sentence, and worse than nothing: they hide the actual state of the file.
//!
//! Its own test binary because the extension registry installs once per process.

use noeta_ide::DocumentStore;

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[]));
}

/// An app + a path dependency that declares a native entry crate and imports the namespace its Rust
/// half would register. Both the app's entry and the dependency's own modules import it, so an
/// uncomposed link produces one E0019 per file — the shape the real reproduction has at 19 files.
fn native_dep_project(name: &str) -> noeta_test_temp::TempPath {
    let root = noeta_test_temp::TempDir::new(name);
    let (app, dep) = (root.join("app"), root.join("imgfx"));
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(dep.join("native")).unwrap();
    std::fs::write(
        dep.join("noeta.toml"),
        "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
    )
    .unwrap();
    // Nothing builds the crate; resolution only validates that it is there.
    std::fs::write(
        dep.join("native").join("Cargo.toml"),
        "[package]\nname = \"imgfx-native\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    for module in ["fx", "util"] {
        std::fs::write(
            dep.join(format!("{module}.noe")),
            "use imgfx.raw\npub fn one(): int { return raw.one(); }\n",
        )
        .unwrap();
    }
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nimgfx = { path = \"../imgfx\" }\n\
         [trust]\nnative = [\"acme/imgfx\"]\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use imgfx.raw\necho raw.one();\n").unwrap();
    root.into_child("app/main.noe")
}

/// A pure-Noeta app + path dependency: the control. Nothing native, so nothing may be withheld.
fn pure_noeta_project(name: &str) -> noeta_test_temp::TempPath {
    let root = noeta_test_temp::TempDir::new(name);
    let (app, dep) = (root.join("app"), root.join("kit"));
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("noeta.toml"),
        "[package]\nname = \"acme/kit\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(dep.join("api.noe"), "pub fn one(): int { return 1; }\n").unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nkit = { path = \"../kit\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use kit.api\necho api.one();\n").unwrap();
    root.into_child("app/main.noe")
}

fn open(entry: &std::path::Path) -> (DocumentStore, String) {
    install();
    let uri = format!("file://{}", entry.display());
    let text = std::fs::read_to_string(entry).unwrap();
    let mut store = DocumentStore::default();
    store.open(&uri, text);
    (store, uri)
}

/// The editor reports **one** diagnostic naming the uncomposed package — not the cascade.
#[test]
fn an_uncomposed_native_dependency_is_reported_once_and_by_name() {
    let entry = native_dep_project("ide-uncomposed");
    let (store, uri) = open(&entry);
    let (diags, _) = store.diagnostics(&uri).expect("the document is open");
    assert_eq!(
        diags.len(),
        1,
        "the unresolved-import cascade is withheld: {diags:?}"
    );
    let message = &diags[0].message;
    assert!(
        message.contains("acme/imgfx"),
        "the missing package is named: {message}"
    );
    assert!(
        message.contains("noeta check"),
        "the command that fixes it is named: {message}"
    );
    // It has to be locatable: an editor drops a diagnostic it cannot place.
    assert_eq!(diags[0].span.source.0, 0, "reported on the open document");
}

/// The control: a project with no native dependency is untouched — a clean program stays clean, and
/// nothing is withheld from it.
#[test]
fn a_pure_noeta_project_is_unaffected() {
    let entry = pure_noeta_project("ide-composed-control");
    let (store, uri) = open(&entry);
    let (diags, _) = store.diagnostics(&uri).expect("the document is open");
    assert!(diags.is_empty(), "diagnostics: {diags:?}");
}
