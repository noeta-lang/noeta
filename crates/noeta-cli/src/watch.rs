//! `--watch` — the restart-on-change dev loop (server-hmr W0).
//!
//! Watch mode wraps the *whole invocation*: `noeta run --watch app.noe` (equally `serve`, `test`,
//! or any other subcommand) re-executes `noeta run app.noe` as a child process and restarts it
//! whenever a project source file changes. The flag is stripped from argv **before** clap parsing,
//! which is what makes it uniform across derive-built commands and extension-contributed ones
//! (`noeta serve` is an `ExtCommand`) without either knowing watch exists.
//!
//! Full restart is deliberate at this slice: the startup cache makes a cold start cheap
//! (~milliseconds + compile of the changed file), and restart remains the permanent fallback for
//! changes the hot path cannot absorb (`SwapBlocker` — signature/layout/namespace changes). The
//! W1 hot path swaps body-level edits into the running process instead of restarting; it slots in
//! underneath this same watcher.
//!
//! Watched: `*.noe` plus `noeta.toml` / `noeta.lock` under the current directory, recursively
//! (hidden directories like `.git` are ignored). Events are debounced so an editor's
//! write-then-rename lands as one restart.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

/// How long to keep draining filesystem events after the first relevant one before restarting —
/// long enough to coalesce an editor's multi-event save, short enough to feel immediate.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// How often the loop checks whether the child exited on its own while also polling for events.
const POLL: Duration = Duration::from_millis(100);

/// The child-exit sentinel meaning "restart me now" (server-hmr W1): the in-process hot path
/// exits with this code when an edit needs a full restart (a `SwapBlocker`, or a change outside
/// the entry file), and the `--watch` wrapper restarts immediately instead of waiting for the
/// next filesystem event.
pub(crate) const HOT_RESTART_CODE: i32 = 91;

/// If the invocation carries `--watch`, strip it and run the restart-on-change loop instead of a
/// single execution. `None` means no flag: the ordinary CLI proceeds. Called before clap parsing
/// (see module docs for why).
pub(crate) fn maybe_watch() -> Option<ExitCode> {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let before = args.len();
    args.retain(|a| a != "--watch");
    if args.len() == before {
        return None;
    }
    // A native-dependency program's directives (`@openapi`) and modules live only inside its
    // composed toolchain — the registry symbols are not linked into this stock binary. So the impact
    // session `watch_loop` builds would link against a std-only registry and resolve none of them.
    // Delegate the whole `--watch` invocation to the composed toolchain first (`exec`, same argv,
    // `NOETA_COMPOSED=1`); it re-enters here with the guard set, so `maybe_delegate` returns without
    // delegating again and the loop — session and all — runs with every native extension registered
    // and `current_exe()` pointing at the composed binary (so spawned child runs stay composed).
    // A pure-Noeta program resolves no native crates and delegates nothing, looping here as before.
    if let Some(entry) = args
        .iter()
        .map(PathBuf::from)
        .find(|p| p.extension().is_some_and(|e| e == "noe"))
    {
        // Absolute: the entry arrives as typed (`main.noe`), and `maybe_delegate` finds the manifest
        // from the entry's *parent* — empty for a bare relative name, which would skip delegation.
        let entry = std::fs::canonicalize(&entry)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&entry));
        if let Err(code) = crate::compose::maybe_delegate(&entry) {
            return Some(code);
        }
    }
    Some(watch_loop(&args))
}

