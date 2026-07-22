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
/// edit of `entry` it parses, checks (transactional: red code is reported and the old version
/// keeps serving), diffs against the last version the VM consumed, and deposits swappable plans
/// into `mailbox` — the run thread applies them at its next scheduler tick. Anything the live
/// process cannot absorb — a [`SwapBlocker`], a change to any *other* project file, an entry file
/// that declares a `namespace` (its definitions get qualified identity the raw parse won't match)
/// — exits the process with [`HOT_RESTART_CODE`] so the `--watch` wrapper restarts it.
pub(crate) fn spawn_hot_watcher(
    entry: std::path::PathBuf,
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_host_real::Notify>,
) {
    std::thread::spawn(move || hot_watcher(entry, mailbox, wake));
}

fn hot_watcher(
    entry: std::path::PathBuf,
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_host_real::Notify>,
) {
    let entry_canon = entry.canonicalize().unwrap_or_else(|_| entry.clone());
    // Files an expansion hook read (an `@openapi` spec). A change to one is never entry-swappable —
    // it regenerates members — so it must reach the `all_entry` check below and force a restart,
    // which means passing the event filter first. Computed once: the hot process is restarted
    // wholesale (and this recomputed) whenever the entry itself changes.
    let reads = noeta_ide::impact::spec_reads(&entry);
    // The baseline: the source that is currently RUNNING (read back at spawn — the run thread
    // just compiled exactly this file).
    let Ok(mut applied_src) = std::fs::read_to_string(&entry) else {
        eprintln!(
            "[hot] cannot re-read {} — falling back to restarts",
            entry.display()
        );
        return;
    };
    // A namespaced entry's definitions carry qualified identity through the linker; the raw
    // per-file diff would rebind unqualified names. Restart-only until the differ learns
    // qualification.
    let hot_capable = parse_entry(&entry, &applied_src).is_some_and(|p| {
        !p.stmts
            .iter()
            .any(|s| matches!(s, noeta_ast::Stmt::Namespace { .. }))
    });

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
        if !all_entry || !hot_capable {
            eprintln!("[hot] change outside the entry file — restarting");
            std::process::exit(HOT_RESTART_CODE);
        }
        let Ok(new_src) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let Some(new_program) = parse_entry(&entry, &new_src) else {
            eprintln!("[hot] parse error — still serving the old version");
            continue;
        };
        // The transactional gate: red code never swaps; the old version keeps serving. The
        // rendered diagnostics also ride the channel's error slot to live LiveView clients
        // (the browser overlay, server-hmr L3) — waking the run thread to deliver promptly.
        // The hot-reparsed entry is one source (id 0), checked under the entry package's edition.
        let mut editions = noeta_lexer::EditionMap::new();
        editions.set(
            noeta_span::SourceId::FIRST,
            noeta_pm::manifest::root_edition(&entry),
        );
        let checked = crate::context::check_under(&new_program, &editions);
        if !checked.diagnostics.is_empty() {
            let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "<entry>", &new_src);
            let mut rendered = String::new();
            for d in &checked.diagnostics {
                let one = crate::render(&source, d);
                eprint!("{one}");
                rendered.push_str(&one);
            }
            eprintln!("[hot] check failed — still serving the old version");
            if let Ok(mut slot) = mailbox.error.lock() {
                *slot = Some(rendered);
            }
            wake_all(&wake);
            continue;
        }
        // Diff against the last version this watcher DEPOSITED (server-hmr F5). The watcher is the
        // single depositor, so `applied_src` is the exact baseline of the append-only broadcast
        // queue: each plan is diffed against its predecessor, and every worker applies the queue
        // in order — no per-consumer reconciliation.
        let Some(applied_program) = parse_entry(&entry, &applied_src) else {
            std::process::exit(HOT_RESTART_CODE);
        };
        match noeta_compiler::hotswap::diff_programs(
            &applied_program,
            &applied_src,
            &new_program,
            &new_src,
        ) {
            noeta_compiler::hotswap::SwapDiff::Unchanged => {}
            noeta_compiler::hotswap::SwapDiff::Swap(plan) => {
                // Convert the compiler's `SwapPlan` into the VM's compiler-free `HotFragment` at the
                // boundary (native-size slice 2): the watcher owns the compiler, the VM must not.
                let fragment = noeta_vm::HotFragment {
                    fragment: plan.fragment,
                    rerun_top_level: plan.rerun_top_level,
                    added: plan.added,
                    changed: plan.changed,
                };
                match mailbox.plans.lock() {
                    Ok(mut plans) => plans.push(fragment),
                    Err(_) => return,
                }
                applied_src = new_src;
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
fn wake_all(wake: &noeta_host_real::Notify) {
    wake.notify_waiters();
    wake.notify_one();
}

fn parse_entry(entry: &Path, src: &str) -> Option<noeta_ast::Program> {
    let source = noeta_span::Source::new(
        noeta_span::SourceId::FIRST,
        entry.display().to_string(),
        src,
    );
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    (lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty()).then_some(parsed.program)
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
