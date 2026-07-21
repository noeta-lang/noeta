//! Package manager: path dependencies (P2.1), registry + git-tag dependencies (P2.3),
//! git-forge registries, provenance/keys, and cross-package tiers.

use crate::support::*;

// --- package manager: path dependencies (P2.1) --------------------------------------------------

/// A single-file program declaring its own tier (`@tier(fuzz, config: Fuzz)`): the source of the
/// tier-providers e2e fixtures. The runner reads each root's knobs via `attributes_of::<Fuzz>()`
/// (block-stamped or per-fn), proving the T1 stamping + T5 reflection + T2 declaration + T4
/// dispatch composition in one program.
const DECLARED_TIER_PROGRAM: &str = "\
@attribute(Function)\n\
struct Fuzz { cases: int }\n\
@tier(fuzz, config: Fuzz)\n\
fn run_fuzz(roots: List<TierRoot>): void {\n\
    echo \"fuzzing ${roots.len()} roots\"\n\
    configs = attributes_of::<Fuzz>()\n\
    for root in roots {\n\
        mut cases = 10\n\
        for c in configs { if c.target == root.name { cases = c.value.cases } }\n\
        echo \"${root.name}: ${cases} cases\"\n\
        run = root.run\n\
        run()\n\
    }\n\
}\n\
@fuzz(cases: 500) {\n\
    fn checks_math(): void { echo \"  ran checks_math\" }\n\
}\n\
@fuzz fn bare_root(): void { echo \"  ran bare_root\" }\n";

#[test]
fn a_declared_tier_dispatches_to_its_runner() {
    // `noeta fuzz <file>` — an unknown subcommand naming a declared tier — activates that tier and
    // invokes the runner in-process with the roots; block knobs stamp (500), a bare root gets the
    // runner's default (10).
    let file = temp_program("tier_decl_single", DECLARED_TIER_PROGRAM);
    lang().arg("fuzz").arg(&file).assert().success().stdout(
        predicate::str::contains("fuzzing 2 roots")
            .and(predicate::str::contains("checks_math: 500 cases"))
            .and(predicate::str::contains("  ran checks_math"))
            .and(predicate::str::contains("bare_root: 10 cases")),
    );
}

