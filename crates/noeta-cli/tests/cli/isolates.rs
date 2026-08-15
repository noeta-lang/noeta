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
         pub fn new(): Node { return Node { next: none } } }\n\
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
         pub fn new(): Counter { return Counter { n: 42 } } }\n\
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
    let dir = temp_root().join("noeta_cli_test_isolate_dtor_cycle");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let marker = dir.join("markers.log");
    let marker_path = marker.to_str().expect("utf-8 path");
    let src = format!(
        "use std.fs\n\
         class Node {{ pub mut peer: ?Node\n\
         pub fn new(): Node {{ return Node {{ peer: none }} }}\n\
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

// --- real-path cancellation (isolate-cancel) ----------------------------------------
//
// The deterministic sandbox cancels a *cooperative* task, which is already parked between polls, so
// the request is honored exactly where it lands and the report is trivially honest. A real isolate
// is an OS thread that is running, and it used to be neither: `h.cancel()` reached nothing but the
// parent's own bookkeeping, so `join` reported `Err(Cancelled)` while the worker ran to completion
// on its own thread — every side effect landing, the `concurrent` block closing over a thread still
// executing, and the process blocking on that thread at exit. These three cases pin what replaced
// that: the request crosses the thread boundary, the worker honors it at its next **safepoint**, and
// the reported outcome is what actually happened.
//
// **A house rule for everything below: a worker that is supposed to be cancelled must not be able to
// finish.** Every one of these tests pairs a worker with a fixed `sleep(N)` in the parent, and where
// the worker's body is a *quantity of work* that pairing is a race — the assertion holds only while
// the work happens to outlast the sleep. Sizing the work up does not fix that; it widens odds that
// contention narrows again, multiplicatively. An unbounded loop has no odds to lose: `cancelled` is
// the only reachable outcome, a working cancel is the only way the program terminates, and a broken
// one hangs into the harness timeout — loud, and pointing at the right thing.
//
// Three tests below deliberately do *not* follow it, because their worker is already unable to win
// the race for a structural reason rather than a numerical one:
//
// - `..._cancel_after_completion_reports_ok` wants the opposite outcome (the request arriving too
//   late), and its worker is a `return` — it cannot lose a 200 ms race.
// - `..._cancel_reaches_a_worker_parked_on_timers` bounds its worker in *real time* (1000 × 5 ms
//   sleeps), not in work, so no machine can finish it inside a 200 ms cancel. Its wall-clock ceiling
//   is the real claim there and is kept.
// - `..._cancel_does_not_preempt_a_native_call` blocks its worker in a FIFO read until an external
//   thread unwedges it at 700 ms, long after the 300 ms cancel; the flag is already set when the
//   worker resumes, so it stops at its first back-edge whatever the loop's size.

#[test]
fn run_real_isolate_cancel_stops_a_compute_bound_worker() {
    // The core claim: a compute-bound isolate — a loop with no suspension point at all — is
    // genuinely cancellable, because the worker's dispatch loop polls the cancellation flag at the
    // same safepoints the GC uses (frame transfers and taken loop back-edges). A worker isolate is
    // always tier 0 (the JIT is never armed on a worker thread), so those two sites are the whole
    // mechanism here.
    //
    // **The loop has no exit but cancellation**, which is what makes the claim structural rather
    // than statistical. It used to count to 40 000 000 — about 3.9 s interpreted against a 200 ms
    // cancel, a comfortable-looking 19× margin that is still a race, and the sibling test below had
    // the same shape with a margin that went negative on a fast disk. A loop that cannot terminate
    // on its own has no margin to lose: reaching the `cancelled` line at all proves the worker
    // stopped, and a broken poll hangs into the 60 s timeout instead of quietly reporting `ok=`.
    //
    // The wall-clock ceiling is kept but no longer carries the claim, so it is set well clear of any
    // plausible loaded-machine run rather than tuned against the uncancelled runtime (which no
    // longer exists). It now only catches "stopped, but nowhere near the next safepoint".
    let file = temp_program(
        "isolate_cancel_compute",
        "use std.io\n\
         use std.task.{sleep}\n\
         async fn spin(): int {\n\
         mut n = 0\n\
         while true { n = n + 1 }\n\
         return n\n\
         }\n\
         async fn run(): int {\n\
         concurrent {\n\
         h = isolate spin()\n\
         sleep(200).await\n\
         h.cancel()\n\
         io.outln(match h.join() { Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" })\n\
         }\n\
         return 0\n\
         }\n\
         echo run().await",
    );
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("cancelled\n0\n");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "a cancelled compute-bound isolate must stop at its next safepoint; the loop is unbounded, \
         so reaching this line at all proves it stopped — this only catches a stop that took \
         absurdly long; took {elapsed:?}"
    );
}

