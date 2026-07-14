//! End-to-end tests for the `lang` binary itself: the `run` and `repl` subcommands, driven through
//! a real process so the CLI glue, exit codes, stdout/stderr split, and the REPL's interactive
//! behaviour are all exercised (none of which the library-level tests can reach). The conformance
//! corpus runner moved to its own dev binary (`noeta-conformance`), with its CLI tests alongside it.

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// The workspace root, so `run` sees `examples/`.
fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Write a one-off program into its own private temp *directory* and return its path. The
/// directory isolation matters: `lang run` resolves sibling `.noe` modules from the entry's
/// directory (M1.9), so a bare temp file dropped into the shared `std::env::temp_dir()` would make
/// the loader scan — and parse — every other test's (or stray) `.noe` file as a candidate module.
/// A dedicated directory guarantees the entry is the only module in scope.
fn temp_program(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noeta_cli_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("main.noe");
    std::fs::write(&path, src).expect("write temp program");
    path
}

fn lang() -> Command {
    let mut cmd = Command::cargo_bin("noeta").expect("the `noeta` binary builds");
    // Hermetic startup cache: keep `cargo test` from reading or writing the developer's real
    // ~/.cache/noeta. One per-test-target dir is safe to share across all tests — entries are keyed
    // by source + binary identity, and the atomic per-pid store handles the parallel test processes.
    // Tests that exercise the cache directly override this with their own dir.
    cmd.env(
        "NOETA_CACHE_DIR",
        concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
    );
    cmd
}

/// Build the lean `noeta-runner` binary (debug, reusing the workspace's `target/debug` so its deps
/// are already compiled) and return its path, for `NOETA_RUNNER` — so a `--exe` test stapes onto a
/// ready runner instead of paying the CLI's default on-demand `--release` build. `None` if there is
/// no build toolchain (the caller then skips), mirroring `build_aot_archive`.
fn lean_runner_path() -> Option<PathBuf> {
    let bin = if cfg!(windows) {
        "noeta-runner.exe"
    } else {
        "noeta-runner"
    };
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(workspace())
        .args(["build", "-p", "noeta-runner"])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: building the lean runner failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let path = workspace().join("target/debug").join(bin);
    path.exists().then_some(path)
}

// --- `run` ------------------------------------------------------------------------

#[test]
fn a_bare_file_path_runs_the_program() {
    // `noeta <file.noe>` (no `run` subcommand) executes the file and forwards trailing args — the
    // shortcut a `#!/usr/bin/env noeta` shebang relies on. The shebang line itself is tolerated.
    let file = temp_program(
        "bare_run",
        "#!/usr/bin/env noeta\nuse std.args\necho args.all()[1]\n",
    );
    lang()
        .arg(&file)
        .arg("payload")
        .assert()
        .success()
        .stdout("payload\n");
}

#[test]
fn a_typo_subcommand_still_errors() {
    // The bare-file shortcut must not swallow a genuine mistake: an unknown "subcommand" that is not
    // an existing file still gets clap's error (and its did-you-mean).
    lang()
        .arg("biuld")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[cfg(unix)]
#[test]
fn an_executable_shebang_script_runs() {
    use std::os::unix::fs::PermissionsExt;
    // The real thing: a `.noe` file with a `#!<noeta>` shebang, `chmod +x`, executed directly. The
    // OS invokes `<noeta> <script> <args>`, which the bare-file shortcut runs.
    let bin = assert_cmd::cargo::cargo_bin("noeta");
    let file = temp_program(
        "shebang_exec",
        &format!("#!{}\nuse std.args\necho args.all()[1]\n", bin.display()),
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = std::process::Command::new(&file)
        .arg("payload")
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .output()
        .expect("run the executable shebang script");
    assert!(
        out.status.success(),
        "script failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "payload\n");
}

#[test]
fn run_real_host_uuids_are_real() {
    // Under `noeta run` (the real host, id-entropy U3) `id.uuid()` draws OS entropy and
    // `id.uuid_v7()` real wall time — unlike the sandbox's pinned values (`std/id_uuid.noe`),
    // here we assert the *shape*: canonical form, correct version/variant, distinct v4s, and
    // non-decreasing v7 timestamps.
    let file = temp_program(
        "run_uuid",
        "use std.{id}\necho id.uuid()\necho id.uuid()\necho id.uuid_v7()\necho id.uuid_v7()\n",
    );
    let out = lang().arg("run").arg(&file).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "four ids, one per line:\n{stdout}");
    for (i, id) in lines.iter().enumerate() {
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12],
            "canonical 8-4-4-4-12 form: {id}"
        );
        assert!(
            id.chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {id}"
        );
        let version = if i < 2 { "4" } else { "7" };
        assert_eq!(&groups[2][..1], version, "version nibble of {id}");
        assert!(
            matches!(&groups[3][..1], "8" | "9" | "a" | "b"),
            "variant bits of {id}"
        );
    }
    // Real entropy: consecutive v4s differ (a collision is a 2^-122 event, i.e. a wiring bug).
    assert_ne!(lines[0], lines[1]);
    // Real wall time: the v7 48-bit timestamp is past 2026-01-01 and non-decreasing.
    let ms = |id: &str| u64::from_str_radix(&id.replace('-', "")[..12], 16).unwrap();
    assert!(
        ms(lines[2]) > 1_767_225_600_000,
        "v7 dates after 2026: {}",
        lines[2]
    );
    assert!(ms(lines[3]) >= ms(lines[2]), "v7 time is non-decreasing");
}

#[test]
fn run_executes_a_program_to_stdout() {
    let file = temp_program("run_ok", "echo \"hello\"; echo 1 + 2;");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("hello\n3\n");
}

#[test]
fn run_reports_runtime_error_and_exits_1() {
    let file = temp_program("run_runtime", "echo missing_name;");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0005"));
}

#[test]
fn run_reports_parse_error_and_exits_1() {
    let file = temp_program("run_parse", "echo ;");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0003"));
}

