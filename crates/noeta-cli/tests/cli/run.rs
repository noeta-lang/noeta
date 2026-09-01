//! `noeta run` end-to-end: bare-file/shebang invocation, exit codes, the startup cache,
//! argument pass-through, the real host (env/os/fs/net/time), `--tier`, abort stack traces,
//! and `--jit-stats`.

use crate::support::*;

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
fn the_plain_run_fast_path_agrees_with_the_parsed_path() {
    // `noeta run <file>` — exactly three argv entries, no flags — skips building the clap command
    // tree and calls `cmd_run` directly (~215 k instructions retired, ~9% of process startup, for a
    // tree a flagless `run` reads nothing out of). This pins the seam: the shortcut and the three
    // routes that still go through clap must produce the same bytes for the same program.
    let file = temp_program(
        "plain_run_fast_path",
        "use std.args\necho \"n=${args.all().len()}\"\n",
    );
    // The fast path itself.
    let fast = lang().arg("run").arg(&file).assert().success();
    let fast_out = String::from_utf8(fast.get_output().stdout.clone()).unwrap();
    assert_eq!(fast_out, "n=1\n");
    // A flag anywhere declines the fast path — clap parses it, same program, same output.
    lang()
        .arg("run")
        .arg("--no-cache")
        .arg(&file)
        .assert()
        .success()
        .stdout(fast_out.clone());
    // So does the bare-file shortcut, which reaches `cmd_run` through clap's unknown-subcommand
    // recovery.
    lang().arg(&file).assert().success().stdout(fast_out);
    // And a `--`-separated program argument still reaches the program (the fast path must not
    // swallow it).
    lang()
        .arg("run")
        .arg(&file)
        .arg("--")
        .arg("extra")
        .assert()
        .success()
        .stdout("n=2\n");
}

