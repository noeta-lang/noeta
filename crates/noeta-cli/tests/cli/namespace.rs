//! Namespace-protection arc: reserved scopes, claim flow (OIDC/OAuth/domain), provenance,
//! transparency log, advisory feed, publish cooldown — against a mock registry.

use crate::support::*;

// ===== namespace-protection arc tests (merged) =====

/// A tiny in-process HTTP/1.1 server for the CLI e2e: `handler(method, path, body) -> (status, json)`.
/// Handles connections sequentially on a background thread; returns the base URL.
fn mock_http(handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).unwrap();
            }
            let (status, json) = handler(&method, &path, &String::from_utf8_lossy(&body));
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                json.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[test]
fn noeta_add_derives_the_import_root() {
    // namespace-protection #3: with no key given, `add` derives the import root from the dependency's
    // own `[package]` name — and because the derived key then *matches* the package's root, there is
    // no mismatch warning.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_add_derive");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("widgets");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(lib.join("m.noe"), "pub fn v(): int { return 1; }\n").unwrap();

    // No positional key — derived from `acme/widgets` → `widgets`.
    lang()
        .current_dir(&app)
        .args(["add", "--path", "../widgets"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("using import root `widgets`")
                .and(predicate::str::contains("added `widgets`")),
        )
        // Derived key == the package root, so there is no rename warning.
        .stderr(predicate::str::contains("module root is").not());

    let manifest = std::fs::read_to_string(app.join("noeta.toml")).unwrap();
    assert!(
        manifest.contains("widgets = { path = \"../widgets\" }"),
        "derived key used as the dep key: {manifest}"
    );
}

#[test]
fn binding_a_package_under_its_own_scope_is_not_a_rename() {
    // A package keyed by its SCOPE (`para/aether` under `para`) is the spelling the package guide
    // teaches, the spelling the package's own modules declare (`namespace para.aether`), and the
    // one that lets two packages of a scope share an import root. Warning about it told an author
    // their correct code was surprising — a warning on the happy path is a warning people learn to
    // ignore. A genuine rename (a key that is neither the package root nor its scope) still warns.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_add_scope_key");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("widgets");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(lib.join("m.noe"), "pub fn v(): int { return 1; }\n").unwrap();

    // Keyed by the scope — quiet.
    lang()
        .current_dir(&app)
        .args(["add", "acme", "--path", "../widgets"])
        .assert()
        .success()
        .stderr(predicate::str::contains("module root is").not());

    // Keyed by neither the root nor the scope — a real rename, and still reported. A second,
    // distinct package: binding the SAME one twice resolves to one instance, so it would not
    // exercise the warning at all.
    let other = base.join("gizmos");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("noeta.toml"),
        "[package]\nname = \"acme/gizmos\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(other.join("m.noe"), "pub fn v(): int { return 2; }\n").unwrap();
    lang()
        .current_dir(&app)
        .args(["add", "gadgets", "--path", "../gizmos"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "binds a package whose own module root is `gizmos`",
        ));
}

#[test]
fn noeta_add_writes_and_verifies_a_package_claim_on_a_path_dependency() {
    // `--package` used to be refused outright with `--path`, so `add` could not produce the entry
    // the manifest reference (and every first-party scope-array member) is written with. It writes
    // it now — and, because the identity on a path dependency is a claim rather than a selector,
    // checks it against the target's own `[package] name` BEFORE touching the manifest.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_add_path_package");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("widgets");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    let original = "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n";
    std::fs::write(app.join("noeta.toml"), original).unwrap();
    std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/widgets\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(lib.join("m.noe"), "pub fn v(): int { return 1; }\n").unwrap();

    // A claim that does not match the tree: refused, and the manifest is left exactly as it was.
    lang()
        .current_dir(&app)
        .args([
            "add",
            "acme",
            "--path",
            "../widgets",
            "--package",
            "totally/wrong",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("totally/wrong").and(predicate::str::contains("acme/widgets")),
        );
    assert_eq!(
        std::fs::read_to_string(app.join("noeta.toml")).unwrap(),
        original,
        "a refused claim must not modify the manifest"
    );

    // The true claim: written into the entry, and the graph resolves with it.
    lang()
        .current_dir(&app)
        .args([
            "add",
            "acme",
            "--path",
            "../widgets",
            "--package",
            "acme/widgets",
        ])
        .assert()
        .success();
    let manifest = std::fs::read_to_string(app.join("noeta.toml")).unwrap();
    assert!(
        manifest.contains("acme = { path = \"../widgets\", package = \"acme/widgets\" }"),
        "the claim is written into the entry: {manifest}"
    );
    // And it round-trips: the manifest `add` just wrote is one `check` accepts.
    lang()
        .current_dir(&app)
        .args(["check", "."])
        .assert()
        .success();
}

#[test]
fn noeta_add_refuses_a_builtin_import_root() {
    // namespace-protection #2/#3: binding a dependency under `std` would shadow the compiler's own
    // `use std.…` namespace — refused before the manifest is touched.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_add_reserved");
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("lib");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    let manifest = "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n";
    std::fs::write(app.join("noeta.toml"), manifest).unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(lib.join("m.noe"), "").unwrap();

    lang()
        .current_dir(&app)
        .args(["add", "std", "--path", "../lib"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("built-in import root"));
    // The manifest is untouched — the guard runs before the edit.
    assert_eq!(
        std::fs::read_to_string(app.join("noeta.toml")).unwrap(),
        manifest,
        "a refused add must not modify the manifest"
    );
}

#[test]
fn noeta_add_warns_when_a_release_introduces_a_new_committer() {
    // namespace-protection committer signal: adding a git dependency whose release commit was authored
    // by someone with no prior history in an established repo surfaces a soft warning (with a link to
    // the commit), so the user can review before trusting it. The add itself still succeeds.
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_committer_signal");
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("greetlib");
    let app = base.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&app).unwrap();

    let commit = |file: &str, contents: &str, name: &str, email: &str| {
        std::fs::write(repo.join(file), contents).unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", file]] {
            let ok = std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_COMMITTER_EMAIL", email)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        }
    };
    git_in(&["init", "-q"], &repo);
    // Alice ships v0.9.0; the v1.0.0 release *range* (v0.9.0..v1.0.0) then contains a first-time
    // committer, Bob. The signal must span that range, not just look at the tip commit.
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greetlib\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    commit(
        "g.noe",
        "pub fn hi(): int { return 1; }\n",
        "Alice Maintainer",
        "alice@example.com",
    );
    git_in(&["tag", "v0.9.0"], &repo);
    commit(
        "CHANGELOG.md",
        "# 1.0.0\n",
        "Bob Newcomer",
        "bob@example.com",
    );
    git_in(&["tag", "v1.0.0"], &repo);

    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "echo 1;\n").unwrap();

    lang()
        .current_dir(&app)
        .args([
            "add",
            "greet",
            "--git",
            repo.to_str().unwrap(),
            "--tag",
            "v1.0.0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("added `greet`"))
        .stderr(
            predicate::str::contains("committer(s) new to this repo")
                .and(predicate::str::contains("Bob Newcomer")),
        );
}

#[test]
fn noeta_audit_flags_a_dependency_with_a_known_advisory() {
    // End-to-end (namespace-protection #1, advisory feed): a mock registry serves a *signed* advisory
    // for the resolved dependency `acme/greet@1.0.0`. `noeta audit` fetches the feed, verifies every
    // signature against the served (then pinned) advisory key, matches the version against the
    // advisory's range, prints the hit, and exits non-zero so CI catches it.
    use ed25519_dalek::{Signer, SigningKey};
    use noeta_pm::advisory::{self, Advisory};

    let to_hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let sk = SigningKey::from_bytes(&[3u8; 32]);
    let pub_hex = to_hex(sk.verifying_key().as_bytes());

    // Build the advisory, sign its canonical bytes exactly as the registry would.
    let mut adv = Advisory {
        id: "NOETA-2026-0007".to_string(),
        package: "acme/greet".to_string(),
        ranges: ">=1.0.0, <2.0.0".to_string(),
        patched: Some("2.0.0".to_string()),
        severity: "high".to_string(),
        summary: "greeting injection via unescaped punctuation".to_string(),
        details: String::new(),
        url: "https://example.com/advisories/7".to_string(),
        withdrawn: false,
        seq: 0,
        signature: String::new(),
        log_index: Some(0),
        tier: advisory::AdvisoryTier::Operator,
        bundle: None,
        upstream_id: None,
        upstream_url: None,
        cvss: None,
    };
    adv.signature = to_hex(&sk.sign(&adv.canonical_bytes()).to_bytes());
    let digest = advisory::feed_digest(std::slice::from_ref(&adv));
    let head_sig = to_hex(&sk.sign(&advisory::feed_head_bytes(1, &digest)).to_bytes());

    // Bind the advisory into a single-leaf transparency log so the audit can verify its inclusion. The
    // leaf record is the advisory's canonical bytes; a one-leaf tree's root *is* that leaf.
    let record = String::from_utf8(adv.canonical_bytes()).unwrap();
    let leaf = noeta_pm::transparency::leaf_hash(record.as_bytes());
    let root_hex = to_hex(&leaf);
    let log_sk = SigningKey::from_bytes(&[5u8; 32]);
    let log_pub_hex = to_hex(log_sk.verifying_key().as_bytes());
    let cp_msg = format!("noeta-log-checkpoint-v1\n1\n{root_hex}\n");
    let log_cp_sig = to_hex(&log_sk.sign(cp_msg.as_bytes()).to_bytes());
    let record_json = record.replace('\n', "\\n"); // canonical bytes contain only newlines to escape

    let advisory_json = format!(
        r#"{{"id":"{}","package":"{}","ranges":"{}","patched":"2.0.0","severity":"{}","summary":"{}","details":"","url":"{}","withdrawn":false,"seq":0,"signature":"{}","log_index":0}}"#,
        adv.id, adv.package, adv.ranges, adv.severity, adv.summary, adv.url, adv.signature,
    );
    let feed = format!(r#"{{"advisories":[{advisory_json}]}}"#);
    let key_json = format!(r#"{{"public_key":"{pub_hex}"}}"#);
    let checkpoint = format!(r#"{{"count":1,"digest":"{digest}","signature":"{head_sig}"}}"#);
    let log_key_json = format!(r#"{{"public_key":"{log_pub_hex}"}}"#);
    let log_cp_json =
        format!(r#"{{"tree_size":1,"root_hash":"{root_hex}","signature":"{log_cp_sig}"}}"#);
    let log_incl_json = format!(
        r#"{{"index":0,"tree_size":1,"root_hash":"{root_hex}","record":"{record_json}","proof":[]}}"#
    );

    let base = mock_http(move |_method, path, _body| match path {
        "/v1/advisories" => (200, feed.clone()),
        "/v1/advisories/key" => (200, key_json.clone()),
        "/v1/advisories/checkpoint" => (200, checkpoint.clone()),
        "/v1/log/key" => (200, log_key_json.clone()),
        "/v1/log/checkpoint" => (200, log_cp_json.clone()),
        p if p.starts_with("/v1/log/advisory/") => (200, log_incl_json.clone()),
        // Path deps aren't logged, so the release transparency section makes no proof calls; anything
        // else the audit probes is a 404 it tolerates.
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    });

    let entry = path_dep_project("pm_audit_advisory");
    let app_dir = entry.parent().unwrap();
    // Opt this project into failing on an operator-tier advisory (advisory-intake arc, tier 5): the
    // default is warn, so a CI gate declares `[trust].advisories = "fail"` to break the build on a hit.
    std::fs::write(
        app_dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nhi = { path = \"../greetlib\" }\n\
         [trust]\nadvisories = \"fail\"\n",
    )
    .unwrap();
    lang()
        .arg("audit")
        .arg(app_dir)
        .env("NOETA_REGISTRY_URL", &base)
        .assert()
        .failure()
        .stdout(predicate::str::contains("Security advisories:"))
        .stdout(predicate::str::contains("NOETA-2026-0007"))
        .stdout(predicate::str::contains("greeting injection"))
        // The intake tier is shown per advisory (advisory-intake arc, tier 5).
        .stdout(predicate::str::contains("[operator/high]"))
        // The advisory is verified as included in the transparency log (log-binding).
        .stdout(predicate::str::contains("included in the transparency log"));

    // The advisory-feed head is now pinned in the lockfile (trust-on-first-use).
    let lock = std::fs::read_to_string(app_dir.join("noeta.lock")).unwrap();
    assert!(
        lock.contains("[advisory]"),
        "lock should pin the advisory head:\n{lock}"
    );
    assert!(lock.contains(&pub_hex), "lock should pin the advisory key");
}

#[test]
fn noeta_audit_fails_loudly_when_the_advisory_feed_does_not_verify() {
    // audit row 4a: a feed that does not VERIFY used to print `not checked — {err}` on *stdout* and
    // exit 0 — so an advisory-format drift between the client and the registry (which makes every
    // per-advisory signature fail) looked exactly like a clean audit in CI, with the graph never
    // checked against the feed at all. It must now be reported on stderr and exit non-zero.
    //
    // The fixture is the drift itself: the advisory is signed, then its `summary` is altered on the
    // wire. The head is signed over the *served* advisory, so the feed digest and head both verify —
    // the only thing that fails is the per-advisory signature over canonical bytes, exactly the shape
    // a canonical-bytes divergence between the Rust and TypeScript halves produces.
    use ed25519_dalek::{Signer, SigningKey};
    use noeta_pm::advisory::{self, Advisory};

    let to_hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let sk = SigningKey::from_bytes(&[3u8; 32]);
    let pub_hex = to_hex(sk.verifying_key().as_bytes());

    let mut adv = Advisory {
        id: "NOETA-2026-0009".to_string(),
        package: "acme/greet".to_string(),
        ranges: ">=1.0.0, <2.0.0".to_string(),
        patched: Some("2.0.0".to_string()),
        severity: "critical".to_string(),
        summary: "remote code execution in the greeting parser".to_string(),
        details: String::new(),
        url: String::new(),
        withdrawn: false,
        seq: 0,
        signature: String::new(),
        log_index: None,
        tier: advisory::AdvisoryTier::Operator,
        bundle: None,
        upstream_id: None,
        upstream_url: None,
        cvss: None,
    };
    // Sign the real advisory, then serve a DIFFERENT summary under that signature.
    adv.signature = to_hex(&sk.sign(&adv.canonical_bytes()).to_bytes());
    let served_summary = "remote code execution in the greeting parser (tampered)";
    let tampered = Advisory {
        summary: served_summary.to_string(),
        ..adv.clone()
    };
    // The signed head attests to exactly what is served, so the head and digest checks both pass.
    let digest = advisory::feed_digest(std::slice::from_ref(&tampered));
    let head_sig = to_hex(&sk.sign(&advisory::feed_head_bytes(1, &digest)).to_bytes());

    let advisory_json = format!(
        r#"{{"id":"{}","package":"{}","ranges":"{}","patched":"2.0.0","severity":"{}","summary":"{served_summary}","details":"","url":"","withdrawn":false,"seq":0,"signature":"{}"}}"#,
        adv.id, adv.package, adv.ranges, adv.severity, adv.signature,
    );
    let feed = format!(r#"{{"advisories":[{advisory_json}]}}"#);
    let key_json = format!(r#"{{"public_key":"{pub_hex}"}}"#);
    let checkpoint = format!(r#"{{"count":1,"digest":"{digest}","signature":"{head_sig}"}}"#);

    let base = mock_http(move |_method, path, _body| match path {
        "/v1/advisories" => (200, feed.clone()),
        "/v1/advisories/key" => (200, key_json.clone()),
        "/v1/advisories/checkpoint" => (200, checkpoint.clone()),
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    });

    let entry = path_dep_project("pm_audit_feed_tampered");
    let app_dir = entry.parent().unwrap();
    let assert = lang()
        .arg("audit")
        .arg(app_dir)
        .env("NOETA_REGISTRY_URL", &base)
        .assert()
        // Non-zero, and specifically the failure exit — not a usage/setup 2.
        .code(1);
    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        stderr.contains("the advisory feed did not verify"),
        "the verification failure belongs on stderr:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("NOT checked against the advisory feed"),
        "the audit must say the graph went unchecked:\nstderr:\n{stderr}"
    );
    // The old silent shape must be gone: no lowercase stdout note, and above all no claim of a clean
    // advisory result for a feed that was never read.
    assert!(
        !stdout.contains("not checked — "),
        "the failure must not be a grey stdout note:\n{stdout}"
    );
    assert!(
        !stdout.contains("no advisories affect"),
        "a feed that did not verify must never read as clean:\n{stdout}"
    );
    // And nothing from an unverified feed is pinned as trusted.
    let lock = std::fs::read_to_string(app_dir.join("noeta.lock")).unwrap_or_default();
    assert!(
        !lock.contains("[advisory]"),
        "an unverified feed head must not be pinned:\n{lock}"
    );
}

#[test]
fn noeta_audit_stays_clean_when_the_advisory_feed_verifies_or_is_simply_absent() {
    // The other half of audit row 4a: making a verification failure loud must not make the *routine*
    // outcomes loud. Three runs against a hosted registry, all exit 0 — a feed that verifies and
    // matches nothing, a registry that runs no advisory feed at all, and a registry that cannot be
    // reached (offline is evidence of nothing; this section is best-effort by contract).
    use ed25519_dalek::{Signer, SigningKey};
    use noeta_pm::advisory::{self, Advisory};

    let to_hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let pub_hex = to_hex(sk.verifying_key().as_bytes());

    // A real, correctly signed advisory — against a package that is not in this graph.
    let mut adv = Advisory {
        id: "NOETA-2026-0011".to_string(),
        package: "other/unrelated".to_string(),
        ranges: ">=1.0.0".to_string(),
        patched: None,
        severity: "medium".to_string(),
        summary: "unrelated package, unrelated problem".to_string(),
        details: String::new(),
        url: String::new(),
        withdrawn: false,
        seq: 0,
        signature: String::new(),
        log_index: None,
        tier: advisory::AdvisoryTier::Operator,
        bundle: None,
        upstream_id: None,
        upstream_url: None,
        cvss: None,
    };
    adv.signature = to_hex(&sk.sign(&adv.canonical_bytes()).to_bytes());
    let digest = advisory::feed_digest(std::slice::from_ref(&adv));
    let head_sig = to_hex(&sk.sign(&advisory::feed_head_bytes(1, &digest)).to_bytes());
    let advisory_json = format!(
        r#"{{"id":"{}","package":"{}","ranges":"{}","severity":"{}","summary":"{}","details":"","url":"","withdrawn":false,"seq":0,"signature":"{}"}}"#,
        adv.id, adv.package, adv.ranges, adv.severity, adv.summary, adv.signature,
    );
    let feed = format!(r#"{{"advisories":[{advisory_json}]}}"#);
    let key_json = format!(r#"{{"public_key":"{pub_hex}"}}"#);
    let checkpoint = format!(r#"{{"count":1,"digest":"{digest}","signature":"{head_sig}"}}"#);

    let base = mock_http(move |_method, path, _body| match path {
        "/v1/advisories" => (200, feed.clone()),
        "/v1/advisories/key" => (200, key_json.clone()),
        "/v1/advisories/checkpoint" => (200, checkpoint.clone()),
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    });

    let entry = path_dep_project("pm_audit_feed_clean");
    let app_dir = entry.parent().unwrap();
    lang()
        .arg("audit")
        .arg(app_dir)
        .env("NOETA_REGISTRY_URL", &base)
        .assert()
        .success()
        .stdout(predicate::str::contains("no advisories affect"));
    // The verified head is pinned, so the clean run really did read and verify the feed.
    let lock = std::fs::read_to_string(app_dir.join("noeta.lock")).unwrap();
    assert!(
        lock.contains("[advisory]"),
        "clean run pins the head:\n{lock}"
    );

    // A registry with no advisory feed at all (404 on the feed key): a note, not a failure — a
    // self-hosted or private registry that runs no feed must not fail everyone's CI.
    let bare = mock_http(|_method, _path, _body| (404, r#"{"error":"not found"}"#.to_string()));
    let entry = path_dep_project("pm_audit_feed_absent");
    lang()
        .arg("audit")
        .arg(entry.parent().unwrap())
        .env("NOETA_REGISTRY_URL", &bare)
        .assert()
        .success()
        .stdout(predicate::str::contains("serves no advisory feed"));

    // An unreachable registry: the fetch fails with a transient error and stays a note.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l); // nothing listens on this port
        format!("http://{addr}")
    };
    let entry = path_dep_project("pm_audit_feed_offline");
    lang()
        .arg("audit")
        .arg(entry.parent().unwrap())
        .env("NOETA_REGISTRY_URL", &dead)
        .assert()
        .success()
        .stdout(predicate::str::contains("not checked — "));
}

#[test]
fn noeta_claim_by_domain_posts_the_domain_proof() {
    // namespace-protection #1 (domain proof): `noeta claim <scope> --domain <domain>` skips the GitHub
    // path and posts a `domain` proof to the registry (which then verifies the well-known file server
    // side). The mock registry captures the body and confirms the proof shape.
    let (tx, rx) = std::sync::mpsc::channel();
    let base = mock_http(move |method, path, body| {
        tx.send((method.to_string(), path.to_string(), body.to_string()))
            .unwrap();
        (
            201,
            r#"{"status":"scope claimed","owner":"acme.dev"}"#.to_string(),
        )
    });

    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .args([
            "claim",
            "acme",
            "--domain",
            "acme.dev",
            "--token",
            "domain-publish-token-123456",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("scope claimed"));

    let (method, path, body) = rx.recv().unwrap();
    assert_eq!(method, "POST");
    assert_eq!(path, "/v1/scopes/claim");
    assert!(
        body.contains(r#""domain":"acme.dev""#),
        "body carries the domain proof: {body}"
    );
    assert!(body.contains(r#""scope":"acme""#), "body: {body}");
    // The GitHub proofs must not appear on the domain path.
    assert!(
        !body.contains("github_token") && !body.contains("\"oidc\""),
        "body: {body}"
    );
}

#[test]
fn noeta_claim_device_flow_works_without_a_configured_client_id() {
    // Outside GitHub Actions, `noeta claim` falls back to the device flow using the BUILT-IN
    // client id of the hosted registry's OAuth app — no NOETA_GITHUB_CLIENT_ID required. Pin the
    // OAuth endpoint to an unroutable host so the test proves the flow is *attempted* (the
    // failure names the device-code request, not a missing client id) while staying hermetic.
    lang()
        .env("NOETA_REGISTRY_URL", "https://registry.invalid")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .env_remove("NOETA_GITHUB_CLIENT_ID")
        .env("NOETA_GITHUB_OAUTH_URL", "http://127.0.0.1:1")
        .args(["claim", "acme"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("device")
                .and(predicate::str::contains("NOETA_GITHUB_CLIENT_ID").not()),
        );
}

#[test]
fn noeta_advisory_report_posts_a_public_report() {
    // advisory-intake arc, tier 4: `noeta advisory report <package> <summary>` POSTs an unauthenticated
    // report to the registry. The mock captures the request and confirms its shape.
    let (tx, rx) = std::sync::mpsc::channel();
    let base = mock_http(move |method, path, body| {
        tx.send((method.to_string(), path.to_string(), body.to_string()))
            .unwrap();
        (
            201,
            r#"{"status":"report filed","id":"rep-abc-123","note":"queued for triage"}"#
                .to_string(),
        )
    });

    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .args([
            "advisory",
            "report",
            "acme/imgfx",
            "looks like it leaks memory",
            "--details",
            "repro attached",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("report filed"))
        .stdout(predicate::str::contains("rep-abc-123"));

    let (method, path, body) = rx.recv().unwrap();
    assert_eq!(method, "POST");
    assert_eq!(path, "/v1/reports");
    assert!(body.contains(r#""package":"acme/imgfx""#), "body: {body}");
    assert!(body.contains("looks like it leaks memory"), "body: {body}");
    assert!(body.contains("repro attached"), "body: {body}");
}

#[test]
fn noeta_watch_scope_pins_a_baseline_and_reports_clean() {
    // advisory-intake arc, tier 6: `noeta watch-scope <scope>` verifies the advisory feed + transparency
    // log and pins a baseline on the first run. Here the mock serves a verifiable *empty* feed and log
    // (an empty tree's root and an empty feed's digest are both sha256 of nothing), so the first run
    // establishes the baseline and reports clean, writing a state file.
    use ed25519_dalek::{Signer, SigningKey};
    use noeta_pm::advisory;

    let to_hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let adv_sk = SigningKey::from_bytes(&[3u8; 32]);
    let adv_pub = to_hex(adv_sk.verifying_key().as_bytes());
    let log_sk = SigningKey::from_bytes(&[5u8; 32]);
    let log_pub = to_hex(log_sk.verifying_key().as_bytes());

    // An empty feed: digest over no advisories == sha256 of the empty string, which is also an empty
    // Merkle tree's root — so one value serves both the feed digest and the log root.
    let empty = advisory::feed_digest(&[]);
    let feed_head = format!("noeta-advisory-feed-v1\n0\n{empty}\n");
    let feed_sig = to_hex(&adv_sk.sign(feed_head.as_bytes()).to_bytes());
    let log_cp = format!("noeta-log-checkpoint-v1\n0\n{empty}\n");
    let log_sig = to_hex(&log_sk.sign(log_cp.as_bytes()).to_bytes());

    let feed = r#"{"advisories":[]}"#.to_string();
    let adv_key = format!(r#"{{"public_key":"{adv_pub}"}}"#);
    let checkpoint = format!(r#"{{"count":0,"digest":"{empty}","signature":"{feed_sig}"}}"#);
    let log_key = format!(r#"{{"public_key":"{log_pub}"}}"#);
    let log_checkpoint =
        format!(r#"{{"tree_size":0,"root_hash":"{empty}","signature":"{log_sig}"}}"#);

    let base = mock_http(move |_method, path, _body| match path {
        "/v1/advisories" => (200, feed.clone()),
        "/v1/advisories/key" => (200, adv_key.clone()),
        "/v1/advisories/checkpoint" => (200, checkpoint.clone()),
        "/v1/log/key" => (200, log_key.clone()),
        "/v1/log/checkpoint" => (200, log_checkpoint.clone()),
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    });

    let state = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("watch_acme.toml");
    let _ = std::fs::remove_file(&state);
    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .args(["watch-scope", "acme", "--state"])
        .arg(&state)
        .assert()
        .success()
        .stdout(predicate::str::contains("baseline pinned"))
        .stdout(predicate::str::contains(
            "no suppression or rewrite detected",
        ));
    // The baseline is now pinned in the state file for the next run to diff against.
    let pinned = std::fs::read_to_string(&state).unwrap();
    assert!(
        pinned.contains(&adv_pub),
        "state pins the advisory key:\n{pinned}"
    );
    assert!(
        pinned.contains("log_tree_size"),
        "state pins the log checkpoint:\n{pinned}"
    );
}

#[test]
fn noeta_advisory_reports_lists_the_operator_and_scope_queues() {
    // advisory-intake residual a: `noeta advisory reports` lists the promotable queue. Without `--scope`
    // it hits the operator queue (admin token); with it, the scope owner's own queue (scope token). The
    // mock captures the request path/auth so we confirm each identity is presented to the right endpoint.
    let (tx, rx) = std::sync::mpsc::channel();
    let base = mock_http(move |method, path, _body| {
        tx.send((method.to_string(), path.to_string())).unwrap();
        (
            200,
            r#"{"reports":[{"id":"rep-1","package":"acme/imgfx","ranges":">=1.0.0","summary":"leaks memory","status":"pending"}]}"#
                .to_string(),
        )
    });

    // Operator queue: admin token, no scope.
    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .env("NOETA_REGISTRY_ADMIN_TOKEN", "admin-secret")
        .args(["advisory", "reports"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rep-1"))
        .stdout(predicate::str::contains("acme/imgfx"))
        .stdout(predicate::str::contains("leaks memory"));
    let (m, p) = rx.recv().unwrap();
    assert_eq!(m, "GET");
    assert_eq!(p, "/v1/reports?status=pending");

    // Scope-owner queue: scope token, `--scope acme`.
    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .env("NOETA_REGISTRY_TOKEN", "acme-scope-token")
        .env_remove("NOETA_REGISTRY_ADMIN_TOKEN")
        .args(["advisory", "reports", "--scope", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rep-1"));
    let (m, p) = rx.recv().unwrap();
    assert_eq!(m, "GET");
    assert_eq!(p, "/v1/scopes/acme/reports?status=pending");
}

#[test]
fn noeta_advisory_promote_operator_prefills_from_the_report_and_posts() {
    // advisory-intake residual a: `noeta advisory promote --operator` fetches the report, prefills the
    // advisory from it (package/summary/ranges), and POSTs to /promote with the admin token and NO
    // bundle (an operator advisory). The mock serves the report then captures the promote body.
    let (tx, rx) = std::sync::mpsc::channel();
    let base = mock_http(move |method, path, body| {
        if method == "GET" {
            return (
                200,
                r#"{"report":{"id":"rep-9","package":"acme/imgfx","ranges":">=1.0.0, <1.3.0","summary":"confirmed leak","details":"repro attached","url":"https://ex.test/r","status":"pending"}}"#
                    .to_string(),
            );
        }
        tx.send((method.to_string(), path.to_string(), body.to_string()))
            .unwrap();
        (
            201,
            r#"{"status":"report promoted","advisory":"NOETA-2026-0100","tier":"operator"}"#
                .to_string(),
        )
    });

    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .env("NOETA_REGISTRY_ADMIN_TOKEN", "admin-secret")
        .args([
            "advisory",
            "promote",
            "rep-9",
            "--operator",
            "--id",
            "NOETA-2026-0100",
            "--severity",
            "medium",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "operator advisory `NOETA-2026-0100`",
        ));

    let (method, path, body) = rx.recv().unwrap();
    assert_eq!(method, "POST");
    assert_eq!(path, "/v1/reports/rep-9/promote");
    assert!(body.contains(r#""id":"NOETA-2026-0100""#), "body: {body}");
    assert!(
        body.contains(r#""package":"acme/imgfx""#),
        "prefilled package: {body}"
    );
    assert!(body.contains(r#""severity":"medium""#), "body: {body}");
    // Prefilled from the report (not passed on the command line).
    assert!(body.contains("confirmed leak"), "prefilled summary: {body}");
    assert!(
        body.contains(r#""ranges":">=1.0.0, <1.3.0""#),
        "prefilled ranges: {body}"
    );
    // An operator advisory carries no keyless bundle.
    assert!(
        !body.contains("\"bundle\""),
        "operator promote has no bundle: {body}"
    );
}

#[test]
fn noeta_advisory_promote_scope_owner_signs_the_same_keyless_bundle() {
    // advisory-intake residual a: the scope-owner promote path produces the SAME keyless Sigstore bundle
    // a fresh `advisory publish` does — prefilled from the report. Hermetic: an ambient CI identity mints
    // the bundle via an in-process Fulcio + Rekor, and the mock registry serves the report then captures
    // the promote body, confirming it carries a bundle and the report's package.
    if !git_available() {
        return;
    }
    use noeta_pm::keyless_fixtures::{TestSigstore, spawn_mock};
    use std::sync::Arc;

    const IDENTITY: &str =
        "https://github.com/acme/tools/.github/workflows/advise.yaml@refs/heads/main";
    const ISSUER: &str = "https://token.actions.githubusercontent.com";

    let base_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("advisory_promote_keyless");
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&base_dir).unwrap();

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
        spawn_mock(move |_m, _p, _b| (200, s.github_token_response()))
    };
    let trust_root = base_dir.join("trusted_root.json");
    std::fs::write(&trust_root, sigstore.trusted_root_json()).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let registry = mock_http(move |method, path, body| {
        if method == "GET" {
            return (
                200,
                r#"{"report":{"id":"rep-k","package":"acme/imgfx","ranges":">=1.0.0","summary":"owner-confirmed leak","status":"pending"}}"#
                    .to_string(),
            );
        }
        tx.send((path.to_string(), body.to_string())).unwrap();
        (
            201,
            r#"{"status":"report promoted","advisory":"ACME-2026-0009","tier":"publisher"}"#
                .to_string(),
        )
    });

    lang()
        .env("NOETA_REGISTRY_URL", &registry)
        .env("NOETA_REGISTRY_TOKEN", "acme-scope-token")
        .env_remove("NOETA_REGISTRY_ADMIN_TOKEN")
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
            "advisory",
            "promote",
            "rep-k",
            "--id",
            "ACME-2026-0009",
            "--severity",
            "high",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "publisher advisory `ACME-2026-0009`",
        ))
        .stdout(predicate::str::contains(format!("keyless: {IDENTITY}")));

    let (path, body) = rx.recv().unwrap();
    assert_eq!(path, "/v1/reports/rep-k/promote");
    assert!(
        body.contains(r#""package":"acme/imgfx""#),
        "prefilled package: {body}"
    );
    // The scope-owner promotion carries a keyless bundle (the same attestation `advisory publish` makes).
    assert!(
        body.contains("\"bundle\""),
        "publisher promote carries a bundle: {body}"
    );
    assert!(
        body.contains("dsseEnvelope") || body.contains("mediaType"),
        "bundle is a Sigstore bundle: {body}"
    );
}

#[test]
fn noeta_claim_requires_the_hosted_registry() {
    // namespace-protection #1: claiming a scope talks to the hosted registry. With no overrides the
    // default chain lands on the built-in hosted registry, so the only route with no claim endpoint
    // is `NOETA_REGISTRY_DIR` (the file-backed local index) — `noeta claim` explains that rather
    // than failing opaquely or silently falling through to the network.
    let reg = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("claim_local_dir_registry");
    std::fs::create_dir_all(&reg).unwrap();
    lang()
        .env_remove("NOETA_REGISTRY_URL")
        .env("NOETA_REGISTRY_DIR", &reg)
        .args(["claim", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs the hosted registry"));
}

#[test]
fn noeta_claim_uses_the_github_device_flow_off_ci() {
    // The full laptop path (namespace-protection #1): with no CI OIDC, `noeta claim` runs the GitHub
    // OAuth device flow (mocked), then POSTs the resulting access token to the registry claim endpoint
    // (mocked). Both endpoints live on one mock server, dispatched by path.
    let (tx, rx) = std::sync::mpsc::channel();
    let base = mock_http(move |_method, path, body| {
        match path {
        "/login/device/code" => (
            200,
            r#"{"device_code":"DC","user_code":"WDJB-MJHT","verification_uri":"https://github.test/device","expires_in":900,"interval":0}"#
                .to_string(),
        ),
        "/login/oauth/access_token" => (200, r#"{"access_token":"gho_laptop"}"#.to_string()),
        "/v1/scopes/claim" => {
            tx.send(body.to_string()).unwrap();
            (201, r#"{"status":"scope claimed","scope":"lapco","owner":"lapco"}"#.to_string())
        }
        _ => (404, "{}".to_string()),
    }
    });

    lang()
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .env("NOETA_REGISTRY_URL", &base)
        .env("NOETA_GITHUB_OAUTH_URL", &base)
        .env("NOETA_GITHUB_CLIENT_ID", "test-client-id")
        .args(["claim", "lapco"])
        .assert()
        .success()
        // The device code is shown to the user, and the generated publish token is printed to save.
        .stdout(
            predicate::str::contains("WDJB-MJHT")
                .and(predicate::str::contains("scope claimed"))
                .and(predicate::str::contains("NOETA_REGISTRY_TOKEN")),
        );

    // The claim POST carried the device-flow access token as `github_token`.
    let claim_body = rx.recv().unwrap();
    assert!(
        claim_body.contains("\"github_token\":\"gho_laptop\""),
        "claim sent the device-flow token: {claim_body}"
    );
    assert!(claim_body.contains("\"scope\":\"lapco\""), "{claim_body}");
}

#[test]
fn noeta_scope_require_provenance_validates_and_needs_a_registry() {
    // namespace-protection #1 Phase 1: the CLI validates `--root` before it would ever contact the
    // network, and — since the default chain now ends at the built-in hosted registry — the one
    // route with no policy endpoint is `NOETA_REGISTRY_DIR` (the file-backed local index), which
    // gets an explanation rather than a silent fall-through to the network.
    lang()
        .env_remove("NOETA_REGISTRY_URL")
        .args(["scope", "require-provenance", "para", "--root", "nonsense"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "`--root` must be `key` or `keyless`",
        ));
    let reg = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scope_policy_local_dir_registry");
    std::fs::create_dir_all(&reg).unwrap();
    lang()
        .env_remove("NOETA_REGISTRY_URL")
        .env("NOETA_REGISTRY_DIR", &reg)
        .args(["scope", "require-provenance", "para"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs the hosted registry"));
}

#[test]
fn publish_cooldown_holds_back_a_freshly_published_registry_version() {
    // namespace-protection #1 (publish cooldown): `[trust].publish_cooldown` makes the resolver skip a
    // registry release published within the window, so an advisory/yank can catch a compromised release
    // before it auto-propagates. Here the mock registry serves only a *just-published* version, so with
    // a 1-day cooldown resolution fails closed (nothing old enough) — proving the filter is wired
    // through the HTTP index and the trust policy. The failure happens during version selection, before
    // any git materialization, so the test needs no real repo.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let feed = format!(
        r#"{{"name":"acme/imgfx","versions":[{{"version":"1.0.0","url":"https://x/acme/imgfx","tag":"v1.0.0","sha":"abc","yanked":false,"published_at_unix":{now_ms}}}]}}"#
    );
    let base = mock_http(move |_method, path, _body| {
        if path == "/v1/packages/acme/imgfx" {
            (200, feed.clone())
        } else {
            (404, r#"{"error":"not found"}"#.to_string())
        }
    });

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_cooldown_e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nfx = { version = \"^1.0\", package = \"acme/imgfx\" }\n\
         [trust]\npublish_cooldown = \"1d\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.noe"), "use fx.core.go;\necho go();\n").unwrap();

    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .arg("check")
        .arg(dir.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("publish cooldown"));

    // The same project without the cooldown gets past selection (it then fails later trying to fetch
    // the mock's bogus git coordinates — a *different* error, proving cooldown was the gate above).
    std::fs::write(
        dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nfx = { version = \"^1.0\", package = \"acme/imgfx\" }\n",
    )
    .unwrap();
    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .arg("check")
        .arg(dir.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("publish cooldown").not());

    // With the cooldown *and* an exact pin on the fresh version, the consumer's deliberate choice
    // bypasses the window — selection succeeds (again failing later on the bogus git coords, not on
    // the cooldown).
    std::fs::write(
        dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nfx = { version = \"=1.0.0\", package = \"acme/imgfx\" }\n\
         [trust]\npublish_cooldown = \"1d\"\n",
    )
    .unwrap();
    lang()
        .env("NOETA_REGISTRY_URL", &base)
        .arg("check")
        .arg(dir.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("publish cooldown").not());
}

#[test]
fn require_provenance_refuses_an_unsigned_registry_dependency() {
    // namespace-protection #1, Phase 1 (consumer side): `[trust].require_provenance` demands a scope's
    // releases carry verified provenance. The published `acme/greet` is unsigned, so a consumer that
    // requires provenance for `acme` refuses to resolve it — while an unconstrained consumer still can.
    if !git_available() {
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_require_prov_e2e");
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("greet_repo");
    let app = base.join("app");
    let reg = base.join("registry");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&app).unwrap();

    git_in(&["init", "-q"], &repo);
    std::fs::write(
        repo.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.2.0\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("hello.noe"),
        "pub fn greeting(): string { return \"hi\"; }\n",
    )
    .unwrap();
    git_in(&["add", "."], &repo);
    git_in(&["commit", "-q", "-m", "release"], &repo);
    git_in(&["tag", "v1.2.0"], &repo);

    // Publish it UNSIGNED (no key, no ambient identity) to the local index.
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
        .success();

    std::fs::write(
        app.join("main.noe"),
        "use gc.hello.greeting;\necho greeting();\n",
    )
    .unwrap();

    // A consumer that requires provenance for `acme` refuses the unsigned release.
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [trust]\nrequire_provenance = [\"acme\"]\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("require_provenance")
                .and(predicate::str::contains("unattested")),
        );

    // Without the policy, the same unsigned dependency still resolves (gradual-adoption default).
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ngc = { version = \"^1.0\", package = \"acme/greet\" }\n",
    )
    .unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("hi"));
}