#[test]
fn cache_info_path_and_clear() {
    let cache_dir = PathBuf::from(concat!(env!("CARGO_TARGET_TMPDIR"), "/cache-cmd"));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let file = temp_program("cache_cmd", "echo 40 + 2\n");

    // `cache <args>` against our dedicated dir.
    let cache = |args: &[&str]| {
        let mut cmd = lang();
        cmd.env("NOETA_CACHE_DIR", &cache_dir).args(args);
        cmd.assert()
    };

    // Empty to start.
    cache(&["cache", "info"])
        .success()
        .stdout(predicate::str::contains("0 entries"));
    cache(&["cache", "path"])
        .success()
        .stdout(predicate::str::contains(cache_dir.to_str().unwrap()));

    // A run populates exactly one entry.
    lang()
        .env("NOETA_CACHE_DIR", &cache_dir)
        .env_remove("NOETA_NO_CACHE")
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("42\n");
    cache(&["cache", "info"])
        .success()
        .stdout(predicate::str::contains("1 entry"));

    // Clear empties it.
    cache(&["cache", "clear"])
        .success()
        .stdout(predicate::str::contains("removed 1"));
    cache(&["cache", "info"])
        .success()
        .stdout(predicate::str::contains("0 entries"));

    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[test]
fn startup_cache_is_semantically_invisible() {
    // The transparent startup cache (M3) must be *semantically invisible*: a warm run (cache hit —
    // decode a stored module and run it) produces byte-identical stdout, stderr, and exit code to an
    // uncached run (compile from source). Each program is observed three ways and all must agree:
    //   - baseline: NOETA_NO_CACHE (never touches the cache),
    //   - cold:     a fresh cache dir — a miss that compiles and populates,
    //   - warm:     the same dir again — a hit, a wholly different code path.
    // This is the regression wall for the cache; the timing win is verified elsewhere.
    let programs: &[(&str, &str)] = &[
        ("arith", "echo 1 + 2 * 3\n"),
        ("string", "mut n = 21\necho \"n=${n * 2}\"\n"),
        ("loop", "mut s = 0\nfor i in 0..10 { s = s + i }\necho s\n"),
        ("func", "fn sq(n: int): int { return n * n }\necho sq(9)\n"),
        ("list", "echo [1, 2, 3].len()\n"),
        // Compiles + caches, then aborts at runtime (exit 1): proves the exit code and stderr trace
        // survive a cache hit too, not just clean stdout.
        ("panic", "echo \"before\"\npanic(\"boom\")\n"),
    ];

    for &(name, src) in programs {
        let file = temp_program(&format!("cache_inv_{name}"), src);
        let cache_dir =
            PathBuf::from(concat!(env!("CARGO_TARGET_TMPDIR"), "/cache-inv")).join(name);
        let _ = std::fs::remove_dir_all(&cache_dir);

        // One observation of (stdout, stderr, exit code). `use_cache` selects the cached path (a
        // dedicated dir) or the NOETA_NO_CACHE baseline. `.assert()` does not fail on a non-zero
        // exit, so the `panic` fixture is captured like any other.
        let observe = |use_cache: bool| {
            let mut cmd = lang();
            cmd.arg("run").arg(&file);
            if use_cache {
                cmd.env_remove("NOETA_NO_CACHE")
                    .env("NOETA_CACHE_DIR", &cache_dir);
            } else {
                cmd.env("NOETA_NO_CACHE", "1");
            }
            let out = cmd.assert().get_output().clone();
            (out.stdout, out.stderr, out.status.code())
        };

        let baseline = observe(false);
        let cold = observe(true); // miss → compile → populate
        let warm = observe(true); // hit → decode → run

        assert_eq!(
            cold, baseline,
            "{name}: cold (miss) run must match the uncached baseline"
        );
        assert_eq!(
            warm, baseline,
            "{name}: warm (cache-hit) run must match the uncached baseline"
        );

        // Every fixture here compiles (even `panic`, which fails only at runtime), so the cold run
        // must have left an entry — proving the warm run was a genuine hit, not a silent no-op.
        let entries = std::fs::read_dir(&cache_dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "noeb"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(
            entries, 1,
            "{name}: the cold run should populate exactly one cache entry"
        );

        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}

#[test]
fn run_missing_file_exits_2() {
    lang()
        .arg("run")
        .arg("/no/such/file.noe")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot read"));
}

// --- program argument pass-through (`noeta run FILE -- <args>`) --------------------
//
// The program reads its arguments with `args.all()`. On the real host this is the script path as
// the program name (argv[0]) followed by any args given after `--`, mirroring what a shipped
// `noeta build --exe` binary sees via the real process argv when invoked directly.

/// A program that echoes its whole argument vector, one element per line.
const ECHO_ARGS: &str = "use std.{args}\nfor a in args.all() {\n  echo a\n}\n";

#[test]
fn run_passes_through_args_after_dash_dash() {
    let file = temp_program("run_args_passthrough", ECHO_ARGS);
    // Everything after `--` reaches the program verbatim — including a hyphen-prefixed flag, which
    // the `--` separator protects from being parsed as a `noeta` option.
    lang()
        .arg("run")
        .arg(&file)
        .arg("--")
        .arg("--verbose")
        .arg("input.txt")
        .arg("two words")
        .assert()
        .success()
        .stdout(format!(
            "{}\n--verbose\ninput.txt\ntwo words\n",
            file.display()
        ));
}

#[test]
fn run_without_passthrough_reports_only_the_program_name() {
    // With no `--`, `args.all()` is just the program name (argv[0]) — the toolchain's own
    // `noeta run` prefix never leaks into the program's view.
    let file = temp_program("run_args_none", ECHO_ARGS);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout(format!("{}\n", file.display()));
}

// --- real OS-thread isolates (isolates I.4b, out-of-oracle) ------------------------
//
// These run on the CLI's real (VM) path, where a channel-free `isolate f(args)` executes on its own
// OS thread with args/result copy-marshalled across the boundary. Correctness here exercises the
// whole real path — thread spawn, `Wire` marshal/rebuild of value-type args and results (incl. a
// `struct` and a marshalled global function), and the structured join — which the deterministic
// differential (cooperative sandbox) never covers.

#[test]
fn run_real_isolate_returns_marshalled_result() {
    // A struct argument in, an int result back — the value-type graph round-trips across the thread.
    let file = temp_program(
        "isolate_struct",
        "struct Point { x: int; y: int }\n\
         async fn dist2(p: Point): int { return p.x * p.x + p.y * p.y }\n\
         async fn run(): int {\n\
         mut r = 0\n\
         concurrent { d = isolate dist2(Point { x: 3, y: 4 }); r = d.await }\n\
         return r\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("25\n");
}

#[test]
fn run_real_isolates_fan_out_and_join() {
    // Three isolates run in parallel and their results are summed after the structured join; the
    // isolate body calls a *global* function, exercising the marshalled-globals snapshot.
    let file = temp_program(
        "isolate_fanout",
        "fn sq(n: int): int { return n * n }\n\
         async fn work(n: int): int { return sq(n) }\n\
         async fn run(): int {\n\
         mut total = 0\n\
         concurrent {\n\
         a = isolate work(2)\n\
         b = isolate work(3)\n\
         c = isolate work(4)\n\
         total = a.await + b.await + c.await\n\
         }\n\
         return total\n\
         }\n\
         echo run().await",
    );
    // 2²+3²+4² = 29.
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("29\n");
}

#[test]
fn run_real_isolate_borrowed_arg_mutation_is_isolated() {
    // P-PAR S2: a promotable data argument is borrow-shared (promoted once into the parent's
    // SharedRegion), not copied per worker. The worker "mutating" its parameter must therefore
    // hit the COW slow path — `is_uniquely_owned` is false on a shared object — and copy, never
    // touching the parent's graph. Worker sees 4 elements after its append; parent still 3.
    let file = temp_program(
        "isolate_borrow_cow",
        "struct Rec { a: int; b: int }\n\
         async fn tweak(l: List<Rec>): int {\n\
         mut m = l\n\
         m ~= [Rec { a: 9, b: 9 }]\n\
         return m.len()\n\
         }\n\
         async fn run(): int {\n\
         corpus = [Rec { a: 1, b: 2 }, Rec { a: 3, b: 4 }, Rec { a: 5, b: 6 }]\n\
         mut got = 0\n\
         concurrent { h = isolate tweak(corpus); got = h.await }\n\
         return got * 100 + corpus.len()\n\
         }\n\
         echo run().await",
    );
    // Worker length 4, parent length 3 → 403.
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("403\n");
}

#[test]
fn run_real_isolate_borrowed_arg_round_trips_as_result() {
    // P-PAR S2: the worker returns (part of) the borrowed graph itself; the result marshal walks
    // the shared objects read-only and ships an owned copy home. Also fans the same corpus to two
    // workers, exercising the promote-once memo path.
    let file = temp_program(
        "isolate_borrow_roundtrip",
        "async fn first(l: List<int>): int { return l[0] }\n\
         async fn last(l: List<int>): int { return l[l.len() - 1] }\n\
         async fn run(): int {\n\
         corpus = [7, 8, 9]\n\
         mut total = 0\n\
         concurrent {\n\
         a = isolate first(corpus)\n\
         b = isolate last(corpus)\n\
         total = a.await * 10 + b.await\n\
         }\n\
         return total\n\
         }\n\
         echo run().await",
    );
    // first = 7, last = 9 → 79.
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("79\n");
}

#[test]
fn run_real_isolates_actually_run_in_parallel() {
    // Two isolates each sleep 300ms of real wall-clock time on their own thread. Run in parallel the
    // program finishes in well under the ~600ms a sequential run would take; a generous 550ms ceiling
    // keeps the test robust on a loaded machine while still failing if the isolates serialized.
    let file = temp_program(
        "isolate_parallel",
        "use std.task.{sleep}\n\
         async fn work(ms: int): int { sleep(ms).await; return ms }\n\
         async fn run(): int {\n\
         mut total = 0\n\
         concurrent { a = isolate work(300); b = isolate work(300); total = a.await + b.await }\n\
         return total\n\
         }\n\
         echo run().await",
    );
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("600\n");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(550),
        "two 300ms isolates should run in parallel (<550ms), took {elapsed:?} — did they serialize?"
    );
}

#[test]
fn run_real_isolate_channel_producer_consumer() {
    // Isolates I.4c: a producer runs as a real isolate on its own thread and streams over a channel
    // the parent (consumer) drains — the channel crosses the thread boundary as a shared queue. A
    // capacity-2 buffer means the two threads genuinely hand messages back and forth; 1+2+3+4 = 10.
    let file = temp_program(
        "isolate_xchan",
        "async fn produce(tx: Sender<int>): void {\n\
         for i in 1..5 { tx.send(i).await }\n\
         tx.close()\n\
         }\n\
         async fn run(): int {\n\
         (tx, rx) = channel::<int>(2)\n\
         mut total = 0\n\
         concurrent {\n\
         isolate produce(tx)\n\
         mut running = true\n\
         while running {\n\
         r = rx.recv().await\n\
         (v, keep) = match r { some(x) => (x, true), none => (0, false) }\n\
         total = total + v\n\
         running = keep\n\
         }\n\
         }\n\
         return total\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("10\n");
}

#[test]
fn run_real_isolate_channel_backpressure_capacity_one() {
    // A capacity-1 cross-thread channel forces the producer isolate and the parent consumer to
    // alternate (send blocks the producer's cooperative scheduler until the parent drains a slot).
    // Completing correctly proves the shared channel's cooperative poll interoperates across threads
    // without deadlocking — the hazard that motivated splitting channels into I.4c. 0+1+2 = 3.
    let file = temp_program(
        "isolate_backpressure",
        "async fn produce(tx: Sender<int>): void {\n\
         for i in 0..3 { tx.send(i).await }\n\
         tx.close()\n\
         }\n\
         async fn run(): int {\n\
         (tx, rx) = channel::<int>(1)\n\
         mut total = 0\n\
         concurrent {\n\
         isolate produce(tx)\n\
         mut running = true\n\
         while running {\n\
         r = rx.recv().await\n\
         (v, keep) = match r { some(x) => (x, true), none => (0, false) }\n\
         total = total + v\n\
         running = keep\n\
         }\n\
         }\n\
         return total\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("3\n");
}

// --- M2.3: `lang run` uses the real host (real env/args + real-disk IO) ------------

#[test]
fn run_reads_the_real_environment() {
    // `env.get` reads the REAL process environment (RealHost), not the sandbox fixture —
    // proven by injecting a variable the child process sees. (Conformance still runs the
    // sandbox fixture; only `lang run` is on the real host.)
    let file = temp_program("run_env", "use std.{env};\necho env.get(\"LANG_E2E_VAR\");");
    lang()
        .arg("run")
        .arg(&file)
        .env("LANG_E2E_VAR", "from-host")
        .assert()
        .success()
        .stdout("from-host\n");
}

#[test]
fn run_os_reports_the_real_machine_and_execs_real_processes() {
    // `os.*` introspection reads the REAL machine (RealHost) — platform is a known constant,
    // the rest are asserted non-trivial — and `os.exec` runs a REAL subprocess. `env.set`
    // writes the overlay that both `env.get` and the exec child's environment observe.
    let src = "use std.{os, env}\n\
               echo os.platform()\n\
               echo os.cpus() > 0\n\
               echo os.pid() > 1\n\
               env.set(\"LANG_E2E_OVERLAY\", \"through\")\n\
               echo env.get(\"LANG_E2E_OVERLAY\")\n\
               r = os.exec(\"sh\", [\"-c\", \"echo $LANG_E2E_OVERLAY\"])\n\
               echo r.ok()\n\
               echo r.stdout().trim()\n";
    let file = temp_program("run_os", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout(format!(
            "{}\ntrue\ntrue\nthrough\ntrue\nthrough\n",
            std::env::consts::OS
        ));
}

#[test]
fn run_os_exit_sets_the_process_exit_code() {
    // `os.exit(code)` terminates `noeta run` itself with the requested code — cleanly: prior
    // output is kept, nothing after runs, and no diagnostic/traceback is printed.
    let file = temp_program(
        "run_os_exit",
        "use std.{os};\necho \"before\";\nos.exit(7);\necho \"unreachable\";",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .code(7)
        .stdout("before\n")
        .stderr("");
}

#[test]
fn run_os_spawn_controls_a_real_child_process() {
    // `os.spawn` starts a REAL child without waiting and hands back a `Process` handle: `pid()` is
    // the real OS pid, `wait()` captures its output via the drain threads, and `kill()` + `wait()`
    // reports a non-success status. Proves the real-host lifecycle, not the sandbox script.
    let src = "use std.{os}\n\
               p = os.spawn(\"echo\", [\"child says hi\"])\n\
               echo p.pid() > 0\n\
               r = p.wait()\n\
               echo r.status()\n\
               echo r.stdout().trim()\n\
               // a killed child does not exit 0\n\
               s = os.spawn(\"sleep\", [\"5\"])\n\
               s.kill()\n\
               echo s.wait().ok()\n";
    let file = temp_program("run_os_spawn", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("true\n0\nchild says hi\nfalse\n");
}

#[test]
fn run_os_spawn_streams_stdout_and_feeds_stdin() {
    // Streaming over a REAL child: read a slow producer's lines with `read_line` as they arrive
    // (each blocks until the child emits it), confirm `wait()` still returns the whole capture,
    // and feed a `cat` child through stdin, closing it to signal EOF. Proves the drain-thread +
    // shared-buffer streaming path, not the sandbox script.
    let src = "use std.{os}\n\
               p = os.spawn(\"sh\", [\"-c\", \"for i in 1 2 3; do echo line-$i; sleep 0.05; done\"])\n\
               echo p.read_line()\n\
               echo p.read_line()\n\
               echo p.read_line()\n\
               echo p.read_line()\n\
               echo p.wait().stdout().trim().replace(\"\\n\", \"|\")\n\
               c = os.spawn(\"cat\", [])\n\
               c.write(\"in-a\\nin-b\\n\")\n\
               c.close_stdin()\n\
               echo c.read_line()\n\
               echo c.read_line()\n\
               echo c.read_line()\n\
               echo c.wait().ok()\n";
    let file = temp_program("run_os_stream", src);
    lang().arg("run").arg(&file).assert().success().stdout(
        "some(line-1)\nsome(line-2)\nsome(line-3)\nnone\nline-1|line-2|line-3\n\
         some(in-a)\nsome(in-b)\nnone\ntrue\n",
    );
}

#[test]
fn run_os_spawn_streams_stderr_and_reads_by_chars() {
    // Over a REAL child: `read_err_line` streams stderr on its own cursor, and `read(n)` reads up
    // to n characters (multibyte-aware) from stdout — distinct from the line-oriented read_line.
    let src = "use std.{os}\n\
               p = os.spawn(\"sh\", [\"-c\", \"echo out; echo e1 1>&2; echo e2 1>&2\"])\n\
               echo p.read_line()\n\
               echo p.read_err_line()\n\
               echo p.read_err_line()\n\
               echo p.read_err_line()\n\
               echo p.wait().status()\n\
               q = os.spawn(\"echo\", [\"héllo\"])\n\
               echo q.read(3)\n\
               echo q.read(2)\n\
               echo q.read_line()\n\
               echo q.read(1)\n";
    let file = temp_program("run_os_stream2", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("some(out)\nsome(e1)\nsome(e2)\nnone\n0\nsome(hél)\nsome(lo)\nsome()\nnone\n");
}

#[test]
fn run_does_real_disk_io() {
    // `fs.write`/`fs.read` hit the REAL disk (RealHost), relative to the working directory.
    let dir = std::env::temp_dir().join("noeta_cli_realfs_dir");
    std::fs::create_dir_all(&dir).expect("create work dir");
    let _ = std::fs::remove_file(dir.join("e2e.txt"));
    let file = temp_program(
        "run_realfs",
        "use std.{fs};\nfs.write(\"e2e.txt\", \"on disk\");\necho fs.read(\"e2e.txt\");",
    );
    lang()
        .arg("run")
        .arg(&file)
        .current_dir(&dir)
        .assert()
        .success()
        .stdout("on disk\n");
    // The file really landed on disk (not an in-memory sandbox).
    assert_eq!(
        std::fs::read_to_string(dir.join("e2e.txt")).expect("file on disk"),
        "on disk"
    );
    let _ = std::fs::remove_file(dir.join("e2e.txt"));
}

#[test]
fn run_reads_files_asynchronously_on_the_real_executor() {
    // Track A.4c: `fs.read_async(path)` returns a `Future<string>` the async context awaits. On the
    // CLI's real executor the reads hit the REAL disk and run concurrently on tokio; here two files
    // are read in a `concurrent` block and awaited for their contents. (Conformance covers the
    // deterministic sandbox path; this proves the real-disk, real-executor path.)
    let dir = std::env::temp_dir().join("noeta_cli_async_read_dir");
    std::fs::create_dir_all(&dir).expect("create work dir");
    std::fs::write(dir.join("a.txt"), "alpha").expect("write a");
    std::fs::write(dir.join("b.txt"), "beta").expect("write b");
    let src = "use std.{fs}\n\
               async fn load(path: string): string {\n\
               \x20   return fs.read_async(path).await\n\
               }\n\
               concurrent {\n\
               \x20   a = spawn load(\"a.txt\")\n\
               \x20   b = spawn load(\"b.txt\")\n\
               \x20   echo \"a=\" ~ a.await\n\
               \x20   echo \"b=\" ~ b.await\n\
               }\n\
               echo \"done\"\n";
    let file = temp_program("run_async_read", src);
    lang()
        .arg("run")
        .arg(&file)
        .current_dir(&dir)
        .assert()
        .success()
        .stdout("a=alpha\nb=beta\ndone\n");
    let _ = std::fs::remove_file(dir.join("a.txt"));
    let _ = std::fs::remove_file(dir.join("b.txt"));
}

#[test]
fn run_async_metadata_twins_on_the_real_executor() {
    // Extern-types X6: `fs.exists_async`/`remove_async`/`list_async` have NO real body — the
    // real executor's None fallback runs their sync body against the RealHost at spawn. This
    // exercises that degradation path end-to-end on real disk.
    let dir = std::env::temp_dir().join("noeta_cli_async_meta_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    std::fs::write(dir.join("keep.txt"), "k").expect("write keep");
    std::fs::write(dir.join("gone.txt"), "g").expect("write gone");
    let src = "use std.{fs}\n\
               async fn run(): void {\n\
               \x20   echo \"exists=\" ~ fs.exists_async(\"keep.txt\").await\n\
               \x20   echo \"removed=\" ~ fs.remove_async(\"gone.txt\").await\n\
               \x20   echo \"exists-after=\" ~ fs.exists_async(\"gone.txt\").await\n\
               }\n\
               run().await\n";
    let file = temp_program("run_async_meta", src);
    lang()
        .arg("run")
        .arg(&file)
        .current_dir(&dir)
        .assert()
        .success()
        .stdout("exists=true\nremoved=true\nexists-after=false\n");
    // The removal really happened on disk.
    assert!(!dir.join("gone.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "hits the real network; run explicitly"]
fn run_http_get_over_the_real_network() {
    // http arc H2/H3 on the real host: `http.get` (sync) and `http.get_async(...).await`
    // (RealBody::Async on the executor's runtime) both reach a live endpoint. `#[ignore]` so CI
    // stays hermetic — run explicitly when online.
    let src = "use std.{http}\n\
               async fn run(): void {\n\
               \x20   echo http.get(\"https://example.com/\").status()\n\
               \x20   echo http.get_async(\"https://example.com/\").await.ok()\n\
               }\n\
               run().await\n";
    let file = temp_program("run_http_real", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("200\ntrue\n");
}

#[test]
fn run_bcrypt_round_trips_on_real_entropy() {
    // Crypto arc C4: on the RealHost the bcrypt salt comes from OS entropy, so the hash string
    // is unpredictable — but it must verify against the password that made it (and not against
    // another), and carry the requested cost in its self-describing prefix.
    let src = "use std.{crypto}\n\
               h = crypto.bcrypt_hash(\"hunter2\", 4)\n\
               echo h.starts_with(\"$2b$04$\")\n\
               echo crypto.bcrypt_verify(\"hunter2\", h)\n\
               echo crypto.bcrypt_verify(\"wr0ng\", h)\n\
               echo crypto.random_bytes(16).len()\n";
    let file = temp_program("run_bcrypt_real", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("true\ntrue\nfalse\n16\n");
}

#[test]
fn run_writes_files_asynchronously_on_the_real_executor() {
    // Track A.10: `fs.write_async`/`append_async` hit the REAL disk via the real executor's tokio
    // runtime, awaited like any future. A write/append/read round-trip lands on disk.
    let dir = std::env::temp_dir().join("noeta_cli_async_write_dir");
    std::fs::create_dir_all(&dir).expect("create work dir");
    let _ = std::fs::remove_file(dir.join("w.txt"));
    let src = "use std.{fs}\n\
               async fn run(): void {\n\
               \x20   fs.write_async(\"w.txt\", \"hello\").await\n\
               \x20   fs.append_async(\"w.txt\", \" world\").await\n\
               \x20   echo fs.read_async(\"w.txt\").await\n\
               }\n\
               run().await\n";
    let file = temp_program("run_async_write", src);
    lang()
        .arg("run")
        .arg(&file)
        .current_dir(&dir)
        .assert()
        .success()
        .stdout("hello world\n");
    // The bytes really landed on disk.
    assert_eq!(
        std::fs::read_to_string(dir.join("w.txt")).expect("file on disk"),
        "hello world"
    );
    let _ = std::fs::remove_file(dir.join("w.txt"));
}

#[test]
fn run_async_read_of_a_missing_file_is_an_io_error() {
    // An IO failure surfaces at the `.await` as an E0021 abort — the same error channel synchronous
    // `fs.read` uses, just deferred to when the async read is polled to completion.
    let src = "use std.{fs}\n\
               async fn load(path: string): string { return fs.read_async(path).await }\n\
               echo load(\"definitely_missing_async.txt\").await\n";
    let file = temp_program("run_async_read_missing", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicates::str::contains("E0021"));
}

#[test]
fn run_sleeps_in_real_time_on_the_real_executor() {
    // Track A.4: `lang run` pairs the real host with the real wall-clock executor, so an awaited
    // `sleep(ms)` genuinely takes real time (the sandbox executor would jump logical time and finish
    // instantly). Two tasks in a `concurrent` block interleave — `b`'s shorter sleep finishes first —
    // producing the *same* byte-for-byte output as the sandbox differential, but taking ~150ms of
    // real time. We assert both: the interleaved output and a real-time lower bound.
    let src = "use std.task.{sleep}\n\
               async fn work(name: string, ms: int): int {\n\
               \x20   echo name ~ \" start\"\n\
               \x20   sleep(ms).await\n\
               \x20   echo name ~ \" end\"\n\
               \x20   return ms\n\
               }\n\
               concurrent {\n\
               \x20   a = spawn work(\"a\", 150)\n\
               \x20   b = spawn work(\"b\", 50)\n\
               \x20   echo \"sum=\" ~ (a.await + b.await)\n\
               }\n\
               echo \"done\"\n";
    let file = temp_program("run_real_sleep", src);
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        // `a` suspends at its 150ms sleep; `b` runs and finishes first (50ms); then `a`. The
        // handles are awaited for their `int` results, summed to 200.
        .stdout("a start\nb start\nb end\na end\nsum=200\ndone\n");
    // The longer sleep (150ms) really elapsed — proof the executor is the real one, not the sandbox
    // (which would return in well under this). A generous margin keeps the test non-flaky.
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(120),
        "the real executor should sleep ~150ms of wall-clock time, took {:?}",
        start.elapsed()
    );
}

#[test]
fn run_orders_example_produces_the_headline_output() {
    lang()
        .current_dir(workspace())
        .arg("run")
        .arg("examples/orders.noe")
        .assert()
        .success()
        .stdout(
            "Placed: Order #1 awaiting payment\n\
             Order #2 awaiting payment\n\
             Cannot place an empty order\n\
             Item 0 has a negative price\n",
        );
}

// --- `run --tier` (object-model slice 6: `@debug` inline-code activation) -----------

const DEBUG_PROGRAM: &str = "fn f(x: int): void {\n\
         @debug { echo \"debug: x is ${x}\"; }\n\
         echo \"result: ${x * 2}\";\n\
     }\n\
     f(5);\n";

#[test]
fn run_strips_debug_blocks_by_default() {
    // Without `--tier`, a `@debug { … }` block is stripped before lowering: its `echo` never runs.
    let file = temp_program("run_debug_off", DEBUG_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("result: 10\n");
}

#[test]
fn run_tier_debug_activates_debug_blocks() {
    // `--tier debug` compiles the `@debug` block in, in place — the debug `echo` runs before the
    // unconditional one, proving inline (not appended) activation in statement position.
    let file = temp_program("run_debug_on", DEBUG_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--tier")
        .arg("debug")
        .assert()
        .success()
        .stdout("debug: x is 5\nresult: 10\n");
}

#[test]
fn run_tier_unknown_is_e0036() {
    let file = temp_program("run_tier_bad", "@tsetup { echo \"x\"; }\necho \"hi\";\n");
    lang()
        .arg("run")
        .arg(&file)
        .arg("--tier")
        .arg("tsetup")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

// --- `test` (object-model slice 6: the `@test` runner) -----------------------------

/// A program whose `@test` block holds a mix of passing and failing tests. The top-level `echo`
/// must NOT run (the runner runs the tests, not the program's `main`).
const MIXED_TESTS: &str = "fn add(a: int, b: int): int { return a + b; }\n\
     echo \"main effect must not run\";\n\
     @test {\n\
         fn adds(): void { assert(add(2, 3) == 5); }\n\
         fn fails(): void { assert(add(1, 1) == 3, \"math is hard\"); }\n\
         fn panics(): void { panic(\"boom\"); }\n\
     }\n";

#[test]
fn test_name_filter_runs_only_the_named_test() {
    // `--name` (ide-ui U3): the editor's run-one-test seam — only the named fn runs, so a suite
    // with failures exits 0 when the selected test passes.
    let file = temp_program("test_name_filter", MIXED_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .args(["--name", "adds"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ok    adds")
                .and(predicate::str::contains("fails").not())
                .and(predicate::str::contains("1 passed, 0 failed, 1 total")),
        );
    // An unmatched name runs nothing and succeeds (like an empty group).
    lang()
        .arg("test")
        .arg(&file)
        .args(["--name", "nope"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests matching --name"));
}

#[test]
fn test_json_reports_machine_readable_outcomes() {
    // `--json` (ide-ui U3): one JSON object on stdout — per-test outcomes + totals, no human
    // report lines — with the same exit-code semantics.
    let file = temp_program("test_json", MIXED_TESTS);
    let assert = lang()
        .arg("test")
        .arg(&file)
        .arg("--json")
        .assert()
        .failure()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(json["passed"], 1);
    assert_eq!(json["failed"], 2);
    assert_eq!(json["total"], 3);
    let tests = json["tests"].as_array().expect("tests array");
    let fails = tests
        .iter()
        .find(|t| t["name"] == "fails")
        .expect("fails outcome");
    assert_eq!(fails["passed"], false);
    assert!(
        fails["message"]
            .as_str()
            .unwrap_or_default()
            .contains("math is hard"),
        "{fails}"
    );
    assert!(
        !stdout.contains("running"),
        "no human header in --json mode"
    );
}

#[test]
fn test_runs_all_tests_and_reports_failures() {
    // Default: every test runs even after a failure; exit 1 because some failed. The passing
    // tests are reported `ok`, the failing ones `FAIL` with their message, and the program's own
    // top-level `echo` never runs.
    let file = temp_program("test_mixed", MIXED_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("ok    adds")
                .and(predicate::str::contains("FAIL  fails"))
                .and(predicate::str::contains("assertion failed: math is hard"))
                .and(predicate::str::contains("FAIL  panics"))
                .and(predicate::str::contains("panic: boom"))
                .and(predicate::str::contains("1 passed, 2 failed, 3 total"))
                .and(predicate::str::contains("main effect must not run").not()),
        );
}

#[test]
fn test_all_passing_exits_0() {
    let file = temp_program(
        "test_pass",
        "fn add(a: int, b: int): int { return a + b; }\n\
         @test {\n\
             fn adds(): void { assert(add(2, 3) == 5); }\n\
             fn truthy(): void { assert(true); }\n\
         }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed, 2 total"));
}

#[test]
fn test_fail_fast_stops_early() {
    // `--fail-fast --jobs 1` makes the stop deterministic: the first failure halts the run and the
    // remaining tests are reported as not run.
    let file = temp_program("test_failfast", MIXED_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--fail-fast")
        .arg("--jobs")
        .arg("1")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("not run (stopped early)"));
}

#[test]
fn test_no_tests_is_success() {
    let file = temp_program("test_none", "echo \"hi\";\n");
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests found"));
}

#[test]
fn test_annotation_form_is_discovered() {
    // `@test fn …` (the annotation form, slice 6c) is grouping sugar for a one-item block: the
    // runner discovers an annotated fn exactly as it does a fn inside `@test { … }`, and the two
    // forms mix freely. The program's own top-level `echo` still does not run.
    let file = temp_program(
        "test_annotation",
        "fn add(a: int, b: int): int { return a + b; }\n\
         echo \"main must not run\";\n\
         @test fn annotated(): void { assert(add(2, 3) == 5); }\n\
         @test { fn blocked(): void { assert(add(1, 1) == 2); } }\n",
    );
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("ok    annotated")
            .and(predicate::str::contains("ok    blocked"))
            .and(predicate::str::contains("2 passed, 0 failed, 2 total"))
            .and(predicate::str::contains("main must not run").not()),
    );
}

#[test]
fn test_white_box_private_field_access() {
    // Slice 6d: an in-source `@test` block gets white-box access to its module's private fields —
    // it reads/writes/constructs `Account.balance` (private) directly and passes. (Ordinary code
    // doing the same would be E0035, exercised in the checker's unit tests.)
    let file = temp_program(
        "test_whitebox",
        "class Account {\n\
             mut balance: int\n\
             fn new(b: int): Account { return Account { balance: b }; }\n\
         }\n\
         @test fn touches_internals(): void {\n\
             mut a = Account { balance: 0 };\n\
             a.balance = 50;\n\
             assert(a.balance == 50);\n\
         }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn test_unknown_tier_is_e0036() {
    let file = temp_program("test_badtier", "@tset { fn x(): void { assert(true); } }\n");
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

// --- test metadata attributes (object-model slice 6h) ------------------------------

/// A program exercising `#[Skip]` / `#[Name(...)]` / `#[Group(...)]` on `@test` fns (built-in
/// prelude attributes, no user definition). The attributes lead the annotation, one per line.
const ATTR_TESTS: &str = "fn add(a: int, b: int): int { return a + b }\n\
     #[Skip]\n\
     @test fn not_ready(): void { assert(false) }\n\
     #[Name(\"adds two numbers\")]\n\
     @test fn add_test(): void { assert(add(1, 1) == 2) }\n\
     #[Group(\"fast\")]\n\
     @test fn fast_one(): void { assert(add(2, 2) == 4) }\n\
     #[Group(\"slow\")]\n\
     @test fn slow_one(): void { assert(add(3, 3) == 6) }\n";

#[test]
fn test_skip_is_reported_not_run_and_does_not_fail() {
    // `#[Skip]` test is listed `skip`, never run (its false `assert` would fail), and the suite
    // still passes. `#[Name("…")]` renames a test in the report.
    let file = temp_program("test_attrs", ATTR_TESTS);
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("skip  not_ready")
            .and(predicate::str::contains("ok    adds two numbers")) // the #[Name] display name
            .and(predicate::str::contains(
                "3 passed, 0 failed, 1 skipped, 4 total",
            ))
            .and(predicate::str::contains("FAIL").not()),
    );
}

#[test]
fn test_skip_reason_is_shown() {
    // `#[Skip("reason")]` (slice 6i — `Skip.reason` defaults to `""`, so the bare and reasoned forms
    // both work) shows the reason after the skipped test's name.
    let file = temp_program(
        "test_skip_reason",
        "#[Skip(\"flaky on CI\")]\n@test fn flaky(): void { assert(false) }\n",
    );
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("skip  flaky (flaky on CI)")
            .and(predicate::str::contains("1 skipped")),
    );
}

#[test]
fn test_group_filter_runs_only_that_group() {
    let file = temp_program("test_group", ATTR_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--group")
        .arg("fast")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ok    fast_one")
                .and(predicate::str::contains("slow_one").not())
                .and(predicate::str::contains("1 passed, 0 failed, 1 total")),
        );
}

#[test]
fn test_group_with_no_match_reports_empty() {
    let file = temp_program("test_group_none", ATTR_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--group")
        .arg("nonexistent")
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests in group `nonexistent`"));
}

#[test]
fn test_data_runs_once_per_row() {
    // `#[Data([…])]` expands a one-param test to one case per row, reported `name[row]` and run in
    // isolation. A failing row is reported individually while the others pass; `#[Name]` renames the
    // base. The `total` counts cases (4 rows + 1 row = 5), not annotations.
    let file = temp_program(
        "test_data",
        "fn ok(n: int): bool { return n > 0 }\n\
         #[Data([1, 2, 0])]\n\
         @test fn positive(n: int): void { assert(ok(n)) }\n\
         #[Name(\"lengths\")]\n\
         #[Data([\"a\", \"bb\"])]\n\
         @test fn nonempty(s: string): void { assert(s != \"\") }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("ok    positive[1]")
                .and(predicate::str::contains("ok    positive[2]"))
                .and(predicate::str::contains("FAIL  positive[0]"))
                .and(predicate::str::contains("ok    lengths[\"a\"]"))
                .and(predicate::str::contains("ok    lengths[\"bb\"]"))
                .and(predicate::str::contains("4 passed, 1 failed, 5 total")),
        );
}

#[test]
fn test_data_type_mismatched_row_fails_that_case() {
    // A row whose literal does not match the parameter type fails just that case (a type error),
    // not the whole run.
    let file = temp_program(
        "test_data_mismatch",
        "#[Data([1, \"two\"])]\n@test fn t(n: int): void { assert(n > 0) }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("ok    t[1]")
                .and(predicate::str::contains("FAIL  t[\"two\"]"))
                .and(predicate::str::contains("1 passed, 1 failed, 2 total")),
        );
}

// --- `bench` (object-model slice 6: the `@bench` runner) ---------------------------

#[test]
fn bench_runs_and_reports_each_benchmark() {
    // `lang bench` discovers `@bench` blocks (block + annotation form), measures each, and reports
    // a per-iteration line. Timings are non-deterministic, so only the structure is asserted. The
    // program's own top-level `echo` does not run (the runner runs benches, not the file). A small
    // iteration count keeps the test fast.
    let file = temp_program(
        "bench_ok",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         echo \"main must not run\"\n\
         @bench(iterations: 5) fn small(): void { work(10) }\n\
         @bench(iterations: 5) { fn blocked(): void { work(10) } }\n",
    );
    lang().arg("bench").arg(&file).assert().success().stdout(
        predicate::str::contains("running 2 benchmarks")
            .and(predicate::str::contains("small"))
            .and(predicate::str::contains("blocked"))
            .and(predicate::str::contains("/iter"))
            .and(predicate::str::contains("2 ran, 0 failed, 2 total"))
            .and(predicate::str::contains("main must not run").not()),
    );
}

#[test]
fn bench_positional_iterations_arg_is_read() {
    // A positional `@bench(N)` sets the iteration count, the same as named `@bench(iterations: N)`
    // (name-based dispatch unlocked positional tier args, bound through the shared schema).
    let file = temp_program(
        "bench_positional",
        "fn work(n: int): int { return n }\n@bench(4) fn small(): void { work(1) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("small").and(predicate::str::contains("(4 iterations)")));
}

#[test]
fn bench_name_filter_and_json_report() {
    // `--name` runs exactly the named bench; `--json` reports one machine-readable object with
    // per-bench fields (the editor/CI seam, mirroring `noeta test --json`).
    let file = temp_program(
        "bench_ux",
        "fn work(n: int): int { return n }\n\
         @bench(iterations: 4) {\n\
             fn fast(): void { work(1) }\n\
             fn slow(): void { work(2) }\n\
         }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--name")
        .arg("fast")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("running 1 benchmark")
                .and(predicate::str::contains("fast"))
                .and(predicate::str::contains("slow").not()),
        );
    let out = lang()
        .arg("bench")
        .arg(&file)
        .arg("--name")
        .arg("fast")
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert_eq!(json["total"], 1);
    assert_eq!(json["failed"], 0);
    assert_eq!(json["benches"][0]["name"], "fast");
    assert_eq!(json["benches"][0]["iterations"], 4);
    assert!(json["benches"][0]["perIterNs"].is_f64());
}

#[test]
fn bench_calibrates_without_an_iteration_count() {
    // No `--iterations`, no `#[Bench]`: the count is calibrated (grown until a run meets the
    // time target), so the report shows a real count and a real measurement.
    let file = temp_program(
        "bench_calibrate",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         @bench fn body(): void { work(100) }\n",
    );
    let out = lang()
        .arg("bench")
        .arg(&file)
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    let iters = json["benches"][0]["iterations"].as_u64().expect("count");
    assert!(iters >= 64, "calibration must grow past the seed: {iters}");
}

#[test]
fn bench_baseline_saves_and_compares() {
    // `--save-baseline` persists a run (per entry file, in the cache dir); `--baseline` diffs
    // against it — the human report gains a delta, the JSON a `baselineDeltaPct`.
    // Enough per-iteration work that the two-point measurement is reliably non-zero — a zero
    // baseline has no defined delta.
    let file = temp_program(
        "bench_baseline",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         @bench(iterations: 2000) fn b(): void { work(500) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--save-baseline")
        .arg("cli-test")
        .assert()
        .success();
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("cli-test")
        .assert()
        .success()
        .stdout(predicate::str::contains("% vs cli-test"));
    let out = lang()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("cli-test")
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert!(json["benches"][0]["baselineDeltaPct"].is_f64());
    // An unknown baseline is a clear error.
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("nope")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no baseline `nope`"));
}

#[test]
fn bench_max_regress_gates_ci() {
    // The CI gate: an absurdly permissive limit passes; an impossible limit (any measurement
    // "regresses" past -1000%) fails with the offending bench named on stderr.
    let file = temp_program(
        "bench_gate",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         @bench(iterations: 2000) fn b(): void { work(500) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--save-baseline")
        .arg("gate")
        .assert()
        .success();
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("gate")
        .arg("--max-regress")
        .arg("100000")
        .assert()
        .success();
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("gate")
        .arg("--max-regress")
        .arg("-1000")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("regressed"));
}

#[test]
fn bench_invalid_arg_is_a_construction_error() {
    // Tier directive args construct the tier's config attribute (`@bench(iterations: true)` ⇒
    // `#[Bench(iterations: true)]`), so a wrong-typed knob is rejected by the ordinary attribute
    // construction gate (E0007, `bool` not assignable to `iterations: int`) — reported up front
    // rather than silently ignored.
    let file = temp_program(
        "bench_bad_arg",
        "@bench(iterations: true) fn b(): void { return }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007").and(predicate::str::contains("iterations")));
}

#[test]
fn bench_per_fn_attribute_overrides_block_arg() {
    // The block's `@bench(iterations: N)` is distribution sugar; a fn carrying its own
    // `#[Bench(…)]` keeps it — the per-fn knob wins.
    let file = temp_program(
        "bench_override",
        "fn work(n: int): int { return n }\n\
         @bench(iterations: 4) {\n\
             fn inherits(): void { work(1) }\n\
             #[Bench(iterations: 2)]\n\
             fn overrides(): void { work(1) }\n\
         }\n",
    );
    lang().arg("bench").arg(&file).assert().success().stdout(
        predicate::str::contains("inherits")
            .and(predicate::str::contains("(4 iterations)"))
            .and(predicate::str::contains("overrides"))
            .and(predicate::str::contains("(2 iterations)")),
    );
}

#[test]
fn bench_no_benches_is_success() {
    let file = temp_program("bench_none", "echo \"hi\"\n");
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("no benchmarks found"));
}

#[test]
fn bench_failing_body_is_reported() {
    // A `@bench` whose body aborts (a false `assert`) is a measurement failure, not a crash: the
    // bench is reported FAILED and the process exits non-zero.
    let file = temp_program(
        "bench_fail",
        "@bench(iterations: 2) fn boom(): void { assert(false) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("boom")
                .and(predicate::str::contains("FAILED"))
                .and(predicate::str::contains("1 total")),
        );
}

#[test]
fn bench_unknown_tier_is_e0036() {
    let file = temp_program("bench_badtier", "@bnch { fn x(): void { assert(true) } }\n");
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

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

// --- `--target` (object-model slice 6g: the `noeta.toml` build-target manifest) ----

/// Write a `noeta.toml` alongside a program in its private temp directory, returning the program
/// path. The manifest is discovered by walking up from the entry file's directory.
fn temp_project(name: &str, manifest: &str, src: &str) -> PathBuf {
    let path = temp_program(name, src);
    std::fs::write(path.parent().unwrap().join("noeta.toml"), manifest).expect("write noeta.toml");
    path
}

const TIERED_PROGRAM: &str = "fn f(x: int): void {\n\
         @debug { echo \"dbg ${x}\" }\n\
         echo \"out ${x}\"\n\
     }\n\
     @test fn t(): void { assert(1 + 1 == 2) }\n\
     f(5)\n";

#[test]
fn run_target_activates_its_tiers() {
    // A target that makes the `debug` tier live compiles the `@debug` block in, exactly as
    // `--tier debug` would — but driven by `noeta.toml`.
    let file = temp_project(
        "prof_run",
        "[targets.dev.tiers]\ndebug = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .success()
        .stdout("dbg 5\nout 5\n");
}

#[test]
fn run_minimalist_target_strips_everything() {
    // A target that opts into no tiers leaves every tier block stripped (same as a bare run).
    let file = temp_project("prof_run_min", "[targets.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("prod")
        .assert()
        .success()
        .stdout("out 5\n");
}

#[test]
fn test_target_gates_the_runner() {
    // `lang test --target prod`, where `prod` does not make `test` live, runs nothing and says so.
    let file = temp_project("prof_test_gate", "[targets.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--target")
        .arg("prod")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "tier `test` is not active in target `prod`",
        ));
}

#[test]
fn test_target_with_tier_live_runs() {
    let file = temp_project(
        "prof_test_live",
        "[targets.dev.tiers]\ntest = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("test")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn run_unknown_target_is_an_error() {
    let file = temp_project(
        "prof_unknown",
        "[targets.dev.tiers]\ndebug = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("ghost")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown target `ghost`"));
}

#[test]
fn run_target_without_manifest_is_an_error() {
    // `--target` with no `noeta.toml` anywhere above the entry is a clear error, not a silent run.
    let file = temp_program("prof_no_manifest", "echo \"hi\"\n");
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no `noeta.toml`"));
}

// --- `repl` -----------------------------------------------------------------------

#[test]
fn repl_persists_state_and_prints_trailing_expressions() {
    // A binding in one entry is visible later; a bare trailing expression is printed.
    lang()
        .arg("repl")
        .write_stdin("x = 5\necho x + 1;\n1 + 2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("6").and(predicate::str::contains("3")));
}

#[test]
fn repl_supports_multiline_blocks() {
    lang()
        .arg("repl")
        .write_stdin("fn dbl(n: int): int {\nreturn n * 2;\n}\ndbl(21)\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn repl_recovers_from_a_bad_entry() {
    // The first entry is a syntax error; the session keeps going and evaluates the second.
    lang()
        .arg("repl")
        .write_stdin("echo ;\necho \"ok\";\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"))
        .stderr(predicate::str::contains("E0003"));
}

// --- abort stack traces -----------------------------------------------------------

#[test]
fn a_nested_panic_prints_a_stack_trace() {
    let file = temp_program(
        "trace_nested",
        "fn inner(): int {\n    panic(\"boom\")\n}\nfn outer(): int {\n    return inner()\n}\nmut r = outer()\necho r\n",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "stack trace (most recent call first):",
        ))
        .stderr(predicate::str::contains("at inner (").and(predicate::str::contains(":2)")))
        .stderr(predicate::str::contains("at outer (").and(predicate::str::contains(":5)")))
        .stderr(predicate::str::contains("at main (").and(predicate::str::contains(":7)")));
}

#[test]
fn a_top_level_panic_prints_no_stack_trace() {
    // A single-frame abort's trace would only repeat the diagnostic's own location — omitted.
    let file = temp_program("trace_top", "echo \"before\"\npanic(\"top\")\n");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("panic: top"))
        .stderr(predicate::str::contains("stack trace").not());
}

#[test]
fn a_panicking_isolate_ships_its_stack_trace_home() {
    // The worker's own frames (explode ← the async body) cross the thread boundary and render
    // innermost, above the awaiting parent.
    let file = temp_program(
        "trace_isolate",
        "fn explode(n: int): int {\n    panic(\"worker exploded\")\n}\nasync fn work(n: int): int {\n    return explode(n)\n}\nconcurrent {\n    h = isolate work(5)\n    echo h.await\n}\n",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "isolate panicked: panic: worker exploded",
        ))
        .stderr(predicate::str::contains(
            "stack trace (most recent call first):",
        ))
        .stderr(predicate::str::contains("at explode (").and(predicate::str::contains(":2)")))
        // The async body's synthesized step closure inherits the fn's name (`Func::name`).
        .stderr(predicate::str::contains("at work ("));
}

#[test]
fn a_repl_panic_prints_a_stack_trace_and_the_session_continues() {
    lang()
        .arg("repl")
        .write_stdin(
            "fn boom(): int {\n    panic(\"repl boom\")\n}\nfn mid(): int {\n    return boom()\n}\nmid()\necho \"still alive\"\n",
        )
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "stack trace (most recent call first):",
        ))
        // Each entry is parsed with its own SourceId and kept, so a frame from a function defined in an
        // *earlier* entry resolves to that entry's real file and line — not the name-only degradation
        // the single-SourceId REPL produced. `boom` is entry 0 (panics on its line 2), `mid` entry 1.
        .stderr(predicate::str::contains("at boom (<repl:0>:2)"))
        .stderr(predicate::str::contains("at mid (<repl:1>:2)"))
        // The session survives the panic and evaluates the next entry.
        .stdout(predicate::str::contains("still alive"));
}

// --- `build` / `.noeb` bundles (P-AOT L1.2) -----------------------------------------

#[test]
fn build_then_run_bundle_matches_source_run() {
    // A bundle runs byte-for-byte like its source, but ships no `.noe`.
    let file = temp_program(
        "build_roundtrip",
        "fn sq(n: int): int { return n * n }\nmut t = 0\nfor i in 0..5 {\n    t = t + sq(i)\n}\necho t\n",
    );
    let bundle = file.with_extension("noeb");

    // Source run — the reference output.
    let src_out = lang().arg("run").arg(&file).assert().success();
    let src_stdout = String::from_utf8(src_out.get_output().stdout.clone()).unwrap();
    assert_eq!(src_stdout.trim(), "30");

    // Build the bundle, then run it — identical stdout, and the artifact starts with the magic.
    lang().arg("build").arg(&file).assert().success();
    let blob = std::fs::read(&bundle).expect("bundle written");
    assert_eq!(&blob[0..4], b"NOEB", "artifact carries the .noeb magic");

    lang()
        .arg("run")
        .arg(&bundle)
        .assert()
        .success()
        .stdout(predicate::str::diff(src_stdout));
}

// --- `build --exe` / self-contained executables (P-AOT L2) --------------------------

#[test]
fn build_exe_runs_the_program_with_no_source_or_bundle_alongside() {
    // `noeta build --exe -o app` staples the compiled program onto a copy of the LEAN `noeta-runner`
    // (dev-deps D4a — not the toolchain). The resulting `app` runs the program directly — no `.noe`,
    // no `.noeb`, no `noeta run` — and its stdout matches a source run byte-for-byte. Running it also
    // proves the startup trailer detection fires (the runner would otherwise print usage).
    let Some(runner) = lean_runner_path() else {
        return; // no build toolchain for the lean runner — skip.
    };
    let file = temp_program(
        "build_exe",
        "fn sq(n: int): int { return n * n }\nmut t = 0\nfor i in 0..5 {\n    t = t + sq(i)\n}\necho t\n",
    );
    let app = file.parent().unwrap().join("app");
    let _ = std::fs::remove_file(&app);

    lang()
        .env("NOETA_RUNNER", &runner)
        .arg("build")
        .arg(&file)
        .arg("--exe")
        .arg("-o")
        .arg(&app)
        .assert()
        .success()
        .stderr(predicate::str::contains("self-contained"));

    // The artifact carries neither the `.noeb` magic at its head (it starts with the runtime image)
    // nor the source; running it on its own yields the program's output.
    Command::new(&app).assert().success().stdout("30\n");
    let _ = std::fs::remove_file(&app);
}

#[test]
fn build_exe_reports_a_runtime_abort_from_the_stapled_program() {
    // A panic in a stapled exe surfaces as the same abort a source/bundle run gives — the embedded
    // program runs on the real host through the identical path, just with no source text to quote.
    let Some(runner) = lean_runner_path() else {
        return; // no build toolchain for the lean runner — skip.
    };
    let file = temp_program("build_exe_panic", "echo \"before\"\npanic(\"boom\")\n");
    let app = file.parent().unwrap().join("app_panic");
    let _ = std::fs::remove_file(&app);

    lang()
        .env("NOETA_RUNNER", &runner)
        .arg("build")
        .arg(&file)
        .arg("--exe")
        .arg("-o")
        .arg(&app)
        .assert()
        .success();

    Command::new(&app)
        .assert()
        .failure()
        .stdout("before\n")
        .stderr(predicate::str::contains("panic: boom"));
    let _ = std::fs::remove_file(&app);
}

// --- `build --native` / native AOT executables (P-AOT L3) ---------------------------

/// Build the AOT runtime staticlib (`libnoeta_aot.a`) for `--native` to link against, reusing the
/// workspace's `target/debug` (so its deps are already compiled), and return the archive path plus
/// the native-static-libs link line rustc reports. `None` if the toolchain can't produce it (no
/// `cargo`, or the build failed) — the caller then skips, so the differential is a no-op on a host
/// without a build toolchain rather than a spurious failure.
#[cfg(feature = "jit")]
fn build_aot_archive() -> Option<(PathBuf, String)> {
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(workspace())
        // The link line is scraped from rustc's `native-static-libs` note; under
        // `CARGO_TERM_COLOR=always` (CI) the note arrives ANSI-colored and a stray `\x1b[0m`
        // ends up inside the last `-l` flag. Force plain output regardless of ambient config.
        .env("CARGO_TERM_COLOR", "never")
        .args([
            "rustc",
            "-p",
            "noeta-aot-runtime",
            "--",
            "--print",
            "native-static-libs",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping native AOT test: building the runtime archive failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let notes = String::from_utf8_lossy(&output.stderr);
    let libs = notes
        .lines()
        .find_map(|l| l.split_once("native-static-libs:"))
        .map(|(_, libs)| libs.trim().to_string())
        .unwrap_or_default();
    let archive = workspace().join("target/debug/libnoeta_aot.a");
    archive.exists().then_some((archive, libs))
}

/// Whether a C toolchain (`cc`) is on PATH — `--native`'s linker. Overridable via `NOETA_CC`, as the
/// CLI's linker driver is.
#[cfg(feature = "jit")]
fn has_cc() -> bool {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    std::process::Command::new(cc)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[cfg(feature = "jit")] // `--native` exists only in the JIT-enabled build (it exits 2 otherwise).
fn build_native_matches_a_source_run_byte_for_byte() {
    // P-AOT L3.2b(3), the end-to-end AOT differential: `noeta build --native` compiles the eligible
    // prototypes to machine code, links them into a native binary, and staples the bundle on. That
    // binary — dispatching native bodies for the hot loop / `sq` / `fib` and interpreting the rest —
    // must produce exactly what `noeta run` produces on the same source. This is the linked-binary
    // proof the in-process test (`aot_bound_dispatch_runs_native_in_process`) forecast: only the
    // linker was unproven, and here it runs for real.
    let Some((archive, libs)) = build_aot_archive() else {
        return; // no build toolchain for the runtime archive — skip.
    };
    if !has_cc() {
        eprintln!("skipping native AOT test: no `cc` on PATH");
        return;
    }

    let src = "fn sq(n: int): int { return n * n }\n\
               fn fib(n: int): int { if n < 2 { return n }\n  return fib(n - 1) + fib(n - 2) }\n\
               mut t = 0\nfor i in 0..1000 { t = t + sq(i) }\n\
               echo t\necho fib(20)\necho \"done\"\n";
    let file = temp_program("build_native", src);
    let app = file.parent().unwrap().join("app_native");
    let _ = std::fs::remove_file(&app);

    // Reference: a plain source run.
    let reference = lang().arg("run").arg(&file).assert().success();
    let expected = String::from_utf8(reference.get_output().stdout.clone()).unwrap();

    // Build the native binary, pointing the linker driver at the archive we just built.
    lang()
        .arg("build")
        .arg(&file)
        .arg("--native")
        .arg("-o")
        .arg(&app)
        .env("NOETA_AOT_RUNTIME_LIB", &archive)
        .env("NOETA_AOT_LINK_LIBS", &libs)
        .assert()
        .success()
        .stderr(predicate::str::contains("native AOT"));

    // The native binary runs on its own and matches the source run exactly.
    Command::new(&app).assert().success().stdout(expected);
    let _ = std::fs::remove_file(&app);
}

/// The names of every section in an ELF64 (LE) image — enough to assert a binary was stripped.
/// Hand-rolled so the test needs no `object`/`goblin` dependency; returns `None` for a non-ELF64
/// input (e.g. a macOS Mach-O host), letting the caller skip rather than false-fail.
#[cfg(feature = "jit")]
fn elf_section_names(bytes: &[u8]) -> Option<Vec<String>> {
    let u16le = |o: usize| -> Option<usize> {
        Some(u16::from_le_bytes(bytes.get(o..o + 2)?.try_into().ok()?) as usize)
    };
    let u32le = |o: usize| -> Option<usize> {
        Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?) as usize)
    };
    let u64le = |o: usize| -> Option<usize> {
        Some(u64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?) as usize)
    };
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(4)? != 2 {
        return None; // not ELF64 — skip on this host.
    }
    let (shoff, shentsize, shnum, shstrndx) = (u64le(40)?, u16le(58)?, u16le(60)?, u16le(62)?);
    // The section-name string table blob, located via the shstrndx'th section header.
    let strh = shoff + shstrndx * shentsize;
    let (str_off, str_size) = (u64le(strh + 24)?, u64le(strh + 32)?);
    let strtab = bytes.get(str_off..str_off + str_size)?;
    let mut names = Vec::with_capacity(shnum);
    for i in 0..shnum {
        let name_off = u32le(shoff + i * shentsize)?;
        let end = strtab[name_off..].iter().position(|&b| b == 0)? + name_off;
        names.push(String::from_utf8_lossy(&strtab[name_off..end]).into_owned());
    }
    Some(names)
}

#[test]
#[cfg(feature = "jit")] // `--native` exists only in the JIT-enabled build.
fn build_native_strips_debug_info_from_the_shipped_binary() {
    // A shipped `--native` artifact carries no native debug symbols (`-s` at link, native-size slice
    // 1): its panic tracebacks come from the bundle's own line table, not DWARF, so stripping is free
    // and halves the image (~11 MB → ~5.8 MB on a core program). Guard it structurally — assert the
    // ELF has no `.debug_*` or `.symtab`/`.strtab` sections — rather than by a brittle size ceiling.
    let Some((archive, libs)) = build_aot_archive() else {
        return; // no build toolchain for the runtime archive — skip.
    };
    if !has_cc() {
        eprintln!("skipping native strip test: no `cc` on PATH");
        return;
    }
    let file = temp_program("build_native_strip", "echo \"ok\"\n");
    let app = file.parent().unwrap().join("app_native_strip");
    let _ = std::fs::remove_file(&app);
    lang()
        .arg("build")
        .arg(&file)
        .arg("--native")
        .arg("-o")
        .arg(&app)
        .env("NOETA_AOT_RUNTIME_LIB", &archive)
        .env("NOETA_AOT_LINK_LIBS", &libs)
        .assert()
        .success();

    // Still runs (the stapled bundle survived the strip — it is appended *after* the linked ELF that
    // `-s` stripped) …
    Command::new(&app).assert().success().stdout("ok\n");

    // … and carries no debug/symbol sections.
    let bytes = std::fs::read(&app).unwrap();
    if let Some(sections) = elf_section_names(&bytes) {
        // `.debug_gdb_scripts` is a ~40-byte gdb-autoload stub cranelift emits into the AOT object,
        // not DWARF — `-s` leaves it and it costs nothing, so it is not what "stripped" is about here.
        let leftover: Vec<_> = sections
            .iter()
            .filter(|n| {
                (n.starts_with(".debug") && n.as_str() != ".debug_gdb_scripts")
                    || n.as_str() == ".symtab"
                    || n.as_str() == ".strtab"
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "shipped --native binary should be stripped, but found sections: {leftover:?}"
        );
    }
    let _ = std::fs::remove_file(&app);
}

#[test]
fn aot_runtime_does_not_link_the_compiler_frontend() {
    // native-size slice 2 (structural guard): a shipped `--native` binary runs a *pre-compiled*
    // stapled bundle and must never carry the type checker / compiler / IR pipeline — dead weight in
    // a run-only artifact AND reachable attack surface (the same property the dev-deps arc gave dev
    // tooling, one layer down: L2 out of a shipped L1). The AOT runtime opts out of noeta-vm's
    // `compile` feature; assert the front-end is absent from its (non-dev) dependency graph, so a
    // future edit that re-links it (a new default feature, a compiler dep on noeta-runtime) fails HERE
    // rather than silently re-bloating and re-arming the artifact.
    let out = Command::new(env!("CARGO"))
        .current_dir(workspace())
        .args([
            "tree",
            "-p",
            "noeta-aot-runtime",
            "-e",
            "no-dev",
            "--prefix",
            "none",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    for forbidden in [
        "noeta-compiler v",
        "noeta-check v",
        "noeta-ir v",
        "noeta-ir-passes v",
    ] {
        assert!(
            !tree.contains(forbidden),
            "the AOT runtime must not link `{}` — a run-only artifact carries no compiler front-end \
             (native-size slice 2). Graph:\n{tree}",
            forbidden.trim_end_matches(" v")
        );
    }
}

#[test]
fn bundle_run_rejects_build_time_flags() {
    // Tiers are baked at build time; passing them to a bundle run is a usage error, not a silent
    // no-op.
    let file = temp_program("build_flag_reject", "echo 1\n");
    lang().arg("build").arg(&file).assert().success();
    let bundle = file.with_extension("noeb");
    lang()
        .arg("run")
        .arg(&bundle)
        .arg("--tier")
        .arg("debug")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("apply at build time"));
}

#[test]
fn repl_check_skips_an_ill_typed_entry_and_keeps_the_session_usable() {
    // Under --check (session-checker C2): entry 2 retypes a mut binding → E0007 printed, entry
    // SKIPPED (x keeps its value); entry 3 still runs against the intact session.
    lang()
        .arg("repl")
        .write_stdin("mut x = 5\nx = \"s\"\necho x + 1;\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E0007"))
        .stdout(predicate::str::contains("6"));
}

#[test]
fn repl_check_applies_static_rules_the_unchecked_repl_defers() {
    // A required-signature violation (E0022) is static-only: the unchecked REPL would run it.
    lang()
        .arg("repl")
        .write_stdin("fn f(n) { return n }\necho 1 + 1;\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E0022"))
        .stdout(predicate::str::contains("2"));
}

#[test]
fn repl_check_toggles_at_the_prompt() {
    // ON by default: the retype is rejected (E0007). After `:check off`, the same shape runs
    // (checkerless semantics — the pre-C2 behavior).
    lang()
        .arg("repl")
        .write_stdin("mut a = 1\na = \"s\"\n:check off\nmut b = 2\nb = \"t\"\necho b ~ \"!\";\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E0007"))
        .stdout(predicate::str::contains("t!"));
}

#[test]
fn repl_no_check_flag_restores_checkerless_sessions() {
    lang()
        .arg("repl")
        .arg("--no-check")
        .write_stdin("mut a = 1\na = \"s\"\necho a ~ \"!\";\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("s!"));
}

#[test]
fn repl_checked_codegen_gives_type_of_full_fidelity() {
    // C5: a fully-checked session compiles entries with the checker's site bundle — `type_of` on a
    // list literal recovers the element type, exactly as `noeta run` does...
    lang()
        .arg("repl")
        .write_stdin("echo type_of([1, 2]);\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Type.List(Type.Int)"));
    // ...while a checkerless session stays on the conservative head-only codegen.
    lang()
        .arg("repl")
        .arg("--no-check")
        .write_stdin("echo type_of([1, 2]);\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Type.List(Type.Dyn)"));
}

#[test]
fn repl_load_bootstraps_a_session_with_the_programs_context() {
    // The "tinker" shape: the bootstrap runs to completion (its output first), then the prompt
    // opens with its functions, types, and bindings live — and entries check against them.
    let path = temp_program(
        "repl_load",
        "fn twice(n: int): int { return n * 2 }\nmut base = 21\necho \"booted\"\n",
    );
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&path)
        .write_stdin("echo twice(base);\nbase = 5\ntwice(base)\n")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("booted")
                .and(predicate::str::contains("42"))
                .and(predicate::str::contains("10")),
        );
}

#[test]
fn repl_load_entries_type_check_against_the_bootstrap() {
    // A wrong-arity call against a BOOTSTRAP function is a static error at the prompt.
    let path = temp_program(
        "repl_load_check",
        "fn twice(n: int): int { return n * 2 }\n",
    );
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&path)
        .write_stdin("twice(1, 2)\necho twice(3);\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E00"))
        .stdout(predicate::str::contains("6"));
}

#[test]
fn repl_load_fails_fast_on_a_broken_bootstrap() {
    // A bootstrap that does not type-check exits with its diagnostics — no broken prompt.
    let bad_check = temp_program("repl_load_bad_check", "mut x: int = \"s\"\n");
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&bad_check)
        .write_stdin("echo 1;\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0007"));
    // A bootstrap that panics at run time exits with the abort — same rule.
    let panics = temp_program("repl_load_panics", "panic(\"boom\")\n");
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&panics)
        .write_stdin("echo 1;\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("boom"));
}

// --- `check` (static analysis, no run/build) ---------------------------------------

/// Create a private temp *directory* holding several named `.noe` files and return the directory.
/// Directory isolation matters for the same reason as `temp_program`: the loader treats every
/// sibling `.noe` file as a candidate module, so a shared temp dir would cross-contaminate.
fn temp_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noeta_cli_test_{name}"));
    // Start from a clean directory so a rerun does not see a previous run's stray files.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create nested dir");
        }
        std::fs::write(&path, src).expect("write temp file");
    }
    dir
}

#[test]
fn check_clean_file_succeeds() {
    let file = temp_program(
        "check_clean",
        "fn add(a: int, b: int): int { return a + b }\necho add(2, 3)\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"));
}

#[test]
fn check_type_error_exits_1() {
    let file = temp_program("check_type_err", "echo 1 + true\n");
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("1 error(s)"));
}

#[test]
fn check_syntax_error_exits_1() {
    let file = temp_program("check_syntax_err", "echo $;\n");
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0001"));
}

#[test]
fn check_directory_is_recursive_and_attributes_errors_to_files() {
    // A clean file at the root and an erroring file in a subdirectory: the recursive walk finds both,
    // the directory check fails, and the error renders against the nested file.
    let dir = temp_dir(
        "check_tree",
        &[
            ("a.noe", "fn ok(): int { return 1 }\n"),
            ("sub/bad.noe", "echo 1 + true\n"),
        ],
    );
    lang()
        .arg("check")
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("bad.noe"))
        .stderr(predicate::str::contains("2 files"));
}

#[test]
fn check_shared_erroring_module_is_reported_once() {
    // `m.noe` has one error and is imported by two entries (and is itself an entry in the walk), so it
    // is linked/checked three times — but global dedup means the diagnostic is rendered exactly once.
    let dir = temp_dir(
        "check_shared",
        &[
            (
                "m.noe",
                "namespace App.M;\npub fn boom(): int { return 1 + true }\n",
            ),
            ("main1.noe", "use App.M.{boom}\necho boom()\n"),
            ("main2.noe", "use App.M.{boom}\necho boom()\n"),
        ],
    );
    let out = lang().arg("check").arg(&dir).assert().failure().code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert_eq!(
        stderr.matches("E0007").count(),
        1,
        "the shared module's error is deduplicated to a single rendering:\n{stderr}"
    );
    assert!(stderr.contains("1 error(s)"), "{stderr}");
}

#[test]
fn check_empty_directory_exits_2() {
    let dir = temp_dir("check_empty", &[]);
    lang()
        .arg("check")
        .arg(&dir)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no `.noe` files"));
}

#[test]
fn check_json_emits_a_machine_readable_report_on_stdout() {
    let file = temp_program("check_json_err", "echo 1 + true\n");
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        // The report goes to stdout; stderr carries no human diagnostics in JSON mode.
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["files_checked"], 1);
    assert_eq!(report["errors"], 1);
    assert_eq!(report["warnings"], 0);
    let diags = report["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "E0007");
    assert_eq!(diags[0]["severity"], "error");
    assert_eq!(diags[0]["line"], 1);
    assert!(diags[0]["file"].as_str().unwrap().ends_with("main.noe"));
}

#[test]
fn check_json_clean_is_an_empty_diagnostics_array() {
    let file = temp_program(
        "check_json_ok",
        "fn id(n: int): int { return n }\necho id(1)\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["errors"], 0);
    assert!(report["diagnostics"].as_array().unwrap().is_empty());
}

// --- `fmt` ------------------------------------------------------------------------

/// A private temp directory for a formatter test (no `noeta.toml`, so defaults apply).
fn fmt_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noeta_fmt_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn fmt_stdin_formats_to_stdout() {
    lang()
        .args(["fmt", "--stdin"])
        .write_stdin("fn  f( a ){\n echo a\n}\n")
        .assert()
        .success()
        .stdout("fn f(a) {\n    echo a\n}\n");
}

#[test]
fn fmt_check_lists_unformatted_and_exits_nonzero() {
    let dir = fmt_dir("check");
    let file = dir.join("a.noe");
    std::fs::write(&file, "echo   1\n").unwrap();
    lang()
        .args(["fmt", "--check"])
        .arg(&file)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("a.noe"));
    // --check must not modify the file.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "echo   1\n");
}

#[test]
fn fmt_rewrites_in_place_then_is_clean() {
    let dir = fmt_dir("inplace");
    let file = dir.join("a.noe");
    std::fs::write(&file, "echo   1\n").unwrap();
    lang().arg("fmt").arg(&file).assert().success();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "echo 1\n");
    // Now clean: --check succeeds and lists nothing.
    lang()
        .args(["fmt", "--check"])
        .arg(&file)
        .assert()
        .success();
}

#[test]
fn fmt_declines_unparseable_source() {
    lang()
        .args(["fmt", "--stdin"])
        .write_stdin("fn (\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("does not parse"));
}

// --- package manager: path dependencies (P2.1) --------------------------------------------------

/// Lay out an app + a dependency package under a unique base dir, returning the app's entry path.
/// The app keys the dependency `hi`; the package's own root namespace segment is `greet` (from
/// `acme/greet`), so the loader re-roots `greet.*` → `hi.*` (key ≠ root exercises the rewrite).
fn path_dep_project(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("greetlib");
    std::fs::create_dir_all(&app).expect("mk app");
    std::fs::create_dir_all(&lib).expect("mk lib");
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nhi = { path = \"../greetlib\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use hi.hello.greeting;\necho greeting();\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    // The package's public fn calls a helper in a *second* package module — a package-internal
    // cross-reference the consumer never names, which must still resolve (closed-unit linking).
    std::fs::write(
        lib.join("hello.noe"),
        "namespace greet.hello;\nuse greet.util.punct;\n\
         pub fn greeting(): string { return punct(); }\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("util.noe"),
        "namespace greet.util;\npub fn punct(): string { return \"hi from the dependency\"; }\n",
    )
    .unwrap();
    app.join("main.noe")
}

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

// --- package manager: git-tag dependencies (P2.3) -----------------------------------------------

/// Run a `git` command in `cwd`, asserting success (identity env set so commits work in CI).
fn git_in(args: &[&str], cwd: &std::path::Path) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Commit `manifest` as this repo's `noeta.toml` and tag it — one released version per call.
fn commit_version(repo: &std::path::Path, tag: &str, manifest: &str) {
    std::fs::write(repo.join("noeta.toml"), manifest).unwrap();
    git_in(&["add", "."], repo);
    git_in(&["commit", "-q", "-m", tag], repo);
    git_in(&["tag", tag], repo);
}

/// The commit SHA a tag points at (for the registry index entry).
fn git_sha(repo: &std::path::Path, tag: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "rev-parse", tag])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

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

    // TOFU: replace the registry's scope key with a *different* one — a later resolve must reject it.
    std::fs::write(reg.join("scope__acme.pub"), format!("{}\n", "c".repeat(64))).unwrap();
    lang()
        .env("NOETA_REGISTRY_DIR", &reg)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("changed"));
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
        "version = 1\n\n[[scope]]\nname = \"acme\"\n\
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
            "version = 1\n\n[[scope]]\nname = \"acme\"\npublic_key = \"{}\"\n",
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
    // forged bundle) is rejected against the pin.
    let pinned_elsewhere = lock.replace(
        "acme/greet/.github/workflows/release.yaml",
        "mallory/greet/.github/workflows/release.yaml",
    );
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
        .stdout(predicate::str::contains("added `hi`"))
        // The key `hi` differs from the package's own module root `lib` — a legitimate rename, but
        // `add` warns so `use hi.…` (not `use lib.…`) isn't a surprise (namespace-protection #3).
        .stderr(predicate::str::contains("module root is `lib`"));

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
    std::fs::write(
        lib.join("m.noe"),
        "namespace widgets.core;\npub fn v(): int { return 1; }\n",
    )
    .unwrap();

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
    std::fs::write(lib.join("m.noe"), "namespace lib.core;\n").unwrap();

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
fn noeta_claim_requires_the_hosted_registry() {
    // namespace-protection #1: claiming a scope talks to the hosted registry — without a configured
    // URL, `noeta claim` explains that rather than failing opaquely.
    lang()
        .env_remove("NOETA_REGISTRY_URL")
        .args(["claim", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs the hosted registry"));
}

#[test]
fn noeta_claim_guides_when_not_in_ci() {
    // With a registry URL but no GitHub Actions OIDC environment, `noeta claim` can't prove ownership
    // — it prints actionable guidance (run from a workflow granting `id-token: write`) and exits 1,
    // without ever contacting the registry.
    lang()
        .env("NOETA_REGISTRY_URL", "https://registry.invalid")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .args(["claim", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("GitHub Actions OIDC token"));
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

// --- package manager: composed toolchain (Phase 3, N3.2/N3.3) -----------------------------------

/// Lay out an app + a dependency package carrying a **native entry crate** (the Phase-3 proving
/// package): module `fx` (plain dispatch), extern type `Acc` (plain methods + a higher-order ctx
/// method), and an `fx-info` ExtCommand. The crate depends on this workspace's `noeta-native` by
/// path and exports the composition convention symbol `NOETA_EXTENSIONS`.
fn composed_project(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let dep = base.join("imgfx");
    let krate = dep.join("native");
    std::fs::create_dir_all(&app).expect("mk app");
    std::fs::create_dir_all(krate.join("src")).expect("mk crate");

    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nimgfx = { path = \"../imgfx\" }\n\
         [trust]\nnative = [\"acme/imgfx\"]\ncommands = [\"acme/imgfx\"]\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use imgfx.{fx}\n\n\
         @packed(layout: column) struct Px { r: f32; g: f32; b: f32 }\n\n\
         a = fx.acc();\n\
         a.add(2);\n\
         a.apply(fn(t) => t * 10);\n\
         echo fx.double(21);\n\
         echo a.total();\n\n\
         // The raw-buffer seam, third-party edition (N3.4): the extension's column kernel\n\
         // reduces the app's own @packed type, and its COW-mutating kernel produces a new list.\n\
         impl fx.Pixels for Px {}\n\
         ps = [Px { r: 0.25f32, g: 1.0f32, b: 2.0f32 }, Px { r: 0.5f32, g: 1.0f32, b: 2.0f32 }];\n\
         echo fx.sum_r(ps);\n\
         bright = fx.brighten_all(ps, 0.5f32);\n\
         echo bright[1].g;\n\
         echo ps[1].g;\n\
         echo ps.brighten(2.0f32)[0].r;\n",
    )
    .unwrap();
    std::fs::write(
        app.join("bad.noe"),
        "use imgfx.{fx}\n\necho fx.double(\"nope\");\n",
    )
    .unwrap();

    std::fs::write(
        dep.join("noeta.toml"),
        "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
    )
    .unwrap();

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    std::fs::write(
        krate.join("Cargo.toml"),
        format!(
            "[package]\nname = \"imgfx-native\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\nnoeta-native = {{ path = \"{}\" }}\n\n\
             # dev-deps D5: the mixed package gates its dev formatter behind `fmt` (a real crate would\n\
             # also put `malva`/etc. behind it as `dep:`). Off by default — a shipped runner never\n\
             # enables it; the dev toolchain does.\n\
             [features]\nfmt = []\n\n[workspace]\n",
            workspace.join("crates").join("noeta-native").display()
        ),
    )
    .unwrap();
    std::fs::write(krate.join("src").join("lib.rs"), IMGFX_NATIVE_SRC).unwrap();
    app.join("main.noe")
}

/// The proving extension's Rust source (see [`composed_project`]).
const IMGFX_NATIVE_SRC: &str = r##"
//! The Phase-3 proving extension: one module, one extern type with plain + ctx methods, one
//! CLI command — exercised end-to-end through toolchain composition.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrd};

use noeta_native::registry::{
    BundleFn, BundleReceiver, ConstraintField, ConstraintLayout, ExtBundle, ExtFn, ExtModule,
    ExtType, Extension, NativeOut, NativeValue, PackedConstraint, RetTy, Scalar, SigType,
};
use noeta_native::{
    no_function_error, no_method_error, CommandCtx, CtxError, CtxOut, ErrorKind, ExtCommand,
    ExternValue, Host, NativeCtx, ParsedArgs, Slot, StdError,
};

const FX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "double",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "acc",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Acc")),
    },
];

fn fx_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "double" => match args.first() {
            Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(NativeOut::Scalar(Scalar::Int(n * 2))),
            _ => Err(StdError {
                kind: ErrorKind::ArgType,
                message: "`fx.double` expects an int".to_string(),
            }),
        },
        "acc" => Ok(NativeOut::Extern(noeta_native::ExternBox(Box::new(
            Acc::default(),
        )))),
        _ => Err(no_function_error("fx", func)),
    }
}

// The raw-buffer seam (package-manager N3.4), third-party edition: kernels over the CONSUMER's
// own `@packed` pixel type — a column reduction (zero per-element traffic) and a COW-mutating
// transform producing a new list.
const FX_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "sum_r",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::F32),
    },
    ExtFn {
        name: "brighten_all",
        params: &[SigType::Dyn, SigType::F32],
        ret: RetTy::SameAsArg(0),
    },
];

