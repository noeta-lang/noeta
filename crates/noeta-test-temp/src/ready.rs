//! Waiting for a spawned server to be reachable — and for it to be gone again.
//!
//! # The bug this module exists to make unrepeatable
//!
//! A test that spawns `noeta serve` and then polls the port with a **fixed iteration count** —
//! `for _ in 0..80 { … sleep(50ms) }`, four seconds — says nothing about the server when it gives
//! up. It falls out of the bottom of its loop into a `bool` nobody prints, and the failure surfaces
//! a line later as a bare `Connection refused (os error 111)` from the first real request: not the
//! budget, not how long it actually waited, not what the machine was doing. That is the message
//! that red-lit a whole merge gate beside a build-heavy agent (15-minute load average **16.4**),
//! over a subject — an idle swap reaching every worker — nothing had put in doubt.
//!
//! **And the budget was blamed for it, on the evidence available, wrongly.** The suite failed *one
//! second* in, which is not four seconds of sleeping: the wait had already returned, and something
//! after it was refused. Nothing in the output could have said so. That is the defect — not the
//! number, the silence — and it has a prior conviction: `hot_serve` and `hot_live` sat failing on
//! `main` for weeks reporting `server did not accept within 4s` while the real cause was an `E0005`
//! from the fixture program, on a stderr the test piped to `/dev/null` (`plans/backlog.md`, the
//! fixture-paths sweep).
//!
//! That second half — the `/dev/null` — is closed too, and it is closed *here*, because this is
//! where the failure is worded: every message below quotes what the server said before it stopped
//! (see [`crate::ServerLog`], whose header records the three investigations the missing line cost).
//! `the server process exited (exit status: 1)` names the fact; the next paragraph of the same
//! message now names the cause.
//!
//! The four seconds are worth raising anyway, but they are not what was measured to expire: with
//! this machine at a 1-minute load average of **133 over 20 cores** — 120 spinners, six disk
//! writers and an 8 GiB resident hog — a `noeta serve --parallel 3` spawn still bound in **0.19s**,
//! and eight consecutive runs of the pre-change suite passed. A budget is cheap insurance against a
//! machine slower than any we can produce on purpose; a diagnostic is what pays out every time.
//!
//! The loop was written out **ten times across eight suites**, with **six more** for the reverse
//! direction ("wait until it is gone"), free to disagree on budget (2.5s in `serve`, 4s everywhere
//! else), on interval, and on what happened at the end — some returned an error, some a `bool` the
//! caller turned into a different message, and the teardown copies discarded the result entirely.
//! One rule spelled in sixteen places, already drifted: the same shape this release fixed four
//! times over in the shipping code.
//!
//! # Why the budget can be generous
//!
//! A long budget is only dangerous if a *dead* server also consumes it. So the child process is
//! watched alongside the port ([`wait_until_listening_or_child_exits`]): a server that exits
//! without binding is reported in milliseconds, with its exit status, because no amount of further
//! waiting would change the outcome. What remains to spend the budget on is a server that is merely
//! *slow*, which is exactly the case a fixed four seconds got wrong.
//!
//! # The budget
//!
//! [`readiness_budget`] is 30s, scaled up to 4× by how oversubscribed the machine is
//! (`/proc/loadavg` 1-minute average over the core count), and overridable outright with
//! `NOETA_TEST_READY_SECS`. 30s is ~7× the four seconds it replaces and ~150× the worst startup
//! measured under deliberate 7× oversubscription, and a suite that spawns five servers still cannot
//! hang past a couple of minutes in the pathological case where each is alive but never binds. A
//! timeout names the budget, the elapsed time, the load it was computed from and the last connect
//! error, so the next reader can tell the two failures apart from the message alone.

use std::net::{SocketAddr, TcpStream};
use std::process::Child;
use std::time::{Duration, Instant};

use crate::ServerLog;

/// The budget before any load scaling. Roughly 7× the fixed 4s it replaces, which covers a
/// cold-cached `noeta serve` spawn on a saturated box with two orders of magnitude to spare
/// (0.19s measured at 7× oversubscription), and is still short enough that a suite spawning five
/// servers fails in minutes rather than sitting there.
const BASE: Duration = Duration::from_secs(30);

/// The gap between connect attempts. Loopback connects to a closed port cost microseconds, so this
/// is about not spinning rather than about cost; it also bounds how late a ready server is noticed.
const POLL: Duration = Duration::from_millis(25);