fn watch_loop(args: &[OsString]) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[watch] cannot resolve the noeta executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[watch] cannot start the file watcher: {e}");
            return ExitCode::FAILURE;
        }
    };
    let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
        eprintln!("[watch] cannot watch {}: {e}", root.display());
        return ExitCode::FAILURE;
    }
    eprintln!(
        "[watch] watching {} — restarting `noeta {}` on change (Ctrl-C to stop)",
        root.display(),
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );

    // `serve` gets the in-process HOT path (server-hmr W1): the child owns watching, swapping
    // edits into the live process, and exiting with [`HOT_RESTART_CODE`] when only a full restart
    // can absorb a change — the wrapper then restarts immediately and otherwise stays out of the
    // way (double-watching would restart the server on the very edits the child just swapped).
    // `--parallel` (server-hmr F5) hot-reloads too: the swap broadcasts to every worker isolate
    // via the shared queue, so the whole fleet swaps in place.
    let hot = args.first().is_some_and(|a| a == "serve");
    // Impact-filtered tier watch (server-hmr W3; multi-file since the salsa rework): a
    // `test`/`bench` rerun narrows to the declarations the edit impacted (the runners'
    // `--name` filter), computed by the whole-project engine
    // ([`noeta_ide::impact::ImpactSession`]) — a salsa workspace over the entry's directory,
    // so an edit to an imported module narrows too, instead of degrading to a full rerun.
    // A session that cannot be built (no entry, unreadable project) leaves plain
    // restart-everything watching.
    let impact_entry = (!hot && args.first().is_some_and(|a| a == "test" || a == "bench"))
        .then(|| {
            args.iter()
                .map(PathBuf::from)
                .find(|p| p.extension().is_some_and(|e| e == "noe"))
        })
        .flatten();
    let mut session = impact_entry
        .as_deref()
        .and_then(noeta_ide::impact::ImpactSession::new);
    // The first `.noe` argument of any command, so `run`/`serve` (which build no impact session)
    // can still learn which non-`.noe` files their expansion hooks read and watch those too.
    let watch_entry: Option<PathBuf> = args
        .iter()
        .map(PathBuf::from)
        .find(|p| p.extension().is_some_and(|e| e == "noe"));
    let mut extra: Vec<OsString> = Vec::new();
    loop {
        let mut cmd = Command::new(&exe);
        cmd.args(args).args(&extra);
        if hot {
            cmd.env("NOETA_HOT", "1");
        }
        // The sources this run observes become the next edit's impact baseline (and the
        // workspace re-syncs, absorbing any member-set change since the last run).
        if let Some(s) = session.as_mut() {
            s.rebaseline();
        }
        // The non-`.noe` files an `@openapi` (or any expanding directive) reported reading — a spec.
        // Recomputed each iteration because an edit to the `.noe` can change which spec it names.
        // The session already holds them post-rebaseline; a sessionless mode links once to find
        // them. Folded into the event filter below so a spec edit is not discarded as "not a source".
        let watched_reads: Vec<PathBuf> = match session.as_ref() {
            Some(s) => s.reads().to_vec(),
            None => watch_entry
                .as_deref()
                .map(noeta_ide::impact::spec_reads)
                .unwrap_or_default(),
        };
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[watch] failed to start: {e}");
                return ExitCode::FAILURE;
            }
        };
        if hot {
            let status = wait_ignoring_events(&rx, &mut child);
            if status.code() == Some(HOT_RESTART_CODE) {
                eprintln!("[watch] restarting");
                continue;
            }
            // The server stopped for real (boot-time compile error, crash, Ctrl-C races): wait
            // for the next edit and try again — a red boot must retry once the code is fixed.
            eprintln!("[watch] finished ({status}) — waiting for changes");
            wait_for_change(&rx, &watched_reads);
        } else {
            let mut changed = Vec::new();
            let exited = run_until_change(&rx, &mut child, &mut changed, &watched_reads);
            if let Some(status) = exited {
                // The program finished on its own (a `run` reaching its end, a crash): report and
                // wait for the next change rather than looping hot.
                eprintln!("[watch] finished ({status}) — waiting for changes");
                changed = wait_for_change(&rx, &watched_reads);
            } else {
                // A relevant change arrived while the program was running: stop it and restart.
                let _ = child.kill();
                let _ = child.wait();
            }
            // Decide the next run's filter; an inert edit skips the run entirely.
            loop {
                match next_filter(&mut session, impact_entry.as_deref(), &changed) {
                    Filter::All(reason) => {
                        if let Some(reason) = reason {
                            eprintln!("[watch] rerunning everything: {reason}");
                        }
                        extra.clear();
                        break;
                    }
                    Filter::Names(names) => {
                        eprintln!("[watch] impacted: {}", names.join(", "));
                        extra = names
                            .iter()
                            .flat_map(|n| [OsString::from("--name"), OsString::from(n)])
                            .collect();
                        break;
                    }
                    Filter::Skip => {
                        eprintln!("[watch] nothing impacted — waiting for changes");
                        changed = wait_for_change(&rx, &watched_reads);
                    }
                }
            }
        }
        eprintln!("[watch] change detected — restarting");
    }
}

/// What the next tier run should execute (server-hmr W3).
enum Filter {
    /// Everything — with the reason when the edit was unattributable.
    All(Option<String>),
    /// Exactly these declarations (the runner's `--name` filter drops non-tier names).
    Names(Vec<String>),
    /// Nothing changed behaviorally — no run at all.
    Skip,
}