#[test]
fn run_real_isolate_cancel_after_completion_reports_ok() {
    // **The honest report.** `cancel` is a request; `join` reports what happened. `quick` finishes
    // in microseconds, long before the 200 ms sleep returns, so the cancel arrives too late to stop
    // anything — and `join` must say `Ok(7)`. Before this change it said `Err(Cancelled)`: the
    // parent latched "cancelled" the instant it *asked*, without ever consulting the worker.
    let file = temp_program(
        "isolate_cancel_too_late",
        "use std.io\n\
         use std.task.{sleep}\n\
         async fn quick(): int { return 7 }\n\
         async fn run(): int {\n\
         concurrent {\n\
         h = isolate quick()\n\
         sleep(200).await\n\
         h.cancel()\n\
         io.outln(match h.join() { Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" })\n\
         }\n\
         return 0\n\
         }\n\
         echo run().await",
    );
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("ok=7\n0\n");
}

#[test]
fn run_real_isolate_cancel_is_joined_before_the_block_closes() {
    // Structured concurrency, kept: a `concurrent` block joins everything it spawned — *including*
    // a member it cancelled. The worker counts into a file on real disk; the parent cancels, joins,
    // reads the count at the closing brace, waits, and reads it again. The two reads must be equal:
    // no work happens after the block closes.
    //
    // Before this change the second read was strictly larger (measured 1787 → 3542) — the block
    // returned over a thread that was still running, which is the promise the docs make. Worse, the
    // abandoned worker then raced the parent's exit-time heap teardown: the same program segfaulted
    // in the allocator at process exit, reproducibly.
    //
    // **The worker cannot finish on its own, and that is the fix.** It used to count to 4000, which
    // made the whole test a race between a fixed amount of work and a fixed 100 ms sleep — and on a
    // fast disk the work won: 4000 real `fs.write`s completed inside the sleep, `join` honestly
    // reported `Ok(4000)`, and the test failed with `cancelled` never printed. Measured 2-of-3 on an
    // otherwise-green build, which is the worst thing a guard can do — this one guards a
    // *memory-safety* fix, and a guard people learn to rerun is a guard nobody reads.
    //
    // Sizing the loop up would not have fixed it, only moved the odds: under contention the noise is
    // multiplicative, so a bigger body buys margin at the cost of wall time and still loses
    // eventually. `while true` removes the race instead of widening it. The worker now has no exit
    // but cancellation, so `cancelled` is the only reachable outcome and a *working* cancel is the
    // only way this program terminates at all. A broken one hangs and trips the 60 s timeout below,
    // which is a loud, unambiguous failure rather than a confusing assertion mismatch.
    //
    // The `Ok(v)` arm stays because `join` returns `Result<T, Cancelled>` and the match must be
    // exhaustive (E0011) — it is statically required and dynamically unreachable, which is exactly
    // what it should be here.
    let dir = temp_root().join("noeta_cli_test_isolate_cancel_join");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let ticks = dir.join("ticks.txt");
    let ticks_path = ticks.to_str().expect("utf-8 path");
    let src = format!(
        "use std.{{io, fs}}\n\
         use std.task.{{sleep}}\n\
         async fn busy(path: string): int {{\n\
         mut n = 0\n\
         while true {{ n = n + 1; fs.write(path, \"${{n}}\") }}\n\
         return n\n\
         }}\n\
         async fn run(path: string): int {{\n\
         concurrent {{\n\
         h = isolate busy(path)\n\
         sleep(100).await\n\
         h.cancel()\n\
         io.outln(match h.join() {{ Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" }})\n\
         }}\n\
         at_close = fs.read(path)\n\
         sleep(300).await\n\
         io.outln(if at_close == fs.read(path) then \"stopped\" else \"still running\")\n\
         return 0\n\
         }}\n\
         echo run(\"{ticks_path}\").await"
    );
    let file = dir.join("main.noe");
    std::fs::write(&file, "0").ok();
    std::fs::write(&ticks, "0").expect("seed the tick file");
    std::fs::write(&file, &src).expect("write program");
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("cancelled\nstopped\n0\n");
}