#[test]
fn run_help_and_flag_errors_are_untouched_by_the_fast_path() {
    // A leading `-` in the file position is never a path: `run --help` must still render clap's
    // help, and an unknown flag must still render clap's error, not be handed to the runner as a
    // filename.
    lang()
        .arg("run")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Path to a `.noe` file"));
    lang()
        .arg("run")
        .arg("--nope")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument '--nope'"));
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

// --- M2.3: `lang run` uses the real host (real env/args + real-disk IO) ------------

#[test]
fn run_reads_the_real_environment() {
    // `env.get` reads the REAL process environment (RealHost), not the sandbox fixture —
    // proven by injecting a variable the child process sees. (Conformance still runs the
    // sandbox fixture; only `lang run` is on the real host.)
    let file = temp_program(
        "run_env",
        "use std.{env};\necho env.get(\"LANG_E2E_VAR\") ?? \"<unset>\";",
    );
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
               echo env.get(\"LANG_E2E_OVERLAY\") ?? \"<unset>\"\n\
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
fn run_os_proc_signal_and_wait_async_over_a_real_child() {
    // `signal` sends a real OS signal (process-signals arc): SIGTERM to a `sleep` terminates it, so
    // `wait` reports a non-success status. `wait_async` awaits a real child's exit on the blocking
    // pool and yields its captured output. Proves the real-host `kill(2)` + async-wait paths, not
    // the sandbox script.
    let src = "use std.{os}\n\
               s = os.spawn(\"sleep\", [\"5\"])\n\
               s.signal(\"TERM\")\n\
               echo s.wait().ok()\n\
               async fn collect(): string {\n\
                   p = os.spawn(\"echo\", [\"async hi\"])\n\
                   r = p.wait_async().await\n\
                   return r.stdout().trim()\n\
               }\n\
               concurrent {\n\
                   a = spawn collect()\n\
                   echo a.await\n\
               }\n";
    let file = temp_program("run_os_signal", src);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("false\nasync hi\n");
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
    let dir = temp_root().join("noeta_cli_realfs_dir");
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
    let dir = temp_root().join("noeta_cli_async_read_dir");
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
    let dir = temp_root().join("noeta_cli_async_meta_dir");
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
    let dir = temp_root().join("noeta_cli_async_write_dir");
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

// --- `run --jit-stats` ------------------------------------------------------------

/// `--jit-stats` (P-JIT S0): the report renders to stderr after the program's own output. The
/// declined-loop section is deterministic — a loop carrying a non-native op is declined OSR
/// synchronously at its 50th back-edge (`worth_osr` says no), so a 200-iteration loop reliably
/// lists the blocking ops with their source lines, regardless of off-thread compile timing.
/// Program output is untouched (the report is stderr-only diagnostics).
///
/// The blocking op here is a **list** method call. `Op::CallMethod` is native only for a *map*
/// method name (the leaf helper serves the map receiver and bails on every other), so `xs.len()`
/// is exactly the shape that still declines — and the fixture doubles as the lock on that static
/// name gate. A map loop no longer belongs here: `m[k] = m.get_or(k, 0) + 1` sustains tier 1.
#[test]
#[cfg(feature = "jit")]
fn run_jit_stats_reports_declined_loops_with_blocking_ops() {
    let file = temp_program(
        "jit_stats_declined",
        "mut xs = [1, 2, 3]\nmut n = 0\nmut i = 0\nwhile i < 200 {\n  n = n + xs.len()\n  i = i + 1\n}\necho n\n",
    );
    let out = lang()
        .arg("run")
        .arg("--jit-stats")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert_eq!(stdout, "600\n", "program output untouched by the report");
    assert!(
        stderr.contains("── JIT report ──"),
        "report header on stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("loops declined tier 1"),
        "the list-method loop is declined:\n{stderr}"
    );
    // The blocking ops are named with their source line (main.noe:5 is the `xs.len()` line).
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

// --- warnings do not block ---------------------------------------------------------

/// A **warning** is reported and the program still runs — the whole program, from its first
/// statement, exiting on its own merits.
///
/// `noeta run` used to gate on "any diagnostic" rather than "any error", so one E0063 (`i32` erases
/// to `int`, so `x is i32` is unanswerable at run time — advisory, and `noeta check` scores it
/// `0 error(s), 1 warning(s)` and exits 0) meant the program never started: no stdout at all, not
/// even the `echo` on the line *before* the warned-about one, and exit 1. A file the checker called
/// fine could not be executed. That also made every new warning a hard stop, which is what makes
/// advisory lints unshippable — so this asserts the three things that must all hold at once: the
/// warning is still reported, the program's full output is produced, and the exit code is the
/// program's own.
///
/// The scrutinee is laundered through `dyn` on purpose. E0063 now fires only where the width is
/// *genuinely* unrecoverable; a binding whose static type names the width (`a: i32 = 5`) is
/// answered by the checker and folded, so the old fixture stopped warning at all — and this test,
/// which is about warnings and not about widths, silently stopped testing anything. Any still-live
/// warning would do; if this one is ever answered too, swap the fixture rather than the assertion.
#[test]
fn a_warning_is_reported_and_the_program_still_runs() {
    let file = temp_program(
        "warning_does_not_block",
        "fn erased(x: dyn): bool { return x is i32 }\n\
         echo \"BEFORE\"\n\
         echo \"is i32 -> ${erased(5)}\"\n\
         echo \"AFTER\"\n",
    );

    // `check` agrees the program is fine: zero errors, exit 0.
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s), 1 warning(s)"));

    // …so `run` must run it. Every line, in order, exit 0 — with the warning still on stderr.
    let out = lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("BEFORE\nis i32 -> false\nAFTER\n");
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("[E0063]") && stderr.contains("Warning"),
        "the warning is still reported, not silenced:\n{stderr}"
    );
}

/// The warning survives repetition. A warned program deliberately skips the startup cache: caching
/// it would short-circuit the whole front-end on the second run, so the lint would appear once and
/// then never again — a warning you cannot see is worse than no warning at all.
#[test]
fn a_warning_is_reported_on_every_run_not_just_the_first() {
    let file = temp_program(
        "warning_survives_cache",
        "fn erased(x: dyn): bool { return x is i32 }\n\
         echo \"is i32 -> ${erased(5)}\"\n",
    );
    for run in 1..=2 {
        let out = lang()
            .arg("run")
            .arg(&file)
            .assert()
            .success()
            .stdout("is i32 -> false\n");
        let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.contains("[E0063]"),
            "run {run} still reports the warning:\n{stderr}"
        );
    }
}

/// The other half of the rule: an **error** still blocks, and still blocks *before* the program
/// produces anything. Without this the fix above could be "let everything through".
#[test]
fn an_error_still_blocks_the_run_entirely() {
    let file = temp_program("error_still_blocks", "echo \"BEFORE\"\nx = nope()\n");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("[E0005]"));
}

/// A program whose only diagnostic is a warning still *builds*, *dumps*, and — the verbs that share
/// `run`'s pipeline — reports the warning while producing its artifact.
#[test]
fn a_warning_does_not_fail_build_or_dump() {
    let file = temp_program(
        "warning_build_dump",
        "fn erased(x: dyn): bool { return x is i32 }\n\
         echo \"is i32 -> ${erased(5)}\"\n",
    );
    let out = lang().arg("dump").arg(&file).assert().success();
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("[E0063]"),
        "dump reports the warning on stderr, keeping stdout the pure disassembly:\n{stderr}"
    );
    assert!(
        String::from_utf8(out.get_output().stdout.clone())
            .unwrap()
            .contains("=== main ==="),
        "the disassembly is still produced"
    );

    let bundle = file.parent().unwrap().join("out.noeb");
    lang()
        .arg("build")
        .arg(&file)
        .arg("-o")
        .arg(&bundle)
        .assert()
        .success()
        .stderr(predicate::str::contains("[E0063]"));
    assert!(bundle.exists(), "the artifact is still written");
}

/// A method a native handle does not have must fail in the **user's** vocabulary, naming the type
/// and the method — never as an internal note about a dispatch registration.
///
/// `Signal`/`Computed`/`Effect` declare their whole surface as context methods and register no plain
/// method dispatch, so every plain method call on one lands on the shared "nothing registered"
/// fallback. That fallback is not an internal condition for these types: it *is* the no-such-method
/// answer, and reporting it as "internal: no method dispatch registered" told a reader who called
/// `.set()` on a computed that they had found a compiler bug. The conformance cases beside this one
/// pin `E0005` at the call; only a text assertion can pin what the message says.
#[test]
fn an_unknown_method_on_a_native_handle_names_the_type_not_the_registry() {
    let file = temp_program(
        "run_handle_no_method",
        "use std.reactive.{computed}\nc = computed(fn() => 5)\nc.set(3)\n",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0005"))
        .stderr(predicate::str::contains(
            "type `std.reactive.Computed` has no method `set`",
        ))
        .stderr(predicate::str::contains("internal:").not());
}