#[test]
fn a_declared_tier_strips_on_a_normal_run() {
    // The declared tier obeys the strip invariant: `noeta run` compiles the same file with the
    // tier inactive — no roots run, no runner is invoked, nothing prints.
    let file = temp_program("tier_decl_strip", DECLARED_TIER_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn tier_declaration_errors_are_e0051() {
    // Redeclaring a built-in tier's name is LEGAL (provider override — dormant until a target
    // selects it); a same-provider duplicate, a non-attribute config, and a wrong runner
    // signature are each an E0051 at the declaration.
    let redeclare = temp_program(
        "tier_decl_redeclare",
        "@tier(bench)\nfn r(roots: List<TierRoot>): void { return }\n",
    );
    lang().arg("check").arg(&redeclare).assert().success();
    let dup = temp_program(
        "tier_decl_dup",
        "@tier(fuzz)\nfn r1(roots: List<TierRoot>): void { return }\n\
         @tier(fuzz)\nfn r2(roots: List<TierRoot>): void { return }\n",
    );
    lang()
        .arg("check")
        .arg(&dup)
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0051").and(predicate::str::contains("more than once")));
    let bad_config = temp_program(
        "tier_decl_badcfg",
        "struct NotAttr { x: int }\n@tier(fuzz, config: NotAttr)\nfn r(roots: List<TierRoot>): void { return }\n",
    );
    lang()
        .arg("check")
        .arg(&bad_config)
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0051").and(predicate::str::contains("@attribute")));
    let bad_sig = temp_program(
        "tier_decl_badsig",
        "@tier(fuzz)\nfn r(n: int): void { return }\n",
    );
    lang()
        .arg("check")
        .arg(&bad_sig)
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0051").and(predicate::str::contains("List<TierRoot>")));
}

/// A single-file program declaring a **text** tier (`@tier(spec, text: "xml")`, text-tiers arc):
/// its `@spec { … }` bodies are captured verbatim by the lexer (same-file two-pass — no manifest
/// involved), adjacency-targeted like `@doc`, and dispatched to the runner as `List<TierText>`.
/// The first body deliberately contains XML quotes (invalid as Noeta tokens) and an escaped brace.
const DECLARED_TEXT_TIER_PROGRAM: &str = "\
@tier(spec, text: \"xml\")\n\
fn run_specs(roots: List<TierText>): void {\n\
    echo \"specs: ${roots.len()}\"\n\
    for root in roots {\n\
        echo \"-- ${root.target}\"\n\
        echo root.text\n\
    }\n\
}\n\
@spec {\n\
  <case name=\"adds\" expect=\"3\"/> with a literal \\} brace\n\
}\n\
fn add(a: int, b: int): int { return a + b }\n";

#[test]
fn a_declared_text_tier_dispatches_its_bodies() {
    // `noeta spec <file>` invokes the text tier's runner with one TierText per body: `target` is
    // the adjacency-resolved declaration, `text` the verbatim body with escapes undone.
    let file = temp_program("text_tier_single", DECLARED_TEXT_TIER_PROGRAM);
    lang().arg("spec").arg(&file).assert().success().stdout(
        predicate::str::contains("specs: 1")
            .and(predicate::str::contains("-- add"))
            .and(predicate::str::contains(
                "<case name=\"adds\" expect=\"3\"/> with a literal } brace",
            )),
    );
}

#[test]
fn a_declared_text_tier_strips_on_a_normal_run() {
    // The strip invariant holds for text tiers: `noeta run` never sees the bodies (they parse to
    // no code at all), so the program runs clean and prints nothing.
    let file = temp_program("text_tier_strip", DECLARED_TEXT_TIER_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn text_tier_declaration_errors_are_e0051() {
    // `config:` and `text:` are mutually exclusive, and a text tier's runner takes
    // `List<TierText>` — each violation is an E0051 at the declaration.
    let both = temp_program(
        "text_tier_both",
        "@attribute(Function)\nstruct Knobs { n: int }\n@tier(spec, config: Knobs, text: \"xml\")\nfn r(roots: List<TierText>): void { return }\n",
    );
    lang()
        .arg("check")
        .arg(&both)
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0051").and(predicate::str::contains("no knobs")));
    let bad_sig = temp_program(
        "text_tier_badsig",
        "@tier(spec, text: \"xml\")\nfn r(roots: List<TierRoot>): void { return }\n",
    );
    lang()
        .arg("check")
        .arg(&bad_sig)
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0051").and(predicate::str::contains("List<TierText>")));
}

/// A consumer app + a `fuzzkit` path dependency that declares the `fuzz` tier. The consumer only
/// imports the runner; the config struct links implicitly through the `@tier` reference, and the
/// consumer's `@fuzz(cases: 7)` knob crosses the package boundary through the stamped attribute.
fn tier_dep_project(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("fuzzkit");
    std::fs::create_dir_all(&app).expect("mk app");
    std::fs::create_dir_all(&lib).expect("mk lib");
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nfuzzkit = { path = \"../fuzzkit\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use fuzzkit.tiers.run_fuzz\n\
         @fuzz(cases: 7) {\n    fn app_case(): void { echo \"  app_case ran\" }\n}\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/fuzz\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("tiers.noe"),
        "namespace fuzz.tiers;\n\
         @attribute(Function)\npub struct Fuzz { cases: int }\n\
         @tier(fuzz, config: Fuzz)\n\
         pub fn run_fuzz(roots: List<TierRoot>): void {\n\
             echo \"fuzzkit: ${roots.len()} roots\"\n\
             configs = attributes_of::<Fuzz>()\n\
             for root in roots {\n\
                 mut cases = 10\n\
                 for c in configs { if c.target == root.name { cases = c.value.cases } }\n\
                 echo \"${root.name}: ${cases} cases\"\n\
                 run = root.run\n\
                 run()\n\
             }\n\
         }\n",
    )
    .unwrap();
    app.join("main.noe")
}

/// The provider-override e2e: `fuzzkit` also declares `@tier(bench, config: Fuzz)`; the app's
/// `custom` target maps `bench = "fuzzkit"`. Extends [`tier_dep_project`]'s fixture.
fn tier_override_project(name: &str) -> PathBuf {
    let entry = tier_dep_project(name);
    let app_dir = entry.parent().unwrap().to_path_buf();
    let lib = app_dir.parent().unwrap().join("fuzzkit");
    let mut tiers = std::fs::read_to_string(lib.join("tiers.noe")).unwrap();
    tiers.push_str(
        "@tier(bench, config: Fuzz)\n\
         pub fn run_bench_alt(roots: List<TierRoot>): void {\n\
             echo \"ALT BENCH: ${roots.len()} roots\"\n\
             for root in roots { run = root.run; run() }\n\
         }\n",
    );
    std::fs::write(lib.join("tiers.noe"), tiers).unwrap();
    std::fs::write(
        app_dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nfuzzkit = { path = \"../fuzzkit\" }\n\
         [targets.custom.tiers]\nbench = \"fuzzkit\"\n",
    )
    .unwrap();
    std::fs::write(
        app_dir.join("main.noe"),
        "use fuzzkit.tiers.run_bench_alt\n\
         @bench(cases: 3) {\n    fn measures(): void { echo \"  measures ran\" }\n}\n",
    )
    .unwrap();
    entry
}

#[test]
fn a_target_overrides_a_builtin_tier_provider() {
    // `bench = "fuzzkit"` in the target's tiers map: `--target custom` stamps fuzzkit's config
    // (`cases:` is a `Fuzz` knob, not std's `Bench`) and dispatches to fuzzkit's runner; without
    // the target the native bench runs and the same knob is correctly rejected against std's
    // `Bench { iterations }`.
    let entry = tier_override_project("tier_override_bench");
    lang()
        .arg("bench")
        .arg(&entry)
        .arg("--target")
        .arg("custom")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ALT BENCH: 1 roots")
                .and(predicate::str::contains("  measures ran")),
        );
    lang()
        .arg("bench")
        .arg(&entry)
        .assert()
        .failure()
        .stderr(predicate::str::contains("has no field `cases`"));
}

#[test]
fn a_provider_that_declares_no_such_tier_is_an_error() {
    // The target maps `bench = "fuzzkit"` but the package declares no `@tier(bench)` — a clear
    // error naming both sides, not a silent native fallback.
    let entry = tier_dep_project("tier_override_missing");
    let app_dir = entry.parent().unwrap();
    std::fs::write(
        app_dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nfuzzkit = { path = \"../fuzzkit\" }\n\
         [targets.custom.tiers]\nbench = \"fuzzkit\"\n",
    )
    .unwrap();
    lang()
        .arg("bench")
        .arg(&entry)
        .arg("--target")
        .arg("custom")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("declares no"));
}

#[test]
fn tests_in_a_dependency_using_program_run() {
    // The built-in tier runners load with dependency resolution (the fold-in after tier-providers):
    // a `@test` exercising an imported package fn links and runs, exactly as `noeta run` would.
    let entry = tier_dep_project("tier_dep_test_runner");
    std::fs::write(
        entry.parent().unwrap().join("main.noe"),
        "use fuzzkit.tiers.helper_answer\n\
         @test fn dep_helper_works(): void { assert(helper_answer() == 42) }\n",
    )
    .unwrap();
    let lib = entry.parent().unwrap().parent().unwrap().join("fuzzkit");
    std::fs::write(
        lib.join("tiers.noe"),
        "namespace fuzz.tiers;\npub fn helper_answer(): int { return 42 }\n",
    )
    .unwrap();
    lang()
        .arg("test")
        .arg(&entry)
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn a_dependency_declared_tier_dispatches_cross_package() {
    // The third-party proof (tier-providers T4): the tier, its config attribute, and its runner
    // all live in a path dependency; the consumer opts in with one `use` and writes `@fuzz` blocks.
    let entry = tier_dep_project("tier_dep_dispatch");
    lang().arg("fuzz").arg(&entry).assert().success().stdout(
        predicate::str::contains("fuzzkit: 1 roots")
            .and(predicate::str::contains("app_case: 7 cases"))
            .and(predicate::str::contains("  app_case ran")),
    );
    // And the same program runs clean with the tier stripped.
    lang()
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/// A consumer app + a `speckit` path dependency declaring a **text** tier (text-tiers arc). The
/// consumer writes `@spec { <xml/> }` bodies with no local declaration at all — the loader's
/// program-wide lex (union of every package's `@tier(…, text:)` decls) is what makes the body
/// capture verbatim across the package boundary.
fn text_tier_dep_project(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("speckit");
    std::fs::create_dir_all(&app).expect("mk app");
    std::fs::create_dir_all(&lib).expect("mk lib");
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nspeckit = { path = \"../speckit\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use speckit.tiers.run_specs\n\
         @spec {\n  <case name=\"adds\" expect=\"3\"/>\n}\n\
         fn add(a: int, b: int): int { return a + b }\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/spec\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("tiers.noe"),
        "namespace spec.tiers;\n\
         @tier(spec, text: \"xml\")\n\
         pub fn run_specs(roots: List<TierText>): void {\n\
             echo \"speckit: ${roots.len()} bodies\"\n\
             for root in roots {\n\
                 echo \"-- ${root.target}\"\n\
                 echo root.text\n\
             }\n\
         }\n",
    )
    .unwrap();
    app.join("main.noe")
}

#[test]
fn a_dependency_declared_text_tier_captures_cross_package() {
    // The third-party text-tier proof: the declaration lives in a path dependency; the consumer's
    // `@spec { … }` XML body (quotes and all — invalid as Noeta tokens) still captures verbatim,
    // targets the adjacent fn, and dispatches to the dependency's runner.
    let entry = text_tier_dep_project("text_tier_dep");
    lang().arg("spec").arg(&entry).assert().success().stdout(
        predicate::str::contains("speckit: 1 bodies")
            .and(predicate::str::contains("-- add"))
            .and(predicate::str::contains(
                "<case name=\"adds\" expect=\"3\"/>",
            )),
    );
    // The same program runs and checks clean with the tier stripped.
    lang()
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    lang().arg("check").arg(&entry).assert().success();
}

#[test]
fn a_path_dependency_resolves_and_runs() {
    let entry = path_dep_project("pm_pathdep_run");
    lang()
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from the dependency"));
}

#[test]
fn a_dependency_syntax_error_is_reported_against_the_dependency_file() {
    // A `.noe` file inside a dependency package that fails to PARSE used to be swallowed — the
    // module never reached the link pool, and the consumer got
    // `[E0019] no module `greet.hello` in this project` pointed at its own `use`. That sends the
    // reader to inspect their import and the package's module naming while the real fault — a
    // syntax error in a file they were never told about — goes unreported. The parse error must be
    // what prints, naming the dependency's file, and the E0019 cascade must not print at all.
    for cmd in ["check", "run"] {
        let entry = path_dep_project(&format!("pm_dep_parse_error_{cmd}"));
        let lib = entry.parent().unwrap().parent().unwrap().join("greetlib");
        std::fs::write(
            lib.join("hello.noe"),
            "namespace greet.hello;\npub fn greeting(): string {\n  let ] = ;\n}\n",
        )
        .unwrap();
        let assert = lang().arg(cmd).arg(&entry).assert().failure();
        let out = String::from_utf8_lossy(&assert.get_output().stderr).to_string()
            + &String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(
            out.contains("hello.noe"),
            "`noeta {cmd}` should point at the dependency file that failed to parse, got:\n{out}"
        );
        assert!(
            !out.contains("no module"),
            "the misleading `no module` cascade must be suppressed, got:\n{out}"
        );
    }
}

#[test]
fn noeta_audit_reports_the_trust_footprint() {
    // A pure path-dependency project: audit lists the dependency and reports no elevated authority.
    let entry = path_dep_project("pm_audit");
    let app_dir = entry.parent().unwrap();
    lang()
        .arg("audit")
        .arg(app_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/greet"))
        .stdout(predicate::str::contains("0 package(s) run native code"))
        .stdout(predicate::str::contains("native   : (none)"));
}

#[test]
fn noeta_check_resolves_cross_package_use() {
    // `noeta check` must see dependency packages too (package-manager P2.1c), so a cross-package
    // `use` that references a real exported symbol checks clean rather than erroring.
    let entry = path_dep_project("pm_check_crosspkg");
    lang().arg("check").arg(&entry).assert().success();

    // And a reference to a *missing* dependency export is a real error (the dep is genuinely linked,
    // not opaquely stubbed away).
    let dir = entry.parent().unwrap();
    std::fs::write(dir.join("main.noe"), "use hi.hello.nope;\necho nope();\n").unwrap();
    lang()
        .arg("check")
        .arg(dir.join("main.noe"))
        .assert()
        .failure();
}

#[test]
fn a_transitive_path_dependency_resolves_and_runs() {
    // app → mid → low, each a path package. `mid` keys its own dependency `deep` (≠ low's root
    // segment `low`), so the graph walk must rewrite mid's internal `use deep.base.leaf` to low's
    // global segment for it to link — a transitive reference the app never names (P2.4).
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_transitive_run");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let mid = base.join("midlib");
    let low = base.join("lowlib");
    for d in [&app, &mid, &low] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nmid = { path = \"../midlib\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use mid.api.top;\necho top();\n").unwrap();
    std::fs::write(
        mid.join("noeta.toml"),
        "[package]\nname = \"acme/mid\"\nversion = \"1.0.0\"\n\
         [dependencies]\ndeep = { path = \"../lowlib\" }\n",
    )
    .unwrap();
    std::fs::write(
        mid.join("api.noe"),
        "namespace mid.api;\nuse deep.base.leaf;\npub fn top(): string { return leaf(); }\n",
    )
    .unwrap();
    std::fs::write(
        low.join("noeta.toml"),
        "[package]\nname = \"acme/low\"\nversion = \"2.3.0\"\n",
    )
    .unwrap();
    std::fs::write(
        low.join("base.noe"),
        "namespace low.base;\npub fn leaf(): string { return \"deep transitive value\"; }\n",
    )
    .unwrap();

    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("deep transitive value"));
}

#[test]
fn a_version_conflict_is_reported() {
    // app depends on two packages that each pull a third at *different* versions — our flat link
    // model permits one version per package, so this is an explainable conflict (P2.4).
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_version_conflict");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let a = base.join("a");
    let b = base.join("b");
    let c1 = base.join("c1");
    let c2 = base.join("c2");
    for d in [&app, &a, &b, &c1, &c2] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\na = { path = \"../a\" }\nb = { path = \"../b\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
    // a and b both depend on acme/shared, but at different on-disk versions.
    std::fs::write(
        a.join("noeta.toml"),
        "[package]\nname = \"acme/a\"\nversion = \"1.0.0\"\n\
         [dependencies]\ns = { path = \"../c1\" }\n",
    )
    .unwrap();
    std::fs::write(
        a.join("a.noe"),
        "namespace a.x;\npub fn f(): int { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        b.join("noeta.toml"),
        "[package]\nname = \"acme/b\"\nversion = \"1.0.0\"\n\
         [dependencies]\ns = { path = \"../c2\" }\n",
    )
    .unwrap();
    std::fs::write(
        b.join("b.noe"),
        "namespace b.x;\npub fn g(): int { return 2; }\n",
    )
    .unwrap();
    std::fs::write(
        c1.join("noeta.toml"),
        "[package]\nname = \"acme/shared\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        c1.join("s.noe"),
        "namespace shared.core;\npub fn h(): int { return 3; }\n",
    )
    .unwrap();
    std::fs::write(
        c2.join("noeta.toml"),
        "[package]\nname = \"acme/shared\"\nversion = \"2.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        c2.join("s.noe"),
        "namespace shared.core;\npub fn h(): int { return 4; }\n",
    )
    .unwrap();

    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("acme/shared"));
}

#[test]
fn a_published_package_resolves_as_a_registry_dependency() {
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_registry_e2e");
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("greet_repo");
    let app = base.join("app");
    let reg = base.join("registry");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&app).unwrap();

    // The package to publish: a tagged git repo with a [package] identity.
    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.2.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("hello.noe"),
        "namespace greet.hello;\npub fn greeting(): string { return \"hello from the registry\"; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "release"], &repo);
    git_in(&["tag", "v1.2.0"], &repo);

    // Publish it to the (local/offline) registry index.
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.2.0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("published `acme/greet` 1.2.0"));

    // A consumer depends on it by version, naming the registry package (decoupled from the key).
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use gc.hello.greeting;\necho greeting();\n",
    )
    .unwrap();

    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from the registry"));
}

#[test]
fn the_lockfile_pins_registry_selection_and_bypasses_the_index() {
    // The lock fast path (audit F1): once `noeta.lock` pins a registry version, later builds
    // adopt the pin — an upstream publish must not float the selection, and the index must not
    // even be consulted (offline builds). `noeta update` (or a changed requirement) re-solves.
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_lock_pins");
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("greet_repo");
    let app = base.join("app");
    let reg = base.join("registry");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&app).unwrap();

    // v1.0.0: the version the app will lock.
    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("hello.noe"),
        "namespace greet.hello;\npub fn greeting(): string { return \"one point oh\"; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "v1.0.0"], &repo);
    git_in(&["tag", "v1.0.0"], &repo);
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .success();

    // The consumer resolves ^1.0 → 1.0.0 and writes the lock.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use gc.hello.greeting;\necho greeting();\n",
    )
    .unwrap();
    let run = |dir: &std::path::Path| {
        let mut cmd = lang();
        cmd.env("NOETA_REGISTRY_DIR", &reg)
            .arg("run")
            .arg(dir.join("main.noe"));
        cmd
    };
    run(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("one point oh"));
    assert!(
        app.join("noeta.lock").exists(),
        "the resolve pinned the lock"
    );

    // Upstream publishes v1.1.0 (still within ^1.0).
    std::fs::write(
        repo.join("hello.noe"),
        "namespace greet.hello;\npub fn greeting(): string { return \"one point one\"; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.1.0\"\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "v1.1.0"], &repo);
    git_in(&["tag", "v1.1.0"], &repo);
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.1.0",
        ])
        .assert()
        .success();

    // The locked build DOES NOT float to 1.1.0.
    run(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("one point oh"));

    // …and does not consult the index at all: with the registry gone, the build still resolves
    // (lock + store). This is the offline guarantee the lock's own docs promise.
    let hidden = base.join("registry_hidden");
    std::fs::rename(&reg, &hidden).unwrap();
    run(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("one point oh"));
    std::fs::rename(&hidden, &reg).unwrap();

    // A changed requirement is frontier drift → live re-solve picks 1.1.0.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.1\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    run(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("one point one"));

    // Back on ^1.0 the fresh lock (now pinning 1.1.0, which satisfies ^1.0) keeps 1.1.0 — and
    // `noeta update` keeps it too (highest compatible). The pin, not the range, decides.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    run(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("one point one"));
}

