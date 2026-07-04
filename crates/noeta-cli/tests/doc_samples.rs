//! Documentation quality gate: every ```` ```lang ```` code sample in the `docs/` wiki must compile
//! and run cleanly through the `lang` binary. This keeps the docs honest — a sample that stops
//! working (a renamed method, a changed diagnostic, a dropped feature) fails CI instead of quietly
//! misleading readers.
//!
//! ## The fence convention
//!
//! A fenced block's info string selects how it is checked:
//!
//! * ```` ```lang ```` — a complete program; it **must** run to a zero exit (`lang run` succeeds).
//! * ```` ```lang ignore ```` — an illustrative fragment (references a type defined elsewhere on the
//!   page, or shows declaration syntax in isolation). Not executed, exactly like a Rust `ignore`
//!   doctest. Keep these to a minimum; prefer a self-contained runnable block.
//! * ```` ```lang error ```` — a sample that is *meant* to fail (an error demo); it must exit non-zero.
//!
//! ```` ```console ````, ```` ```toml ````, and other languages are ignored entirely.
//!
//! When you add or edit a sample, run `cargo test -p lang-cli --test doc_samples`. If a genuinely
//! complete example fails, fix the example (or the code it documents); only tag `ignore` when the
//! block truly cannot stand alone.

use std::path::PathBuf;

use assert_cmd::Command;

fn docs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// One extracted fenced `lang` sample.
struct Sample {
    file: String,
    /// 1-based line of the opening fence, for a clickable failure location.
    line: usize,
    tag: String,
    code: String,
}

/// Pull every ```` ```lang[ tag] ```` block out of a markdown source.
fn extract(file: &str, text: &str) -> Vec<Sample> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if let Some(rest) = trimmed.strip_prefix("```lang") {
            // A real `lang` fence: nothing after `lang` (untagged) or a whitespace-separated tag.
            // Guards against `lang`-prefixed info strings like ```` ```language ````.
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
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
    let dir = std::env::temp_dir().join(format!("lang_doc_sample_{idx}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create sample dir");
    let path = dir.join("main.noe");
    std::fs::write(&path, &sample.code).expect("write sample");

    let output = Command::cargo_bin("noeta")
        .expect("the `lang` binary builds")
        .current_dir(&dir)
        .arg("run")
        .arg(&path)
        .output()
        .expect("spawn lang");
    let _ = std::fs::remove_dir_all(&dir);

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
        let wanted = if expect_error { "fail" } else { "run cleanly" };
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

#[test]
fn doc_samples_compile_and_run() {
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
        "found no `lang` code samples in {dir:?}"
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