#[test]
fn run_real_isolate_cancel_still_runs_the_workers_destructors() {
    // A cancelled worker unwinds through the same path a panic takes, and that path *runs user
    // code*: every live frame local's `destruct`, on a fresh frame stack that re-enters the dispatch
    // loop. The cancellation poll therefore has to stand down before that happens — a flag still set
    // aborts each destructor at its own first frame transfer, before its first op, and
    // `run_destructor` discards the abort, so a cancelled worker would silently skip the cleanup a
    // completed one does. Two mechanisms make that hold: `observe_cancel` clears the flag once the
    // request has been honored, and `run_destructor` lifts it around every destructor besides, so a
    // cancel landing *during* cleanup does not truncate it either.
    //
    // `held` lives in the synchronous frame the cancel interrupts, and its `destruct` must land
    // exactly once — the same once an uncancelled run produces. (An `async fn`'s own locals live in
    // the state machine's capture cells rather than a frame, and those are *not* destructor-aware on
    // the worker path today — measured, and true of an uncancelled worker too, so it is a separate
    // pre-existing gap and not something to pin here.) Worker stdout never returns to the parent, so
    // the marker goes to real disk.
    //
    // The spin loop is unbounded for the same reason as its two siblings above: a bounded one races
    // the parent's fixed sleep, and a worker that *completes* runs the same destructor once, so the
    // marker assertion cannot tell the two apart — only the `cancelled` stdout can, and that is
    // precisely the assertion the race breaks. With no exit but cancellation there is nothing to
    // race.
    let dir = temp_root().join("noeta_cli_test_isolate_cancel_dtor");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let marker = dir.join("markers.log");
    let marker_path = marker.to_str().expect("utf-8 path");
    let src = format!(
        "use std.{{io, fs}}\n\
         use std.task.{{sleep}}\n\
         class Res {{ pub tag: string\n\
         pub fn new(t: string): Res {{ return Res {{ tag: t }} }}\n\
         destruct {{ fs.append(\"{marker_path}\", \"x\\n\") }} }}\n\
         fn spin(): int {{\n\
         held = Res.new(\"held\")\n\
         mut n = 0\n\
         while true {{ n = n + 1 }}\n\
         return n + held.tag.len()\n\
         }}\n\
         async fn work(): int {{ return spin() }}\n\
         async fn run(): int {{\n\
         concurrent {{\n\
         h = isolate work()\n\
         sleep(200).await\n\
         h.cancel()\n\
         io.outln(match h.join() {{ Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" }})\n\
         }}\n\
         return 0\n\
         }}\n\
         echo run().await"
    );
    let file = dir.join("main.noe");
    std::fs::write(&file, &src).expect("write program");
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("cancelled\n0\n");
    let markers = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        markers.lines().count(),
        1,
        "a cancelled worker must still run its live locals' destructors; got: {markers:?}"
    );
}