#[test]
fn a_git_forge_registry_resolves_and_runs_a_package() {
    // private-registries (end-to-end): a consumer maps a scope to a git forge via `[registries]` and
    // resolves a package from that forge's repos + tags. Hermetic — a local directory is the forge (a
    // `git:<path>` base), so no network and no auth (public path). Proves the full chain: per-scope
    // routing → GitForgeIndex (tags → versions) → git materialization → run. The `github:`/`gitlab:`
    // shorthands parse to the same GitForge base (unit-tested), so this exercises them all.
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_git_forge_registry_e2e");
    let _ = std::fs::remove_dir_all(&base);
    let host = base.join("host"); // the forge host
    let repo = host.join("acme").join("greet"); // the org/repo = acme/greet
    let app = base.join("app");
    let cache = base.join("forge-cache");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&app).unwrap();

    // The package repo, with a tagged release (versions are tags — no publish step).
    git_in(&["init", "-q", "-b", "main"], &repo);
    std::fs::write(
        repo.join("hello.noe"),
        "namespace greet.hello;\npub fn greeting(): string { return \"hello from the org registry\"; }\n",
    )
    .unwrap();
    commit_version(
        &repo,
        "v1.2.0",
        "[package]\nname = \"acme/greet\"\nversion = \"1.2.0\"\n",
    );

    // The consumer routes scope `acme` to the forge base (`<host>/acme`); everything else stays on the
    // default. A `git:<path>` base clones a local repo, so no network.
    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"me/app\"\nversion = \"0.1.0\"\n\
             [registries]\nacme = \"git:{}/acme\"\n\
             [dependencies]\ngc = {{ version = \"^1.0\", package = \"acme/greet\" }}\n",
            host.display()
        ),
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use gc.hello.greeting;\necho greeting();\n",
    )
    .unwrap();

    lang()
        .env("NOETA_GIT_FORGE_CACHE", &cache)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from the org registry"));
}

