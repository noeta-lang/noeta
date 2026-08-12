//! Documentation quality gate: every ```` ```noeta ```` code sample in the `docs/` wiki must compile
//! and run cleanly through the `noeta` binary. This keeps the docs honest — a sample that stops
//! working (a renamed method, a changed diagnostic, a dropped feature) fails CI instead of quietly
//! misleading readers.
//!
//! ## The fence convention
//!
//! A fenced block's info string selects how it is checked:
//!
//! * ```` ```noeta ```` — a complete program; it **must** run to a zero exit (`noeta run` succeeds).
//! * ```` ```noeta check ```` — a sample the gate cannot *execute* (a server binds a real socket
//!   and runs until Ctrl-C) or a page fragment referencing names defined elsewhere, but that
//!   **must** still type-check. Checked in **session mode** (like a REPL entry): a genuine type
//!   error — a renamed API, a wrong operand, bad arity — fails CI, while an unresolved *external*
//!   name is tolerated (F1), so a snippet need not restate its whole context.
//! * ```` ```noeta ignore ```` — an illustrative fragment (references a type defined elsewhere on the
//!   page, or shows declaration syntax in isolation). Not verified at all, exactly like a Rust
//!   `ignore` doctest. Keep these to a minimum; prefer a runnable block, or `check` if it at least
//!   stands alone.
//! * ```` ```noeta error ```` — a sample that is *meant* to fail (an error demo); it must exit non-zero.
//!
//! ```` ```console ```` and other languages are ignored entirely.
//!
//! ## `toml` blocks
//!
//! A ```` ```toml ```` block is checked too, by a separate test: it must parse as a manifest this
//! toolchain accepts. The code samples were gated and the *configuration* was not, which is the
//! half a reader copies verbatim into `noeta.toml` — and the parser is strict enough to be worth
//! asking (`reject_unknown_tables` refuses a table the schema does not define, and a provider must
//! name a real dependency). `[package]` is optional, so a page's fragment — `[directives]` alone,
//! a lone `[targets.dev]` — is a valid manifest and is checked as written.
//!
//! Tag a block ```` ```toml ignore ```` when it is deliberately **not** a `noeta.toml`: a
//! `Cargo.toml` for a native package, a Spin manifest, a `noeta.lock` excerpt.
//!
//! When you add or edit a sample, run `cargo test -p noeta-cli --test doc_samples`. If a genuinely
//! complete example fails, fix the example (or the code it documents); only tag `ignore` when the
//! block truly cannot stand alone.

use std::path::PathBuf;

use assert_cmd::Command;

fn docs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// One extracted fenced `noeta` sample.
struct Sample {
    file: String,
    /// 1-based line of the opening fence, for a clickable failure location.
    line: usize,
    tag: String,
    code: String,
}

/// Pull every ```` ```noeta[ tag] ```` block out of a markdown source.
fn extract(file: &str, text: &str) -> Vec<Sample> {
    extract_fenced(file, text, "noeta")
}

/// Pull every ```` ```<lang>[ tag] ```` block out of a markdown source. Shared by the `noeta`
/// gate below and the `toml` one, so the two cannot disagree about what a fenced block is.
fn extract_fenced(file: &str, text: &str, lang: &str) -> Vec<Sample> {
    let fence = format!("```{lang}");
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if let Some(rest) = trimmed.strip_prefix(fence.as_str())
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let tag = rest.trim().to_string();
            let open = i + 1;
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                body.push(lines[i]);
                i += 1;
            }
            out.push(Sample {
                file: file.to_string(),
                line: open,
                tag,
                code: body.join("\n"),
            });
        }
        i += 1;
    }
    out
}