/// Decide the next run's filter from the changed paths, via the whole-project impact session.
/// Anything that breaks attribution — no session (not a tier command, or the project would not
/// anchor one), a manifest change, an edit the engine cannot attribute — degrades to a full
/// rerun; the session itself narrows edits to any project module (multi-file impact).
fn next_filter(
    session: &mut Option<noeta_ide::impact::ImpactSession>,
    entry: Option<&Path>,
    changed: &[PathBuf],
) -> Filter {
    if session.is_none() {
        return Filter::All(None);
    }
    // The manifest/lockfile govern dependency resolution and editions — the workspace itself
    // is stale, not just a member. Rebuild the session against the new resolution and rerun
    // everything once.
    if changed.iter().any(|p| {
        p.file_name()
            .is_some_and(|n| n == "noeta.toml" || n == "noeta.lock")
    }) {
        *session = entry.and_then(noeta_ide::impact::ImpactSession::new);
        return Filter::All(Some("the manifest changed".into()));
    }
    let Some(s) = session.as_mut() else {
        return Filter::All(None);
    };
    match s.impact_of_changes(changed) {
        noeta_ide::impact::Impact::Decls(decls) if decls.is_empty() => Filter::Skip,
        noeta_ide::impact::Impact::Decls(decls) => Filter::Names(decls),
        noeta_ide::impact::Impact::All { reason } => Filter::All(Some(reason)),
    }
}

/// Wait for the child to exit while draining (and ignoring) watcher events — the hot child owns
/// reacting to changes; the wrapper only cares how it exits.
fn wait_ignoring_events(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    child: &mut Child,
) -> std::process::ExitStatus {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status;
        }
        let _ = rx.recv_timeout(POLL);
    }
}

/// Drive one child run: poll for its exit while draining watcher events. Returns `Some(status)`
/// if the child exited before any relevant change, `None` when a (debounced) relevant change
/// arrived while it was still running.
fn run_until_change(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    child: &mut Child,
    changed: &mut Vec<PathBuf>,
    reads: &[PathBuf],
) -> Option<std::process::ExitStatus> {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        match rx.recv_timeout(POLL) {
            Ok(event) if relevant(&event, reads) => {
                collect_paths(&event, changed, reads);
                drain(rx, changed, reads);
                return None;
            }
            // Irrelevant event, timeout tick, or a watcher error: keep polling. A disconnected
            // watcher can produce no further events; treat it like a quiet tick (the child keeps
            // running — watch just degrades to plain execution).
            _ => {}
        }
    }
}

/// Block until a relevant change arrives (used after the child finished on its own); returns the
/// changed source paths the debounced burst touched.
fn wait_for_change(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    reads: &[PathBuf],
) -> Vec<PathBuf> {
    loop {
        match rx.recv() {
            Ok(event) if relevant(&event, reads) => {
                let mut changed = Vec::new();
                collect_paths(&event, &mut changed, reads);
                drain(rx, &mut changed, reads);
                return changed;
            }
            Ok(_) => {}
            Err(_) => {
                // Watcher gone — nothing further can arrive; park indefinitely rather than spin.
                std::thread::park();
            }
        }
    }
}

/// Debounce: keep draining events for [`DEBOUNCE`] after the first relevant one, collecting the
/// source paths they touch.
fn drain(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    changed: &mut Vec<PathBuf>,
    reads: &[PathBuf],
) {
    while let Ok(event) = rx.recv_timeout(DEBOUNCE) {
        collect_paths(&event, changed, reads);
    }
}

/// Append `event`'s project-source paths (the filter [`relevant_path`] applies) to `changed`.
fn collect_paths(
    event: &notify::Result<notify::Event>,
    changed: &mut Vec<PathBuf>,
    reads: &[PathBuf],
) {
    if let Ok(event) = event {
        changed.extend(
            event
                .paths
                .iter()
                .filter(|p| relevant_path(p, reads))
                .cloned(),
        );
    }
}

fn relevant(event: &notify::Result<notify::Event>, reads: &[PathBuf]) -> bool {
    let Ok(event) = event else { return false };
    // Mutations only. Access events MUST be ignored — the restarted program *reads* its own
    // sources to compile them, and reacting to those reads is a restart storm.
    let mutation = matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    );
    mutation && event.paths.iter().any(|p| relevant_path(p, reads))
}