#[test]
fn a_git_forge_resolve_tolerates_the_auth_token_override() {
    // private-registries S5: with NOETA_GITHUB_TOKEN set, every git subprocess gets a scoped
    // `-c http.https://github.com.extraHeader=…` auth config. Resolution against the local forge (a
    // file path, not github.com) must still succeed — proving git accepts the arg and the header is
    // scoped so it doesn't interfere. (Authenticating a *real* private repo needs real GitHub; here we
    // prove the plumbing is inert where it should be.)
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_git_forge_token_e2e");
    let _ = std::fs::remove_dir_all(&base);
    let host = base.join("host");
    let repo = host.join("acme").join("greet");
    let app = base.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&app).unwrap();

    git_in(&["init", "-q", "-b", "main"], &repo);
    std::fs::write(
        repo.join("hello.noe"),
        "namespace greet.hello;\npub fn greeting(): string { return \"hello (token path)\"; }\n",
    )
    .unwrap();
    commit_version(
        &repo,
        "v1.0.0",
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    );

    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"me/app\"\nversion = \"0.1.0\"\n\
             [registries]\nacme = \"git:{}/acme\"\n\
             [dependencies]\ngc = {{ version = \"^1.0\", package = \"acme/greet\" }}\n",
            host.display()
        ),
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use gc.hello.greeting;\necho greeting();\n",
    )
    .unwrap();

    lang()
        .env("NOETA_GIT_FORGE_CACHE", base.join("cache"))
        .env("NOETA_GITHUB_TOKEN", "ghp_faketoken_for_plumbing_test")
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hello (token path)"));
}