/// Run one sample in its own isolated directory (the module loader scans sibling `.noe` files, and
/// an `fs`-writing sample must not touch the repo), returning `Err(message)` on the wrong outcome.
fn check(sample: &Sample, idx: usize) -> Result<(), String> {
    if sample.tag == "ignore" {
        return Ok(());
    }

    // This gate runs with **zero extensions beyond std** — the `para` packages live in their own
    // repos (github.com/noeta-lang/para-*) and their docs (with `para.*` samples) moved with them,
    // gated by each package's own CI.
    // A `check`-tagged sample type-checks without executing (it would bind sockets / never exit,
    // or it is a page fragment referencing names defined elsewhere). A doc fragment is a
    // REPL-like snippet, so it is checked in **session mode** — parse + type-check, with unknown
    // external names deferred (F1) rather than a hard error. A genuine type error (arity, a
    // wrong operand) is still caught. `run`/untagged samples must be complete programs and run.
    if sample.tag == "check" {
        let source =
            noeta_span::Source::new(noeta_span::SourceId::FIRST, "sample.noe", &sample.code);
        let lexed = noeta_lexer::lex(&source);
        let parsed = noeta_parser::parse(&source, &lexed.tokens);
        let mut diags = lexed.diagnostics.clone();
        diags.extend(parsed.diagnostics);
        diags.extend(noeta_check::SessionChecker::new().check_entry(&parsed.program));
        return if diags.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{}:{} (tag \"check\") was expected to type-check cleanly but did not — {}",
                sample.file, sample.line, diags[0].message
            ))
        };
    }

    let dir = noeta_test_temp::TempDir::new(&format!("doc-sample-{idx}"));
    let path = dir.join("main.noe");
    std::fs::write(&path, &sample.code).expect("write sample");

    let verb = "run";
    let output = Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        // Hermetic startup cache — don't touch the developer's real ~/.cache/noeta during tests.
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .current_dir(&dir)
        .arg(verb)
        .arg(&path)
        .output()
        .expect("spawn noeta");

    let ran = output.status.success();
    let expect_error = sample.tag == "error";
    if ran == expect_error {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first = stderr
            .lines()
            .chain(stdout.lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("<no output>");
        let wanted = if expect_error {
            "fail"
        } else if verb == "check" {
            "type-check cleanly"
        } else {
            "run cleanly"
        };
        return Err(format!(
            "{}:{} (tag {:?}) was expected to {} but did not — {}",
            sample.file,
            sample.line,
            if sample.tag.is_empty() {
                "<none>"
            } else {
                &sample.tag
            },
            wanted,
            first.trim()
        ));
    }
    Ok(())
}

/// Every ```` ```toml ```` block in the wiki must be a manifest the toolchain accepts.
///
/// The `noeta` gate proves the *code* in the docs still works and said nothing about the
/// *configuration*, which is the half a reader copies verbatim into `noeta.toml`. A stale
/// `[targets]` shape or a table that no longer exists reads exactly like a working one, and the
/// manifest parser is strict enough to catch both: `reject_unknown_tables` refuses a table the
/// schema does not define, and `[package]` is optional, so a page's fragment — `[directives]` on
/// its own, a lone `[targets.dev]` — is a valid manifest and parses as written.
///
/// Blocks that are not Noeta manifests (a `Cargo.toml` for a native package, a Spin manifest)
/// carry ```` ```toml ignore ````, the same escape the `noeta` fences use.
#[test]
fn doc_toml_blocks_are_valid_manifests() {
    let dir = docs_dir();
    let mut blocks = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read docs/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read markdown");
        blocks.extend(extract_fenced(&name, &text, "toml"));
    }
    assert!(!blocks.is_empty(), "found no `toml` blocks in {dir:?}");

    let mut failures = Vec::new();
    let (mut checked, mut ignored) = (0, 0);
    for block in &blocks {
        if block.tag == "ignore" {
            ignored += 1;
            continue;
        }
        if !block.tag.is_empty() {
            failures.push(format!(
                "{}:{} — unknown toml fence tag {:?} (only `ignore` is defined)",
                block.file, block.line, block.tag
            ));
            continue;
        }
        checked += 1;
        if let Err(err) = noeta_pm::manifest::Manifest::parse(&block.code) {
            failures.push(format!(
                "{}:{} is not a manifest this toolchain accepts — {err}\n    tag it \
                 ```toml ignore` if it is deliberately not a `noeta.toml`",
                block.file, block.line
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} toml blocks are not valid manifests:\n\n{}\n",
        failures.len(),
        blocks.len(),
        failures.join("\n")
    );
    eprintln!("doc toml: {checked} manifests checked, {ignored} ignored");
}

#[test]
fn doc_samples_compile_and_run() {
    // Seed the process registry with the std set (and nothing else): the in-process session checks
    // below (`check`-tagged samples) need a registry, and referencing noeta-stdlib here is what
    // links its load-time default provider into this test binary at all. The gate runs with ZERO
    // extensions beyond std — the `para` packages' docs are gated in their own repos.
    noeta_stdlib::registry::default_seeded();

    let dir = docs_dir();
    let mut samples = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read docs/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read markdown");
        samples.extend(extract(&name, &text));
    }
    assert!(
        !samples.is_empty(),
        "found no `noeta` code samples in {dir:?}"
    );

    let mut failures = Vec::new();
    let (mut run, mut ignored) = (0, 0);
    for (idx, sample) in samples.iter().enumerate() {
        if sample.tag == "ignore" {
            ignored += 1;
        } else {
            run += 1;
        }
        if let Err(msg) = check(sample, idx) {
            failures.push(msg);
        }
    }

    eprintln!(
        "doc samples: {} checked, {ignored} ignored, {} total across the wiki",
        run,
        samples.len()
    );
    assert!(
        failures.is_empty(),
        "{} documentation sample(s) did not behave as tagged:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