fn packed_error(func: &str) -> CtxError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("`fx.{func}` expects a packed pixel list"),
    }
    .into()
}

fn fx_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        // Sum the first (`r`) component across the buffer, layout-aware through the neutral
        // view: a column list's `r`s are one contiguous run; a row list strides.
        "sum_r" => {
            let mut sum: Option<f32> = None;
            ctx.with_packed(args[0], &mut |v, bytes| {
                if v.fields.len() == 3 {
                    let (run, step) = if v.column {
                        (&bytes[..v.count * 4], 4)
                    } else {
                        (bytes, v.byte_size)
                    };
                    sum = Some(
                        run.chunks(step)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .sum(),
                    );
                }
            })?;
            match sum {
                Some(s) => Ok(CtxOut::Out(NativeOut::Scalar(Scalar::F32(s)))),
                None => Err(packed_error(func)),
            }
        }
        // Add `delta` to every component — value semantics through the copy-on-write mutable
        // borrow; the transformed list arrives as a fresh slot, the input stays intact.
        "brighten_all" => {
            let NativeValue::Scalar(Scalar::F32(delta)) = ctx.view(args[1])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: "`fx.brighten_all` expects an f32 delta".to_string(),
                }
                .into());
            };
            match ctx.with_packed_mut(args[0], &mut |_, bytes| {
                for c in bytes.chunks_exact_mut(4) {
                    let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]) + delta;
                    c.copy_from_slice(&v.to_le_bytes());
                }
            })? {
                Some(result) => Ok(CtxOut::Slot(result)),
                None => Err(packed_error(func)),
            }
        }
        _ => Err(no_function_error("fx", func).into()),
    }
}