#[cfg(unix)]
#[test]
fn run_real_isolate_cancel_does_not_preempt_a_native_call() {
    // **The named limit**, pinned so it stays named. A worker blocked *inside the host* — here a
    // `fs.read` on a FIFO with no writer — is not executing Noeta, so it reaches no safepoint and
    // the cancellation request cannot land. The `concurrent` block waits for it, deliberately:
    // abandoning a worker mid-syscall leaves a thread outliving its scope, still owning its heap and
    // its handles (which is what the old behavior did, and it raced the parent's exit-time teardown
    // into a segfault).
    //
    // The test proves both halves without hanging. A helper thread unwedges the read after 700 ms,
    // so the program cannot possibly finish before then — that elapsed floor *is* the "could not be
    // preempted" claim (measured without the unwedge: the process sits in the block's join
    // indefinitely). And once the read returns, the worker has a loop still to run, so it reaches a
    // safepoint and reports `cancelled`: the request was never lost while the worker was blocked, it
    // simply could not take effect until the worker was executing Noeta again.
    //
    // The loop after the read is load-bearing rather than padding. Without it the body's remaining
    // work is a `return`, which finishes before any safepoint comes around — and `Ok(8)` is then the
    // *honest* answer (the request arrived too late to stop anything), which is a fine outcome but a
    // useless assertion.
    let dir = temp_root().join("noeta_cli_test_isolate_cancel_native");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let fifo = dir.join("fifo");
    let fifo_path = fifo.to_str().expect("utf-8 path");
    let made = std::process::Command::new("mkfifo")
        .arg(fifo_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        return; // no `mkfifo` on this box — nothing to pin, and nothing to fail either
    }
    let src = format!(
        "use std.{{io, fs}}\n\
         use std.task.{{sleep}}\n\
         fn spin(iters: int): int {{ mut n = 0; while n < iters {{ n = n + 1 }} return n }}\n\
         async fn wedged(path: string): int {{ return fs.read(path).len() + spin(40000000) }}\n\
         async fn run(): int {{\n\
         concurrent {{\n\
         h = isolate wedged(\"{fifo_path}\")\n\
         sleep(300).await\n\
         h.cancel()\n\
         io.outln(match h.join() {{ Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" }})\n\
         }}\n\
         return 0\n\
         }}\n\
         echo run().await"
    );
    let file = dir.join("main.noe");
    std::fs::write(&file, &src).expect("write program");
    // Unwedge from outside: opening the FIFO for writing (and closing) lets the worker's read
    // return. Well after the 300 ms cancel, so the cancel really does land on a blocked worker.
    // Deliberately **not** joined: opening a FIFO for writing blocks until a reader appears, so if
    // the program never got that far this thread would never return, and joining it would turn a
    // clear assertion failure into a hung test. The harness's exit reaps it.
    let writer_path = fifo.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(700));
        let _ = std::fs::write(&writer_path, "unwedged");
    });
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout("cancelled\n0\n");
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(600),
        "the run must have waited on the blocked native read (unwedged at 700ms) rather than \
         preempting it at the 300ms cancel; took {:?}",
        start.elapsed()
    );
}

#[test]
fn run_real_isolate_cancel_reaches_a_worker_parked_on_timers() {
    // The scheduler-loop half of the cancellation poll. A worker whose body only ever *awaits* runs
    // no bytecode between suspensions, so the dispatch loop's safepoints never come around — the
    // check in the driving loops (`join_scope` / `drive_future_outcome`) is the only one it can
    // reach. This watcher sleeps in 5 ms slices for what would be 5 s; cancelled 200 ms in, it must
    // stop promptly rather than sit out the remaining wait.
    //
    // Slices here, one long sleep in the test below: both stop promptly now. The slicing used to be
    // load-bearing (a single long sleep parked the worker inside the executor's real-time wait,
    // which no poll could interrupt — measured at 2.8 s for a 3 s sleep cancelled at 200 ms); the
    // cancellation **wake** closed that, and this case keeps covering the *poll* half — a worker
    // that suspends constantly and is stopped between two rounds rather than mid-block.
    let file = temp_program(
        "isolate_cancel_timers",
        "use std.io\n\
         use std.task.{sleep}\n\
         async fn watcher(ms: int): int {\n\
         mut w = 0\n\
         while w < ms { sleep(5).await; w = w + 5 }\n\
         return 1\n\
         }\n\
         async fn run(): int {\n\
         concurrent {\n\
         h = isolate watcher(5000)\n\
         sleep(200).await\n\
         h.cancel()\n\
         io.outln(match h.join() { Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" })\n\
         }\n\
         return 0\n\
         }\n\
         echo run().await",
    );
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("cancelled\n0\n");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(2_000),
        "a cancelled worker parked on timers must stop at the next scheduler round, not sit out its \
         remaining 4.8s; took {elapsed:?}"
    );
}

// --- a worker's output on the live path (isolate-output) ---------------------------