#[test]
fn a_registry_dependency_without_a_package_is_an_error() {
    // A bare registry requirement names no package identity, so it can't be resolved — the error
    // points the user to add `package = "company/pkg"` (P2.5).
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_registrydep");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nhi = \"^1.0\"\n",
    )
    .unwrap();
    std::fs::write(base.join("main.noe"), "echo 1;\n").unwrap();
    lang()
        .arg("run")
        .arg(base.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("names no package"));
}

#[test]
fn publishing_a_package_with_a_path_dependency_is_rejected() {
    // Phase 4 #3: a published package must depend only via the registry — a path/git dependency
    // would leave a consumer unable to resolve it. The lint fails before touching git.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_publish_lint");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("noeta.toml"),
        "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n\
         [dependencies]\nhelper = { path = \"../helper\" }\n",
    )
    .unwrap();
    lang()
        .current_dir(&base)
        .env("NOETA_REGISTRY_DIR", base.join("registry"))
        .args([
            "publish",
            "--git",
            "https://example.com/acme/lib",
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("depend only via the registry"))
        .stderr(predicate::str::contains("helper"));
}

#[test]
fn a_target_scoped_dependency_links_only_under_its_target() {
    // dev-deps D2, finally wired (audit-5 F3): `[targets.<name>.dependencies]` was parsed,
    // validated, and documented — but no production path resolved it, so a declared dev-only
    // dependency silently did nothing. Now: the dep links under `--target dev` and stays out of
    // the default build.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("target_scoped_deps");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("devlib");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/devlib\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("api.noe"),
        "namespace devlib.api;\npub fn marker(): string { return \"dev tooling linked\"; }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [targets.dev.dependencies]\ndevtools = { path = \"../devlib\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use devtools.api.marker;\necho marker();\n",
    )
    .unwrap();

    // Under the target, the dep resolves and runs.
    lang()
        .args(["run", "--target", "dev"])
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("dev tooling linked"));
    // Without it, the dependency is absent — the import can't bind, so the run fails.
    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure();
}

#[test]
fn publish_routes_through_the_manifest_registries_scope_map() {
    // Private-registries follow-up: `noeta publish` must open the registry the manifest's
    // `[registries]` map routes the package's OWN scope to — the same routing resolution uses —
    // not the environment default. Routing `acme` to a git forge makes the outcome observable
    // without a network: the forge index's publish() is a hard "no publish endpoint" error, and
    // the default local index (which would silently accept the release) must NOT be written.
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_publish_routing");
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("lib");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&["init", "-q"], &repo);
    commit_version(
        &repo,
        "v1.0.0",
        "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n\
         [registries]\nacme = \"github:acme\"\n",
    );
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", base.join("registry"))
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "via the `[registries]` source for `acme`",
        ))
        .stderr(predicate::str::contains("no publish endpoint"));
    // The environment-default local index was never touched.
    assert!(
        !base.join("registry").exists(),
        "publish must not fall through to the default registry when the scope is routed"
    );
}

// --- package manager: git-tag dependencies (P2.3) -----------------------------------------------

#[test]
fn provenance_signs_verifies_and_pins_the_scope_key() {
    // End-to-end provenance (Phase 4 #2): `noeta key new` → a signed publish → a consumer that
    // verifies the signature and pins the scope key (TOFU); then a *changed* key is rejected.
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_provenance");
    let _ = std::fs::remove_dir_all(&base);
    let reg = base.join("registry");
    let repo = base.join("greet_repo");
    let app = base.join("app");
    let key = base.join("signing.key");
    for d in [&reg, &repo, &app] {
        std::fs::create_dir_all(d).unwrap();
    }

    // A tagged package repo (registry-form deps only — none here).
    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("m.noe"),
        "namespace greet.core;\npub fn v(): int { return 1; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "r"], &repo);
    git_in(&["tag", "v1.0.0"], &repo);

    // Generate a signing key.
    lang()
        .args(["key", "new", "--out", key.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("public key"));

    // Publish, signed (cwd = the package repo so it finds greet's manifest). GITHUB_ACTIONS is
    // scrubbed so a CI run of this suite doesn't trip ambient keyless detection.
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_SIGNING_KEY", &key)
        .env_remove("GITHUB_ACTIONS")
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[signed]"));

    // The consumer resolves it, verifies the signature, and pins the scope key.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 42;\n").unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout("42\n");
    let lock = std::fs::read_to_string(app.join("noeta.lock")).expect("lock written");
    assert!(lock.contains("[[scope]]"), "scope key pinned: {lock}");
    assert!(lock.contains("acme"), "scope name pinned: {lock}");

    // TOFU: replace the registry's scope key with a *different* one. The LOCKED build is
    // unaffected — its release was verified at first resolve and the lock pins the exact content
    // (SHA + tree hash), so the lock fast path never re-consults the registry. That bypass is the
    // pin's offline/reproducibility guarantee: a registry gone rogue *after* first use can't
    // reach a pinned build at all.
    std::fs::write(reg.join("scope__acme.pub"), format!("{}\n", "c".repeat(64))).unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout("42\n");
    // …but any resolve that DOES consult the index rejects: a fresh consumer (no lock — the
    // classic key-swap-then-forge scenario) verifies the release's signature against the served
    // key, and the release was signed under the original one.
    std::fs::remove_file(app.join("noeta.lock")).unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("provenance"));
}