#[derive(Debug, Default)]
struct Acc {
    total: AtomicI64,
}

impl ExternValue for Acc {
    fn type_name(&self) -> &'static str {
        "Acc"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other
            .as_any()
            .downcast_ref::<Acc>()
            .is_some_and(|o| o.total.load(AtomicOrd::Relaxed) == self.total.load(AtomicOrd::Relaxed))
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<acc {}>", self.total.load(AtomicOrd::Relaxed))
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(Acc {
            total: AtomicI64::new(self.total.load(AtomicOrd::Relaxed)),
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

const ACC_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "add",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "total",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
];

fn acc_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let acc = recv
        .as_any_mut()
        .downcast_mut::<Acc>()
        .expect("receiver is an Acc");
    match method {
        "add" => match args.first() {
            Some(NativeValue::Scalar(Scalar::Int(n))) => {
                acc.total.fetch_add(*n, AtomicOrd::Relaxed);
                Ok(NativeOut::Unit)
            }
            _ => Err(StdError {
                kind: ErrorKind::ArgType,
                message: "`Acc.add` expects an int".to_string(),
            }),
        },
        "total" => Ok(NativeOut::Scalar(Scalar::Int(
            acc.total.load(AtomicOrd::Relaxed),
        ))),
        _ => Err(no_method_error("Acc", method)),
    }
}

const ACC_CTX_METHODS: &[ExtFn] = &[ExtFn {
    name: "apply",
    params: &[SigType::Fn(&[SigType::Int], &SigType::Int)],
    ret: RetTy::Concrete(SigType::Unit),
}];

/// `acc.apply(f)` — replace the total with `f(total)`: the higher-order ctx seam, third-party
/// edition (closure call-back through `NativeCtx`).
fn acc_ctx_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "apply" => {
            let mut total = 0;
            ctx.with_extern(recv, &mut |e| {
                if let Some(acc) = e.as_any().downcast_ref::<Acc>() {
                    total = acc.total.load(AtomicOrd::Relaxed);
                }
            })?;
            let arg = ctx.intern(NativeOut::Scalar(Scalar::Int(total)))?;
            let out = ctx.call(args[0], &[arg])?;
            let NativeValue::Scalar(Scalar::Int(new_total)) = ctx.view(out)? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: "`Acc.apply` closure must return an int".to_string(),
                }
                .into());
            };
            ctx.free(arg);
            ctx.free(out);
            ctx.with_extern(recv, &mut |e| {
                if let Some(acc) = e.as_any().downcast_ref::<Acc>() {
                    acc.total.store(new_total, AtomicOrd::Relaxed);
                }
            })?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(no_method_error("Acc", method).into()),
    }
}