/// Per-attempt connect timeout, so one attempt against a bound-but-not-accepting socket (a full
/// backlog) cannot swallow the whole budget in a single syscall.
const ATTEMPT: Duration = Duration::from_secs(1);

/// How long a server is still given after its process is seen to have exited.
///
/// Not zero, because these suites spawn a *wrapper* (`noeta serve --watch` supervises the process
/// that owns the socket): a wrapper that exits does not by itself prove nothing will ever accept.
/// Two seconds is enough to see a socket its child already bound, and still turns a genuinely dead
/// server from a 30-second wait into a 2-second one.
const GRACE_AFTER_EXIT: Duration = Duration::from_secs(2);

/// The environment variable that overrides the budget outright, in whole seconds.
pub const BUDGET_ENV: &str = "NOETA_TEST_READY_SECS";

/// How long [`wait_until_listening`] and friends will wait: `NOETA_TEST_READY_SECS` if set,
/// otherwise 30s scaled by how oversubscribed the machine is (1× up to 4×).
///
/// Computed once per process. The knob is read once because a budget that changes under a running
/// suite is a suite whose failures cannot be reproduced from its own output; the *load* is sampled
/// per wait, so a machine that gets busy mid-run still gets the longer budget.
pub fn readiness_budget() -> Duration {
    static KNOB: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    let knob = KNOB.get_or_init(|| {
        std::env::var(BUDGET_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    });
    scaled_budget(*knob, machine_load())
}

/// The budget rule itself, with both of its inputs handed in — so it is testable without mutating
/// this process's environment, which in a multi-threaded test binary is the very race
/// [`crate::TempDir`] exists to prevent one directory up.
fn scaled_budget(knob: Option<Duration>, load: Load) -> Duration {
    knob.unwrap_or_else(|| BASE.mul_f64(load.factor))
}

/// What the machine looked like when the budget was computed — carried into the timeout message,
/// because "waited 30s" and "waited 30s at 4× ambient load" are different reports.
#[derive(Clone, Copy, Debug)]
struct Load {
    /// The 1-minute load average, or `None` where `/proc/loadavg` is not readable (non-Linux).
    one_minute: Option<f64>,
    /// Cores available to this process.
    cores: usize,
    /// `1.0..=4.0`: runnable work per core, floored at 1 (an idle machine does not shorten the
    /// budget) and capped at 4 (past that the machine is thrashing and a longer wait is not the
    /// answer — it is also how a runaway load average cannot wedge a suite for an hour).
    factor: f64,
}

fn machine_load() -> Load {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let one_minute = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
    let factor = one_minute.map_or(1.0, |load| (load / cores as f64).clamp(1.0, 4.0));
    Load {
        one_minute,
        cores,
        factor,
    }
}

impl std::fmt::Display for Load {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.one_minute {
            Some(load) => write!(
                f,
                "{}s base × {:.2} for a 1-minute load of {load:.2} over {} cores",
                BASE.as_secs(),
                self.factor,
                self.cores
            ),
            None => write!(f, "{}s base, load unknown", BASE.as_secs()),
        }
    }
}

/// Wait until something accepts on `addr`, and hand back the connection that proved it.
///
/// Prefer [`wait_until_listening_or_child_exits`] wherever the spawned process is in reach: it
/// turns "the server died on startup" from a full-budget wait into an immediate report that names
/// the exit status. Use this one where it is not — a helper that only receives an address, or a
/// server this process did not spawn.
///
/// The returned [`TcpStream`] is a real connection with nothing sent on it. Dropping it is what
/// every caller that only wanted the readiness signal does; `serve`'s empty-probe regression keeps
/// it, because a connect-then-close *is* the hostile probe it means to make.
pub fn wait_until_listening(addr: &str) -> Result<TcpStream, String> {
    wait_to_listen(addr, None, None, readiness_budget())
}

/// Wait until something accepts on `addr`, giving up early if `child` exits without binding.
///
/// This is the form that lets the budget be generous. A server that fails to start — a bad
/// argument, a port taken, a panic in the runtime — is reported as soon as its process is reaped
/// (plus a short grace window for a wrapper whose own child owns the socket), with the exit status
/// in the message, rather than after the wall-clock budget that exists for *slow* startups.
///
/// `log` is the [`ServerLog`] the child was spawned into ([`ServerLog::spawn`]), and it is a
/// **required** argument rather than an option because the exit status alone was never enough. The
/// three incidents this module's header recounts each ended at `the server process exited (exit
/// status: 1)`, and the sentence that would have ended them in five minutes —
/// `[E0021] cannot bind …: Address already in use`, `[E0005] …` — was on the stderr the caller had
/// sent to `/dev/null`. A caller who has a child in reach has a log, or the diagnostic dies again.
pub fn wait_until_listening_or_child_exits(
    child: &mut Child,
    addr: &str,
    log: &ServerLog,
) -> Result<TcpStream, String> {
    wait_to_listen(addr, Some(child), Some(log), readiness_budget())
}