#[test]
fn keyless_trust_pins_downgrades_and_switches_are_enforced_end_to_end() {
    // The keyless trust model (Phase 5, K3) through the whole stack: the `noeta.lock` scope pin
    // drives resolution. Negative paths only — they need no verifiable bundle (a *positive*
    // keyless resolve is exercised with minted bundles in the K4 publish tests).
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_keyless_trust");
    let _ = std::fs::remove_dir_all(&base);
    let reg = base.join("registry");
    let repo = base.join("greet_repo");
    let app = base.join("app");
    for d in [&reg, &repo, &app] {
        std::fs::create_dir_all(d).unwrap();
    }

    // A tagged package repo, published UNSIGNED (no signing key in the environment).
    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("m.noe"),
        "namespace greet.core;\npub fn v(): int { return 1; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "r"], &repo);
    git_in(&["tag", "v1.0.0"], &repo);
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env_remove("NOETA_SIGNING_KEY")
        .env_remove("GITHUB_ACTIONS")
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("UNSIGNED"));

    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 42;\n").unwrap();

    // 1. Downgrade rejection: the scope is keyless-pinned in the lock, but the registry serves an
    //    unsigned release — exactly what a compromised registry smuggling a forged release looks
    //    like. The resolve must fail and name the defense.
    std::fs::write(
        app.join("noeta.lock"),
        "version = 2\n\n[[scope]]\nname = \"acme\"\n\
         issuer = \"https://token.actions.githubusercontent.com\"\n\
         identity = \"https://github.com/acme/greet/.github/workflows/r.yaml@refs/heads/main\"\n",
    )
    .unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("downgrade"));

    // 2. Keyless verification is live in the CLI: an unpinned consumer receiving a garbage bundle
    //    must fail *in the verifier* (malformed bundle), not silently accept.
    let _ = std::fs::remove_file(app.join("noeta.lock"));
    let entry = reg.join("acme__greet.toml");
    let text = std::fs::read_to_string(&entry).unwrap();
    std::fs::write(&entry, format!("{text}bundle = \"{{}}\"\n")).unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("Sigstore bundle"));

    // 3. Root-switch rejection: a key-pinned scope served a keyless release. Never implicit —
    //    any OIDC identity could otherwise take over a key-pinned scope.
    std::fs::write(
        app.join("noeta.lock"),
        format!(
            "version = 2\n\n[[scope]]\nname = \"acme\"\npublic_key = \"{}\"\n",
            "b".repeat(64)
        ),
    )
    .unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("never implicit"));
}

#[test]
fn interactive_oob_publish_signs_keyless_end_to_end() {
    // The interactive browser-login path (Phase 5, K6), driven the way a human drives OOB mode:
    // the CLI prints the sign-in URL and prompts for a code; this test "is" the user — it visits
    // the URL (the mock OIDC provider, PKCE enforced), reads the code off the page, and types it
    // into the CLI's stdin. The publish then runs the same Fulcio/Rekor/bundle path as CI, and a
    // consumer resolve pins the EMAIL identity.
    if !git_available() {
        return;
    }
    use noeta_pm::keyless_fixtures::{TestSigstore, spawn_mock};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::Arc;

    const EMAIL: &str = "maintainer@example.test";
    const ISSUER: &str = "https://oauth2.noeta.test";

    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_keyless_interactive");
    let _ = std::fs::remove_dir_all(&base);
    let reg = base.join("registry");
    let repo = base.join("greet_repo");
    let app = base.join("app");
    for d in [&reg, &repo, &app] {
        std::fs::create_dir_all(d).unwrap();
    }

    let sigstore = Arc::new(TestSigstore::new(ISSUER, EMAIL));
    let oidc = {
        let s = sigstore.clone();
        spawn_mock(move |m, p, b| s.handle_oidc(m, p, b))
    };
    let fulcio = {
        let s = sigstore.clone();
        spawn_mock(move |m, p, b| s.handle_fulcio(m, p, b))
    };
    let rekor = {
        let s = sigstore.clone();
        spawn_mock(move |m, p, b| s.handle_rekor(m, p, b))
    };
    let trust_root = base.join("trusted_root.json");
    std::fs::write(&trust_root, sigstore.trusted_root_json()).unwrap();

    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("m.noe"),
        "namespace greet.core;\npub fn v(): int { return 1; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "r"], &repo);
    git_in(&["tag", "v1.0.0"], &repo);

    // Publish interactively (OOB), piping stdio so the test can play the user.
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"))
        .current_dir(&repo)
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .env("NOETA_REGISTRY_DIR", &reg)
        .env_remove("NOETA_SIGNING_KEY")
        .env_remove("GITHUB_ACTIONS")
        .env("NOETA_OIDC_URL", &oidc)
        .env("NOETA_FULCIO_URL", &fulcio)
        .env("NOETA_REKOR_URL", &rekor)
        .env("NOETA_SIGSTORE_TRUST_ROOT", &trust_root)
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
            "--interactive",
            "--oob",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn noeta publish");

    // Read the CLI's output until it announces the sign-in URL.
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let auth_url = loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).expect("read publish output"),
            0,
            "publish ended before printing the sign-in URL"
        );
        let trimmed = line.trim();
        if trimmed.starts_with("http://") {
            break trimmed.to_string();
        }
    };

    // "Visit" the sign-in page: GET the auth URL against the mock provider, which enforces the
    // PKCE parameters and shows the verification code.
    let page = {
        let rest = auth_url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_once('/').unwrap();
        let mut stream = std::net::TcpStream::connect(host).expect("connect mock oidc");
        write!(
            stream,
            "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.split_once("\r\n\r\n").unwrap().1.to_string()
    };
    let code = serde_json::from_str::<serde_json::Value>(&page).expect("login page JSON")["code"]
        .as_str()
        .expect("code on page")
        .to_string();

    // Type the code at the prompt.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{code}\n").as_bytes())
        .unwrap();

    let mut rest_out = String::new();
    stdout.read_to_string(&mut rest_out).unwrap();
    let status = child.wait().expect("publish exits");
    assert!(status.success(), "publish failed: {rest_out}");
    assert!(
        rest_out.contains(&format!("keyless: {EMAIL}")),
        "expected keyless email identity in: {rest_out}"
    );

    // The consumer verifies and pins the email identity.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 42;\n").unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_SIGSTORE_TRUST_ROOT", &trust_root)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout("42\n");
    let lock = std::fs::read_to_string(app.join("noeta.lock")).expect("lock written");
    assert!(lock.contains(&format!("identity = \"{EMAIL}\"")), "{lock}");
    assert!(lock.contains(&format!("issuer = \"{ISSUER}\"")), "{lock}");
}