const FX_INFO: ExtCommand = ExtCommand {
    name: "fx-info",
    about: "Prove an extension-contributed command dispatches through composition",
    args: &[],
    run: fx_info_run,
};

fn fx_info_run(_ctx: &mut dyn CommandCtx, _args: &ParsedArgs) -> u8 {
    println!("imgfx: native extension ok");
    0
}

// A third-party METHOD BUNDLE (kernel-methods K6): the consumer's own @packed pixel type opts in
// with `impl fx.Pixels for Px {}` and gains `ps.brighten(delta)` — same COW raw-buffer kernel as
// `fx.brighten_all`, in method position, statically routed through the composed toolchain.
const PIXELS_BUNDLE: ExtBundle = ExtBundle {
    name: "Pixels",
    constraint: PackedConstraint {
        fields: &[
            ConstraintField::F32,
            ConstraintField::F32,
            ConstraintField::F32,
        ],
        layout: ConstraintLayout::Any,
    },
    methods: &[BundleFn {
        sig: ExtFn {
            name: "brighten",
            params: &[SigType::F32],
            ret: RetTy::SameAsArg(0),
        },
        receiver: BundleReceiver::Bulk,
    }],
    ctx_dispatch: pixels_bundle_dispatch,
};

fn pixels_bundle_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: noeta_native::Slot,
    args: &[noeta_native::Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        // `ps.brighten(delta)` ≡ `fx.brighten_all(ps, delta)` — one kernel, two surfaces.
        "brighten" => {
            let mut all = Vec::with_capacity(args.len() + 1);
            all.push(recv);
            all.extend_from_slice(args);
            fx_ctx_dispatch("brighten_all", ctx, &all)
        }
        _ => Err(no_method_error("fx.Pixels", method).into()),
    }
}

