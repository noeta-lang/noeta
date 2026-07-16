//! `noeta doc` (object-model slice 6f): the `@doc` text-tier extractor and docs publishing.

use crate::support::*;

// --- `doc` (object-model slice 6f: the `@doc` text-tier extractor) ------------------

#[test]
fn doc_extracts_verbatim_blocks() {
    // `lang doc` pulls each `@doc { … }` block's verbatim body to stdout, dedented and with a
    // source-location header. The prose contains markdown punctuation that is not valid code; it is
    // captured untouched. The program's own code does not run (no `echo` output).
    let file = temp_program(
        "doc_ok",
        "@doc {\n\
        \x20   # Title\n\
        \x20   A *bold* claim about `add`.\n\
        }\n\
        fn add(a: int, b: int): int { return a + b }\n\
        echo \"must not run\"\n",
    );
    lang().arg("doc").arg(&file).assert().success().stdout(
        predicate::str::contains("# Title")
            .and(predicate::str::contains("A *bold* claim about `add`."))
            .and(predicate::str::contains("<!-- "))
            .and(predicate::str::contains("must not run").not()),
    );
}

#[test]
fn doc_attaches_to_the_following_declaration() {
    // Adjacency: a `@doc` block immediately above a declaration documents it — `noeta doc`'s
    // header carries the symbol — while a non-attached block (here, file-leading above `use`) is
    // the module doc with the bare header. With the `doc` tier live, the attached block is
    // stamped as `#[Doc]`, so `attributes_of::<Doc>()` surfaces it at runtime; on a default run
    // nothing is stamped (production carries no doc text).
    let file = temp_program(
        "doc_attach",
        "@doc { The module. }\n\
         use std.math.sqrt\n\
         @doc { Adds two ints. }\n\
         fn add(a: int, b: int): int { return a + b }\n\
         for d in attributes_of::<Doc>() { echo d.target; echo d.value.text }\n\
         echo \"end\"\n",
    );
    lang().arg("doc").arg(&file).assert().success().stdout(
        predicate::str::contains("· add -->")
            .and(predicate::str::contains("Adds two ints."))
            .and(predicate::str::contains("The module.")),
    );
    // Default run: the doc tier is stripped — no runtime docstrings.
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("add").not());
    // `--tier doc`: the attached block is a runtime docstring.
    lang()
        .arg("run")
        .arg(&file)
        .arg("--tier")
        .arg("doc")
        .assert()
        .success()
        .stdout(predicate::str::contains("add").and(predicate::str::contains("Adds two ints.")));
}

#[test]
fn doc_out_generates_the_registry_artifact() {
    // `noeta doc --out DIR` generates the package documentation artifact: a schema-versioned
    // `docs.json` keyed by the `[package]` identity (the canonical, registry-indexable form)
    // plus a Markdown tree — public API only for a namespaced package module, signatures carrying
    // the `@tier`/`@attribute` directives, prose woven by adjacency.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("docgen_pkg");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("noeta.toml"),
        "[package]\nname = \"acme/mathy\"\nversion = \"2.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        base.join("lib.noe"),
        "@doc { Mathy helpers. }\n\
         namespace mathy.lib;\n\
         @doc { Adds two ints. }\n\
         pub fn add(a: int, b: int): int { return a + b }\n\
         fn hidden(): int { return 0 }\n",
    )
    .unwrap();
    let out = base.join("docs");
    lang()
        .arg("doc")
        .arg(base.join("lib.noe"))
        .arg("--out")
        .arg(&out)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "documented 1 module (1 declaration)",
        ));
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("docs.json")).unwrap())
            .expect("valid docs.json");
    assert_eq!(json["schema"], 1);
    assert_eq!(json["package"]["name"], "acme/mathy");
    assert_eq!(json["package"]["version"], "2.1.0");
    assert_eq!(json["modules"][0]["namespace"], "mathy.lib");
    assert_eq!(json["modules"][0]["doc"], "Mathy helpers.");
    let items = json["modules"][0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "private decls are excluded: {items:?}");
    assert_eq!(items[0]["name"], "add");
    assert_eq!(items[0]["signature"], "pub fn add(a: int, b: int): int");
    assert_eq!(items[0]["doc"], "Adds two ints.");
    // The Markdown rendering exists and carries the same content.
    let page = std::fs::read_to_string(out.join("lib.md")).unwrap();
    assert!(page.contains("pub fn add(a: int, b: int): int"), "{page}");
    assert!(
        std::fs::read_to_string(out.join("index.md"))
            .unwrap()
            .contains("acme/mathy `2.1.0`")
    );
}