#[test]
fn keyless_publish_verifies_pins_and_defends_end_to_end() {
    // The POSITIVE keyless loop through the real CLI (Phase 5, K4): an ambient CI identity
    // publishes a release whose Sigstore bundle is minted by a hermetic in-process Fulcio + CT
    // log + Rekor; a consumer resolves it, verifies the bundle offline under the DEFAULT policy
    // against the matching trust root, and TOFU-pins the identity in `noeta.lock`; a later
    // identity change is rejected.
    if !git_available() {
        return;
    }
    use noeta_pm::keyless_fixtures::{TestSigstore, spawn_mock};
    use std::sync::Arc;

    const IDENTITY: &str =
        "https://github.com/acme/greet/.github/workflows/release.yaml@refs/heads/main";
    const ISSUER: &str = "https://token.actions.githubusercontent.com";

    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_keyless_publish");
    let _ = std::fs::remove_dir_all(&base);
    let reg = base.join("registry");
    let repo = base.join("greet_repo");
    let app = base.join("app");
    for d in [&reg, &repo, &app] {
        std::fs::create_dir_all(d).unwrap();
    }

    // The hermetic Sigstore: mock Fulcio + Rekor + the GitHub Actions token endpoint, plus the
    // trust root file that binds their public halves.
    let sigstore = Arc::new(TestSigstore::new(ISSUER, IDENTITY));
    let fulcio = {
        let s = sigstore.clone();
        spawn_mock(move |m, p, b| s.handle_fulcio(m, p, b))
    };
    let rekor = {
        let s = sigstore.clone();
        spawn_mock(move |m, p, b| s.handle_rekor(m, p, b))
    };
    let token_endpoint = {
        let s = sigstore.clone();
        spawn_mock(move |method, path, _| {
            assert_eq!(method, "GET");
            assert!(path.contains("audience=sigstore"), "path: {path}");
            (200, s.github_token_response())
        })
    };
    let trust_root = base.join("trusted_root.json");
    std::fs::write(&trust_root, sigstore.trusted_root_json()).unwrap();

    // A tagged package repo.
    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("m.noe"),
        "namespace greet.core;\npub fn v(): int { return 1; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "r"], &repo);
    git_in(&["tag", "v1.0.0"], &repo);

    // Publish from "CI": the ambient GitHub Actions identity signs keyless. No signing key.
    lang()
        .current_dir(&repo)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env_remove("NOETA_SIGNING_KEY")
        .env("GITHUB_ACTIONS", "true")
        .env(
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            format!("{token_endpoint}/token"),
        )
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "mock-runner-token")
        .env("NOETA_FULCIO_URL", &fulcio)
        .env("NOETA_REKOR_URL", &rekor)
        .env("NOETA_SIGSTORE_TRUST_ROOT", &trust_root)
        .args([
            "publish",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("keyless: {IDENTITY}")));

    // A consumer resolves it: the bundle verifies offline (default policy) and the identity is
    // TOFU-pinned in the lock.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 42;\n").unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_SIGSTORE_TRUST_ROOT", &trust_root)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout("42\n");
    let lock = std::fs::read_to_string(app.join("noeta.lock")).expect("lock written");
    assert!(lock.contains("[[scope]]"), "keyless pin written: {lock}");
    assert!(
        lock.contains(&format!("identity = \"{IDENTITY}\"")),
        "{lock}"
    );
    assert!(lock.contains(&format!("issuer = \"{ISSUER}\"")), "{lock}");

    // The audit names the pinned keyless identity.
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_SIGSTORE_TRUST_ROOT", &trust_root)
        .args(["audit", app.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("keyless").and(predicate::str::contains(IDENTITY)));

    // TOFU holds: a release later signed by a DIFFERENT identity (the registry re-serving a
    // forged bundle) is rejected against the pin. The scope pin is checked wherever the index is
    // actually consulted, so force a LIVE resolve by dropping the package's version pin (a fully
    // locked build never re-fetches the bundle — its content is already SHA + hash pinned).
    let pinned_elsewhere = lock
        .replace(
            "acme/greet/.github/workflows/release.yaml",
            "mallory/greet/.github/workflows/release.yaml",
        )
        .replace("version = \"1.0.0\"\n", "");
    std::fs::write(app.join("noeta.lock"), pinned_elsewhere).unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_SIGSTORE_TRUST_ROOT", &trust_root)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("identity mismatch"));
}

#[test]
fn a_registry_diamond_backtracks_to_a_compatible_set() {
    // The end-to-end proof of PubGrub range resolution (Phase 4, S5): a diamond where the greedy
    // pick (highest `foo`) forces an incompatible `bar`, but a lower `foo` resolves. The walk must
    // read candidate deps from the index and backtrack — a greedy resolver would fail here.
    //   app → foo ^1.0, baz ^1.0
    //   foo 1.1 → bar ^2.0 ;  foo 1.0 → bar ^1.0 ;  baz 1.0 → bar ^1.0 ;  bar ∈ {1.0, 2.0}
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_diamond");
    let _ = std::fs::remove_dir_all(&base);
    let reg = base.join("registry");
    let app = base.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&reg).unwrap();

    let dep = |name: &str, req: &str| {
        format!("[dependencies]\nd = {{ version = \"{req}\", package = \"{name}\" }}\n")
    };
    let make_repo = |name: &str| {
        let repo = base.join(name.replace('/', "_"));
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&["init", "-q"], &repo);
        repo
    };

    // foo: 1.0.0 (bar ^1.0) then 1.1.0 (bar ^2.0).
    let foo = make_repo("foo");
    commit_version(
        &foo,
        "v1.0.0",
        &format!(
            "[package]\nname = \"acme/foo\"\nversion = \"1.0.0\"\n{}",
            dep("acme/bar", "^1.0")
        ),
    );
    commit_version(
        &foo,
        "v1.1.0",
        &format!(
            "[package]\nname = \"acme/foo\"\nversion = \"1.1.0\"\n{}",
            dep("acme/bar", "^2.0")
        ),
    );
    // bar: 1.0.0 then 2.0.0 (no deps).
    let bar = make_repo("bar");
    commit_version(
        &bar,
        "v1.0.0",
        "[package]\nname = \"acme/bar\"\nversion = \"1.0.0\"\n",
    );
    commit_version(
        &bar,
        "v2.0.0",
        "[package]\nname = \"acme/bar\"\nversion = \"2.0.0\"\n",
    );
    // baz: 1.0.0 (bar ^1.0).
    let baz = make_repo("baz");
    commit_version(
        &baz,
        "v1.0.0",
        &format!(
            "[package]\nname = \"acme/baz\"\nversion = \"1.0.0\"\n{}",
            dep("acme/bar", "^1.0")
        ),
    );

    // Write the registry index entries (each version → git coords + deps), matching LocalIndex format.
    let entry = |file: &str, body: String| std::fs::write(reg.join(file), body).unwrap();
    let version = |v: &str, url: &std::path::Path, tag: &str, deps: &str| {
        format!(
            "[[version]]\nversion = \"{v}\"\nurl = \"{}\"\ntag = \"{tag}\"\nsha = \"{}\"\n{deps}",
            url.display(),
            git_sha(url, tag)
        )
    };
    entry(
        "acme__foo.toml",
        format!(
            "{}{}",
            version(
                "1.0.0",
                &foo,
                "v1.0.0",
                "[[version.deps]]\npackage = \"acme/bar\"\nreq = \"^1.0\"\n"
            ),
            version(
                "1.1.0",
                &foo,
                "v1.1.0",
                "[[version.deps]]\npackage = \"acme/bar\"\nreq = \"^2.0\"\n"
            ),
        ),
    );
    entry(
        "acme__bar.toml",
        format!(
            "{}{}",
            version("1.0.0", &bar, "v1.0.0", ""),
            version("2.0.0", &bar, "v2.0.0", ""),
        ),
    );
    entry(
        "acme__baz.toml",
        version(
            "1.0.0",
            &baz,
            "v1.0.0",
            "[[version.deps]]\npackage = \"acme/bar\"\nreq = \"^1.0\"\n",
        ),
    );

    // The app depends on foo ^1.0 and baz ^1.0 by version (the deps are resolved, not used).
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\n\
         foo = { version = \"^1.0\", package = \"acme/foo\" }\n\
         baz = { version = \"^1.0\", package = \"acme/baz\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 42;\n").unwrap();

    // A greedy resolver would error (foo 1.1 → bar 2.0 clashes with baz's bar ^1.0). Success means
    // the walk backtracked to foo 1.0 + bar 1.0.
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout("42\n");

    // The lock records the backtracked set: only 1.0.0 versions, never 1.1.0 / 2.0.0.
    let lock = std::fs::read_to_string(app.join("noeta.lock")).expect("lock written");
    assert!(lock.contains("acme/foo"), "{lock}");
    assert!(lock.contains("acme/bar"), "{lock}");
    assert!(
        !lock.contains("1.1.0"),
        "foo must resolve to 1.0.0, not 1.1.0:\n{lock}"
    );
    assert!(
        !lock.contains("2.0.0"),
        "bar must resolve to 1.0.0, not 2.0.0:\n{lock}"
    );
}