#[derive(Debug, Clone, Copy)]
struct ImgfxExtension;

impl Extension for ImgfxExtension {
    fn name(&self) -> &'static str {
        "imgfx"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "fx",
            functions: FX_FNS,
            dispatch: fx_dispatch,
            ctx_functions: FX_CTX_FNS,
            ctx_dispatch: Some(fx_ctx_dispatch),
            bundles: &[PIXELS_BUNDLE],
            ..ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[ExtType {
            name: "Acc",
            methods: ACC_METHODS,
            dispatch: acc_method_dispatch,
            ctx_methods: ACC_CTX_METHODS,
            ctx_dispatch: Some(acc_ctx_dispatch),
            ..ExtType::DEFAULTS
        }]
    }
    fn commands(&self) -> &'static [ExtCommand] {
        &[FX_INFO]
    }
    // dev-deps D5: a DEV-only capability — a tier-body formatter — gated behind the `fmt` feature.
    // The runtime capabilities above (module/type/command) always compile; this one, and the marker
    // string it carries, only when `fmt` is enabled. A shipped composed runner is built with default
    // features (fmt OFF), so the formatter and marker are absent from the artifact; the dev toolchain
    // would enable `fmt` to reflow this extension's tier bodies under `noeta fmt`.
    #[cfg(feature = "fmt")]
    fn body_formatters(&self) -> &'static [noeta_native::registry::BodyFormatter] {
        &[("imgfx", imgfx_reformat)]
    }
}