/// A path the watch loop reacts to: `*.noe`, a manifest/lockfile, or a **file an expansion hook
/// reported reading** (an `@openapi` spec), none inside a hidden directory.
///
/// The `reads` set is why a spec change restarts at all: without it a `.json`/`.yaml` change is
/// invisible to the loop, and editing (or creating) the spec would leave the generated client
/// stale. `reads` paths are canonicalized; `notify` reports canonical paths, so the comparison
/// holds. A hidden-directory read is still excluded — nothing legitimate reads from one, and it is
/// where build caches churn.
fn relevant_path(path: &Path, reads: &[PathBuf]) -> bool {
    let hidden = path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.starts_with('.') && s.len() > 1 && s != "..")
    });
    if hidden {
        return false;
    }
    let is_noe = path.extension().is_some_and(|e| e == "noe");
    let is_manifest = path
        .file_name()
        .is_some_and(|n| n == "noeta.toml" || n == "noeta.lock");
    let is_read = reads.iter().any(|r| r == path);
    is_noe || is_manifest || is_read
}

// ------------------------------------------------------------------ the in-process hot watcher

/// Spawn the hot-reload watcher thread (server-hmr W1) inside a `NOETA_HOT` serve process. On an
/// edit of `entry` it **re-links the project** (the same load the boot did, `tail` and all),
/// checks it (transactional: red code is reported and the old version keeps serving), diffs the
/// entry unit against the last version the VM consumed, and deposits swappable plans into
/// `mailbox` — the run thread applies them at its next scheduler tick. Anything the live process
/// cannot absorb — a [`SwapBlocker`], a change to any *other* project file — exits the process
/// with [`HOT_RESTART_CODE`] so the `--watch` wrapper restarts it.
///
/// `tail` is the driver's synthesized entry call (`server.serve(port, fetch, host)`), passed so
/// each re-link is byte-for-byte the boot's: it goes through the loader, not onto the linked
/// program, for the same reasons [`noeta_loader::load_with_deps_appending`] documents. `applied` is
/// the entry unit of the program the VM **actually compiled** — the diff baseline, handed over
/// rather than re-derived here, so it cannot disagree with what is running and so nothing slow
/// stands between this thread starting and its file watcher being armed.
///
/// `front` is the **boot's own resolved dependency graph** (audit-10), handed over for the same
/// reason `applied` is: a re-link must link against the graph the running program was built with.
/// This used to re-resolve from scratch on every edit — see [`relink_entry_unit`].
pub(crate) fn spawn_hot_watcher(
    entry: std::path::PathBuf,
    tail: Vec<noeta_ast::Stmt>,
    applied: EntryUnit,
    front: std::sync::Arc<noeta_runner::compile::FrontFacts>,
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_host_real::Notify>,
) {
    std::thread::spawn(move || hot_watcher(entry, tail, applied, front, mailbox, wake));
}

