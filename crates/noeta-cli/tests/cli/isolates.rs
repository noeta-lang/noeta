//! Real OS-thread isolates through `noeta run` (isolates I.4b, out-of-oracle).

use crate::support::*;

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
fn run_real_isolate_collects_cycles_at_its_own_safepoints() {
    // A worker isolate building reference cycles in a loop, with the safepoint-GC threshold pinned
    // tiny so the WORKER's own thread-local trigger fires many mid-run collections (memory-
    // management 6.x: per-isolate heaps collect at per-isolate safepoints). Correct output +
    // clean exit prove the worker's roots (its depth-0 callee/future transients included) survive
    // every collection while the stranded cycles are reclaimed.
    let file = temp_program(
        "isolate_cycle_gc",
        "class Node { pub mut next: ?Node\n\
         fn new(): Node { return Node { next: none } } }\n\
         async fn spin(count: int): int {\n\
         mut i = 0\n\
         while i < count {\n\
         a = Node.new()\n\
         b = Node.new()\n\
         a.next = some(b)\n\
         b.next = some(a)\n\
         i = i + 1\n\
         }\n\
         return i\n\
         }\n\
         async fn run(): int {\n\
         mut r = 0\n\
         concurrent { d = isolate spin(20000); r = d.await }\n\
         return r\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .env("NOETA_GC_THRESHOLD", "128")
        .assert()
        .success()
        .stdout("20000\n");
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

// --- worker environment limits: unshippable globals (isolates I.4b) -----------------

#[test]
fn run_real_isolate_class_global_read_is_a_precise_error() {
    // A reference `class` global cannot cross into a worker's fresh heap (it has identity —
    // deep-copying it would silently split "the" instance in two). The worker body *reads* it, so
    // the parent skips the global at spawn and the read fails AT USE with a precise diagnostic that
    // names the global, its type, and the fix — not the old confusing "cannot find `counter`" (nor,
    // worse, a silent stale copy). The checker already rejects a `class` *argument*/*result*
    // (E0042); this closes the *global* path it does not see.
    let file = temp_program(
        "isolate_class_global",
        "class Counter { pub n: int\n\
         fn new(): Counter { return Counter { n: 42 } } }\n\
         counter = Counter.new()\n\
         async fn work(x: int) use (counter): int { return counter.n + x }\n\
         async fn run(): int {\n\
         mut r = 0\n\
         concurrent { d = isolate work(5); r = d.await }\n\
         return r\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("counter"))
        .stderr(predicate::str::contains("Counter"))
        .stderr(predicate::str::contains("cannot be shared with an isolate"));
}

#[test]
fn run_real_isolate_struct_global_ships_by_copy() {
    // The documented workaround (and the value-type contrast to the class case above): a value
    // `struct` global HAS no identity, so it marshals by copy and every worker reads its own
    // snapshot — the isolate body sees the parent's value with no diagnostic. 42 + 5 = 47.
    let file = temp_program(
        "isolate_struct_global",
        "struct Config { n: int }\n\
         config = Config { n: 42 }\n\
         async fn work(x: int) use (config): int { return config.n + x }\n\
         async fn run(): int {\n\
         mut r = 0\n\
         concurrent { d = isolate work(5); r = d.await }\n\
         return r\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("47\n");
}

#[test]
fn run_real_isolate_worker_teardown_runs_leaked_cycle_destructors() {
    // Isolates I.4b worker-teardown gap: a worker that strands reference cycles (`a.peer = b;
    // b.peer = a` on a `class`) must reap them at its OWN teardown and run their `__destruct` — just
    // like the main heap's exit reapers. Worker stdout never returns to the parent, so each
    // destructor appends a marker to a file on real disk (the worker runs on `RealHost`); after the
    // structured join the parent reads it. Before the fix the cycle leaked untouched and the file
    // was empty; now 3 iterations × 2 nodes = 6 destructors fire.
    let dir = std::env::temp_dir().join("noeta_cli_test_isolate_dtor_cycle");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let marker = dir.join("markers.log");
    let marker_path = marker.to_str().expect("utf-8 path");
    let src = format!(
        "use std.fs\n\
         class Node {{ pub mut peer: ?Node\n\
         fn new(): Node {{ return Node {{ peer: none }} }}\n\
         destruct {{ fs.append(\"{marker_path}\", \"x\\n\") }} }}\n\
         fn spin(count: int): int {{\n\
         mut i = 0\n\
         while i < count {{\n\
         a = Node.new()\n\
         b = Node.new()\n\
         a.peer = some(b)\n\
         b.peer = some(a)\n\
         i = i + 1\n\
         }}\n\
         return i\n\
         }}\n\
         async fn work(n: int): int {{ return spin(n) }}\n\
         async fn run(): int {{\n\
         mut r = 0\n\
         concurrent {{ d = isolate work(3); r = d.await }}\n\
         return r\n\
         }}\n\
         echo run().await"
    );
    let file = dir.join("main.noe");
    std::fs::write(&file, &src).expect("write program");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("3\n");
    let markers = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        markers.lines().count(),
        6,
        "the worker's teardown must run all 6 stranded-cycle destructors; got: {markers:?}"
    );
}

// --- real-path cross-isolate deadlock detection (isolates I.4c) ---------------------

#[test]
fn run_real_isolate_channel_deadlock_errors_not_hangs() {
    // A genuine cross-thread deadlock: a worker isolate blocks forever on `recv` over a shared
    // channel that nobody ever sends to or closes, while the parent awaits the worker. The sandbox
    // oracle catches this cooperative stall as E0010; the real (parallel) scheduler must detect the
    // all-parties-blocked state — every registered scheduler parked with no timer, no IO, and no live
    // counterparty — and raise the *same* diagnostic instead of spinning/hanging. The 20s timeout is
    // the regression guard: before I.4c this hung forever.
    let file = temp_program(
        "isolate_deadlock",
        "async fn stuck(rx: Receiver<int>): int {\n\
         r = rx.recv().await\n\
         return match r { some(x) => x, none => 0 }\n\
         }\n\
         async fn run(): int {\n\
         (tx, rx) = channel::<int>(1)\n\
         mut result = 0\n\
         concurrent {\n\
         h = isolate stuck(rx)\n\
         result = h.await\n\
         }\n\
         return result\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(20))
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0010"))
        .stderr(predicate::str::contains("deadlock"));
}