fn wait_to_listen(
    addr: &str,
    mut child: Option<&mut Child>,
    log: Option<&ServerLog>,
    budget: Duration,
) -> Result<TcpStream, String> {
    let load = machine_load();
    let started = Instant::now();
    let mut deadline = started + budget;
    let mut exited: Option<String> = None;
    // Assigned by the first attempt below, and only ever read after one — there is no "before the
    // first connect" state to name.
    let mut last_error;

    loop {
        match connect(addr) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_error = e.to_string(),
        }
        // The process is checked after the connect attempt, never before it: a server that bound
        // and then exited in the same breath still counts as having been reachable, and checking
        // in this order means the last word belongs to the socket rather than to the race.
        if exited.is_none()
            && let Some(c) = child.as_deref_mut()
            && let Some(status) = c.try_wait().ok().flatten()
        {
            exited = Some(status.to_string());
            // Whatever is left of the budget is now worth at most the grace window.
            deadline = deadline.min(Instant::now() + GRACE_AFTER_EXIT);
        }
        if Instant::now() >= deadline {
            let waited = started.elapsed().as_secs_f64();
            let why = match exited {
                Some(status) => format!(
                    "nothing ever accepted on {addr}: the server process exited ({status}) and \
                     still nothing had bound {:.1}s later, after {waited:.1}s in all — the server \
                     never came up, so the {:.1}s readiness budget was not what failed here (last \
                     connect error: {last_error})",
                    GRACE_AFTER_EXIT.as_secs_f64(),
                    budget.as_secs_f64(),
                ),
                None => format!(
                    "nothing accepted on {addr} within the readiness budget: waited {waited:.1}s \
                     of {:.1}s ({load}){} — last connect error: {last_error}. If this machine was \
                     loaded rather than the server broken, raise the budget with \
                     {BUDGET_ENV}=<seconds>.",
                    budget.as_secs_f64(),
                    if child.is_some() {
                        ", and the server process is still running"
                    } else {
                        ""
                    },
                ),
            };
            // The point of the whole exercise: a server that failed to start has already said why,
            // and this is the moment to repeat it. Both branches get it — a process that exited
            // says it in its dying words, and one that is alive but never binds is usually stuck
            // saying something too (a check that keeps failing, a port it will not stop retrying).
            return Err(match log {
                Some(log) => format!("{why}\n\n{}", log.quoted()),
                None => why,
            });
        }
        std::thread::sleep(POLL);
    }
}

/// Wait until nothing accepts on `addr` any more — the shutdown direction, for a test that has just
/// sent SIGINT and means to prove the listener closed.
pub fn wait_until_closed(addr: &str) -> Result<(), String> {
    wait_until_closed_within(addr, readiness_budget())
}

/// The teardown direction: give a killed server a moment to stop accepting, and do not care whether
/// it did.
///
/// A teardown polls the port only so the fixture directory is not removed out from under a live
/// server; it asserts nothing, so it must not spend the readiness budget — five spawns × 30s of
/// waiting on a child nobody is measuring would dwarf the suite it belongs to. The direction where
/// the close *is* the claim is [`wait_until_closed`], which does spend it.
pub fn settle_closed(addr: &str) {
    let _ = wait_until_closed_within(addr, TEARDOWN);
}

/// How long [`settle_closed`] gives a killed server. Unchanged from the hand-written teardown loops
/// it replaces (40 × 50ms), because nothing depends on the outcome.
const TEARDOWN: Duration = Duration::from_secs(2);