fn hot_watcher(
    entry: std::path::PathBuf,
    tail: Vec<noeta_ast::Stmt>,
    mut applied: EntryUnit,
    front: std::sync::Arc<noeta_runner::compile::FrontFacts>,
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_host_real::Notify>,
) {
    let entry_canon = entry.canonicalize().unwrap_or_else(|_| entry.clone());
    // ARM THE WATCH FIRST. Everything else here — reading spec reads, and once upon a time
    // re-linking to recover the baseline — takes long enough that an edit saved right after boot
    // lands in the gap and is never seen at all: no event, no swap, no output, the developer's
    // first edit silently ignored. `notify` queues into the channel from its own thread, so events
    // arriving while the rest of this setup runs are waiting in `rx` when the loop starts.
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let Ok(mut watcher) = notify::recommended_watcher(tx) else {
        eprintln!("[hot] cannot start the file watcher — edits will not reload");
        return;
    };
    let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut roots = vec![root];
    // The entry may live outside the cwd; watch its directory too.
    if let Some(parent) = entry_canon.parent()
        && !roots.iter().any(|r| entry_canon.starts_with(r))
    {
        roots.push(parent.to_path_buf());
    }
    for r in &roots {
        if watcher.watch(r, RecursiveMode::Recursive).is_err() {
            eprintln!(
                "[hot] cannot watch {} — edits there will not reload",
                r.display()
            );
        }
    }
    // Files an expansion hook read (an `@openapi` spec). A change to one is never entry-swappable —
    // it regenerates members — so it must reach the `all_entry` check below and force a restart,
    // which means passing the event filter first. Computed once: the hot process is restarted
    // wholesale (and this recomputed) whenever the entry itself changes.
    let reads = noeta_ide::impact::spec_reads(&entry);
    loop {
        let event = match rx.recv() {
            Ok(event) => event,
            Err(_) => return,
        };
        if !relevant(&event, &reads) {
            continue;
        }
        let mut paths: Vec<std::path::PathBuf> = match &event {
            Ok(e) => e.paths.clone(),
            Err(_) => Vec::new(),
        };
        // Debounce, collecting every path the edit burst touched.
        while let Ok(more) = rx.recv_timeout(DEBOUNCE) {
            if relevant(&more, &reads)
                && let Ok(e) = &more
            {
                paths.extend(e.paths.iter().cloned());
            }
        }
        // Only source-relevant paths participate: an editor's atomic save emits a rename event
        // carrying BOTH paths (temp file + target), and the temp half must not read as "a change
        // outside the entry file".
        paths.retain(|p| relevant_path(p, &reads));
        let all_entry = paths
            .iter()
            .all(|p| p.canonicalize().map(|c| c == entry_canon).unwrap_or(false));
        if !all_entry {
            eprintln!("[hot] change outside the entry file — restarting");
            std::process::exit(HOT_RESTART_CODE);
        }
        // The transactional gate: red code never swaps; the old version keeps serving. The
        // rendered diagnostics also ride the channel's error slot to live LiveView clients
        // (the browser overlay, server-hmr L3) — waking the run thread to deliver promptly.
        let (new_unit, sites) = match relink_entry_unit(&entry, &tail, &front) {
            Ok(checked) => checked,
            Err(err) => {
                eprint!("{}", err.text());
                // Red code: report, keep serving, and put the diagnostics under the browser's
                // overlay. An unreadable project (a half-written file mid-save) is no verdict at
                // all — the next event carries the finished text.
                if let RelinkError::Diagnostics(rendered) = err {
                    eprintln!("[hot] check failed — still serving the old version");
                    if let Ok(mut slot) = mailbox.error.lock() {
                        *slot = Some(rendered);
                    }
                    wake_all(&wake);
                }
                continue;
            }
        };
        // Diff against the last version this watcher DEPOSITED (server-hmr F5). The watcher is the
        // single depositor, so `applied` is the exact baseline of the append-only broadcast
        // queue: each plan is diffed against its predecessor, and every worker applies the queue
        // in order — no per-consumer reconciliation.
        match noeta_compiler::hotswap::diff_programs(
            &applied.program,
            &applied.src,
            &new_unit.program,
            &new_unit.src,
        ) {
            noeta_compiler::hotswap::SwapDiff::Unchanged => {}
            noeta_compiler::hotswap::SwapDiff::Swap(plan) => {
                // Convert the compiler's `SwapPlan` into the VM's compiler-free `HotFragment` at the
                // boundary (native-size slice 2): the watcher owns the compiler, the VM must not.
                //
                // The gate's own `Sites` ride along (server-hmr H5). The check above is of the
                // WHOLE new program and the fragment's statements are clones of that program's,
                // spans intact — so every worker draining this deposit compiles the swapped code
                // with the same site-keyed codegen and precise destructor relevance a restart
                // would give it, instead of the checkerless compile that degraded a long editing
                // session relative to a cold start. The bundle crosses the VM core opaquely
                // (`noeta_vm::FragmentSites`), which is why the core still names no checker.
                //
                // The bundle is program-sized (measured: ~0.7 B per source byte — 204 KiB for a
                // 293 KB app, against 14 KiB for the one-function fragment beside it), so the queue
                // reclaims each plan's payload as soon as the last worker has installed it; the
                // generation slot stays, the bundle does not (see `noeta_vm::HotChannel`).
                mailbox.deposit(noeta_vm::HotFragment {
                    fragment: plan.fragment,
                    rerun_top_level: plan.rerun_top_level,
                    added: plan.added,
                    changed: plan.changed,
                    sites: Some(std::sync::Arc::new(sites)),
                });
                applied = new_unit;
                // A green deposit supersedes any pending red-check overlay, and the wake rouses
                // every (possibly idle) worker to apply it now rather than at its next request.
                if let Ok(mut err) = mailbox.error.lock() {
                    err.take();
                }
                wake_all(&wake);
            }
            noeta_compiler::hotswap::SwapDiff::NeedsRestart(blockers) => {
                for b in &blockers {
                    eprintln!("[hot] restart needed: {b}");
                }
                std::process::exit(HOT_RESTART_CODE);
            }
        }
    }
}