/// The gated dev formatter (see `body_formatters`). Its distinctive marker proves compilation: it is
/// in the binary iff the `fmt` feature was on.
#[cfg(feature = "fmt")]
fn imgfx_reformat(
    body: &str,
    _indent: &str,
    _sub: &noeta_native::registry::SubFormat,
) -> Option<String> {
    const MARKER: &str = "IMGFX_FMT_ONLY_MARKER_7c4e9a";
    Some(format!("{MARKER}:{}", body.trim()))
}

/// The composition convention (package-manager Phase 3): the entry crate exports its units as a
/// slice — one crate, any number of units.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ImgfxExtension];
"##;

/// Point the compose build at the workspace's existing debug artifacts (the shim links the
/// already-built noeta-cli lib in seconds instead of a cold release build).
/// Serializes the composition-heavy e2e tests. Each shells out to `cargo` into the **shared**
/// workspace target dir (`composed_env`'s `NOETA_COMPOSE_TARGET_DIR`, set for speed so the composes
/// reuse the workspace's already-built debug deps), where every shim crate is named `noeta-composed`.
/// Running two at once lets cargo's concurrent manifest resolution trip over that shared artifact
/// (`can't find bin … src/main.rs`). Production never points two composes at one target dir, so this
/// is purely a test-harness concern — the guard runs these few tests one at a time. Poison-tolerant:
/// a panicking compose test must not wedge the others.
static COMPOSE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn compose_guard() -> std::sync::MutexGuard<'static, ()> {
    COMPOSE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn composed_env(cmd: &mut Command) -> &mut Command {
    cmd.env("NOETA_COMPOSE_DEBUG", "1").env(
        "NOETA_COMPOSE_TARGET_DIR",
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).parent().unwrap(),
    )
}