/// [`wait_until_closed`] with an explicit budget.
fn wait_until_closed_within(addr: &str, budget: Duration) -> Result<(), String> {
    let load = machine_load();
    let started = Instant::now();
    let deadline = started + budget;
    let mut last = "the port was still accepting".to_string();
    loop {
        match connect(addr) {
            // A refusal on loopback is the definitive answer: nothing holds the port.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(()),
            Ok(_) => {}
            // ANY OTHER error means something is still there — most importantly a connect that
            // *times out*, which is what a bound socket whose accept queue has filled does. Reading
            // that as "closed" is how this assertion goes vacuous, and it is not hypothetical: with
            // nothing accepting the polls, the queue fills after ~128 of them, every later attempt
            // times out, and a listener that never closed at all reports closed about three seconds
            // in. Found by pointing this at a `TcpListener` that never calls `accept` — a wedged
            // server, which is exactly what the two suites asserting on a drain exist to catch.
            Err(e) => last = format!("connecting failed with `{e}`, which is not a refusal"),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{addr} did not stop accepting connections within {:.1}s of a {:.1}s budget \
                 ({load}) — {last}. If this machine was loaded rather than the server wedged, \
                 raise the budget with {BUDGET_ENV}=<seconds>.",
                started.elapsed().as_secs_f64(),
                budget.as_secs_f64(),
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// One connect attempt, bounded. A literal `host:port` goes through `connect_timeout` so a bound
/// socket that never accepts cannot block past [`ATTEMPT`]; anything needing resolution falls back
/// to the plain connect.
fn connect(addr: &str) -> std::io::Result<TcpStream> {
    match addr.parse::<SocketAddr>() {
        Ok(sock) => TcpStream::connect_timeout(&sock, ATTEMPT),
        Err(_) => TcpStream::connect(addr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The knob wins when it is set, and the scaled base applies when it is not. Asserted on the
    /// rule rather than on a mutated process environment: `set_var` from a test thread races every
    /// other test in the binary, which is the same class of bug one directory up.
    #[test]
    fn the_budget_honors_the_environment_knob() {
        let idle = Load {
            one_minute: Some(0.1),
            cores: 20,
            factor: 1.0,
        };
        assert_eq!(
            scaled_budget(Some(Duration::from_secs(7)), idle),
            Duration::from_secs(7)
        );
        assert_eq!(scaled_budget(None, idle), BASE);
    }

    /// The scaling never *shrinks* the budget on an idle machine and never runs away on a wedged
    /// one — the two properties that make a load-scaled budget safe to depend on. The load that
    /// broke the old fixed budget (16.4 over 20 cores) is included as the case of record.
    #[test]
    fn the_load_factor_is_bounded_at_both_ends() {
        let live = machine_load();
        assert!(
            (1.0..=4.0).contains(&live.factor),
            "factor {} out of range",
            live.factor
        );
        assert!(readiness_budget() >= BASE && readiness_budget() <= BASE * 4);

        let at = |load: f64| {
            let cores = 20;
            let factor = (load / cores as f64).clamp(1.0, 4.0);
            scaled_budget(
                None,
                Load {
                    one_minute: Some(load),
                    cores,
                    factor,
                },
            )
        };
        assert_eq!(at(0.3), BASE, "an idle machine gets the base, not less");
        assert_eq!(
            at(16.4),
            BASE,
            "the load that broke the 4s budget still gets 30s — the base is the fix there"
        );
        assert_eq!(at(60.0), BASE * 3);
        assert_eq!(at(400.0), BASE * 4, "and the scaling is capped");
    }

    /// An address on which nothing can be listening, for the tests that assert a *failure* to
    /// connect.
    ///
    /// Not [`crate::free_port`]: that hands out a port for a server to **bind**, and it cannot
    /// promise the port stays empty. It draws by binding `:0` and dropping the socket, and its
    /// claim registry is a marker *file* — which stops another `free_port` caller from being handed
    /// the same port, and says nothing to the kernel, which will hand that port to anyone who binds
    /// `:0` next. The tests below then poll the address for the whole grace window
    /// ([`GRACE_AFTER_EXIT`], two seconds), so *any* listener appearing anywhere in those two
    /// seconds — a sibling test in this binary, another crate's tests under `cargo test
    /// --workspace`, another process on the machine — makes the connect succeed and the assertion
    /// fail. Reproduced at 30 runs in 40 by modelling that window under port churn; the merge gate
    /// hit it for real once, and it reads as a failure in whatever crate happens to be named.
    ///
    /// Port 1 removes the race rather than narrowing it: it is privileged, so no unprivileged
    /// process on the machine can bind it, and the connect is refused in microseconds instead of
    /// polling a port that might fill. (Running the suite as root would forfeit that, and nothing
    /// here does.)
    const DEAD_ADDR: &str = "127.0.0.1:1";

    /// A listening socket is seen immediately, and the connection comes back for the caller to use.
    #[test]
    fn a_bound_port_is_ready_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let started = Instant::now();
        let stream = wait_until_listening(&addr).expect("a bound port is listening");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(stream.peer_addr().unwrap().to_string(), addr);
    }

    /// The defect's own shape, inverted: an address nothing will ever bind must fail *with the
    /// diagnostic* — budget, elapsed, load and the knob's name — rather than hang or say only
    /// `Connection refused`.
    #[test]
    fn a_dead_address_times_out_with_the_budget_in_the_message() {
        let started = Instant::now();
        // Straight to the inner wait so the budget can be one second: a test that asserts on a
        // *timeout* must not sit out the real one.
        let err = wait_to_listen(DEAD_ADDR, None, None, Duration::from_secs(1))
            .expect_err("nothing is bound to that port");
        assert!(started.elapsed() < Duration::from_secs(10), "it hung");
        for expected in ["waited", "1.0s", "load", BUDGET_ENV, DEAD_ADDR] {
            assert!(err.contains(expected), "{expected:?} missing from: {err}");
        }
    }

    /// A process that exits without binding is reported as such, and fast — this is what allows the
    /// budget to be 30s in the first place.
    #[test]
    fn a_server_that_exits_without_binding_is_reported_at_once() {
        let log = ServerLog::new("exits-at-once");
        let mut child = log
            .spawn(&mut std::process::Command::new("true"))
            .expect("spawn /bin/true");
        let started = Instant::now();
        let err = wait_until_listening_or_child_exits(&mut child, DEAD_ADDR, &log)
            .expect_err("/bin/true binds nothing");
        assert!(
            started.elapsed() < GRACE_AFTER_EXIT + Duration::from_secs(3),
            "a dead server must not consume the readiness budget: {:?}",
            started.elapsed()
        );
        assert!(
            err.contains("exited") && err.contains("never came up"),
            "the message must name the exit rather than the budget: {err}"
        );
    }

    /// **The defect of record, in miniature.** A server that dies telling you exactly why must have
    /// said it *in the failure message*. This is the hermetic half of the proof — the
    /// `noeta-cli` suite `serve::a_server_that_cannot_bind_says_so_in_the_failure` drives the real
    /// binary into a real `[E0021]` — and it is the half that runs on every `cargo test`.
    ///
    /// Before the capture existed this same message ended at `(last connect error: Connection
    /// refused …)`, and three separate investigations started there.
    #[test]
    fn a_dying_server_is_quoted_in_the_message_that_reports_it() {
        let log = ServerLog::new("dying-words");
        let mut child = log
            .spawn(std::process::Command::new("sh").args([
                "-c",
                "echo '[E0021] cannot bind 127.0.0.1:1: Address already in use' >&2; exit 1",
            ]))
            .expect("spawn sh");
        let err = wait_until_listening_or_child_exits(&mut child, DEAD_ADDR, &log)
            .expect_err("that server binds nothing");
        assert!(
            err.contains("E0021") && err.contains("Address already in use"),
            "the server's own words are missing from the failure it caused: {err}"
        );
    }

    /// The shutdown direction, both ways round.
    #[test]
    fn the_closed_direction_waits_for_the_listener_to_go_away() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let err = wait_until_closed_within(&addr, Duration::from_millis(200))
            .expect_err("the listener is still open");
        assert!(err.contains("did not stop accepting"), "{err}");
        drop(listener);
        wait_until_closed_within(&addr, Duration::from_secs(5)).expect("a closed port is closed");
    }

    /// **Only a refusal means closed.** A listener whose accept queue is full stops completing
    /// connects — they time out instead — and an earlier draft read any connect failure as "the
    /// listener closed", so a *wedged* server (bound, never accepting) reported a clean shutdown
    /// and the two suites that assert on a drain would have passed without one. The queue fills on
    /// its own after ~128 unaccepted polls, which is about three seconds of polling: slow enough
    /// that the short-budget test above never saw it, and well inside the budget a real drain gets.
    #[test]
    fn a_listener_that_never_accepts_is_not_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // Fill the accept queue: the listener never calls `accept`, so these stay pending. Held in
        // a `Vec` because dropping them frees nothing — an unaccepted connection keeps its slot.
        let _pending: Vec<TcpStream> = (0..256)
            .map_while(|_| TcpStream::connect_timeout(&addr.parse().unwrap(), ATTEMPT).ok())
            .collect();
        let err = wait_until_closed_within(&addr, Duration::from_millis(500))
            .expect_err("a wedged listener has NOT closed");
        assert!(
            err.contains("not a refusal"),
            "the timeout must be reported as 'still there', not as a close: {err}"
        );
    }
}