/// Rouse every worker executor parked on the shared wake (server-hmr F5): `notify_waiters` wakes
/// all currently-parked accepts at once, and `notify_one` leaves a stored permit for a worker
/// racing into its wait.
///
/// **Measured reach.** An idle `--parallel N` fleet applies a deposited swap in **every** worker
/// before the next request, at N = 1, 2, 3 and 5 — `parallel_hot` asserts exactly that, of the one
/// request made after the swap, with no retry. Without the wake, every worker answers one request
/// with pre-swap code (measured 1 of 1 and 3 of 3), which is what that test fails on.
///
/// The reach used to be one or two workers whatever N was, and this function was blamed for it. It
/// was not this function: giving each consumer its own `Notify` was built and measured, to rule out
/// the `notify_one` single-permit race, and changed nothing — because every worker *was* being
/// roused and then losing the swap on the far side of the wake, in the mailbox's `try_lock` drain. A
/// wake buys an idle worker exactly one scheduler tick, and a drain that gives up on contention
/// needs a second one it will never get. See `noeta_vm::HotChannel::drain`, which now blocks.
fn wake_all(wake: &noeta_host_real::Notify) {
    wake.notify_waiters();
    wake.notify_one();
}

/// The entry file's own statements **as the linker qualified them**, with the text they were parsed
/// from. The diff baseline and each candidate are both this, so a swap plan's declarations carry
/// the identities the running module actually bound.
pub(crate) struct EntryUnit {
    program: noeta_ast::Program,
    src: String,
}

impl EntryUnit {
    /// The entry unit of a **linked program the driver already built** — the boot's, which is the
    /// diff baseline: the code the VM compiled and is serving.
    ///
    /// The linked program is every module's declarations merged; only the entry's belong in a diff
    /// whose two sides are two versions of that one file. Statements with an **empty** span are the
    /// driver's synthesized entry call (`server.serve(port, fetch, host)`, stamped at offset 0 of
    /// the entry source): they are not in the file the user edits, they have no text to fingerprint,
    /// and a re-running swap that replayed them would start a second server.
    pub(crate) fn of(program: &noeta_ast::Program, entry: &noeta_span::Source) -> EntryUnit {
        EntryUnit {
            program: noeta_ast::Program {
                stmts: program
                    .stmts
                    .iter()
                    .filter(|stmt| {
                        let span = stmt.span();
                        span.source == entry.id() && !span.is_empty()
                    })
                    .cloned()
                    .collect(),
                span: program.span,
            },
            src: entry.text().to_string(),
        }
    }
}

/// Why a re-link produced no candidate to diff.
enum RelinkError {
    /// The project could not be read or its graph not resolved — no verdict, wait for the next edit.
    Unreadable(String),
    /// Rendered lex/parse/link/type diagnostics: red code, which never swaps.
    Diagnostics(String),
}

impl RelinkError {
    /// What to print either way — diagnostics, or the reason the project could not be read.
    pub(crate) fn text(&self) -> &str {
        match self {
            RelinkError::Unreadable(text) | RelinkError::Diagnostics(text) => text,
        }
    }

    /// A shared-front-half failure ([`crate::context::load_entry_with_tail`]) as a relink verdict.
    ///
    /// The split is by *kind*, not by exit code: the two diagnostic-carrying variants are red code
    /// (report, keep serving the old version, put them under the browser overlay); everything else
    /// is a project that could not be read at all — a half-written file mid-save, an unresolvable
    /// graph — which is no verdict, so the next event's finished text decides.
    fn from_link(failure: noeta_runner::CompileFailure) -> RelinkError {
        let (text, _) = failure.to_text();
        match failure {
            noeta_runner::CompileFailure::Load(_)
            | noeta_runner::CompileFailure::Diagnostics { .. } => RelinkError::Diagnostics(text),
            noeta_runner::CompileFailure::Message(_)
            | noeta_runner::CompileFailure::Unreadable(_) => {
                RelinkError::Unreadable(format!("[hot] {text}"))
            }
        }
    }
}

