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

// --- `run` ------------------------------------------------------------------------

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
fn bench_invalid_arg_is_e0037() {
    // An argument of the wrong type for the tier's schema is an InvalidDirectiveArgument (E0037),
    // reported up front rather than silently ignored.
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
        .stderr(predicate::str::contains("E0037"));
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

// --- `--profile` (object-model slice 6g: the `noeta.toml` build-profile manifest) ----

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
fn run_profile_activates_its_tiers() {
    // A profile that makes the `debug` tier live compiles the `@debug` block in, exactly as
    // `--tier debug` would — but driven by `noeta.toml`.
    let file = temp_project(
        "prof_run",
        "[profiles.dev.tiers]\ndebug = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("dev")
        .assert()
        .success()
        .stdout("dbg 5\nout 5\n");
}

#[test]
fn run_minimalist_profile_strips_everything() {
    // A profile that opts into no tiers leaves every tier block stripped (same as a bare run).
    let file = temp_project("prof_run_min", "[profiles.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("prod")
        .assert()
        .success()
        .stdout("out 5\n");
}

#[test]
fn test_profile_gates_the_runner() {
    // `lang test --profile prod`, where `prod` does not make `test` live, runs nothing and says so.
    let file = temp_project("prof_test_gate", "[profiles.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--profile")
        .arg("prod")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "tier `test` is not active in profile `prod`",
        ));
}

#[test]
fn test_profile_with_tier_live_runs() {
    let file = temp_project(
        "prof_test_live",
        "[profiles.dev.tiers]\ntest = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("test")
        .arg(&file)
        .arg("--profile")
        .arg("dev")
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn run_unknown_profile_is_an_error() {
    let file = temp_project(
        "prof_unknown",
        "[profiles.dev.tiers]\ndebug = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("ghost")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown profile `ghost`"));
}

#[test]
fn run_profile_without_manifest_is_an_error() {
    // `--profile` with no `noeta.toml` anywhere above the entry is a clear error, not a silent run.
    let file = temp_program("prof_no_manifest", "echo \"hi\"\n");
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
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
    // `noeta build --exe -o app` staples the compiled program onto a copy of the runtime binary.
    // The resulting `app` runs the program directly — no `.noe`, no `.noeb`, no `noeta run` — and
    // its stdout matches a source run byte-for-byte. Running it also proves the startup trailer
    // detection fires (the toolchain would otherwise demand a subcommand).
    let file = temp_program(
        "build_exe",
        "fn sq(n: int): int { return n * n }\nmut t = 0\nfor i in 0..5 {\n    t = t + sq(i)\n}\necho t\n",
    );
    let app = file.parent().unwrap().join("app");
    let _ = std::fs::remove_file(&app);

    lang()
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
    let file = temp_program("build_exe_panic", "echo \"before\"\npanic(\"boom\")\n");
    let app = file.parent().unwrap().join("app_panic");
    let _ = std::fs::remove_file(&app);

    lang()
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
fn build_aot_archive() -> Option<(PathBuf, String)> {
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(workspace())
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
fn has_cc() -> bool {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    std::process::Command::new(cc)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
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
    std::fs::write(app.join("main.noe"), "use hi.hello.greeting;\necho greeting();\n").unwrap();
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
    std::fs::write(a.join("a.noe"), "namespace a.x;\npub fn f(): int { return 1; }\n").unwrap();
    std::fs::write(
        b.join("noeta.toml"),
        "[package]\nname = \"acme/b\"\nversion = \"1.0.0\"\n\
         [dependencies]\ns = { path = \"../c2\" }\n",
    )
    .unwrap();
    std::fs::write(b.join("b.noe"), "namespace b.x;\npub fn g(): int { return 2; }\n").unwrap();
    std::fs::write(
        c1.join("noeta.toml"),
        "[package]\nname = \"acme/shared\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(c1.join("s.noe"), "namespace shared.core;\npub fn h(): int { return 3; }\n")
        .unwrap();
    std::fs::write(
        c2.join("noeta.toml"),
        "[package]\nname = \"acme/shared\"\nversion = \"2.0.0\"\n",
    )
    .unwrap();
    std::fs::write(c2.join("s.noe"), "namespace shared.core;\npub fn h(): int { return 4; }\n")
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
        .args(["publish", "--git", repo.to_str().unwrap(), "--tag", "v1.2.0"])
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
    std::fs::write(app.join("main.noe"), "use gc.hello.greeting;\necho greeting();\n").unwrap();

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
    std::fs::write(app.join("main.noe"), "use hi.hello.greeting;\necho greeting();\n").unwrap();

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
    std::fs::write(lib.join("m.noe"), "namespace lib.core;\npub fn v(): int { return 42; }\n")
        .unwrap();

    lang()
        .current_dir(&app)
        .args(["add", "hi", "--path", "../lib"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added `hi`"));

    let manifest = std::fs::read_to_string(app.join("noeta.toml")).unwrap();
    assert!(manifest.contains("# my app"), "comment preserved: {manifest}");
    assert!(manifest.contains("hi = { path = \"../lib\" }"), "dep added: {manifest}");
    assert!(app.join("noeta.lock").is_file(), "lock written");

    // The added dependency actually resolves and runs.
    lang().arg("run").arg(app.join("main.noe")).assert().success().stdout("42\n");
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
    std::fs::write(dep_repo.join("m.noe"), "namespace up.core;\npub fn v(): int { return 7; }\n")
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
    assert!(lock.contains("acme/pinned"), "lock names the package: {lock}");
    assert!(lock.contains("source = \"git\""), "lock records the git source: {lock}");
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