#[test]
fn build_exe_of_a_native_dep_app_strips_the_mixed_crates_formatter() {
    let _guard = compose_guard();
    // dev-deps D5, the capstone: a shipped native-dependency app carries its runtime handler but not
    // the mixed crate's dev formatter. `build --exe` composes a RUNNER (lean base + imgfx runtime
    // extension, `fmt` OFF) and staples the bundle. We prove both halves:
    //   1. the artifact RUNS the native handler (`fx.double(21)` → 42) — the extension is composed in;
    //   2. the gated formatter is STRIPPED — its distinctive marker is absent from the binary.
    let entry = composed_project("d5_exe_strip");
    let app_bin = entry.parent().unwrap().join("app_native_exe");
    let _ = std::fs::remove_file(&app_bin);

    // The runner composition needs the lean runner binary as its base? No — the *composed* runner IS
    // the base (built from the shim). `composed_env` reuses the workspace's debug artifacts so this
    // stays a fast debug composition rather than a cold release build.
    composed_env(&mut lang())
        .arg("build")
        .arg(&entry)
        .arg("--exe")
        .arg("-o")
        .arg(&app_bin)
        .assert()
        .success()
        .stderr(predicate::str::contains("self-contained"));

    // 1. Runs the native handler — success alone proves it (an unknown `imgfx` module would abort);
    //    the first echoed line is `fx.double(21)` = 42.
    Command::new(&app_bin)
        .assert()
        .success()
        .stdout(predicate::str::starts_with("42\n"));

    // 2. The dev formatter is absent from the shipped artifact.
    let bytes = std::fs::read(&app_bin).expect("read the artifact");
    let marker = b"IMGFX_FMT_ONLY_MARKER_7c4e9a";
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker),
        "the composed runner leaked the mixed crate's dev formatter into the shipped artifact — \
         the `fmt` feature was not stripped"
    );
    let _ = std::fs::remove_file(&app_bin);
}

/// Find the composed binary a delegation cached under an (isolated) compose cache dir —
/// `<cache>/compose/<key>/bin/noeta-composed`. Exactly one exists per distinct composition.
fn find_composed_binary(cache: &std::path::Path) -> Option<PathBuf> {
    let compose = cache.join("compose");
    for key in std::fs::read_dir(&compose).ok()? {
        let bin = key.ok()?.path().join("bin").join("noeta-composed");
        if bin.is_file() {
            return Some(bin);
        }
    }
    None
}

#[test]
fn dev_toolchain_composition_includes_a_mixed_crates_formatter() {
    let _guard = compose_guard();
    // dev-deps D5b, the mirror of the capstone: a *dev toolchain* composed for the same native-dep app
    // turns the mixed crate's `fmt` feature ON, so its tier-body formatter (and its marker) compile IN
    // — exactly the capability a shipped runner strips. We compose the toolchain via a delegating dev
    // command (`check`) and confirm the cached composed binary carries the formatter marker.
    let entry = composed_project("d5b_toolchain_fmt");
    // Isolate the compose cache so we can locate *this* composition's binary (and not touch the user's).
    let cache = entry.parent().unwrap().join("cache");
    let _ = std::fs::remove_dir_all(&cache);

    composed_env(&mut lang())
        .arg("check")
        .arg(&entry)
        .env("NOETA_CACHE_DIR", &cache)
        .assert()
        .success();

    let composed = find_composed_binary(&cache).expect("a composed toolchain binary was cached");
    let bytes = std::fs::read(&composed).expect("read the composed toolchain");
    let marker = b"IMGFX_FMT_ONLY_MARKER_7c4e9a";
    assert!(
        bytes.windows(marker.len()).any(|w| w == marker),
        "the dev toolchain composition did not enable the mixed crate's `fmt` feature — its \
         formatter marker is absent from {}",
        composed.display()
    );
}

#[test]
fn doc_api_in_a_composed_toolchain_documents_the_native_package() {
    // Publish-time native-package docs (the mechanism `noeta publish` uses): a native package's
    // module surface exists only in its compiled Rust, so its API docs are generated by running
    // `noeta doc --api --root <pkg>` INSIDE a composed toolchain that links the package's extension
    // — a client-side build (here the composed toolchain), never anything on the registry. This
    // proves the composed toolchain emits the package's own surface, scoped away from std.
    let entry = composed_project("docs_api_native");
    let cache = entry.parent().unwrap().join("cache");
    let _ = std::fs::remove_dir_all(&cache);

    // A delegating dev command composes + caches the toolchain (the imgfx extension linked).
    composed_env(&mut lang())
        .arg("check")
        .arg(&entry)
        .env("NOETA_CACHE_DIR", &cache)
        .assert()
        .success();
    let composed = find_composed_binary(&cache).expect("a composed toolchain binary was cached");

    // Generate the package's own API docs in the composed toolchain.
    let out = std::process::Command::new(&composed)
        .args(["doc", "--api", "--root", "imgfx"])
        .output()
        .expect("run `doc --api` in the composed toolchain");
    assert!(
        out.status.success(),
        "doc --api failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8(out.stdout).expect("docs.json is UTF-8");
    // The package's own module and its plain + higher-order functions are documented…
    assert!(
        json.contains("\"imgfx.fx\""),
        "fx module documented:\n{json}"
    );
    for f in ["\"double\"", "\"acc\"", "\"sum_r\"", "\"brighten_all\""] {
        assert!(json.contains(f), "fx function {f} documented");
    }
    // …and std is excluded by the root scope (a package documents only itself).
    assert!(
        !json.contains("\"std.math\""),
        "--root imgfx must exclude the stdlib"
    );

    // The publish namespace lint (`--lint`) rejects the fixture's `Acc` extern type: it omits
    // `namespace:`, so it defaults to `std` — a package must namespace its types under its own
    // root, or a consumer's re-rooting (and the published docs) can't reach them. This is the gate
    // `noeta publish` runs; it would block publishing this (deliberately sloppy) fixture. Reuses
    // the already-cached composed binary, so no rebuild.
    let lint = std::process::Command::new(&composed)
        .args(["doc", "--api", "--root", "imgfx", "--lint"])
        .output()
        .expect("run the publish namespace lint");
    assert!(
        !lint.status.success(),
        "the lint must reject a package type that defaults to the std namespace"
    );
    let msg = String::from_utf8_lossy(&lint.stderr);
    assert!(
        msg.contains("Acc") && msg.contains("namespace"),
        "the lint names the offending type and its namespace: {msg}"
    );
}

#[cfg(feature = "jit")] // `--native` exists only in the JIT-enabled build.
#[test]
fn build_native_of_a_native_dep_app_runs_the_composed_handler() {
    let _guard = compose_guard();
    // dev-deps `--native` gap, closed: a native-dependency app built with `--native` links a *composed
    // AOT runtime* (the lean runtime + the imgfx native extension) so the self-contained native binary
    // resolves the `imgfx` module and runs its handler. Before this, `--native` linked the stock
    // `libnoeta_aot.a` (no extension seam) and aborted on the unknown native module.
    if !has_cc() {
        eprintln!("skipping native-dep AOT test: no `cc` on PATH");
        return;
    }
    let entry = composed_project("native_dep_aot");
    let app_bin = entry.parent().unwrap().join("app_native_aot");
    let _ = std::fs::remove_file(&app_bin);

    // The composed toolchain (the delegation target) builds the composed AOT staticlib and `cc`-links
    // it against the program's AOT object. `composed_env` reuses the workspace debug artifacts so both
    // compositions stay fast; the env is inherited across the `exec` delegation.
    composed_env(&mut lang())
        .arg("build")
        .arg(&entry)
        .arg("--native")
        .arg("-o")
        .arg(&app_bin)
        .assert()
        .success()
        .stderr(predicate::str::contains("native AOT"));

    // The native binary runs on its own and resolves the native handler (`fx.double(21)` → 42).
    Command::new(&app_bin)
        .assert()
        .success()
        .stdout(predicate::str::starts_with("42\n"));

    // And it is lean: the composed AOT runtime pulls the mixed crate at default features, so its dev
    // formatter (and marker) are stripped from the shipped native artifact — same guarantee as `--exe`.
    let bytes = std::fs::read(&app_bin).expect("read the native artifact");
    let marker = b"IMGFX_FMT_ONLY_MARKER_7c4e9a";
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker),
        "the composed AOT runtime leaked the mixed crate's dev formatter into the native artifact"
    );
    let _ = std::fs::remove_file(&app_bin);
}

#[test]
fn composed_toolchain_end_to_end() {
    let _guard = compose_guard();
    let entry = composed_project("pm_compose_e2e");
    let app = entry.parent().unwrap().to_path_buf();

    // Step 1 asserts a compose-cache MISS, but the shared test cache dir outlives test
    // invocations — once the binary and fixture are both stable, a second `cargo test` would hit
    // the previous run's entry and see no banner. Clear the compose cache (only) for idempotence;
    // the step-2 hit is then proven within this run.
    let _ = std::fs::remove_dir_all(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("noeta-cache/compose"),
    );

    // 1. First run: composes (banner on stderr), then dispatches the native module, the extern
    //    type's plain methods, the higher-order ctx method, and the raw-buffer kernels (N3.4:
    //    `sum_r` reduces the app's own @packed column type; `brighten_all` produces a new list
    //    while — copy-on-write — the input stays intact: 1.5 then 1.0) — all composed.
    composed_env(&mut lang())
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("42")
                .and(predicate::str::contains("20"))
                .and(predicate::str::contains("0.75"))
                .and(predicate::str::contains("1.5\n1.0"))
                .and(predicate::str::contains("2.25")),
        )
        .stderr(predicate::str::contains("composing the toolchain"));

    // 2. Second run: content-addressed cache hit — no compose banner, same output.
    composed_env(&mut lang())
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(predicate::str::contains("42"))
        .stderr(predicate::str::contains("composing the toolchain").not());

    // 3. `noeta check` sees the extension's signatures: a wrong-typed argument to the native fn
    //    is a *static* error (the composed binary IS the checker), and the good file checks clean.
    composed_env(&mut lang())
        .arg("check")
        .arg(app.join("bad.noe"))
        .assert()
        .failure();
    composed_env(&mut lang())
        .arg("check")
        .arg(&entry)
        .assert()
        .success();

    // 4. An extension-contributed command is an unknown subcommand to the stock binary; the
    //    cwd-manifest fallback composes (cache hit) and the composed binary dispatches it.
    composed_env(&mut lang())
        .arg("fx-info")
        .current_dir(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("imgfx: native extension ok"));
}

#[test]
#[cfg(unix)]
fn an_unknown_subcommand_falls_back_to_a_noeta_prefixed_binary_on_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_external_cmd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tool = dir.join("noeta-hello");
    std::fs::write(
        &tool,
        "#!/bin/sh\necho \"hello from external: $1\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    // PATH includes only our dir — the fallback finds `noeta-hello`, forwards trailing args, and
    // the exit code passes through.
    lang()
        .arg("hello")
        .arg("world")
        .env("PATH", &dir)
        .assert()
        .code(7)
        .stdout(predicate::str::contains("hello from external: world"));

    // Without the binary on PATH the ordinary clap error renders (exit 2, mentions the name).
    lang()
        .arg("hello")
        .env("PATH", env!("CARGO_TARGET_TMPDIR"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("hello"));
}

// --- `run --jit-stats` ------------------------------------------------------------

/// `--jit-stats` (P-JIT S0): the report renders to stderr after the program's own output. The
/// declined-loop section is deterministic — a map-dominated loop is declined OSR synchronously at
/// its 50th back-edge (`worth_osr` says no), so a 200-iteration loop reliably lists the blocking
/// ops with their source lines, regardless of off-thread compile timing. Program output is
/// untouched (the report is stderr-only diagnostics).
#[test]
#[cfg(feature = "jit")]
fn run_jit_stats_reports_declined_loops_with_blocking_ops() {
    let file = temp_program(
        "jit_stats_declined",
        "mut m: Map<string, int> = {}\nmut i = 0\nwhile i < 200 {\n  key = \"w${i % 5}\"\n  m[key] = m.get_or(key, 0) + 1\n  i = i + 1\n}\necho m[\"w0\"]\n",
    );
    let out = lang()
        .arg("run")
        .arg("--jit-stats")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert_eq!(stdout, "40\n", "program output untouched by the report");
    assert!(
        stderr.contains("── JIT report ──"),
        "report header on stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("loops declined tier 1"),
        "the map loop is declined:\n{stderr}"
    );
    // The blocking ops are named with their source line (main.noe:5 is the get_or/set line).
    assert!(
        stderr.contains("CallMethod") && stderr.contains("main.noe:5"),
        "blocking ops resolved to op + line:\n{stderr}"
    );
}

/// Without `--jit-stats`, a run prints no report — the recording seam stays `None` and stderr
/// carries only the program's own diagnostics (none here).
#[test]
fn run_without_jit_stats_prints_no_report() {
    let file = temp_program("jit_stats_off", "echo 1 + 1\n");
    let out = lang().arg("run").arg(&file).assert().success();
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        !stderr.contains("JIT report"),
        "no report unless asked:\n{stderr}"
    );
}