#[test]
fn published_docs_round_trip_through_the_registry() {
    // The docs-ingestion loop: `noeta publish` generates the package's docs.json and stores it
    // with the release (advisory — a docs failure never blocks a publish); `noeta doc --package`
    // fetches it back (highest version, or pinned with `@`), and `--out` renders the Markdown
    // tree from the stored artifact alone. Runs against the file-backed LocalIndex via
    // NOETA_REGISTRY_DIR, publishing unsigned (no key, no ambient identity).
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("docs_registry");
    let _ = std::fs::remove_dir_all(&base);
    let pkg = base.join("pkg");
    let reg = base.join("registry");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("noeta.toml"),
        "[package]\nname = \"acme/greeter\"\nversion = \"0.3.0\"\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("lib.noe"),
        "@doc { Friendly greetings. }\nnamespace greeter.lib;\n\
         @doc { Greets `who` by name. }\n\
         pub fn greet(who: string): string { return \"hello \" + who }\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&pkg)
                .output()
                .expect("git runs")
                .status
                .success(),
            "git {args:?}"
        );
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "init",
    ]);
    git(&["tag", "v0.3.0"]);

    let url = format!("file://{}", pkg.display());
    lang()
        .current_dir(&pkg)
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("publish")
        .arg("--git")
        .arg(&url)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "docs uploaded (1 module, 1 declaration)",
        ));
    // Fetch: the stored artifact comes back as JSON…
    let out = lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("doc")
        .arg("--package")
        .arg("acme/greeter")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("stored docs are valid JSON");
    assert_eq!(json["package"]["name"], "acme/greeter");
    assert_eq!(json["modules"][0]["items"][0]["name"], "greet");
    // …and renders to Markdown from the artifact alone (version-pinned form).
    let rendered = base.join("rendered");
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("doc")
        .arg("--package")
        .arg("acme/greeter@0.3.0")
        .arg("--out")
        .arg(&rendered)
        .assert()
        .success();
    let page = std::fs::read_to_string(rendered.join("lib.md")).unwrap();
    assert!(page.contains("pub fn greet(who: string): string"), "{page}");
    // An unknown version has no docs — a clear error.
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("doc")
        .arg("--package")
        .arg("acme/greeter@9.9.9")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no docs stored"));
}

#[test]
fn para_html_publishes_and_resolves_from_the_registry() {
    // The para-namespace arc's registry round-trip: the first-party `para-html` (pure Noeta source)
    // package publishes to a `LocalIndex`, and a **fresh consumer resolves it FROM the index** — a
    // registry dep (`{ version, package }`), not a path — locking the git source + sha + content
    // hash, so `use para.html.*` resolves from the registry-fetched package. The native `para-p2p`
    // twin's registry-from-index distribution awaits the physical split (its entry crate path-deps
    // workspace crates absent from a standalone clone); its `[trust]` gate is covered elsewhere.
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/para-html");
    if !src.join("noeta.toml").is_file() {
        return; // the package isn't present in this checkout — nothing to exercise.
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("para_html_registry");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let repo = base.join("para-html-repo");
    let reg = base.join("registry");
    let cache = base.join("cache");
    let cons = base.join("consumer");
    std::fs::create_dir_all(&cons).unwrap();

    // 1. A standalone git repo of the package (its `noeta.toml` is already at its root).
    assert!(
        std::process::Command::new("cp")
            .args(["-r".as_ref(), src.as_os_str(), repo.as_os_str()])
            .status()
            .expect("cp runs")
            .success(),
        "copy the package into a standalone repo"
    );
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git runs")
                .status
                .success(),
            "git {args:?}"
        );
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "v0.1.0",
    ]);
    git(&["tag", "v0.1.0"]);

    // 2. Publish to the LocalIndex.
    let url = format!("file://{}", repo.display());
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .args(["publish", "--git", &url, "--tag", "v0.1.0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("published `para/html` 0.1.0"));

    // 3. A fresh consumer depends on it via the REGISTRY (version + package), not a path.
    std::fs::write(
        cons.join("noeta.toml"),
        "[package]\nname = \"noeta/reg_consumer\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\npara = { version = \"^0.1\", package = \"para/html\" }\n",
    )
    .unwrap();
    std::fs::write(
        cons.join("main.noe"),
        "use para.html.{render, Html, handle}\n\
         use std.reactive.signal\n\
         use std.http.{Request, Response}\n\
         c = signal(1)\n\
         fn page(): Html { return @html { <h1>${c.get()}</h1> } }\n\
         fn fetch(req: Request): Response { return handle(req, \"t\", page, fn(n: string) {}) }\n",
    )
    .unwrap();

    // 4. Resolve from the index → the lockfile pins the git source + sha + content hash.
    lang()
        .current_dir(&cons)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .arg("update")
        .assert()
        .success();
    let lock = std::fs::read_to_string(cons.join("noeta.lock")).unwrap();
    for needle in [
        "name = \"para/html\"",
        "source = \"git\"",
        "tag = \"v0.1.0\"",
        "sha = ",
        "hash = ",
    ] {
        assert!(lock.contains(needle), "lock missing `{needle}`:\n{lock}");
    }

    // 5. `use para.html.*` resolves and type-checks against the registry-fetched package.
    lang()
        .current_dir(&cons)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .args(["check", "main.noe"])
        .assert()
        .success();
}

#[test]
fn doc_no_blocks_is_success_with_note() {
    let file = temp_program("doc_none", "echo \"hi\"\n");
    lang()
        .arg("doc")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no `@doc` blocks"));
}

#[test]
fn doc_unterminated_block_is_reported() {
    // A `@doc {` whose braces never balance is a lex error surfaced by the loader, not a silent
    // swallow.
    let file = temp_program("doc_unterminated", "@doc {\n  # never closed\n");
    lang().arg("doc").arg(&file).assert().failure().code(1);
}