/// Re-link the project exactly as the serve boot linked it — [`crate::context::load_entry_with_tail`],
/// the same front half the boot ran, with the same `tail` — then check it, and return the **entry
/// unit** (the entry file's own statements, qualified) together with **that check's
/// [`noeta_check::Sites`]**.
///
/// "Exactly as the boot linked it" is literal since audit-10: `front` is the boot's own
/// [`noeta_runner::compile::FrontFacts`], reused rather than re-resolved. It used to call
/// `resolve_graph` itself on every edit, which was wrong in three ways and slow in a fourth. It
/// **re-solved the dependency graph per keystroke-save**; with `[trust].require_transparency` set
/// that solve is by definition a live registry round trip, so a hot reload made a network call per
/// save and a transient failure came back as `Unreadable` — "no verdict", printed and skipped, the
/// developer's edit silently discarded. It refreshed `noeta.lock` on that path (harmless only
/// because the writer skips an unchanged write). And it could in principle link the swap candidate
/// against a *different* graph than the running program was built from. None of that buys any
/// freshness: a change to `noeta.toml`, `noeta.lock`, or any file but the entry never reaches here
/// at all — the watcher above exits with [`HOT_RESTART_CODE`] and the wrapper restarts the process,
/// which resolves the graph again from scratch. Dependency freshness is the *restart's* job.
///
/// The bundle is not a by-product: it is the codegen half of the check the gate already runs. A
/// swap installed without it compiles checkerless — no packed-list layouts, no `type_of` fidelity,
/// no decode recipes, conservative destructor relevance — so a served program would drift away
/// from its own cold start with every edit. It travels to the install with the plan
/// ([`noeta_vm::HotFragment::sites`]).
///
/// Linking is the point. A module's path derives from its file, so the entry's `fn fetch` is bound
/// as `pkg.main.fetch` in the running module, and its call to a sibling module is α-renamed to that
/// sibling's canonical name. A raw per-file *parse* of the entry sees neither: its plain `fetch`
/// would install into a fresh global slot and the live handler would keep serving the old body —
/// a swap that reports success and changes nothing. Qualification is the linker's job, so the hot
/// path re-runs the linker rather than approximating it.
///
/// Checking the whole program (not the entry alone) is the same trade the other way: package
/// provenance, per-source editions, and every module's diagnostics are what the transactional gate
/// is supposed to gate on.
fn relink_entry_unit(
    entry: &Path,
    tail: &[noeta_ast::Stmt],
    front: &std::sync::Arc<noeta_runner::compile::FrontFacts>,
) -> Result<(EntryUnit, noeta_check::Sites), RelinkError> {
    let program = crate::context::load_entry_with_tail(
        entry,
        tail,
        crate::context::Front::Given(std::sync::Arc::clone(front)),
    )
    .map_err(RelinkError::from_link)?;
    let (loaded, entry_source, _) = program.into_loaded();
    let checked = loaded.check();
    // Only errors block a hot swap. A warning still reaches the developer — printed here, since the
    // edit that introduced it is the moment it is worth seeing — but the edit swaps in regardless.
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return Err(RelinkError::Diagnostics(noeta_diagnostics::render_mapped(
            &loaded.sources,
            checked.diagnostics.iter(),
        )));
    }
    if !checked.diagnostics.is_empty() {
        eprint!(
            "{}",
            noeta_diagnostics::render_mapped(&loaded.sources, checked.diagnostics.iter())
        );
    }
    Ok((EntryUnit::of(&loaded.program, &entry_source), checked.sites))
}

#[cfg(test)]
mod tests {
    use super::*;

    // No expansion reads in play for the source/manifest/hidden cases — the second argument only
    // adds paths, it never removes one.
    const NO_READS: &[PathBuf] = &[];

    #[test]
    fn source_and_manifest_paths_are_relevant() {
        assert!(relevant_path(Path::new("src/main.noe"), NO_READS));
        assert!(relevant_path(Path::new("/abs/dir/app.noe"), NO_READS));
        assert!(relevant_path(Path::new("noeta.toml"), NO_READS));
        assert!(relevant_path(Path::new("deps/noeta.lock"), NO_READS));
    }

    #[test]
    fn hidden_dirs_backups_and_foreign_files_are_not() {
        assert!(!relevant_path(Path::new(".git/objects/ab.noe"), NO_READS));
        assert!(!relevant_path(Path::new("src/.cache/x.noe"), NO_READS));
        assert!(!relevant_path(Path::new("src/main.noe~"), NO_READS));
        assert!(!relevant_path(Path::new("src/lib.rs"), NO_READS));
        assert!(!relevant_path(Path::new("Cargo.toml"), NO_READS));
    }

    #[test]
    fn relative_prefixes_do_not_count_as_hidden() {
        assert!(relevant_path(Path::new("./src/main.noe"), NO_READS));
        assert!(relevant_path(Path::new("../sibling/app.noe"), NO_READS));
    }

    /// A driver's synthesized entry call: one statement stamped at offset 0 of the entry source,
    /// exactly as `serve`'s `entry_tail` builds it.
    fn fake_tail() -> Vec<noeta_ast::Stmt> {
        vec![noeta_ast::Stmt::Echo {
            value: noeta_ast::Expr::Int {
                value: 7,
                span: noeta_span::Span::empty_at(0),
            },
            span: noeta_span::Span::empty_at(0),
        }]
    }