#[test]
fn a_git_tag_dependency_is_fetched_and_run() {
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_gitdep_run");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let dep_repo = base.join("greetlib_repo");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&dep_repo).unwrap();

    // A local tagged package repo (root segment `greet`, from acme/greet).
    git_in(&["init", "-q"], &dep_repo);
    std::fs::write(
        dep_repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dep_repo.join("hello.noe"),
        "namespace greet.hello;\npub fn greeting(): string { return \"hi from a git dep\"; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &dep_repo);
    git_in(&["commit", "-q", "-m", "release"], &dep_repo);
    git_in(&["tag", "v1.0.0"], &dep_repo);

    // The app depends on it by git URL (a local path is a valid git URL), keyed `hi`.
    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nhi = {{ git = \"{}\", tag = \"v1.0.0\" }}\n",
            dep_repo.display()
        ),
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use hi.hello.greeting;\necho greeting();\n",
    )
    .unwrap();

    // Isolate the package store under the test's cache dir (set by `lang()`), so the fetch is hermetic.
    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from a git dep"));
}

#[test]
fn noeta_add_edits_the_manifest_and_resolves() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_add");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("lib");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    // The app starts with no dependencies; a comment must survive the edit.
    std::fs::write(
        app.join("noeta.toml"),
        "# my app\n[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use hi.core.v;\necho v();\n").unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("m.noe"),
        "namespace lib.core;\npub fn v(): int { return 42; }\n",
    )
    .unwrap();

    lang()
        .current_dir(&app)
        .args(["add", "hi", "--path", "../lib"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added `hi`"));

    let manifest = std::fs::read_to_string(app.join("noeta.toml")).unwrap();
    assert!(
        manifest.contains("# my app"),
        "comment preserved: {manifest}"
    );
    assert!(
        manifest.contains("hi = { path = \"../lib\" }"),
        "dep added: {manifest}"
    );
    assert!(app.join("noeta.lock").is_file(), "lock written");

    // The added dependency actually resolves and runs.
    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout("42\n");
}

#[test]
fn noeta_update_rewrites_the_lock() {
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_update");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let dep_repo = base.join("uplib_repo");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&dep_repo).unwrap();
    git_in(&["init", "-q"], &dep_repo);
    std::fs::write(
        dep_repo.join("noeta.toml"),
        "[package]\nname = \"acme/up\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dep_repo.join("m.noe"),
        "namespace up.core;\npub fn v(): int { return 7; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &dep_repo);
    git_in(&["commit", "-q", "-m", "r"], &dep_repo);
    git_in(&["tag", "v1.0.0"], &dep_repo);
    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nu = {{ git = \"{}\", tag = \"v1.0.0\" }}\n",
            dep_repo.display()
        ),
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use u.core.v;\necho v();\n").unwrap();

    lang()
        .current_dir(&app)
        .arg("update")
        .assert()
        .success()
        .stdout(predicate::str::contains("updated noeta.lock"));
    assert!(app.join("noeta.lock").is_file());
}

#[test]
fn a_git_dependency_is_pinned_and_reproduces_offline() {
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_gitdep_lock");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let dep_repo = base.join("pinlib_repo");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&dep_repo).unwrap();

    git_in(&["init", "-q"], &dep_repo);
    std::fs::write(
        dep_repo.join("noeta.toml"),
        "[package]\nname = \"acme/pinned\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dep_repo.join("m.noe"),
        "namespace pinned.core;\npub fn val(): string { return \"pinned offline value\"; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &dep_repo);
    git_in(&["commit", "-q", "-m", "release"], &dep_repo);
    git_in(&["tag", "v1.0.0"], &dep_repo);

    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\np = {{ git = \"{}\", tag = \"v1.0.0\" }}\n",
            dep_repo.display()
        ),
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use p.core.val;\necho val();\n").unwrap();

    // First run: resolves, fetches, and writes the lock.
    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("pinned offline value"));

    // The lock pins the git source + commit SHA.
    let lock = std::fs::read_to_string(app.join("noeta.lock")).expect("lock written");
    assert!(
        lock.contains("acme/pinned"),
        "lock names the package: {lock}"
    );
    assert!(
        lock.contains("source = \"git\""),
        "lock records the git source: {lock}"
    );
    assert!(lock.contains("sha = "), "lock pins a commit SHA: {lock}");

    // Delete the remote repo entirely; the pinned tree already lives in the store, so a second run
    // reproduces with no network access at all (offline).
    std::fs::remove_dir_all(&dep_repo).unwrap();
    lang()
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("pinned offline value"));
}