/// `noeta run` streams program output as it is produced, and each worker isolate's host inherits
/// that choice — so a worker's **completed lines** go straight to the terminal and must not be
/// captured a second time. What a streaming host cannot deliver is an **unterminated tail**: it
/// stays in the worker's buffer, and before the fix that buffer was dropped when the thread ended,
/// so a worker's last partial line vanished.
///
/// Both halves are asserted here against one exact transcript: each line appears exactly once (no
/// double print from merging what was already streamed), and the tail arrives at the parent's
/// harvest, ahead of everything the parent writes after the `.await`.
#[test]
fn run_real_isolate_output_appears_once_and_its_tail_is_not_dropped() {
    let file = temp_program(
        "isolate_output_live",
        "use std.io\n\
         async fn work(n: int): int {\n\
         echo \"worker line\"\n\
         io.out(\"tail=\")\n\
         return n\n\
         }\n\
         async fn run(): int {\n\
         mut r = 0\n\
         concurrent { h = isolate work(7); r = h.await }\n\
         return r\n\
         }\n\
         echo \"before\"\n\
         echo run().await\n\
         echo \"after\"",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        // `tail=` is the worker's unterminated write; it lands at the harvest, so it prefixes the
        // parent's `echo` of the result rather than being lost.
        .stdout("before\nworker line\ntail=7\nafter\n");
}

#[test]
fn run_real_isolate_cancel_reaches_a_worker_parked_in_one_long_sleep() {
    // **The wake half.** A worker whose only pending work is a single timer is parked *inside*
    // `RealExecutor::advance`, which sleeps real time to the earliest deadline in one call — it is
    // not executing Noeta and its scheduler loop, where the cancellation poll lives, is exactly what
    // it has left. The flag alone was therefore observed only when the sleep ended (measured: a
    // 3 s sleep cancelled at 200 ms stopped 2.8 s later). The parent now fires the worker's
    // `CancelWake` alongside the flag store; the executor's hook ends the sleep, the worker's next
    // round polls the flag, and it stops at once.
    //
    // **The house rule, structurally rather than numerically.** The worker sleeps for ten minutes,
    // so it cannot finish — not on any machine, at any load, since its bound is real time and a
    // busy box only makes it longer. `cancelled` is the only reachable outcome and a working wake is
    // the only way this program terminates; a broken one runs into `assert_cmd`'s 60 s bound, which
    // kills the child and fails loudly. The elapsed ceiling below is a smoke bound on top of that,
    // not the claim — it is two orders of magnitude under the sleep it would have had to sit out.
    let file = temp_program(
        "isolate_cancel_long_sleep",
        "use std.io\n\
         use std.task.{sleep}\n\
         async fn napper(ms: int): int {\n\
         sleep(ms).await\n\
         return 1\n\
         }\n\
         async fn run(): int {\n\
         concurrent {\n\
         h = isolate napper(600000)\n\
         sleep(200).await\n\
         h.cancel()\n\
         io.outln(match h.join() { Ok(v) => \"ok=\" ~ v, Err(_) => \"cancelled\" })\n\
         }\n\
         return 0\n\
         }\n\
         echo run().await",
    );
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("cancelled\n0\n");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "a cancelled worker parked in one long sleep must be woken, not sit out its remaining \
         599.8s; took {elapsed:?}"
    );
}

/// **The read half**, and the row's headline example. Its sibling above parks the worker in a
/// timer, which `RealExecutor::advance` owns; this one parks it in a *host* leaf — a blocking read
/// of a child's stdout — which the executor knows nothing about. Ending the executor's wait cannot
/// help here, because there is no wait: the worker is asleep on a condvar inside the host, holding
/// the buffer of a child that has nothing to say.
///
/// The house rule again, structurally: `cat` with a piped stdin nobody writes to and nobody closes
/// produces no output and does not exit, so this program has exactly one way to terminate. A broken
/// interruption runs into `assert_cmd`'s 60 s bound and fails loudly rather than passing slowly.
#[cfg(unix)]
#[test]
fn run_real_isolate_cancel_reaches_a_worker_parked_in_a_child_read() {
    let file = temp_program(
        "isolate_cancel_child_read",
        "use std.io\n\
         use std.os\n\
         use std.task.{sleep}\n\
         async fn listener(): int {\n\
         p = os.spawn(\"cat\", [])\n\
         p.read_line()\n\
         return 1\n\
         }\n\
         async fn run(): int {\n\
         concurrent {\n\
         h = isolate listener()\n\
         sleep(200).await\n\
         h.cancel()\n\
         io.outln(match h.join() { Ok(v) => \"ok=\" ~ v, Err(_) => \"stopped\" })\n\
         }\n\
         return 0\n\
         }\n\
         echo run().await",
    );
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout("stopped\n0\n");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "a cancelled worker parked in a child read must be roused inside the host; took {elapsed:?}"
    );
}