    /// The boot's resolved front for a fixture — what the serve boot hands the watcher. Resolved
    /// once here exactly as the boot resolves it, so a test re-link runs the production path.
    fn boot_front(entry: &Path) -> std::sync::Arc<noeta_runner::compile::FrontFacts> {
        std::sync::Arc::new(
            noeta_runner::compile::resolve_front_with(entry, &[], &None, None)
                .expect("the fixture's dependency graph resolves"),
        )
    }

    fn fn_names(program: &noeta_ast::Program) -> Vec<String> {
        program
            .stmts
            .iter()
            .filter_map(|s| match s {
                noeta_ast::Stmt::Fn(decl) => Some(decl.name.as_str().to_string()),
                _ => None,
            })
            .collect()
    }

    /// The defect this whole path exists to prevent: inside a package, a module's path derives from
    /// its file, so the running module binds the entry's `fn fetch` as `hotpkg.main.fetch`. A swap
    /// fragment carrying a *plain* `fetch` installs into a fresh slot — the live handler keeps
    /// serving the old body while the watcher reports success. The entry unit must therefore come
    /// back qualified, and its call to a sibling module α-renamed the same way.
    #[test]
    fn a_packaged_entry_relinks_to_the_qualified_names_the_running_module_bound() {
        let dir = noeta_test_temp::TempDir::new("hot-relink-package");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("noeta.toml"),
            "[package]\nname = \"local/hotpkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/greet.noe"),
            "pub fn greet(): string {\n    return \"hello\"\n}\n",
        )
        .unwrap();
        let entry = dir.join("src/main.noe");
        std::fs::write(
            &entry,
            "use hotpkg.greet.greet\n\nfn body(): string {\n    return greet()\n}\n",
        )
        .unwrap();

        let (unit, _sites) = match relink_entry_unit(&entry, &fake_tail(), &boot_front(&entry)) {
            Ok(checked) => checked,
            Err(err) => panic!("the fixture should link green, got:\n{}", err.text()),
        };
        assert_eq!(
            fn_names(&unit.program),
            vec!["hotpkg.main.body".to_string()]
        );
        // The sibling module's declaration belongs to the linked program, not to the unit being
        // diffed: only the edited file's statements are two versions of one text.
        assert!(!unit.src.contains("hello"));
        assert!(unit.src.contains("fn body"));
    }

    /// The synthesized entry call is not in the file the user edits: it has no text to fingerprint,
    /// and a re-running swap that replayed it would start a second server.
    #[test]
    fn the_drivers_synthesized_entry_call_is_not_part_of_the_unit() {
        let dir = noeta_test_temp::TempDir::new("hot-relink-tail");
        let entry = dir.join("app.noe");
        std::fs::write(&entry, "fn body(): int {\n    return 1\n}\n").unwrap();

        let (unit, _sites) = match relink_entry_unit(&entry, &fake_tail(), &boot_front(&entry)) {
            Ok(checked) => checked,
            Err(err) => panic!("the fixture should link green, got:\n{}", err.text()),
        };
        assert!(
            !unit
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, noeta_ast::Stmt::Echo { .. })),
            "the tail leaked into the diffed unit: {:?}",
            unit.program.stmts
        );
        // A bare entry (no manifest) is nobody's module: its names stay unqualified, as the
        // running module binds them.
        assert_eq!(fn_names(&unit.program), vec!["body".to_string()]);
    }

    /// Red code never reaches the differ — the transactional gate reports and keeps serving.
    #[test]
    fn a_red_entry_comes_back_as_diagnostics_not_a_unit() {
        let dir = noeta_test_temp::TempDir::new("hot-relink-red");
        let entry = dir.join("app.noe");
        std::fs::write(&entry, "fn body(): int {\n    return nope\n}\n").unwrap();

        match relink_entry_unit(&entry, &[], &boot_front(&entry)) {
            Err(RelinkError::Diagnostics(rendered)) => assert!(rendered.contains("nope")),
            Err(RelinkError::Unreadable(text)) => panic!("expected diagnostics, got: {text}"),
            Ok(_) => panic!("red code linked green"),
        }
    }

    #[test]
    fn a_reported_read_is_relevant_even_though_it_is_not_a_noe_file() {
        // The reason the `reads` argument exists (directive-expansion arc): an OpenAPI spec a hook
        // read is not a `.noe` file and matches no manifest name, so it would be filtered out — but
        // editing it must rebuild the generated client. A path listed in `reads` is relevant.
        let reads = [PathBuf::from("api/petstore.json")];
        assert!(relevant_path(Path::new("api/petstore.json"), &reads));
        // A foreign file NOT among the reads stays irrelevant — the branch adds only what was read.
        assert!(!relevant_path(Path::new("api/other.json"), &reads));
    }
}
